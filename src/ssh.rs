//! SSH access to guests, using the system OpenSSH client.
//!
//! Guests are reached through QEMU's user-mode port forward on 127.0.0.1 with a
//! per-VM ed25519 key. A real `ssh` gives a proper terminal (resizing, job
//! control, signals) on every host OS for free.

use crate::vm::Vm;
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

fn find_tool(name: &str) -> Result<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if cfg!(windows) {
        // The optional "OpenSSH Client" feature installs here.
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        dirs.push(PathBuf::from(root).join("System32").join("OpenSSH"));
    }
    dirs.iter()
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
        .with_context(|| {
            format!(
                "{name} not found. vmmbox needs the OpenSSH client{}",
                if cfg!(windows) {
                    " (Settings > Optional features > OpenSSH Client)"
                } else {
                    ""
                }
            )
        })
}

/// Create the VM's keypair in `dir` and return the public key line.
pub fn generate_keypair(dir: &Path) -> Result<String> {
    let keygen = find_tool("ssh-keygen")?;
    let key = dir.join("id_ed25519");
    let out = Command::new(keygen)
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "vmmbox", "-f"])
        .arg(&key)
        .stdin(Stdio::null())
        .output()
        .context("running ssh-keygen")?;
    if !out.status.success() {
        bail!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let public = std::fs::read_to_string(dir.join("id_ed25519.pub"))?;
    Ok(public.trim().to_string())
}

pub struct Ssh {
    ssh: PathBuf,
    key: PathBuf,
    known_hosts: PathBuf,
    port: u16,
    user: String,
}

impl Ssh {
    /// Fail early, with an actionable message, if the OpenSSH client is missing.
    pub fn require_client() -> Result<()> {
        find_tool("ssh")?;
        find_tool("ssh-keygen")?;
        Ok(())
    }

    pub fn for_vm(vm: &Vm) -> Result<Self> {
        Ok(Self {
            ssh: find_tool("ssh")?,
            key: vm.key(),
            known_hosts: vm.known_hosts(),
            port: vm.state.ssh_port,
            user: vm.state.user.clone(),
        })
    }

    /// The ssh options that precede the destination: our private key and known
    /// hosts, no user config, and the forwarded port.
    pub fn args(&self) -> Vec<OsString> {
        // ssh splits `-o` values on whitespace, so quote the path. Forward
        // slashes keep Windows paths free of escape sequences.
        let known_hosts = self.known_hosts.to_string_lossy().replace('\\', "/");
        // `-F`: ignore the user's own ssh config.
        let mut a: Vec<OsString> = ["-F", NULL_DEVICE, "-i"].map(OsString::from).to_vec();
        a.push(self.key.clone().into_os_string());
        a.extend(
            [
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "PreferredAuthentications=publickey",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "LogLevel=ERROR",
                "-o",
                "ServerAliveInterval=30",
            ]
            .map(OsString::from),
        );
        a.push("-o".into());
        a.push(format!("UserKnownHostsFile=\"{known_hosts}\"").into());
        a.push("-p".into());
        a.push(self.port.to_string().into());
        a
    }

    /// An `ssh` command with our options applied; the caller adds the rest.
    pub fn command(&self) -> Command {
        let mut c = Command::new(&self.ssh);
        c.args(self.args());
        c
    }

    pub fn target(&self) -> String {
        format!("{}@127.0.0.1", self.user)
    }

    /// Whether the guest accepts our key right now.
    pub fn probe(&self) -> bool {
        self.probe_detail().is_ok()
    }

