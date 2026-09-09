//! Run a workload in a libkrun microVM whose entire RAM is described to the
//! host's kdump kernel as holding secrets, so that it never reaches a vmcore
//! untouched.
//!
//! The marking itself lives in libkrun (`krun_set_crash_memaction`), which
//! marks every region of guest RAM with `MADV_CRASH_SECRET` / `MADV_CRASH_CACHE`
//! as soon as the memory is mapped and before any vCPU runs. This tool is the
//! thing that decides *what* to run in that VM:
//!
//!   * `python` -- a bare script, with the host's own filesystem as the guest
//!     root, so the guest just has the host's Python. No image, no registry.
//!   * `container` -- an OCI image, flattened into a directory and booted with
//!     the entrypoint, command, environment and workdir the image declares.
//!   * `run` -- an arbitrary command in a rootfs directory you already have.
//!
//! A host kernel that does not track the type asked for drops the request
//! rather than refusing it, so the VM starts either way and `--verify` is what
//! tells the two apart.

mod krun;
mod oci;
mod verify;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{env, io, ptr, thread};

/// What this VM's RAM holds, as the host's kdump kernel sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CrashMemaction {
    /// Guest RAM holds secrets.
    Secret,
    /// Guest RAM holds nothing worth dumping.
    Cache,
    /// Both.
    SecretCache,
    /// Say nothing about guest RAM.
    None,
}

