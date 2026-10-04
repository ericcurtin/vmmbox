//! How the host's home directory reaches the guest: over virtio-fs, a QEMU
//! device plus a file server on the host and a mount in the guest.
//!
//! The server is `vmmbox virtiofsd` on macOS (see `virtiofs/`) and the
//! standard `virtiofsd` on Linux; `fsd.rs` starts it. Every guest kernel has
//! virtio-fs, which is not true of 9p (the RHEL family has none), and it is
//! faster. vmmbox used to offer 9p as well; it no longer does.
//!
//! VMs created before that still have a 9p line in the guest's `/etc/fstab`.
//! They are recognised by their recorded [`Transport`] and switched at their
//! next start, by rewriting that line over SSH (see [`migration_script`]).

use serde::{Deserialize, Serialize};

/// The mount tag shared between the QEMU command line and the guest's `fstab`.
pub const HOME_TAG: &str = "vmmhome";

/// What the guest's `fstab` mounts the shared home with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transport {
    /// A VM from before virtio-fs: records that predate this field mean it too.
    /// Only ever read, to know the VM needs switching; never offered.
    #[default]
    #[serde(rename = "9p")]
    NineP,
    #[serde(rename = "virtiofs")]
    VirtioFs,
}

/// The `/etc/fstab` entry that mounts the share at `path` (already escaped for
/// fstab).
pub fn fstab_line(path: &str) -> String {
    format!("{HOME_TAG} {path} virtiofs defaults,nofail 0 0")
}

/// The `sed` expression that turns the share's 9p `fstab` entry into a
/// virtio-fs one, leaving every other line alone. The mount point is kept.
const REWRITE_FSTAB: &str = r"s#^\(vmmhome[[:space:]]\{1,\}[^[:space:]]\{1,\}[[:space:]]\{1,\}\)9p[[:space:]]\{1,\}[^[:space:]]\{1,\}#\1virtiofs defaults,nofail#";

/// Shell for the guest, as root, that switches the share from 9p to virtio-fs
/// and mounts it at `mount_point`: the VM was started with a virtio-fs device,
/// so its old 9p mount failed at boot (it is `nofail`) and the home is not
/// there yet. Safe to run again.
pub fn migration_script(mount_point: &str) -> String {
    let mp = crate::util::sh_quote(mount_point);
    format!(
        "set -e\n\
         tmp=$(mktemp)\n\
         sed '{REWRITE_FSTAB}' /etc/fstab > \"$tmp\"\n\
         cat \"$tmp\" > /etc/fstab\n\
         rm -f \"$tmp\"\n\
         if command -v systemctl >/dev/null 2>&1; then systemctl daemon-reload || true; fi\n\
         mountpoint -q {mp} || mount {mp}\n\
         [ \"$(findmnt -n -o FSTYPE {mp})\" = virtiofs ]\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fstab_entry() {
        assert_eq!(
            fstab_line("/Users/me"),
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

    /// Run the rewrite the way the guest will, on a copy of an fstab.
    #[cfg(unix)]
    fn rewrite(fstab: &str) -> String {
        let dir = std::env::temp_dir().join(format!("vmmbox-fstab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("fstab");
        std::fs::write(&file, fstab).unwrap();
        let out = std::process::Command::new("sed")
            .arg(REWRITE_FSTAB)
            .arg(&file)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::remove_dir_all(&dir).unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn the_9p_entry_becomes_virtiofs_and_nothing_else_changes() {
        let before = "UUID=abc / xfs defaults 0 1\n\
                      vmmhome /Users/me 9p trans=virtio,version=9p2000.L,msize=512000,cache=mmap,nofail 0 0\n\
                      /dev/vdb /data ext4 defaults 0 2\n";
        let after = "UUID=abc / xfs defaults 0 1\n\
                     vmmhome /Users/me virtiofs defaults,nofail 0 0\n\
                     /dev/vdb /data ext4 defaults 0 2\n";
        assert_eq!(rewrite(before), after);
        // Running it again changes nothing.
        assert_eq!(rewrite(after), after);
        // A mount point with an fstab-escaped space survives.
        assert_eq!(
            rewrite("vmmhome /Users/John\\040Smith 9p trans=virtio,nofail 0 0\n"),
            "vmmhome /Users/John\\040Smith virtiofs defaults,nofail 0 0\n"
        );
    }

    #[test]
    fn the_migration_mounts_and_checks_the_result() {
        let s = migration_script("/Users/John Smith");
        assert!(s.contains("mount '/Users/John Smith'"), "{s}");
        assert!(
            s.contains("= virtiofs ]"),
            "it must fail unless virtiofs is what is mounted"
        );
        assert!(s.starts_with("set -e"));
    }
}
