//! vmmbox's own build of QEMU, installed into `<data>/bin`.
//!
//! The bundle is unmodified upstream QEMU, built by `packaging/qemu/build.sh`
//! and published as a GitHub release. Which build vmmbox expects (version,
//! revision and the SHA-256 of each archive) is pinned at compile time in
//! `qemu_pins.rs`, so a vmmbox release can only ever install the exact bytes it
//! was tested with. A host with no pinned archive simply uses the QEMU found on
//! the system.
//!
//! The archive unpacks into a prefix (`bin/`, `lib/`, `share/qemu/`) rooted at
//! the data directory, which is the layout QEMU's relocatable lookups expect.
//! A manifest of the files it installed lets an upgrade remove stale ones.

use crate::checksum::Algo;
use crate::host::{Arch, Os, Platform};
use crate::http::Http;
use crate::paths::Paths;
use crate::qemu_pins::{MIN_MACOS, PINS, REV, VERSION};
use crate::tools::find_binary;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

const RELEASE_REPO: &str = "ericcurtin/vmmbox";

/// What was installed, recorded so an upgrade can clean up after it.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub version: String,
    pub rev: u32,
    pub target: String,
    /// Paths relative to the data directory, with `/` separators.
    pub files: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No bundle is published for this host; the system QEMU is used.
    NoBundle,
    UpToDate,
    Installed,
}

fn host_target(platform: Platform) -> &'static str {
    match (platform.os, platform.arch) {
        (Os::Mac, Arch::Aarch64) => "aarch64-apple-darwin",
        (Os::Mac, Arch::X86_64) => "x86_64-apple-darwin",
        (Os::Linux, Arch::X86_64) => "x86_64-unknown-linux-gnu",
        (Os::Windows, Arch::X86_64) => "x86_64-pc-windows-gnu",
        // Not supported hosts; Platform::current() rejects these first.
        (Os::Linux | Os::Windows, Arch::Aarch64) => "unsupported",
    }
}

fn pinned_sha(target: &str) -> Option<&'static str> {
    PINS.iter().find(|(t, _)| *t == target).map(|(_, h)| *h)
}

/// Whether this OS release can run the bundle: macOS bundles are built for a
/// minimum release, and an unknown version is treated as too old.
fn os_supported(os: Os, os_version: Option<&str>) -> bool {
    match os {
        Os::Mac => os_version
            .and_then(|v| v.split('.').next())
            .and_then(|major| major.parse::<u32>().ok())
            .is_some_and(|major| major >= MIN_MACOS),
        _ => true,
    }
}

fn host_os_supported(platform: Platform) -> bool {
    os_supported(platform.os, sysinfo::System::os_version().as_deref())
}

/// The bundle target for this host, if a bundle has been published and pinned
/// and this machine can run it.
pub fn target_for(platform: Platform) -> Option<&'static str> {
    if !host_os_supported(platform) {
        return None;
    }
    let target = host_target(platform);
    pinned_sha(target).map(|_| target)
}

/// Why there is no bundle for this host, for `vmmbox setup` to say.
pub fn why_none(platform: Platform) -> String {
    if !host_os_supported(platform) {
        format!("vmmbox's QEMU needs macOS {MIN_MACOS} or newer; the QEMU on your system is used")
    } else {
        "there is no vmmbox build of QEMU for this machine; the QEMU on your system is used".into()
    }
}

fn archive_name(target: &str, version: &str, rev: u32) -> String {
    format!("vmmbox-qemu-{version}-r{rev}-{target}.tar.gz")
}

fn archive_url(target: &str) -> String {
    format!(
        "https://github.com/{RELEASE_REPO}/releases/download/qemu-{VERSION}-r{REV}/{}",
        archive_name(target, VERSION, REV)
    )
}

pub fn installed(paths: &Paths) -> Option<Installed> {
    serde_json::from_slice(&std::fs::read(paths.bundle_manifest()).ok()?).ok()
}

fn is_current(paths: &Paths, target: &str) -> bool {
    installed(paths).is_some_and(|i| {
        i.version == VERSION
            && i.rev == REV
            && i.target == target
            && !i.files.is_empty()
            && i.files.iter().all(|f| paths.root().join(f).exists())
    })
}