    /// Like `probe`, but says what ssh complained about if the guest does not
    /// accept us, for the error shown when a VM never becomes reachable.
    pub fn probe_detail(&self) -> std::result::Result<(), String> {
        let out = self
            .command()
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"])
            .arg(self.target())
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("could not run {}: {e}", self.ssh.display()))?;
        if out.status.success() {
            return Ok(());
        }
        let said = String::from_utf8_lossy(&out.stderr);
        let said = said.trim();
        Err(if said.is_empty() {
            format!(
                "{} exited with status {}",
                self.ssh.display(),
                exit_code(out.status)
            )
        } else {
            said.to_string()
        })
    }

    /// Run a non-interactive command and return its exit code.
    pub fn run_quiet(&self, remote: &str) -> Result<i32> {
        let status = self
            .command()
            .args(["-o", "BatchMode=yes"])
            .arg(self.target())
            .arg(remote)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("running ssh")?;
        Ok(exit_code(status))
    }

    /// Ask the guest to power itself off, giving up after `timeout`. This is how
    /// a VM with no QEMU control channel (Windows) is stopped: the guest shuts
    /// down cleanly and QEMU exits with it. `--no-block` makes systemd answer at
    /// once instead of holding the connection while the guest goes down, so a
    /// successful request is an exit status of 0.
    pub fn power_off(&self, timeout: Duration) -> Result<()> {
        let mut child = self
            .command()
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
            .arg(self.target())
            .arg("sudo -n systemctl poweroff --no-block")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("running ssh")?;
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait().context("waiting for ssh")? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("timed out asking the guest to power off");
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        // ssh has exited, so everything it wrote is in the pipe already.
        let mut why = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut why);
        }
        let why = why.trim();
        let detail = if why.is_empty() {
            String::new()
        } else {
            format!(": {why}")
        };
        match exit_code(status) {
            0 => Ok(()),
            255 => bail!("could not reach the guest over SSH{detail}"),
            code => {
                bail!("the guest did not accept the power-off request (exit status {code}){detail}")
            }
        }
    }

    /// Run `remote` attached to this terminal and return its exit code.
    pub fn run_interactive(&self, remote: &str, tty: bool) -> Result<i32> {
        let mut c = self.command();
        if tty {
            c.arg("-t");
        }
        let status = c
            .arg(self.target())
            .arg(remote)
            .status()
            .context("running ssh")?;
        Ok(exit_code(status))
    }
}

pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    status.code().unwrap_or(255)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// An `Ssh` whose "ssh" is the shell script `body`, which gets ssh's real
    /// arguments, so what vmmbox runs on the guest can be checked.
    fn fake(tag: &str, body: &str) -> (Ssh, PathBuf) {
        let dir = std::env::temp_dir().join(format!("vmmbox-ssh-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("ssh");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ssh = Ssh {
            ssh: script,
            key: dir.join("key"),
            known_hosts: dir.join("known_hosts"),
            port: 2222,
            user: "me".into(),
        };
        (ssh, dir)
    }

    #[test]
    fn a_failed_probe_says_what_ssh_complained_about() {
        let (ssh, dir) = fake(
            "probe",
            "echo 'Permission denied (publickey).' >&2; exit 255",
        );
        assert!(!ssh.probe());
        assert_eq!(
            ssh.probe_detail().unwrap_err(),
            "Permission denied (publickey)."
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_probe_that_succeeds_has_nothing_to_report() {
        let (ssh, dir) = fake("probe-ok", "exit 0");
        assert!(ssh.probe());
        assert!(ssh.probe_detail().is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn power_off_asks_the_guest_to_power_off_and_waits_for_no_answer() {
        let (ssh, dir) = fake("ok", r#"printf '%s\n' "$@" > "$(dirname "$0")/args""#);
        ssh.power_off(Duration::from_secs(10)).unwrap();
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert!(args.contains(&"BatchMode=yes"), "{args:?}");
        assert!(args.contains(&"me@127.0.0.1"), "{args:?}");
        // The command is the last argument; sudo must not ask for a password.
        assert_eq!(
            args.last().copied(),
            Some("sudo -n systemctl poweroff --no-block")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn power_off_reports_an_unreachable_guest() {
        let (ssh, dir) = fake("down", "echo 'Connection refused' >&2; exit 255");
        let err = ssh.power_off(Duration::from_secs(10)).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("could not reach the guest"), "{err}");
        assert!(err.contains("Connection refused"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn power_off_reports_a_refusal() {
        let (ssh, dir) = fake("sudo", "echo 'sudo: a password is required' >&2; exit 1");
        let err = format!("{:#}", ssh.power_off(Duration::from_secs(10)).unwrap_err());
        assert!(err.contains("did not accept"), "{err}");
        assert!(err.contains("a password is required"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A guest that stops answering must not hang `vmmbox stop`.
    #[test]
    fn power_off_gives_up_on_a_wedged_guest() {
        let (ssh, dir) = fake("hang", "sleep 30");
        let started = Instant::now();
        let err = ssh.power_off(Duration::from_millis(400)).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(dir);
    }
}
