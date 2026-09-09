//! The slice of libkrun's C API that krunwipe uses, plus safe wrappers.
//!
//! Declared by hand rather than through krun-sys so that the tool builds
//! straight out of the libkrun tree, without a `make install` first.

use anyhow::{Context, Result, bail};
use std::convert::Infallible;
use std::ffi::{CString, c_char, c_int};
use std::path::Path;
use std::{io, ptr};

pub const KRUN_LOG_TARGET_DEFAULT: c_int = -1;
pub const KRUN_LOG_STYLE_AUTO: u32 = 0;

pub const KRUN_LOG_LEVEL_OFF: u32 = 0;
pub const KRUN_LOG_LEVEL_ERROR: u32 = 1;
pub const KRUN_LOG_LEVEL_WARN: u32 = 2;
pub const KRUN_LOG_LEVEL_INFO: u32 = 3;
pub const KRUN_LOG_LEVEL_DEBUG: u32 = 4;
pub const KRUN_LOG_LEVEL_TRACE: u32 = 5;

/// The virtiofs tag libkrun boots its root filesystem from.
pub const KRUN_FS_ROOT_TAG: &str = "/dev/root";

/// Guest RAM holds secrets.
pub const KRUN_CRASH_MEMACTION_SECRET: u32 = 1 << 0;
/// Guest RAM holds nothing worth dumping.
pub const KRUN_CRASH_MEMACTION_CACHE: u32 = 1 << 1;

pub const KRUN_KERNEL_FORMAT_RAW: u32 = 0;
pub const KRUN_KERNEL_FORMAT_ELF: u32 = 1;
pub const KRUN_KERNEL_FORMAT_PE_GZ: u32 = 2;
pub const KRUN_KERNEL_FORMAT_IMAGE_BZ2: u32 = 3;
pub const KRUN_KERNEL_FORMAT_IMAGE_GZ: u32 = 4;
pub const KRUN_KERNEL_FORMAT_IMAGE_ZSTD: u32 = 5;

pub const KRUN_TSI_HIJACK_INET: u32 = 1 << 0;
pub const KRUN_TSI_HIJACK_UNIX: u32 = 1 << 1;

unsafe extern "C" {
    fn krun_init_log(target_fd: c_int, level: u32, style: u32, options: u32) -> i32;
    fn krun_create_ctx() -> i32;
    fn krun_set_vm_config(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> i32;
    fn krun_set_crash_memaction(ctx_id: u32, types: u32) -> i32;
    fn krun_add_virtiofs3(
        ctx_id: u32,
        c_tag: *const c_char,
        c_path: *const c_char,
        shm_size: u64,
        read_only: bool,
    ) -> i32;
    fn krun_add_virtio_console_default(
        ctx_id: u32,
        input_fd: c_int,
        output_fd: c_int,
        err_fd: c_int,
    ) -> i32;
    fn krun_set_kernel(
        ctx_id: u32,
        kernel_path: *const c_char,
        kernel_format: u32,
        initramfs: *const c_char,
        cmdline: *const c_char,
    ) -> i32;
    fn krun_add_vsock(ctx_id: u32, tsi_features: u32) -> i32;
    fn krun_start_enter(ctx_id: u32) -> i32;
}

/// Turn libkrun's negative-errno return into an error naming the call.
fn check(ret: i32, what: &str) -> Result<()> {
    if ret < 0 {
        let err = io::Error::from_raw_os_error(-ret);
        bail!("{what}: {err}");
    }
    Ok(())
}

fn cstr(s: &str) -> Result<CString> {
    CString::new(s).with_context(|| format!("{s:?} contains a NUL byte"))
}

fn cpath(p: &Path) -> Result<CString> {
    let s = p
        .to_str()
        .with_context(|| format!("{} is not valid UTF-8", p.display()))?;
    cstr(s)
}

/// A libkrun configuration context.
pub struct Ctx(pub u32);

pub fn init_log(level: u32) -> Result<()> {
    // SAFETY: no pointer arguments.
    check(
        unsafe { krun_init_log(KRUN_LOG_TARGET_DEFAULT, level, KRUN_LOG_STYLE_AUTO, 0) },
        "krun_init_log",
    )
}

impl Ctx {
    pub fn new() -> Result<Self> {
        // SAFETY: no arguments.
        let ret = unsafe { krun_create_ctx() };
        check(ret, "krun_create_ctx")?;
        Ok(Ctx(ret as u32))
    }

    pub fn set_vm_config(&self, num_vcpus: u8, ram_mib: u32) -> Result<()> {
        // SAFETY: no pointer arguments.
        check(
            unsafe { krun_set_vm_config(self.0, num_vcpus, ram_mib) },
            "krun_set_vm_config",
        )
    }

    /// Describe the whole of this microVM's guest RAM to the host's kdump
    /// kernel.
    pub fn set_crash_memaction(&self, types: u32) -> Result<()> {
        // SAFETY: no pointer arguments.
        check(
            unsafe { krun_set_crash_memaction(self.0, types) },
            "krun_set_crash_memaction",
        )
    }

    pub fn add_virtiofs(&self, tag: &str, path: &Path, read_only: bool) -> Result<()> {
        let c_tag = cstr(tag)?;
        let c_path = cpath(path)?;
        // SAFETY: both CStrings outlive the call. libkrun copies them.
        check(
            unsafe { krun_add_virtiofs3(self.0, c_tag.as_ptr(), c_path.as_ptr(), 0, read_only) },
            "krun_add_virtiofs3",
        )
    }

    pub fn add_console_default(&self, stdin: c_int, stdout: c_int, stderr: c_int) -> Result<()> {
        // SAFETY: no pointer arguments; the fds are ours and stay open.
        check(
            unsafe { krun_add_virtio_console_default(self.0, stdin, stdout, stderr) },
            "krun_add_virtio_console_default",
        )
    }

    pub fn set_kernel(
        &self,
        kernel: &Path,
        format: u32,
        initramfs: Option<&Path>,
        cmdline: Option<&str>,
    ) -> Result<()> {
        let c_kernel = cpath(kernel)?;
        let c_initramfs = initramfs.map(cpath).transpose()?;
        let c_cmdline = cmdline.map(cstr).transpose()?;
        // SAFETY: every CString outlives the call; absent ones are passed as
        // NULL, which libkrun documents as "not set".
        check(
            unsafe {
                krun_set_kernel(
                    self.0,
                    c_kernel.as_ptr(),
                    format,
                    c_initramfs.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
                    c_cmdline.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
                )
            },
            "krun_set_kernel",
        )
    }

    pub fn add_vsock(&self, tsi_features: u32) -> Result<()> {
        // SAFETY: no pointer arguments.
        check(
            unsafe { krun_add_vsock(self.0, tsi_features) },
            "krun_add_vsock",
        )
    }

    /// Start the microVM. On success this does not return: libkrun runs the
    /// guest in this process and exits with the workload's status.
    pub fn start_enter(&self) -> Result<Infallible> {
        // SAFETY: no pointer arguments.
        let ret = unsafe { krun_start_enter(self.0) };
        check(ret, "krun_start_enter")?;
        unreachable!("krun_start_enter returned successfully")
    }
}