/// Make sure the pinned bundle for this host is installed, downloading it if it
/// is missing or out of date.
pub fn ensure(paths: &Paths, platform: Platform, http: &Http) -> Result<Outcome> {
    let Some(target) = target_for(platform) else {
        return Ok(Outcome::NoBundle);
    };
    if is_current(paths, target) {
        return Ok(Outcome::UpToDate);
    }
    eprintln!(
        "Installing QEMU {VERSION} into {} (one time)...",
        paths.bin().display()
    );
    let sha = pinned_sha(target).context("no pinned checksum")?;
    install_from(paths, http, &archive_url(target), sha, target, VERSION, REV)?;
    Ok(Outcome::Installed)
}

/// Download `url`, verify it against `sha256`, and install it.
fn install_from(
    paths: &Paths,
    http: &Http,
    url: &str,
    sha256: &str,
    target: &str,
    version: &str,
    rev: u32,
) -> Result<()> {
    let dir = paths.root().join(".download");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let part = dir.join(format!("{}.part", archive_name(target, version, rev)));

    let result = (|| -> Result<()> {
        let dl = http.download(url, &part, Algo::Sha256, &format!("QEMU {version}"))?;
        if dl.digest != sha256 {
            bail!(
                "checksum mismatch for {url}: expected {sha256}, got {}; refusing to install it",
                dl.digest
            );
        }
        install_archive(paths, &part, target, version, rev)
    })();
    let _ = std::fs::remove_file(&part);
    result
}

fn tar_binary() -> Result<PathBuf> {
    let mut fallback = Vec::new();
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        fallback.push(PathBuf::from(root).join("System32"));
    }
    find_binary("tar", &[], &fallback).context(
        "tar not found; vmmbox needs it to unpack QEMU (it ships with macOS and Windows 10+)",
    )
}

