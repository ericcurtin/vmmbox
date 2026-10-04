//! The distro catalog and `distro[:version]` references.
//!
//! Each distro publishes a generic cloud image in qcow2 format. A [`Source`]
//! says where to fetch it and where to find its checksum.

use crate::checksum::Algo;
use crate::host::Arch;
use crate::http::Http;
use anyhow::{Context, Result, bail};
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Ubuntu,
    Debian,
    Fedora,
}

#[derive(Debug)]
pub struct Distro {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub family: Family,
    /// Version used when none is given (`vmmbox pull ubuntu`). Bump when a new
    /// release becomes the sensible default.
    pub default_version: &'static str,
}

pub const DISTROS: &[Distro] = &[
    Distro {
        name: "ubuntu",
        aliases: &[],
        family: Family::Ubuntu,
        default_version: "26.04",
    },
    Distro {
        name: "debian",
        aliases: &[],
        family: Family::Debian,
        default_version: "13",
    },
    Distro {
        name: "fedora",
        aliases: &[],
        family: Family::Fedora,
        default_version: "44",
    },
];

/// Debian release number -> suite name used in cloud.debian.org paths.
const DEBIAN_SUITES: &[(&str, &str)] = &[("12", "bookworm"), ("13", "trixie")];

pub fn lookup(name: &str) -> Option<&'static Distro> {
    let name = name.to_ascii_lowercase();
    DISTROS
        .iter()
        .find(|d| d.name == name || d.aliases.contains(&name.as_str()))
}

