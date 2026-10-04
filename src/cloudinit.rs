//! cloud-init NoCloud seed: makes the guest account mirror the host account,
//! installs vmmbox's SSH key, and mounts the host home directory.
//!
//! Design notes:
//!
//! * The guest user gets the host's login name, UID and GID, and its home is
//!   always `/home/<user>`, so `$HOME` is what a Linux user expects. The host
//!   home is shared over 9p, so file ownership matches on both sides:
//!   - If the host home is at `/home/<user>` too (typical on Linux) it is
//!     mounted right there: one direct mount, and that *is* the guest home.
//!   - Otherwise (`/Users/me` on macOS, `/var/home/me` on Silverblue) it is
//!     mounted at its own host path, so absolute host paths work in the guest,
//!     and `/home/<user>` is an ordinary directory on the VM's own disk. Linux
//!     dotfiles, caches and configs then stay out of the host home.
//! * Because the home can be the host's, `~/.ssh/authorized_keys` must not be
//!   used: cloud-init would write through the mount into the host's real
//!   `~/.ssh`. sshd is pointed at a root-owned key file in `/etc/ssh` instead.
//! * The setup script runs from `bootcmd`, which cloud-init executes before it
//!   creates users, so the group exists and the home is mounted by then. It is
//!   idempotent because `bootcmd` runs on every boot.
//! * Sound needs guest-side help: PipeWire packages, membership of the `audio`
//!   group (SSH sessions have no logind seat, so device ACLs don't apply), and
//!   on some distros the sound kernel modules, which cloud kernels leave out.
//!   The module install is a script that runs on first boot and again at later
//!   boots if an in-guest kernel upgrade left the new kernel without them.
//! * A GID like macOS's 20 may already belong to a differently-named system
//!   group (`dialout` on Debian/Ubuntu), so the group is created with `-o`
//!   (non-unique GID).

use crate::host::HostUser;
use crate::util::sh_quote;
use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// 9p mount tag shared between the QEMU command line and the guest fstab.
pub const HOME_TAG: &str = "vmmhome";
const MOUNT_OPTS: &str = "trans=virtio,version=9p2000.L,msize=512000,cache=mmap,nofail";
const SEED_SIZE: u64 = 4 << 20;

pub struct Seed<'a> {
    pub hostname: &'a str,
    pub instance_id: &'a str,
    pub user: &'a HostUser,
    pub ssh_public_key: &'a str,
    /// The user's home directory in the guest, `/home/<user>`.
    pub guest_home: &'a str,
    /// The host's home directory path. When shared it is mounted at this same
    /// path in the guest; that mount is the guest home only if the two match.
    pub host_home: &'a str,
    /// Whether the host home is shared into the guest at all.
    pub share_home: bool,
    /// Packages to install on first boot (the audio stack).
    pub packages: &'a [&'a str],
    /// Command that installs sound kernel modules for the running kernel.
    pub kernel_modules_cmd: Option<&'a str>,
}

/// Escape a path for an fstab field, where whitespace separates fields.
pub fn fstab_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            ' ' => out.push_str("\\040"),
            '\t' => out.push_str("\\011"),
            '\n' => out.push_str("\\012"),
            '\\' => out.push_str("\\134"),
            c => out.push(c),
        }
    }
    out
}

const KERNEL_MODULES_SCRIPT: &str = "/usr/local/sbin/vmmbox-kernel-modules";
const KERNEL_MODULES_UNIT: &str = "/etc/systemd/system/vmmbox-kernel-modules.service";