/// Unpack `archive` and move its files into the data directory, replacing any
/// earlier bundle. Nothing in the data directory is touched until the archive has
/// unpacked and been checked.
fn install_archive(
    paths: &Paths,
    archive: &Path,
    target: &str,
    version: &str,
    rev: u32,
) -> Result<()> {
    let root = paths.root();
    let staging = root.join(".qemu-extract");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    let result = (|| -> Result<()> {
        let status = Command::new(tar_binary()?)
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(&staging)
            .status()
            .context("running tar")?;
        if !status.success() {
            bail!("could not unpack {}", archive.display());
        }

        let mut files = Vec::new();
        collect_files(&staging, &staging, &mut files)?;
        validate(&files)?;

        // Remove what the previous bundle installed, so an upgrade leaves no
        // stale files behind.
        if let Some(old) = installed(paths) {
            for f in &old.files {
                let _ = std::fs::remove_file(root.join(f));
            }
            for f in &old.files {
                remove_empty_parents(root, &root.join(f));
            }
        }

        for rel in &files {
            let (from, to) = (staging.join(rel), root.join(rel));
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ = std::fs::remove_file(&to);
            if std::fs::rename(&from, &to).is_err() {
                std::fs::copy(&from, &to)
                    .with_context(|| format!("installing {}", to.display()))?;
            }
        }

        let manifest = Installed {
            version: version.to_string(),
            rev,
            target: target.to_string(),
            files,
        };
        let tmp = root.join("qemu-bundle.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
        std::fs::rename(&tmp, paths.bundle_manifest())?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Regular files and symlinks under `dir`, as paths relative to `base`.
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_files(base, &path, out)?;
        } else {
            let rel = path.strip_prefix(base)?;
            let parts: Vec<_> = rel.iter().map(|c| c.to_string_lossy()).collect();
            out.push(parts.join("/"));
        }
    }
    out.sort();
    Ok(())
}

/// A bundle must hold a system emulator and qemu-img, and must not try to write
/// outside its prefix.
fn validate(files: &[String]) -> Result<()> {
    let has = |prefix: &str| {
        files.iter().any(|f| {
            f.strip_prefix("bin/")
                .is_some_and(|n| n.starts_with(prefix))
        })
    };
    if !has("qemu-system-") || !has("qemu-img") {
        bail!("the QEMU archive is missing qemu-system or qemu-img");
    }
    if let Some(bad) = files
        .iter()
        .find(|f| f.starts_with('/') || f.split('/').any(|c| c == ".."))
    {
        bail!("the QEMU archive contains an unsafe path: {bad}");
    }
    Ok(())
}

/// Remove now-empty directories above `path`, stopping at `root` and never
/// removing `root` itself or anything outside the bundle's own directories.
fn remove_empty_parents(root: &Path, path: &Path) {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) {
            break;
        }
        // remove_dir only succeeds on an empty directory.
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_must_be_new_enough_for_the_bundle() {
        assert!(os_supported(Os::Mac, Some("15.0")));
        assert!(os_supported(Os::Mac, Some("26.0.1")));
        assert!(os_supported(Os::Mac, Some("15")));
        assert!(!os_supported(Os::Mac, Some("14.7.1")));
        // Not knowing is not good enough to install a binary that may not run.
        assert!(!os_supported(Os::Mac, None));
        assert!(!os_supported(Os::Mac, Some("sequoia")));
        // Other systems have no such floor.
        assert!(os_supported(Os::Linux, None));
    }

    fn scratch(tag: &str) -> (Paths, PathBuf) {
        let root = std::env::temp_dir().join(format!("vmmbox-bundle-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        (Paths::with_root(root.clone()), root)
    }

    /// Build a .tar.gz from (relative path, contents) pairs with the system tar.
    fn make_archive(dir: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let src = dir.join(format!("{name}-src"));
        let _ = std::fs::remove_dir_all(&src);
        for (rel, body) in files {
            let p = src.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
        }
        let archive = dir.join(format!("{name}.tar.gz"));
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg(".")
            .status()
            .unwrap();
        assert!(status.success());
        archive
    }

    fn sha256_file(path: &Path) -> String {
        let mut h = crate::checksum::Hasher::new(Algo::Sha256);
        h.update(&std::fs::read(path).unwrap());
        h.finish_hex()
    }

    const BASE: &[(&str, &str)] = &[
        ("bin/qemu-system-aarch64", "emulator v1"),
        ("bin/qemu-img", "img v1"),
        ("lib/libglib.dylib", "glib v1"),
        ("share/qemu/edk2-aarch64-code.fd", "fw v1"),
    ];

    #[test]
    fn installs_and_records_a_manifest() {
        let (paths, root) = scratch("install");
        let archive = make_archive(&root, "a", BASE);
        install_archive(&paths, &archive, "aarch64-apple-darwin", "9.9.9", 1).unwrap();

        assert_eq!(
            std::fs::read_to_string(paths.bin().join("qemu-system-aarch64")).unwrap(),
            "emulator v1"
        );
        assert!(root.join("lib/libglib.dylib").exists());
        assert!(root.join("share/qemu/edk2-aarch64-code.fd").exists());
        let m = installed(&paths).unwrap();
        assert_eq!((m.version.as_str(), m.rev), ("9.9.9", 1));
        assert!(m.files.contains(&"bin/qemu-img".to_string()), "{m:?}");
        assert!(
            !root.join(".qemu-extract").exists(),
            "staging is cleaned up"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn upgrade_replaces_files_and_removes_stale_ones() {
        let (paths, root) = scratch("upgrade");
        let v1 = make_archive(&root, "v1", BASE);
        install_archive(&paths, &v1, "aarch64-apple-darwin", "1.0.0", 1).unwrap();
        // Things that are not the bundle's must survive an upgrade.
        std::fs::create_dir_all(root.join("vms/ubuntu")).unwrap();
        std::fs::write(root.join("vms/ubuntu/disk.qcow2"), "my vm").unwrap();

        // v2 drops the library, renames the firmware and changes the emulator.
        let v2 = make_archive(
            &root,
            "v2",
            &[
                ("bin/qemu-system-aarch64", "emulator v2"),
                ("bin/qemu-img", "img v2"),
                ("share/qemu/edk2-new.fd", "fw v2"),
            ],
        );
        install_archive(&paths, &v2, "aarch64-apple-darwin", "2.0.0", 1).unwrap();

        assert_eq!(
            std::fs::read_to_string(paths.bin().join("qemu-system-aarch64")).unwrap(),
            "emulator v2"
        );
        assert!(
            !root.join("lib/libglib.dylib").exists(),
            "stale library removed"
        );
        assert!(!root.join("lib").exists(), "and its empty directory");
        assert!(!root.join("share/qemu/edk2-aarch64-code.fd").exists());
        assert!(root.join("share/qemu/edk2-new.fd").exists());
        assert_eq!(installed(&paths).unwrap().version, "2.0.0");
        assert_eq!(
            std::fs::read_to_string(root.join("vms/ubuntu/disk.qcow2")).unwrap(),
            "my vm",
            "user data is never touched"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rejects_archives_that_are_not_a_qemu_bundle() {
        let (paths, root) = scratch("invalid");
        let archive = make_archive(&root, "bad", &[("bin/other", "x")]);
        let err = install_archive(&paths, &archive, "t", "1", 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing qemu-system"), "{err}");
        assert!(installed(&paths).is_none());
        assert!(!paths.bin().exists(), "nothing was installed");

        assert!(
            validate(&[
                "bin/qemu-system-x".into(),
                "bin/qemu-img".into(),
                "../evil".into()
            ])
            .is_err()
        );
        assert!(
            validate(&[
                "bin/qemu-system-x".into(),
                "bin/qemu-img".into(),
                "/etc/x".into()
            ])
            .is_err()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_validation_leaves_the_existing_install_alone() {
        let (paths, root) = scratch("keep");
        let good = make_archive(&root, "good", BASE);
        install_archive(&paths, &good, "t", "1.0.0", 1).unwrap();
        let bad = make_archive(&root, "bad", &[("bin/other", "x")]);
        assert!(install_archive(&paths, &bad, "t", "2.0.0", 1).is_err());
        assert_eq!(installed(&paths).unwrap().version, "1.0.0");
        assert!(paths.bin().join("qemu-system-aarch64").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn downloads_verifies_and_installs() {
        let Ok(http) = Http::new() else {
            return;
        };
        let (paths, root) = scratch("download");
        let archive = make_archive(&root, "dl", BASE);
        let body = std::fs::read(&archive).unwrap();
        let port = crate::testutil::serve(vec![("/file".into(), body)]);
        let url = format!("http://127.0.0.1:{port}/file");

        // The wrong hash must refuse to install anything.
        let err = install_from(&paths, &http, &url, &"0".repeat(64), "t", "1.0.0", 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(installed(&paths).is_none());
        assert!(!paths.bin().exists());
        assert!(
            !root
                .join(".download/vmmbox-qemu-1.0.0-r1-t.tar.gz.part")
                .exists()
        );

        // The right one installs, following a redirect.
        let sha = sha256_file(&archive);
        let url = format!("http://127.0.0.1:{port}/redir");
        install_from(&paths, &http, &url, &sha, "t", "1.0.0", 1).unwrap();
        assert!(paths.bin().join("qemu-img").exists());
        assert_eq!(installed(&paths).unwrap().version, "1.0.0");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn current_means_pinned_version_and_files_present() {
        let (paths, root) = scratch("current");
        let archive = make_archive(&root, "c", BASE);
        let target = "aarch64-apple-darwin";
        assert!(!is_current(&paths, target), "nothing installed yet");
        install_archive(&paths, &archive, target, VERSION, REV).unwrap();
        assert!(is_current(&paths, target));
        assert!(!is_current(&paths, "x86_64-apple-darwin"), "other target");
        std::fs::remove_file(paths.bin().join("qemu-img")).unwrap();
        assert!(
            !is_current(&paths, target),
            "a deleted file triggers a reinstall"
        );

        install_archive(&paths, &archive, target, "0.0.1", REV).unwrap();
        assert!(
            !is_current(&paths, target),
            "an older version is not current"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn urls_and_names() {
        assert_eq!(
            archive_name("aarch64-apple-darwin", "11.0.5", 1),
            "vmmbox-qemu-11.0.5-r1-aarch64-apple-darwin.tar.gz"
        );
        let u = archive_url("aarch64-apple-darwin");
        assert!(
            u.starts_with("https://github.com/ericcurtin/vmmbox/releases/download/qemu-"),
            "{u}"
        );
        assert!(u.ends_with("-aarch64-apple-darwin.tar.gz"), "{u}");
    }
}