pub fn known_names() -> String {
    DISTROS
        .iter()
        .map(|d| d.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A parsed `distro[:version]`.
#[derive(Clone, Debug)]
pub struct ImageRef {
    pub distro: &'static Distro,
    pub version: Option<String>,
}

impl FromStr for ImageRef {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (name, version) = match s.split_once(':') {
            Some((n, v)) => (n, Some(v)),
            None => (s, None),
        };
        let distro = lookup(name)
            .with_context(|| format!("unknown distro '{name}' (available: {})", known_names()))?;
        let version = match version {
            Some("") => bail!("empty version in '{s}'"),
            Some(v) => {
                distro.family.validate_version(v)?;
                Some(v.to_string())
            }
            None => None,
        };
        Ok(Self { distro, version })
    }
}

impl ImageRef {
    pub fn version_or_default(&self) -> String {
        self.version
            .clone()
            .unwrap_or_else(|| self.distro.default_version.to_string())
    }
}

impl Family {
    /// Guest packages for sound: PipeWire with its PulseAudio server (what
    /// browsers and most desktop apps speak), WirePlumber as session manager,
    /// and the ALSA bridge for older apps.
    pub fn audio_packages(self) -> &'static [&'static str] {
        match self {
            Family::Ubuntu | Family::Debian => &[
                "pipewire",
                "pipewire-bin",
                "pipewire-pulse",
                "wireplumber",
                "pipewire-alsa",
                "libpulse0",
            ],
            Family::Fedora => &[
                "pipewire",
                "pipewire-utils",
                "pipewire-pulseaudio",
                "wireplumber",
                "pipewire-alsa",
                "pulseaudio-libs",
            ],
        }
    }

    /// Guest packages for GUI apps: waypipe (carries Wayland to the host), a
    /// font (cloud images have none, so text would not render), and Mesa's
    /// software OpenGL plus Vulkan drivers (the latter includes the Venus driver
    /// that talks to the host GPU when the host QEMU provides one).
    pub fn gui_packages(self) -> &'static [&'static str] {
        match self {
            Family::Ubuntu | Family::Debian => &[
                "waypipe",
                "fonts-dejavu-core",
                "libgl1-mesa-dri",
                "mesa-vulkan-drivers",
            ],
            Family::Fedora => &[
                "waypipe",
                "dejavu-sans-fonts",
                "mesa-dri-drivers",
                "mesa-vulkan-drivers",
            ],
        }
    }

    /// Shell command that installs the kernel modules the guest needs for
    /// virtio sound, for the *running* kernel. Cloud images ship a minimal
    /// module set (sound drivers live in `linux-modules-extra` on Ubuntu and
    /// `kernel-modules` on Fedora). `None` where the stock modules suffice.
    pub fn kernel_modules_cmd(self) -> Option<&'static str> {
        match self {
            Family::Ubuntu => Some(
                "export DEBIAN_FRONTEND=noninteractive; apt-get update -qq && \\
                 apt-get install -y -qq \"linux-modules-extra-$(uname -r)\"",
            ),
            Family::Fedora => Some("dnf install -y -q \"kernel-modules-$(uname -r)\""),
            Family::Debian => None,
        }
    }

    fn validate_version(self, v: &str) -> Result<()> {
        let all_digits =
            |s: &str| !s.is_empty() && s.len() <= 3 && s.bytes().all(|b| b.is_ascii_digit());
        let ok = match self {
            Family::Ubuntu => {
                matches!(v.split_once('.'), Some((y, m)) if y.len() == 2 && m.len() == 2 && all_digits(y) && all_digits(m))
            }
            Family::Debian => DEBIAN_SUITES.iter().any(|(n, _)| *n == v),
            Family::Fedora => all_digits(v),
        };
        if ok {
            return Ok(());
        }
        match self {
            Family::Ubuntu => bail!("invalid Ubuntu version '{v}' (expected e.g. 24.04 or 26.04)"),
            Family::Debian => bail!(
                "unsupported Debian version '{v}' (available: {})",
                DEBIAN_SUITES
                    .iter()
                    .map(|(n, _)| *n)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => bail!("invalid version '{v}' (expected a release number such as 10)"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub url: String,
    pub file_name: String,
    pub checksum_url: String,
    pub checksum_algo: Algo,
}

impl Distro {
    /// Work out where to download `version` of this distro for `arch`.
    /// Only Fedora needs the network here (to learn the current build id).
    pub fn resolve(&self, version: &str, arch: Arch, http: &Http) -> Result<Source> {
        match self.family {
            Family::Fedora => {
                let base = fedora_base(version, arch);
                let listing = http.get_text(&base).with_context(|| {
                    format!(
                        "Fedora {version} cloud images were not found for {}",
                        arch.as_str()
                    )
                })?;
                fedora_source(&listing, &base, version, arch)
            }
            family => static_source(family, version, arch),
        }
    }
}

fn simple(url_dir: &str, file: &str, sums: &str, algo: Algo) -> Source {
    Source {
        url: format!("{url_dir}{file}"),
        file_name: file.to_string(),
        checksum_url: format!("{url_dir}{sums}"),
        checksum_algo: algo,
    }
}

/// Sources whose URLs follow a fixed template.
pub fn static_source(family: Family, v: &str, arch: Arch) -> Result<Source> {
    Ok(match family {
        Family::Ubuntu => simple(
            &format!("https://cloud-images.ubuntu.com/releases/{v}/release/"),
            &format!("ubuntu-{v}-server-cloudimg-{}.img", arch.deb_name()),
            "SHA256SUMS",
            Algo::Sha256,
        ),
        Family::Debian => {
            let suite = DEBIAN_SUITES
                .iter()
                .find(|(n, _)| *n == v)
                .map(|(_, s)| *s)
                .with_context(|| format!("unsupported Debian version '{v}'"))?;
            // "generic", not "genericcloud": the latter ships the trimmed-down
            // cloud kernel, which has no 9p support and so can't mount the host home.
            simple(
                &format!("https://cloud.debian.org/images/cloud/{suite}/latest/"),
                &format!("debian-{v}-generic-{}.qcow2", arch.deb_name()),
                "SHA512SUMS",
                Algo::Sha512,
            )
        }
        Family::Fedora => bail!("Fedora sources are resolved from the release listing"),
    })
}

fn fedora_base(v: &str, arch: Arch) -> String {
    format!(
        "https://dl.fedoraproject.org/pub/fedora/linux/releases/{v}/Cloud/{}/images/",
        arch.as_str()
    )
}

/// Pick the Generic cloud qcow2 (and its CHECKSUM file) out of a Fedora
/// directory listing. File names embed a build id (`44-1.7`) that changes with
/// respins, so they can't be templated.
pub fn fedora_source(listing: &str, base: &str, v: &str, arch: Arch) -> Result<Source> {
    let hrefs = extract_hrefs(listing);
    let suffix = format!(".{}.qcow2", arch.as_str());
    let generic = format!("Fedora-Cloud-Base-Generic-{v}-");
    let legacy = format!("Fedora-Cloud-Base-{v}-");
    let file = hrefs
        .iter()
        .find(|h| (h.starts_with(&generic) || h.starts_with(&legacy)) && h.ends_with(&suffix))
        .with_context(|| format!("no Fedora {v} cloud image listed at {base}"))?;
    let sums = hrefs
        .iter()
        .find(|h| h.starts_with("Fedora-Cloud-") && h.ends_with("-CHECKSUM"))
        .with_context(|| format!("no checksum file listed at {base}"))?;
    Ok(Source {
        url: format!("{base}{file}"),
        file_name: file.to_string(),
        checksum_url: format!("{base}{sums}"),
        checksum_algo: Algo::Sha256,
    })
}

/// Extract `href="..."` targets from an HTML directory index.
fn extract_hrefs(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("href=\"") {
        rest = &rest[i + 6..];
        if let Some(j) = rest.find('"') {
            out.push(&rest[..j]);
            rest = &rest[j..];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<ImageRef> {
        s.parse()
    }

    #[test]
    fn parses_refs() {
        let r = parse("ubuntu").unwrap();
        assert_eq!(r.distro.name, "ubuntu");
        assert_eq!(r.version, None);
        assert_eq!(r.version_or_default(), "26.04");

        let r = parse("ubuntu:24.04").unwrap();
        assert_eq!(r.version.as_deref(), Some("24.04"));
        assert_eq!(parse("fedora:44").unwrap().version.as_deref(), Some("44"));
        assert_eq!(parse("Debian:13").unwrap().distro.name, "debian");
    }

    #[test]
    fn rejects_bad_refs() {
        assert!(
            parse("nosuchdistro")
                .unwrap_err()
                .to_string()
                .contains("available:")
        );
        assert!(parse("ubuntu:").is_err());
        assert!(parse("ubuntu:noble").is_err());
        assert!(parse("ubuntu:24").is_err());
        assert!(parse("debian:99").is_err());
        assert!(parse("fedora:rawhide").is_err());
        // Dropped: their kernels have no 9p, so the host home can't be shared.
        assert!(parse("almalinux").is_err());
        assert!(parse("rocky").is_err());
        assert!(parse("centos-stream").is_err());
    }

    #[test]
    fn ubuntu_urls() {
        let s = static_source(Family::Ubuntu, "24.04", Arch::Aarch64).unwrap();
        assert_eq!(
            s.url,
            "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-arm64.img"
        );
        assert_eq!(
            s.checksum_url,
            "https://cloud-images.ubuntu.com/releases/24.04/release/SHA256SUMS"
        );
        let s = static_source(Family::Ubuntu, "26.04", Arch::X86_64).unwrap();
        assert!(s.url.ends_with("ubuntu-26.04-server-cloudimg-amd64.img"));
    }

    #[test]
    fn debian_urls() {
        let s = static_source(Family::Debian, "13", Arch::Aarch64).unwrap();
        assert_eq!(
            s.url,
            "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-generic-arm64.qcow2"
        );
        assert_eq!(s.checksum_algo, Algo::Sha512);
        let s = static_source(Family::Debian, "12", Arch::X86_64).unwrap();
        assert!(
            s.url
                .contains("/bookworm/latest/debian-12-generic-amd64.qcow2")
        );
    }

    #[test]
    fn fedora_listing() {
        let listing = r#"<a href="?C=N;O=D">Name</a>
<a href="Fedora-Cloud-44-1.7-aarch64-CHECKSUM">x</a>
<a href="Fedora-Cloud-Base-AmazonEC2-44-1.7.aarch64.raw.xz">x</a>
<a href="Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2">x</a>
<a href="Fedora-Cloud-Base-UEFI-UKI-44-1.7.aarch64.qcow2">x</a>"#;
        let base = "https://example.org/images/";
        let s = fedora_source(listing, base, "44", Arch::Aarch64).unwrap();
        assert_eq!(
            s.file_name,
            "Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2"
        );
        assert_eq!(
            s.url,
            "https://example.org/images/Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2"
        );
        assert_eq!(
            s.checksum_url,
            "https://example.org/images/Fedora-Cloud-44-1.7-aarch64-CHECKSUM"
        );
        // Wrong arch / version find nothing rather than the wrong image.
        assert!(fedora_source(listing, base, "44", Arch::X86_64).is_err());
        assert!(fedora_source(listing, base, "43", Arch::Aarch64).is_err());
    }
}
