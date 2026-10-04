//! The local image store: pulled cloud images, one per (distro, version, arch).

use crate::checksum::{Algo, find_checksum};
use crate::distro::Distro;
use crate::host::Arch;
use crate::http::Http;
use crate::paths::Paths;
use crate::util::{dir_size, format_bytes, now_secs};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageMeta {
    pub distro: String,
    pub version: String,
    pub arch: String,
    pub file_name: String,
    pub url: String,
    pub algo: Algo,
    /// Hex digest of the downloaded file, as published by the distro.
    pub sha: String,
    pub size: u64,
    pub pulled_at: u64,
}

impl ImageMeta {
    /// `ubuntu:24.04`
    pub fn reference(&self) -> String {
        format!("{}:{}", self.distro, self.version)
    }
}

pub enum Pulled {
    Downloaded(ImageMeta),
    UpToDate(ImageMeta),
}

pub struct Images<'a> {
    paths: &'a Paths,
}

impl<'a> Images<'a> {
    pub fn new(paths: &'a Paths) -> Self {
        Self { paths }
    }

    fn dir(&self, distro: &str, version: &str, arch: Arch) -> PathBuf {
        self.paths.image_dir(distro, version, arch.as_str())
    }

    pub fn disk_path(&self, distro: &str, version: &str, arch: Arch) -> PathBuf {
        self.dir(distro, version, arch).join("disk.qcow2")
    }

    /// The local image, if it has been fully pulled.
    pub fn find(&self, distro: &str, version: &str, arch: Arch) -> Option<ImageMeta> {
        let dir = self.dir(distro, version, arch);
        let meta = read_meta(&dir)?;
        dir.join("disk.qcow2").is_file().then_some(meta)
    }

    pub fn list(&self) -> Result<Vec<ImageMeta>> {
        let mut out = Vec::new();
        for distro in subdirs(&self.paths.images()) {
            for version in subdirs(&distro) {
                for arch in subdirs(&version) {
                    if let Some(meta) = read_meta(&arch)
                        && arch.join("disk.qcow2").is_file()
                    {
                        out.push(meta);
                    }
                }
            }
        }
        out.sort_by(|a, b| (&a.distro, &a.version, &a.arch).cmp(&(&b.distro, &b.version, &b.arch)));
        Ok(out)
    }

    /// Download (or confirm up to date) the image for `distro:version` on `arch`.
    pub fn pull(&self, http: &Http, distro: &Distro, version: &str, arch: Arch) -> Result<Pulled> {
        let reference = format!("{}:{version}", distro.name);
        let source = distro.resolve(version, arch, http)?;

        let sums = http.get_text(&source.checksum_url).with_context(|| {
            format!(
                "could not fetch checksums for {reference} on {} (does that version exist?)",
                arch.as_str()
            )
        })?;
        let expected =
            find_checksum(&sums, &source.file_name, source.checksum_algo).with_context(|| {
                format!(
                    "{} is not listed in {}",
                    source.file_name, source.checksum_url
                )
            })?;

        if let Some(local) = self.find(distro.name, version, arch)
            && local.sha == expected
            && local.algo == source.checksum_algo
        {
            return Ok(Pulled::UpToDate(local));
        }

        let dir = self.dir(distro.name, version, arch);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let part = dir.join("disk.qcow2.part");

        let result = (|| -> Result<ImageMeta> {
            let dl = http.download(&source.url, &part, source.checksum_algo, &reference)?;
            if dl.digest != expected {
                bail!(
                    "checksum mismatch for {}: expected {expected}, got {}",
                    source.file_name,
                    dl.digest
                );
            }
            verify_qcow2(&part)?;
            std::fs::rename(&part, dir.join("disk.qcow2"))
                .context("installing downloaded image")?;
            let meta = ImageMeta {
                distro: distro.name.to_string(),
                version: version.to_string(),
                arch: arch.as_str().to_string(),
                file_name: source.file_name.clone(),
                url: source.url.clone(),
                algo: source.checksum_algo,
                sha: expected.clone(),
                size: dl.size,
                pulled_at: now_secs(),
            };
            write_meta(&dir, &meta)?;
            Ok(meta)
        })();

        match result {
            Ok(meta) => Ok(Pulled::Downloaded(meta)),
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                Err(e)
            }
        }
    }

    /// Delete the pulled image and return the bytes freed, or `None` if there
    /// is no such image. Virtual machines are unaffected: each has its own copy
    /// of the disk.
    ///
    /// A partly downloaded image is removed too. Every directory on the way
    /// down from the store must be a real one, so that a symlink cannot lead
    /// the delete somewhere else.
    pub fn remove(&self, distro: &str, version: &str, arch: Arch) -> Result<Option<u64>> {
        let mut dir = self.paths.images();
        for part in [distro, version, arch.as_str()] {
            dir.push(part);
            match std::fs::symlink_metadata(&dir) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => bail!("refusing to remove {}: not a directory", dir.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
            }
        }
        let size = dir_size(&dir);
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        // Tidy up the version and distro directories once nothing is left in
        // them; `remove_dir` leaves them alone if something is.
        for _ in 0..2 {
            if !dir.pop() || std::fs::remove_dir(&dir).is_err() {
                break;
            }
        }
        Ok(Some(size))
    }

    /// The local image, pulling it first if it is not present.
    pub fn ensure(
        &self,
        http: &Http,
        distro: &Distro,
        version: &str,
        arch: Arch,
    ) -> Result<ImageMeta> {
        if let Some(meta) = self.find(distro.name, version, arch) {
            return Ok(meta);
        }
        eprintln!(
            "Image {}:{version} not found locally, pulling it",
            distro.name
        );
        match self.pull(http, distro, version, arch)? {
            Pulled::Downloaded(m) | Pulled::UpToDate(m) => {
                eprintln!("Pulled {} ({})", m.reference(), format_bytes(m.size));
                Ok(m)
            }
        }
    }
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

fn read_meta(dir: &Path) -> Option<ImageMeta> {
    serde_json::from_slice(&std::fs::read(dir.join("meta.json")).ok()?).ok()
}

fn write_meta(dir: &Path, meta: &ImageMeta) -> Result<()> {
    let tmp = dir.join("meta.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(meta)?)?;
    std::fs::rename(&tmp, dir.join("meta.json"))?;
    Ok(())
}

/// Cloud images are qcow2; refuse anything else (e.g. an HTML error page).
fn verify_qcow2(path: &Path) -> Result<()> {
    let mut magic = [0u8; 4];
    std::fs::File::open(path)?
        .read_exact(&mut magic)
        .context("downloaded file is too short to be a disk image")?;
    if &magic != b"QFI\xfb" {
        bail!("downloaded file is not a qcow2 image");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qcow2_magic() {
        let dir = std::env::temp_dir().join(format!("vmmbox-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good");
        std::fs::write(&good, b"QFI\xfb\0\0\0\x03rest").unwrap();
        assert!(verify_qcow2(&good).is_ok());
        let bad = dir.join("bad");
        std::fs::write(&bad, b"<html>404</html>").unwrap();
        assert!(verify_qcow2(&bad).is_err());
        let short = dir.join("short");
        std::fs::write(&short, b"QF").unwrap();
        assert!(verify_qcow2(&short).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