impl Seed<'_> {
    pub fn meta_data(&self) -> String {
        format!(
            "instance-id: {}\nlocal-hostname: {}\n",
            self.instance_id, self.hostname
        )
    }

    fn setup_script(&self) -> String {
        let mut s = format!(
            "# vmmbox: mirror the host account (idempotent; bootcmd runs every boot)\n\
             user={user}\n\
             gid={gid}\n\
             cur=$(getent group \"$user\" | cut -d: -f3)\n\
             if [ -z \"$cur\" ]; then\n\
             \x20 groupadd -o -g \"$gid\" \"$user\"\n\
             elif [ \"$cur\" != \"$gid\" ]; then\n\
             \x20 groupmod -o -g \"$gid\" \"$user\"\n\
             fi\n",
            user = sh_quote(&self.user.name),
            gid = self.user.gid,
        );
        if self.share_home {
            let nine_p = format!(
                "{HOME_TAG} {} 9p {MOUNT_OPTS} 0 0",
                fstab_escape(self.host_home)
            );
            s.push_str(&format!(
                "home={host}\n\
                 changed=0\n\
                 mkdir -p \"$home\"\n\
                 if ! grep -qs '^{tag}[[:space:]]' /etc/fstab; then\n\
                 \x20 printf '%s\\n' {nine_p} >> /etc/fstab\n\
                 \x20 changed=1\n\
                 fi\n",
                host = sh_quote(self.host_home),
                tag = HOME_TAG,
                nine_p = sh_quote(&nine_p),
            ));
            s.push_str(
                "if [ \"$changed\" = 1 ] && command -v systemctl >/dev/null 2>&1; then systemctl daemon-reload || true; fi\n\
                 mountpoint -q \"$home\" || mount \"$home\" || echo \"vmmbox: failed to mount the host home at $home\" >&2\n",
            );
        }
        s
    }

    fn kernel_modules_files(&self) -> String {
        let Some(cmd) = self.kernel_modules_cmd else {
            return String::new();
        };
        format!(
            "\x20 - path: {KERNEL_MODULES_SCRIPT}\n\
             \x20   permissions: \"0755\"\n\
             \x20   content: |\n\
             \x20     #!/bin/sh\n\
             \x20     # Install the sound modules for the running kernel, then load them.\n\
             \x20     {cmd}\n\
             \x20     modprobe virtio_snd || true\n\
             \x20 - path: {KERNEL_MODULES_UNIT}\n\
             \x20   permissions: \"0644\"\n\
             \x20   content: |\n\
             \x20     [Unit]\n\
             \x20     Description=Install sound kernel modules for the running kernel\n\
             \x20     After=network-online.target\n\
             \x20     Wants=network-online.target\n\
             \x20     ConditionPathExistsGlob=!/lib/modules/%v/kernel/sound/virtio/virtio_snd.ko*\n\
             \x20     [Service]\n\
             \x20     Type=oneshot\n\
             \x20     ExecStart={KERNEL_MODULES_SCRIPT}\n\
             \x20     [Install]\n\
             \x20     WantedBy=multi-user.target\n"
        )
    }

    pub fn user_data(&self) -> String {
        let script: String = self
            .setup_script()
            .lines()
            .map(|l| format!("    {l}\n"))
            .collect();
        let mut y = format!(
            "#cloud-config\n\
             disable_root: true\n\
             ssh_pwauth: false\n\
             bootcmd:\n  - |\n{script}\
             write_files:\n\
             \x20 - path: /etc/ssh/sshd_config.d/10-vmmbox.conf\n\
             \x20   permissions: \"0644\"\n\
             \x20   content: |\n\
             \x20     # Keys live outside the home directory, which is shared from the host.\n\
             \x20     AuthorizedKeysFile /etc/ssh/vmmbox_authorized_keys\n\
             \x20 - path: /etc/ssh/vmmbox_authorized_keys\n\
             \x20   permissions: \"0644\"\n\
             \x20   content: |\n\
             \x20     {key}\n\
             {modules_files}\
             users:\n\
             \x20 - name: {name}\n\
             \x20   uid: {uid}\n\
             \x20   gecos: {gecos}\n\
             \x20   primary_group: {name}\n\
             \x20   groups: [audio, video, render]\n\
             \x20   shell: /bin/bash\n\
             \x20   lock_passwd: true\n\
             \x20   sudo: \"ALL=(ALL) NOPASSWD:ALL\"\n",
            key = self.ssh_public_key.trim(),
            modules_files = self.kernel_modules_files(),
            name = self.user.name,
            uid = self.user.uid,
            // JSON strings are valid YAML double-quoted scalars.
            gecos = serde_json::to_string(&self.user.full_name).unwrap_or_else(|_| "\"\"".into()),
        );
        // The home is created by cloud-init like any other account's, unless the
        // shared host home is mounted right at it.
        if self.share_home && self.guest_home == self.host_home {
            y.push_str(&format!(
                "    homedir: {}\n    no_create_home: true\n",
                serde_json::to_string(self.guest_home).unwrap_or_default()
            ));
        }
        if !self.packages.is_empty() {
            y.push_str("package_update: true\npackages:\n");
            for p in self.packages {
                y.push_str(&format!("  - {p}\n"));
            }
        }
        // First-boot commands, run in order after packages are installed.
        let mut run: Vec<String> = Vec::new();
        if self.kernel_modules_cmd.is_some() {
            // restorecon: files written by cloud-init may lack the SELinux label
            // systemd needs to execute the script.
            run.push(format!(
                "[ sh, -c, \"command -v restorecon >/dev/null && restorecon {KERNEL_MODULES_SCRIPT} {KERNEL_MODULES_UNIT}; true\" ]"
            ));
            run.push(KERNEL_MODULES_SCRIPT.to_string());
            run.push("[ systemctl, enable, vmmbox-kernel-modules.service ]".to_string());
        }
        // PipeWire runs as socket-activated user units. Make sure they are
        // enabled, keep the user's systemd manager running from boot (a fresh
        // SSH session otherwise races the manager's start-up), and restart it
        // now: `vmmbox start` probes SSH while packages are still installing,
        // so a manager started by a probe never saw the new unit files.
        run.push(
            "[ sh, -c, \"systemctl --global enable pipewire.socket pipewire-pulse.socket wireplumber.service || true\" ]"
                .to_string(),
        );
        run.push(format!("[ loginctl, enable-linger, {} ]", self.user.name));
        run.push(format!(
            "[ systemctl, restart, \"user@{}.service\" ]",
            self.user.uid
        ));
        y.push_str("runcmd:\n");
        for item in run {
            y.push_str(&format!("  - {item}\n"));
        }
        y
    }

    /// Write the seed as a small FAT image labelled `CIDATA`, which cloud-init's
    /// NoCloud datasource discovers by label.
    pub fn write_image(&self, path: &Path) -> Result<()> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        file.set_len(SEED_SIZE)?;
        fatfs::format_volume(
            &mut file,
            fatfs::FormatVolumeOptions::new().volume_label(*b"CIDATA     "),
        )
        .context("formatting the cloud-init seed image")?;
        let fs = fatfs::FileSystem::new(&mut file, fatfs::FsOptions::new())
            .context("opening the cloud-init seed image")?;
        let root = fs.root_dir();
        for (name, body) in [
            ("meta-data", self.meta_data()),
            ("user-data", self.user_data()),
        ] {
            let mut f = root.create_file(name)?;
            f.truncate()?;
            f.write_all(body.as_bytes())?;
            f.flush()?;
        }
        drop(root);
        fs.unmount()
            .context("finalising the cloud-init seed image")?;
        file.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn user() -> HostUser {
        HostUser {
            name: "ecurtin".into(),
            uid: 501,
            gid: 20,
            full_name: "Eric Curtin".into(),
            home: PathBuf::from("/Users/ecurtin"),
        }
    }

    fn seed<'a>(u: &'a HostUser, share: bool) -> Seed<'a> {
        seed_at(u, share, "/Users/ecurtin")
    }

    /// A seed whose host home is at `host_home` (the guest home is always
    /// /home/ecurtin).
    fn seed_at<'a>(u: &'a HostUser, share: bool, host_home: &'a str) -> Seed<'a> {
        Seed {
            hostname: "ubuntu",
            instance_id: "vmmbox-ubuntu-1",
            user: u,
            ssh_public_key: "ssh-ed25519 AAAAC3Nza vmmbox",
            guest_home: "/home/ecurtin",
            host_home,
            share_home: share,
            packages: &["pipewire", "wireplumber"],
            kernel_modules_cmd: Some("apt-get install -y \"linux-modules-extra-$(uname -r)\""),
        }
    }

    #[test]
    fn host_home_elsewhere_leaves_the_guest_home_local() {
        let u = user();
        let ud = seed(&u, true).user_data(); // host home /Users/ecurtin
        assert!(ud.starts_with("#cloud-config\n"));
        assert!(ud.contains("  - name: ecurtin\n    uid: 501\n"));
        assert!(ud.contains("primary_group: ecurtin"));
        // cloud-init creates /home/ecurtin on the VM's disk like any account's
        // home: no explicit homedir, and it must not skip creating it.
        assert!(!ud.contains("homedir"), "{ud}");
        assert!(!ud.contains("no_create_home"), "{ud}");
        // The host home is shared at its own path; there is no bind mount.
        assert!(!ud.contains("bind"), "{ud}");
        assert!(ud.contains("gid=20\n"));
        assert!(ud.contains("groupadd -o -g \"$gid\" \"$user\""));
        assert!(ud.contains("AuthorizedKeysFile /etc/ssh/vmmbox_authorized_keys"));
        assert!(ud.contains("      ssh-ed25519 AAAAC3Nza vmmbox\n"));
        // The key must never go through a (possibly shared) home directory.
        assert!(!ud.contains("ssh_authorized_keys"));
    }

    #[test]
    fn host_home_at_home_user_is_the_guest_home() {
        let u = user();
        let ud = seed_at(&u, true, "/home/ecurtin").user_data();
        // One direct mount, and cloud-init must not try to create the home.
        assert!(ud.contains("homedir: \"/home/ecurtin\""), "{ud}");
        assert!(ud.contains("no_create_home: true"), "{ud}");
        assert!(!ud.contains("bind"), "{ud}");
    }

    #[test]
    fn fstab_escaping() {
        assert_eq!(fstab_escape("/Users/me"), "/Users/me");
        assert_eq!(fstab_escape("/Users/John Smith"), "/Users/John\\040Smith");
        assert_eq!(fstab_escape("/a\\b"), "/a\\134b");
    }

    /// Run the generated boot script in a real shell against a scratch fstab,
    /// with the commands that need root stubbed out, twice (it runs on every
    /// boot). Returns the resulting fstab and the log of stubbed calls.
    fn run_boot_script(host_home: &str, tag: &str) -> (String, String) {
        let u = user();
        let script = seed_at(&u, true, host_home).setup_script();
        let dir = std::env::temp_dir().join(format!("vmmbox-boot-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (fstab, log) = (dir.join("fstab"), dir.join("log"));
        std::fs::write(&fstab, "UUID=1234 / ext4 defaults 0 1\n").unwrap();
        std::fs::write(&log, "").unwrap();
        let script = script.replace("/etc/fstab", &fstab.to_string_lossy());
        let stubs = format!(
            "L='{}'\n\
             mkdir() {{ echo \"mkdir $*\" >> \"$L\"; }}\n\
             mountpoint() {{ return 1; }}\n\
             mount() {{ echo \"mount $*\" >> \"$L\"; }}\n\
             systemctl() {{ echo \"systemctl $*\" >> \"$L\"; }}\n\
             command() {{ return 0; }}\n\
             getent() {{ return 0; }}\n\
             groupadd() {{ :; }}\n\
             groupmod() {{ :; }}\n",
            log.display()
        );
        for _ in 0..2 {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{stubs}{script}"))
                .status()
                .unwrap();
            assert!(status.success());
        }
        let out = (
            std::fs::read_to_string(&fstab).unwrap(),
            std::fs::read_to_string(&log).unwrap(),
        );
        std::fs::remove_dir_all(&dir).unwrap();
        out
    }

    fn count(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    #[test]
    fn host_home_elsewhere_is_one_mount_at_its_own_path() {
        let (fstab, log) = run_boot_script("/Users/ecurtin", "diff");
        // Exactly the 9p share at the host path, written once though the script
        // ran twice, with existing entries kept and nothing else added.
        assert_eq!(
            count(&fstab, "vmmhome /Users/ecurtin 9p trans=virtio"),
            1,
            "{fstab}"
        );
        assert!(!fstab.contains("bind"), "no bind mounts: {fstab}");
        assert!(
            !fstab.contains("/home/ecurtin"),
            "guest home must stay local: {fstab}"
        );
        assert!(fstab.starts_with("UUID=1234 / ext4 defaults 0 1\n"));
        assert_eq!(fstab.lines().count(), 2, "{fstab}");
        // Only the host path is created and mounted; /home/<user> is left to
        // cloud-init.
        assert!(log.contains("mkdir -p /Users/ecurtin"), "{log}");
        assert!(!log.contains("/home/ecurtin"), "{log}");
        assert_eq!(
            count(&log, "mount /Users/ecurtin"),
            2,
            "once per run: {log}"
        );
        // systemd is told about the new fstab once, on the run that changed it.
        assert_eq!(count(&log, "systemctl daemon-reload"), 1, "{log}");
    }

    #[test]
    fn host_home_at_home_user_is_a_single_direct_mount() {
        let (fstab, log) = run_boot_script("/home/ecurtin", "same");
        assert_eq!(count(&fstab, "vmmhome /home/ecurtin 9p"), 1, "{fstab}");
        assert!(
            !fstab.contains("bind"),
            "no bind mount onto itself: {fstab}"
        );
        assert_eq!(fstab.lines().count(), 2, "{fstab}");
        assert_eq!(count(&log, "mount /home/ecurtin"), 2, "{log}"); // once per run
    }

    #[test]
    fn paths_with_spaces_are_escaped_in_fstab_but_not_in_commands() {
        let (fstab, log) = run_boot_script("/Users/John Smith", "space");
        assert!(
            fstab.contains("vmmhome /Users/John\\040Smith 9p "),
            "{fstab}"
        );
        // mount(8) takes the real path and decodes the fstab itself.
        assert!(log.contains("mount /Users/John Smith"), "{log}");
        // The fstab field count is intact: every vmmbox line has exactly 6 fields.
        for line in fstab.lines().filter(|l| l.contains("/Users/")) {
            assert_eq!(line.split_whitespace().count(), 6, "{line}");
        }
    }

    #[test]
    fn unshared_home_user_data() {
        let u = user();
        let ud = seed(&u, false).user_data();
        assert!(!ud.contains("homedir"));
        assert!(!ud.contains("9p"));
        assert!(!ud.contains("no_create_home"));
        assert!(ud.contains("groupadd"));
    }

    #[test]
    fn script_lines_are_indented_inside_block() {
        let u = user();
        let ud = seed(&u, true).user_data();
        let start = ud.find("bootcmd:\n  - |\n").unwrap();
        let block = &ud[start..ud.find("write_files:").unwrap()];
        for line in block.lines().skip(2) {
            assert!(line.starts_with("    "), "unindented script line: {line:?}");
        }
    }

    #[test]
    fn audio_provisioning() {
        let u = user();
        let ud = seed(&u, true).user_data();
        assert!(ud.contains("groups: [audio, video, render]"));
        assert!(ud.contains("package_update: true\npackages:\n  - pipewire\n  - wireplumber\n"));
        assert!(ud.contains("path: /usr/local/sbin/vmmbox-kernel-modules"));
        assert!(ud.contains("      apt-get install -y \"linux-modules-extra-$(uname -r)\"\n"));
        assert!(ud.contains(
            "ConditionPathExistsGlob=!/lib/modules/%v/kernel/sound/virtio/virtio_snd.ko*"
        ));
        assert!(ud.contains("runcmd:"));
        assert!(ud.contains("[ systemctl, enable, vmmbox-kernel-modules.service ]"));
        assert!(ud.contains("[ loginctl, enable-linger, ecurtin ]"));
        // The module script must run before the unit is enabled.
        assert!(
            ud.find("  - /usr/local/sbin/vmmbox-kernel-modules\n") < ud.find("systemctl, enable")
        );
    }

    #[test]
    fn no_module_script_when_not_needed() {
        let u = user();
        let mut s = seed(&u, true);
        s.kernel_modules_cmd = None;
        let ud = s.user_data();
        assert!(!ud.contains("vmmbox-kernel-modules"));
        assert!(ud.contains("packages:"));
        // Lingering is independent of the module script.
        assert!(ud.contains("  - [ loginctl, enable-linger, ecurtin ]\n"));
        assert!(ud.contains("[ systemctl, restart, \"user@501.service\" ]"));
        assert!(ud.contains(
            "systemctl --global enable pipewire.socket pipewire-pulse.socket wireplumber.service"
        ));
    }

    #[test]
    fn meta_data() {
        let u = user();
        assert_eq!(
            seed(&u, true).meta_data(),
            "instance-id: vmmbox-ubuntu-1\nlocal-hostname: ubuntu\n"
        );
    }

    #[test]
    fn seed_image_roundtrip() {
        let u = user();
        let path = std::env::temp_dir().join(format!("vmmbox-seed-{}.img", std::process::id()));
        let s = seed(&u, true);
        s.write_image(&path).unwrap();

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let fs = fatfs::FileSystem::new(file, fatfs::FsOptions::new()).unwrap();
        assert_eq!(fs.volume_label().trim(), "CIDATA");
        let mut text = String::new();
        std::io::Read::read_to_string(
            &mut fs.root_dir().open_file("user-data").unwrap(),
            &mut text,
        )
        .unwrap();
        assert_eq!(text, s.user_data());
        let mut meta = String::new();
        std::io::Read::read_to_string(
            &mut fs.root_dir().open_file("meta-data").unwrap(),
            &mut meta,
        )
        .unwrap();
        assert_eq!(meta, s.meta_data());
        drop(fs);
        std::fs::remove_file(&path).unwrap();
    }
}
