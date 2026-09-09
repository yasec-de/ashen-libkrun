//! Turning an OCI image into something a microVM can boot.
//!
//! libkrun boots from a directory exported over virtiofs, so the image has to
//! be flattened onto the filesystem first. `podman`/`docker export` gives us
//! exactly that, and `image inspect` gives the process configuration that the
//! image's own metadata carries (entrypoint, command, environment, workdir).
//!
//! Files are extracted without restoring ownership: the tool runs unprivileged
//! and the extracted tree is only ever read by our own virtiofs daemon, which
//! serves it to a guest that has its own idea of uid 0 anyway.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{env, fs};

/// The container engine used to materialize images.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Podman,
    Docker,
}

impl Engine {
    fn binary(&self) -> &'static str {
        match self {
            Engine::Podman => "podman",
            Engine::Docker => "docker",
        }
    }

    /// Pick whichever engine is installed, preferring podman for its rootless
    /// default.
    pub fn detect() -> Result<Self> {
        for engine in [Engine::Podman, Engine::Docker] {
            if which(engine.binary()).is_some() {
                return Ok(engine);
            }
        }
        bail!("neither podman nor docker found in PATH; needed to unpack container images")
    }

    pub fn from_name(name: &str) -> Result<Self> {
        match name {
            "podman" => Ok(Engine::Podman),
            "docker" => Ok(Engine::Docker),
            other => bail!("unknown container engine {other:?} (expected podman or docker)"),
        }
    }
}

fn which(binary: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

/// The parts of an OCI image config that decide what the guest runs.
///
/// This is the *image* config (`.Config` of `image inspect`), not a runtime
/// spec, so the fields are the image's defaults rather than a resolved
/// container.
#[derive(Debug, Default, Deserialize)]
pub struct ImageConfig {
    #[serde(rename = "Entrypoint", default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd", default)]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "Env", default)]
    pub env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir", default)]
    pub working_dir: Option<String>,
}

impl ImageConfig {
    /// Resolve the argv to run, following OCI semantics: a user-supplied
    /// command replaces `Cmd` but is still appended to `Entrypoint`.
    pub fn argv(&self, user_cmd: &[String]) -> Result<Vec<String>> {
        let mut argv = self.entrypoint.clone().unwrap_or_default();
        if user_cmd.is_empty() {
            argv.extend(self.cmd.clone().unwrap_or_default());
        } else {
            argv.extend_from_slice(user_cmd);
        }

        if argv.is_empty() {
            bail!("image declares neither Entrypoint nor Cmd; pass the command to run explicitly");
        }
        Ok(argv)
    }

    pub fn workdir(&self) -> &str {
        match self.working_dir.as_deref() {
            Some(dir) if !dir.is_empty() => dir,
            _ => "/",
        }
    }
}

/// An image unpacked into a directory, removed again on drop.
pub struct Rootfs {
    path: PathBuf,
    pub config: ImageConfig,
}

impl Rootfs {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Rootfs {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.path) {
            eprintln!(
                "krunwipe: warning: could not remove {}: {e}",
                self.path.display()
            );
        }
    }
}

/// Run a command, returning stdout, and failing with its stderr.
fn capture(engine: Engine, args: &[&str]) -> Result<String> {
    let output = Command::new(engine.binary())
        .args(args)
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("running {}", engine.binary()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{} {} failed: {}",
            engine.binary(),
            args.join(" "),
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Unpack `image` into a fresh directory under `parent`.
///
/// Creating a container and exporting it flattens the layers for us and pulls
/// the image if it is not present yet.
pub fn unpack(engine: Engine, image: &str, parent: &Path) -> Result<Rootfs> {
    // Create first: it pulls the image if it is not present locally, which
    // `image inspect` on its own would not do.
    let container = capture(engine, &["create", image])
        .with_context(|| format!("creating a container from {image}"))?;

    // From here on the container must be removed whatever happens.
    let result = (|| {
        let config_json = capture(
            engine,
            &["image", "inspect", "--format", "{{json .Config}}", image],
        )
        .with_context(|| format!("inspecting image {image}"))?;
        let config: ImageConfig = serde_json::from_str(&config_json)
            .with_context(|| format!("parsing image config for {image}"))?;

        let path = export_into(engine, &container, parent)?;
        Ok(Rootfs { path, config })
    })();
    let _ = capture(engine, &["rm", "-f", &container]);

    result
}

fn export_into(engine: Engine, container: &str, parent: &Path) -> Result<PathBuf> {
    let path = parent.join(format!("krunwipe-rootfs-{container}"));
    fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;

    let mut export = Command::new(engine.binary())
        .args(["export", container])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {} export", engine.binary()))?;

    let stdout = export.stdout.take().expect("stdout was piped");
    // --no-same-owner: we are unprivileged, and the guest reinterprets
    // ownership anyway. Without it tar fails on every root-owned file.
    let tar = Command::new("tar")
        .args(["-x", "--no-same-owner", "-f", "-", "-C"])
        .arg(&path)
        .stdin(stdout)
        .status()
        .context("running tar to extract the image")?;

    let export_status = export.wait().context("waiting for export")?;

    if !export_status.success() {
        let _ = fs::remove_dir_all(&path);
        bail!("{} export failed", engine.binary());
    }
    if !tar.success() {
        let _ = fs::remove_dir_all(&path);
        bail!("tar failed to extract the image ({tar})");
    }

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ImageConfig {
        ImageConfig {
            entrypoint: Some(vec!["/entry".into()]),
            cmd: Some(vec!["default".into()]),
            env: None,
            working_dir: Some("/app".into()),
        }
    }

    #[test]
    fn user_command_replaces_cmd_but_keeps_entrypoint() {
        let argv = config().argv(&["run".to_string()]).unwrap();
        assert_eq!(argv, vec!["/entry", "run"]);
    }

    #[test]
    fn no_user_command_uses_cmd() {
        let argv = config().argv(&[]).unwrap();
        assert_eq!(argv, vec!["/entry", "default"]);
    }

    #[test]
    fn empty_image_config_needs_an_explicit_command() {
        assert!(ImageConfig::default().argv(&[]).is_err());
        assert_eq!(
            ImageConfig::default().argv(&["sh".to_string()]).unwrap(),
            vec!["sh"]
        );
    }

    #[test]
    fn workdir_defaults_to_root() {
        assert_eq!(config().workdir(), "/app");
        assert_eq!(ImageConfig::default().workdir(), "/");
        let blank = ImageConfig {
            working_dir: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(blank.workdir(), "/");
    }
}