impl CrashMemaction {
    fn bits(self) -> u32 {
        match self {
            CrashMemaction::Secret => krun::KRUN_CRASH_MEMACTION_SECRET,
            CrashMemaction::Cache => krun::KRUN_CRASH_MEMACTION_CACHE,
            CrashMemaction::SecretCache => {
                krun::KRUN_CRASH_MEMACTION_SECRET | krun::KRUN_CRASH_MEMACTION_CACHE
            }
            CrashMemaction::None => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum KernelFormat {
    Raw,
    Elf,
    PeGz,
    ImageBz2,
    ImageGz,
    ImageZstd,
}

impl KernelFormat {
    fn bits(self) -> u32 {
        match self {
            KernelFormat::Raw => krun::KRUN_KERNEL_FORMAT_RAW,
            KernelFormat::Elf => krun::KRUN_KERNEL_FORMAT_ELF,
            KernelFormat::PeGz => krun::KRUN_KERNEL_FORMAT_PE_GZ,
            KernelFormat::ImageBz2 => krun::KRUN_KERNEL_FORMAT_IMAGE_BZ2,
            KernelFormat::ImageGz => krun::KRUN_KERNEL_FORMAT_IMAGE_GZ,
            KernelFormat::ImageZstd => krun::KRUN_KERNEL_FORMAT_IMAGE_ZSTD,
        }
    }

    /// The uncompressed image format each architecture boots.
    fn default_for_host() -> Self {
        if cfg!(target_arch = "x86_64") {
            KernelFormat::Elf
        } else {
            KernelFormat::Raw
        }
    }
}

#[derive(Debug, Args)]
struct Common {
    /// Number of vCPUs.
    #[arg(long, default_value_t = 2, global = true)]
    cpus: u8,

    /// Guest RAM in MiB. All of it is marked.
    #[arg(long, default_value_t = 512, global = true)]
    mem: u32,

    /// What this VM's RAM holds, as the host's kdump kernel sees it.
    #[arg(long, value_enum, default_value_t = CrashMemaction::Secret, global = true)]
    crash_memaction: CrashMemaction,

    /// Guest kernel image. Required unless libkrunfw is installed.
    #[arg(long, global = true)]
    kernel: Option<PathBuf>,

    /// Format of --kernel.
    #[arg(long, value_enum, global = true)]
    kernel_format: Option<KernelFormat>,

    /// Initramfs to boot with --kernel. Mostly useful for bringing up a guest
    /// kernel that cannot mount the virtiofs root on its own.
    #[arg(long, global = true)]
    initramfs: Option<PathBuf>,

    /// Extra guest kernel command line arguments.
    #[arg(long, global = true)]
    kernel_cmdline: Option<String>,

    /// Give the guest network access via libkrun's TSI vsock.
    #[arg(long, global = true)]
    net: bool,

    /// Read the crash marks back out of /proc/self/smaps once the VM is up.
    #[arg(long, global = true)]
    verify: bool,

    /// libkrun log verbosity.
    #[arg(long, value_parser = ["off", "error", "warn", "info", "debug", "trace"],
          default_value = "warn", global = true)]
    log_level: String,
}

impl Common {
    fn log_level_bits(&self) -> u32 {
        match self.log_level.as_str() {
            "off" => krun::KRUN_LOG_LEVEL_OFF,
            "error" => krun::KRUN_LOG_LEVEL_ERROR,
            "info" => krun::KRUN_LOG_LEVEL_INFO,
            "debug" => krun::KRUN_LOG_LEVEL_DEBUG,
            "trace" => krun::KRUN_LOG_LEVEL_TRACE,
            _ => krun::KRUN_LOG_LEVEL_WARN,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "krunwipe",
    about = "Run a Python script or a container in a libkrun microVM whose RAM is tagged for wiping on kdump",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Mode,

    #[command(flatten)]
    common: Common,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Run a Python script, using the host's filesystem as the guest root.
    Python {
        /// The script to run.
        script: PathBuf,

        /// Arguments passed to the script.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,

        /// Python interpreter to run inside the guest.
        #[arg(long, default_value = "python3")]
        interpreter: String,

        /// Directory to expose as the guest root.
        #[arg(long, default_value = "/")]
        rootfs: PathBuf,
    },

    /// Run an OCI container image.
    Container {
        /// Image reference, e.g. docker.io/library/python:3-slim.
        image: String,

        /// Command to run instead of the image's Cmd.
        #[arg(trailing_var_arg = true)]
        cmd: Vec<String>,

        /// Container engine used to unpack the image.
        #[arg(long, value_parser = ["podman", "docker"])]
        runtime: Option<String>,

        /// Where to unpack the image. Defaults to $TMPDIR.
        #[arg(long)]
        unpack_dir: Option<PathBuf>,
    },

    /// Run a command in a rootfs directory you already have.
    Run {
        /// Directory to expose as the guest root.
        rootfs: PathBuf,

        /// Command and arguments.
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
}

/// Everything the VM needs, once the mode has been resolved.
struct Workload {
    rootfs: PathBuf,
    argv: Vec<String>,
    env: Vec<String>,
    workdir: String,
    /// Kept alive so the unpacked image outlives the VM.
    _rootfs_guard: Option<oci::Rootfs>,
}

/// A minimal environment for guests that bring no configuration of their own.
fn default_env() -> Vec<String> {
    vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        "HOME=/root".to_string(),
        "TERM=".to_string() + &env::var("TERM").unwrap_or_else(|_| "linux".to_string()),
    ]
}

fn resolve_workload(mode: Mode) -> Result<Workload> {
    match mode {
        Mode::Python {
            script,
            args,
            interpreter,
            rootfs,
        } => {
            // The guest sees the host's filesystem at the same paths, so an
            // absolute host path is also the guest path. Resolve it here so a
            // relative path on the command line still works.
            let script = script
                .canonicalize()
                .with_context(|| format!("cannot find script {}", script.display()))?;
            if !script.is_file() {
                bail!("{} is not a file", script.display());
            }

            let guest_script = guest_path(&script, &rootfs).with_context(|| {
                format!(
                    "{} is not inside the guest root {}",
                    script.display(),
                    rootfs.display()
                )
            })?;

            let mut argv = vec![interpreter, guest_script];
            argv.extend(args);

            // Run where the user is, if that path exists in the guest too.
            let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            let workdir = guest_path(&cwd, &rootfs).unwrap_or_else(|_| "/".to_string());

            Ok(Workload {
                rootfs,
                argv,
                env: default_env(),
                workdir,
                _rootfs_guard: None,
            })
        }

        Mode::Container {
            image,
            cmd,
            runtime,
            unpack_dir,
        } => {
            let engine = match runtime {
                Some(name) => oci::Engine::from_name(&name)?,
                None => oci::Engine::detect()?,
            };
            let parent = unpack_dir.unwrap_or_else(env::temp_dir);
            let unpacked = oci::unpack(engine, &image, &parent)?;

            let argv = unpacked.config.argv(&cmd)?;
            let env = match unpacked.config.env.clone() {
                Some(env) if !env.is_empty() => env,
                _ => default_env(),
            };
            let workdir = unpacked.config.workdir().to_string();

            Ok(Workload {
                rootfs: unpacked.path().to_path_buf(),
                argv,
                env,
                workdir,
                _rootfs_guard: Some(unpacked),
            })
        }

        Mode::Run { rootfs, argv } => Ok(Workload {
            rootfs,
            argv,
            env: default_env(),
            workdir: "/".to_string(),
            _rootfs_guard: None,
        }),
    }
}

/// Translate a host path into the path the guest will see it at, given which
/// host directory is exposed as the guest root.
fn guest_path(host: &Path, rootfs: &Path) -> Result<String> {
    let rootfs = rootfs
        .canonicalize()
        .unwrap_or_else(|_| rootfs.to_path_buf());
    let relative = host
        .strip_prefix(&rootfs)
        .with_context(|| format!("{} is outside {}", host.display(), rootfs.display()))?;

    let relative = relative.to_string_lossy();
    if relative.is_empty() {
        // The path *is* the exposed root.
        return Ok("/".to_string());
    }
    Ok(format!("/{}", relative.trim_end_matches('/')))
}

/// Report what the kernel says about our mappings, once the guest is running.
///
/// libkrun does not return from `krun_start_enter`, so this runs on its own
/// thread. Guest memory is mapped and marked early in VM construction, but a
/// short-lived guest can be gone again in well under a second, so poll for the
/// marks ratherthan  sleeping past the whole VM's lifetime.
fn spawn_verifier() {
    const POLL_INTERVAL: Duration = Duration::from_millis(20);
    const DEADLINE: Duration = Duration::from_secs(5);

    thread::spawn(move || {
        let started = Instant::now();
        let first = loop {
            match verify::read_marks() {
                Ok(marks) if !marks.is_empty() => break Ok(marks),
                other if started.elapsed() >= DEADLINE => break other,
                _ => thread::sleep(POLL_INTERVAL),
            }
        };

        let first = match first {
            Ok(marks) if marks.is_empty() => {
                eprintln!(
                    "krunwipe: verify: no crash-marked mappings in /proc/self/smaps -- this \
                     host does not track the requested type. It needs CONFIG_CRASH_MEMACTION \
                     and a crash_memaction= boot parameter naming it; \
                     /sys/kernel/crash_memaction lists what it does track"
                );
                return;
            }
            Ok(marks) => marks,
            Err(e) => {
                eprintln!("krunwipe: verify: {e:#}");
                return;
            }
        };

        // One flag covers both types, so smaps cannot say which of them the
        // host tracked -- only that something was marked.
        eprintln!("krunwipe: verify: at guest start: {first}");

        // Keep sampling: the marks are set on the VMA before the guest has
        // faulted anything in, so watching resident pages climb while the
        // marked size stays put is what shows that pages arriving later are
        // covered too, without any of it having been pinned up front.
        let mut peak = first;
        while started.elapsed() < DEADLINE {
            thread::sleep(POLL_INTERVAL);
            match verify::read_marks() {
                Ok(marks) if marks.is_empty() => break,
                Ok(marks) => {
                    if marks.resident_kib > peak.resident_kib {
                        peak = marks;
                    }
                }
                Err(_) => break,
            }
        }

        if peak.resident_kib > first.resident_kib {
            eprintln!("krunwipe: verify: peak:           {peak}");
        }
    });
}

fn raw(stream: &impl AsRawFd) -> RawFd {
    stream.as_raw_fd()
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let common = cli.common;

    if common.mem == 0 {
        bail!("--mem must be greater than zero");
    }
    if common.cpus == 0 {
        bail!("--cpus must be greater than zero");
    }

    let workload = resolve_workload(cli.command)?;

    if !workload.rootfs.is_dir() {
        bail!(
            "guest root {} is not a directory",
            workload.rootfs.display()
        );
    }

    krun::init_log(common.log_level_bits())?;

    let ctx = krun::Ctx::new()?;
    ctx.set_vm_config(common.cpus, common.mem)?;

    let crash_bits = common.crash_memaction.bits();
    ctx.set_crash_memaction(crash_bits)?;

    ctx.add_console_default(raw(&io::stdin()), raw(&io::stdout()), raw(&io::stderr()))?;

    ctx.add_virtiofs(krun::KRUN_FS_ROOT_TAG, &workload.rootfs, false)?;

    if let Some(kernel) = &common.kernel {
        let format = common
            .kernel_format
            .unwrap_or_else(KernelFormat::default_for_host);
        ctx.set_kernel(
            kernel,
            format.bits(),
            common.initramfs.as_deref(),
            common.kernel_cmdline.as_deref(),
        )?;
    } else if common.initramfs.is_some() {
        bail!("--initramfs requires --kernel");
    }

    if common.net {
        ctx.add_vsock(krun::KRUN_TSI_HIJACK_INET | krun::KRUN_TSI_HIJACK_UNIX)?;
    }

    // Hand the workload to libkrun's guest init: the init binary and a
    // /.krun_config.json describing the process are injected into the root
    // filesystem as virtiofs overlay files, so nothing is written to the
    // host's tree.
    let argv: Vec<&str> = workload.argv.iter().map(String::as_str).collect();
    let env: Vec<&str> = workload.env.iter().map(String::as_str).collect();
    let init_config = krun_init::Config::builder()
        .args(&argv)
        .env(&env)
        .workdir(&workload.workdir)
        .build();
    // SAFETY: a null library handle means "search the global symbol
    // namespace", which works because we link libkrun directly. init_config
    // is kept alive below for as long as the VM runs.
    unsafe { init_config.apply(ptr::null_mut(), ctx.0, krun::KRUN_FS_ROOT_TAG) }
        .map_err(|e| anyhow::anyhow!("applying init config: {e}"))?;

    if crash_bits != 0 {
        eprintln!(
            "krunwipe: {} MiB of guest RAM marked {}",
            common.mem,
            describe(crash_bits)
        );
    }

    if common.verify {
        spawn_verifier();
    }

    // Never returns: libkrun runs the guest here and exits with its status.
    ctx.start_enter()?;
    unreachable!()
}

fn describe(bits: u32) -> String {
    let mut parts = Vec::new();
    if bits & krun::KRUN_CRASH_MEMACTION_SECRET != 0 {
        parts.push("secret");
    }
    if bits & krun::KRUN_CRASH_MEMACTION_CACHE != 0 {
        parts.push("cache");
    }
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_bits_follow_the_type() {
        assert_eq!(
            CrashMemaction::Secret.bits(),
            krun::KRUN_CRASH_MEMACTION_SECRET
        );
        assert_eq!(
            CrashMemaction::SecretCache.bits(),
            krun::KRUN_CRASH_MEMACTION_SECRET | krun::KRUN_CRASH_MEMACTION_CACHE
        );
        assert_eq!(CrashMemaction::None.bits(), 0);
    }

    #[test]
    fn guest_paths_are_relative_to_the_exposed_root() {
        assert_eq!(
            guest_path(Path::new("/tmp/x/script.py"), Path::new("/")).unwrap(),
            "/tmp/x/script.py"
        );
        assert_eq!(
            guest_path(Path::new("/srv/root/app/main.py"), Path::new("/srv/root")).unwrap(),
            "/app/main.py"
        );
        assert!(guest_path(Path::new("/etc/passwd"), Path::new("/srv/root")).is_err());
    }

    #[test]
    fn describes_the_requested_types() {
        assert_eq!(describe(krun::KRUN_CRASH_MEMACTION_SECRET), "secret");
        assert_eq!(
            describe(krun::KRUN_CRASH_MEMACTION_SECRET | krun::KRUN_CRASH_MEMACTION_CACHE),
            "secret+cache"
        );
    }
}
