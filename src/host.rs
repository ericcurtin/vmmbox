//! Host platform, accelerator and user identity.

use anyhow::{Result, bail};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Mac,
    Linux,
    Windows,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn as_str(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }

    /// The name Debian/Ubuntu use for this architecture.
    pub fn deb_name(self) -> &'static str {
        match self {
            Arch::X86_64 => "amd64",
            Arch::Aarch64 => "arm64",
        }
    }
}

/// Hardware accelerators. vmmbox only ever runs hardware-accelerated guests,
/// never TCG emulation, so a host without one is simply unsupported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accel {
    Kvm,
    Hvf,
    Whpx,
}

impl Accel {
    pub fn name(self) -> &'static str {
        match self {
            Accel::Kvm => "kvm",
            Accel::Hvf => "hvf",
            Accel::Whpx => "whpx",
        }
    }
}

/// The host platforms vmmbox supports. macOS is Apple silicon only.
const SUPPORTED: &[(Os, Arch)] = &[
    (Os::Mac, Arch::Aarch64),
    (Os::Linux, Arch::X86_64),
    (Os::Windows, Arch::X86_64),
];

#[derive(Clone, Copy, Debug)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

impl Platform {
    pub fn current() -> Result<Self> {
        let os = if cfg!(target_os = "macos") {
            Os::Mac
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(target_os = "windows") {
            Os::Windows
        } else {
            bail!("unsupported host OS: {}", std::env::consts::OS);
        };
        let arch = if cfg!(target_arch = "x86_64") {
            Arch::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            bail!("unsupported host architecture: {}", std::env::consts::ARCH);
        };
        if !SUPPORTED.contains(&(os, arch)) {
            bail!(
                "unsupported host: {} on {} (supported: macOS aarch64, Linux x86_64, Windows x86_64)",
                os.name(),
                arch.as_str()
            );
        }
        Ok(Self { os, arch })
    }

    pub fn accel(self) -> Accel {
        match self.os {
            Os::Linux => Accel::Kvm,
            Os::Mac => Accel::Hvf,
            Os::Windows => Accel::Whpx,
        }
    }

    /// Whether QEMU can share a host directory with the guest on this OS.
    /// QEMU's 9p (virtfs) backend only exists for Linux and macOS hosts.
    pub fn supports_home_share(self) -> bool {
        matches!(self.os, Os::Linux | Os::Mac)
    }

    /// Verify the hardware accelerator is usable right now, with an actionable
    /// error if it is not.
    pub fn check_accel(self) -> Result<()> {
        match self.os {
            Os::Linux => {
                let kvm = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/kvm");
                if let Err(e) = kvm {
                    bail!(
                        "KVM is not usable ({e}). Enable virtualization in firmware, load the \
                         kvm module, and make sure your user can access /dev/kvm \
                         (e.g. `sudo usermod -aG kvm $USER`, then log in again)"
                    );
                }
            }
            Os::Mac => {
                let out = std::process::Command::new("/usr/sbin/sysctl")
                    .args(["-n", "kern.hv_support"])
                    .output();
                let ok = out
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
                    .unwrap_or(false);
                if !ok {
                    bail!("Hypervisor.framework (HVF) is not available on this Mac");
                }
            }
            // WHPX can't be probed without launching QEMU; failures surface from
            // QEMU itself (with the log tail) at start-up.
            Os::Windows => {}
        }
        Ok(())
    }
}

impl Os {
    pub fn name(self) -> &'static str {
        match self {
            Os::Mac => "macOS",
            Os::Linux => "Linux",
            Os::Windows => "Windows",
        }
    }
}

#[derive(Clone, Debug)]
pub struct HostUser {
    /// Login name, sanitised so it is valid in a Linux guest.
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub full_name: String,
    pub home: PathBuf,
}

