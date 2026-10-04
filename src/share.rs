//! How the host's home directory reaches the guest.
//!
//! Two ways, both a QEMU device plus a mount in the guest:
//!
//! * 9p (virtio-9p): built into QEMU on Linux and macOS, but missing from some
//!   guest kernels (the RHEL family has none).
//! * virtio-fs: every guest kernel has it, and it is faster, but QEMU needs a
//!   separate server for it. On macOS that is `vmmbox virtiofsd`, and it needs
//!   the QEMU build vmmbox ships (see `bundle.rs`), since Homebrew's has no
//!   vhost-user.
//!
//! The choice is made when a VM is created and recorded with it, because the
//! guest's `fstab` is written then and names the file system type.

use crate::host::Os;
use serde::{Deserialize, Serialize};

/// The mount tag shared between the QEMU command line and the guest's `fstab`.
pub const HOME_TAG: &str = "vmmhome";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transport {
    /// Records from before virtio-fs existed have no transport, and used 9p.
    #[default]
    #[serde(rename = "9p")]
    NineP,
    #[serde(rename = "virtiofs")]
    VirtioFs,
}

impl Transport {
    pub fn name(self) -> &'static str {
        match self {
            Transport::NineP => "9p",
            Transport::VirtioFs => "virtio-fs",
        }
    }

    /// The `/etc/fstab` entry that mounts the share at `path` (already escaped
    /// for fstab).
    pub fn fstab_line(self, path: &str) -> String {
        match self {
            Transport::NineP => format!(
                "{HOME_TAG} {path} 9p trans=virtio,version=9p2000.L,msize=512000,cache=mmap,nofail 0 0"
            ),
            Transport::VirtioFs => format!("{HOME_TAG} {path} virtiofs defaults,nofail 0 0"),
        }
    }
}

/// What the host's QEMU can share with.
#[derive(Clone, Copy, Debug)]
pub struct Support {
    pub ninep: bool,
    pub virtiofs: bool,
}

/// Pick a transport for a new VM, or `None` if the home cannot be shared.
/// `forced` is `VMMBOX_SHARE`, for testing or for working around a problem.
///
/// virtio-fs is preferred where there is a server for it (macOS): it is faster,
/// and it is the only one the RHEL-family kernels have.
pub fn choose(os: Os, support: Support, forced: Option<&str>) -> Option<Transport> {
    let virtiofs = os == Os::Mac && support.virtiofs;
    match forced {
        Some("9p") => support.ninep.then_some(Transport::NineP),
        Some("virtiofs") => virtiofs.then_some(Transport::VirtioFs),
        _ if virtiofs => Some(Transport::VirtioFs),
        _ => support.ninep.then_some(Transport::NineP),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: Support = Support {
        ninep: true,
        virtiofs: true,
    };

    #[test]
    fn macos_prefers_virtiofs_when_it_can() {
        assert_eq!(choose(Os::Mac, BOTH, None), Some(Transport::VirtioFs));
    }

    #[test]
    fn it_falls_back_to_9p_where_virtiofs_cannot_be_served() {
        let no_vfs = Support {
            ninep: true,
            virtiofs: false,
        };
        // Homebrew's QEMU has no vhost-user.
        assert_eq!(choose(Os::Mac, no_vfs, None), Some(Transport::NineP));
        // Nothing serves virtio-fs on Linux, however capable the QEMU is.
        assert_eq!(choose(Os::Linux, BOTH, None), Some(Transport::NineP));
    }

    #[test]
    fn no_support_at_all_means_no_share() {
        let none = Support {
            ninep: false,
            virtiofs: false,
        };
        assert_eq!(choose(Os::Mac, none, None), None);
        assert_eq!(choose(Os::Windows, none, None), None);
    }

    #[test]
    fn the_environment_can_force_a_transport_but_not_an_impossible_one() {
        assert_eq!(choose(Os::Mac, BOTH, Some("9p")), Some(Transport::NineP));
        assert_eq!(
            choose(Os::Mac, BOTH, Some("virtiofs")),
            Some(Transport::VirtioFs)
        );
        // Asking for what cannot work is no share, not a quiet substitution.
        assert_eq!(choose(Os::Linux, BOTH, Some("virtiofs")), None);
        // An unknown value is ignored.
        assert_eq!(
            choose(Os::Mac, BOTH, Some("nfs")),
            Some(Transport::VirtioFs)
        );
    }

    #[test]
    fn fstab_lines() {
        assert_eq!(
            Transport::NineP.fstab_line("/Users/me"),
            "vmmhome /Users/me 9p trans=virtio,version=9p2000.L,msize=512000,cache=mmap,nofail 0 0"
        );
        assert_eq!(
            Transport::VirtioFs.fstab_line("/Users/me"),
            "vmmhome /Users/me virtiofs defaults,nofail 0 0"
        );
    }

    #[test]
    fn old_records_have_no_transport_and_mean_9p() {
        #[derive(Deserialize)]
        struct Old {
            #[serde(default)]
            t: Transport,
        }
        assert_eq!(
            serde_json::from_str::<Old>("{}").unwrap().t,
            Transport::NineP
        );
        assert_eq!(
            serde_json::from_str::<Old>(r#"{"t":"virtiofs"}"#)
                .unwrap()
                .t,
            Transport::VirtioFs
        );
        assert_eq!(serde_json::to_string(&Transport::NineP).unwrap(), r#""9p""#);
    }
}
