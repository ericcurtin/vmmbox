//! On-disk layout.
//!
//! ```text
//! <root>/images/<distro>/<version>/<arch>/{disk.qcow2,meta.json}
//! <root>/vms/<distro>/{disk.qcow2,seed.img,vm.json,id_ed25519,...}
//! <root>/tools/{bin,lib,share}/...   (programs vmmbox installs for itself)
//! ```
//!
//! `<root>` is `$VMMBOX_HOME` if set. Otherwise, on macOS and Linux it follows
//! the XDG base directory convention: `$XDG_DATA_HOME/vmmbox`, defaulting to
//! `~/.local/share/vmmbox` (on macOS too, rather than the space-containing
//! `~/Library/Application Support`). On Windows it is `%LOCALAPPDATA%\vmmbox`.

use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[cfg(unix)]
fn default_data_dir() -> Option<PathBuf> {
    xdg_data_home(std::env::var_os("XDG_DATA_HOME"), dirs::home_dir())
}

#[cfg(windows)]
fn default_data_dir() -> Option<PathBuf> {
    dirs::data_local_dir()
}

/// `$XDG_DATA_HOME` if it is an absolute path (the spec says relative values
/// must be ignored), else `~/.local/share`.
#[cfg_attr(not(unix), allow(dead_code))]
fn xdg_data_home(xdg: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    match xdg.map(PathBuf::from) {
        Some(p) if p.is_absolute() => Some(p),
        _ => home.map(|h| h.join(".local").join("share")),
    }
}

#[derive(Clone, Debug)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let root = match std::env::var_os("VMMBOX_HOME") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => default_data_dir()
                .context("could not determine the user data directory; set VMMBOX_HOME")?
                .join("vmmbox"),
        };
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn images(&self) -> PathBuf {
        self.root.join("images")
    }

    pub fn image_dir(&self, distro: &str, version: &str, arch: &str) -> PathBuf {
        self.images().join(distro).join(version).join(arch)
    }

    /// A private install prefix for programs vmmbox builds or installs itself
    /// (a GPU-enabled QEMU, the GUI compositor): `bin/`, `lib/`, `share/`.
    pub fn tools(&self) -> PathBuf {
        self.root.join("tools")
    }

    pub fn tools_bin(&self) -> PathBuf {
        self.tools().join("bin")
    }

    pub fn vms(&self) -> PathBuf {
        self.root.join("vms")
    }

    pub fn vm_dir(&self, name: &str) -> PathBuf {
        self.vms().join(name)
    }

    /// Directory for QMP sockets. Unix domain socket paths are limited to ~104
    /// bytes, and the data directory on macOS is long, so sockets live in a
    /// short, private (0700) per-user directory instead.
    #[cfg(unix)]
    pub fn runtime_dir(&self) -> Result<PathBuf> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};

        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(p) if !p.is_empty() => PathBuf::from(p).join("vmmbox"),
            _ => PathBuf::from(format!("/tmp/vmmbox-{uid}")),
        };
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
        // /tmp is world-writable: make sure nobody else pre-created this path.
        let meta = std::fs::symlink_metadata(&dir)?;
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            anyhow::bail!(
                "{} exists but is not a private directory owned by you; remove it and retry",
                dir.display()
            );
        }
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The XDG logic is only used on unix hosts (Windows uses %LOCALAPPDATA%),
    // and its tests use unix-style absolute paths.
    #[cfg(unix)]
    #[test]
    fn defaults_to_dot_local_share() {
        let home = Some(PathBuf::from("/Users/me"));
        assert_eq!(
            xdg_data_home(None, home.clone()),
            Some(PathBuf::from("/Users/me/.local/share"))
        );
        // Empty and relative values are ignored, per the XDG spec.
        assert_eq!(
            xdg_data_home(Some("".into()), home.clone()),
            Some(PathBuf::from("/Users/me/.local/share"))
        );
        assert_eq!(
            xdg_data_home(Some("relative/dir".into()), home),
            Some(PathBuf::from("/Users/me/.local/share"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn honours_absolute_xdg_data_home() {
        assert_eq!(
            xdg_data_home(Some("/data/xdg".into()), Some(PathBuf::from("/Users/me"))),
            Some(PathBuf::from("/data/xdg"))
        );
        assert_eq!(xdg_data_home(None, None), None);
    }

    #[test]
    fn layout() {
        let p = Paths {
            root: PathBuf::from("/r"),
        };
        assert_eq!(
            p.image_dir("ubuntu", "24.04", "aarch64"),
            PathBuf::from("/r/images/ubuntu/24.04/aarch64")
        );
        assert_eq!(p.vm_dir("ubuntu"), PathBuf::from("/r/vms/ubuntu"));
    }
}