/// Make a host account name usable as a Linux login name
/// (`[a-z_][a-z0-9_-]*`, at most 32 chars).
pub fn sanitize_username(raw: &str) -> String {
    let mut s: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        s.push_str("user");
    }
    if !s.starts_with(|c: char| c.is_ascii_lowercase() || c == '_') {
        s.insert(0, '_');
    }
    s.truncate(32);
    s
}

/// Strip characters that would break `/etc/passwd` GECOS fields or YAML.
#[cfg_attr(not(unix), allow(dead_code))]
fn sanitize_gecos(raw: &str) -> String {
    raw.split(',')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, ':' | '"' | '\\' | '\''))
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(unix)]
pub fn current_user() -> Result<HostUser> {
    use anyhow::Context;
    use std::ffi::CStr;
    use std::mem::MaybeUninit;

    // SAFETY: getuid/getgid have no preconditions.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if uid == 0 {
        bail!(
            "refusing to run as root: the VM user mirrors the invoking user, run vmmbox unprivileged"
        );
    }

    let mut pwd = MaybeUninit::<libc::passwd>::uninit();
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the call; `result` is checked below.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            pwd.as_mut_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        bail!("could not look up the passwd entry for uid {uid}");
    }
    // SAFETY: getpwuid_r succeeded, so the struct is initialised and its string
    // pointers point into `buf`, which outlives these reads.
    let (name, gecos, dir) = unsafe {
        let pwd = pwd.assume_init();
        let s = |p: *const libc::c_char| {
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        (s(pwd.pw_name), s(pwd.pw_gecos), s(pwd.pw_dir))
    };

    let home = match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => PathBuf::from(dir),
    };
    let home = home
        .canonicalize()
        .with_context(|| format!("home directory {} does not exist", home.display()))?;

    let name = sanitize_username(&name);
    let full_name = match sanitize_gecos(&gecos) {
        s if s.is_empty() => name.clone(),
        s => s,
    };
    Ok(HostUser {
        name,
        uid,
        gid,
        full_name,
        home,
    })
}

#[cfg(windows)]
pub fn current_user() -> Result<HostUser> {
    use anyhow::Context;
    // Windows identities are SIDs, not numeric ids; use the conventional first
    // Linux user id/gid.
    let raw = std::env::var("USERNAME").context("USERNAME is not set")?;
    let home = std::env::var_os("USERPROFILE").context("USERPROFILE is not set")?;
    let name = sanitize_username(&raw);
    Ok(HostUser {
        full_name: name.clone(),
        name,
        uid: 1000,
        gid: 1000,
        home: PathBuf::from(home),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_supported_hosts_are_exactly_these() {
        // macOS is Apple silicon only: there is no Intel Mac support.
        assert!(!SUPPORTED.contains(&(Os::Mac, Arch::X86_64)));
        assert_eq!(
            SUPPORTED,
            &[
                (Os::Mac, Arch::Aarch64),
                (Os::Linux, Arch::X86_64),
                (Os::Windows, Arch::X86_64)
            ]
        );
    }

    #[test]
    fn usernames() {
        assert_eq!(sanitize_username("ecurtin"), "ecurtin");
        assert_eq!(sanitize_username("Eric Curtin"), "eric_curtin");
        assert_eq!(sanitize_username("john.doe"), "john_doe");
        assert_eq!(sanitize_username("1user"), "_1user");
        assert_eq!(sanitize_username(""), "user");
        assert_eq!(sanitize_username(&"a".repeat(40)).len(), 32);
    }

    #[test]
    fn gecos() {
        assert_eq!(sanitize_gecos("Eric Curtin,,,"), "Eric Curtin");
        assert_eq!(sanitize_gecos("Ev\"il:name"), "Evilname");
    }

    #[test]
    fn support_matrix() {
        assert!(SUPPORTED.contains(&(Os::Mac, Arch::Aarch64)));
        assert!(!SUPPORTED.contains(&(Os::Linux, Arch::Aarch64)));
        assert!(!SUPPORTED.contains(&(Os::Windows, Arch::Aarch64)));
    }
}
