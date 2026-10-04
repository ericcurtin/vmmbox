//! SSH access to guests, using the system OpenSSH client.
//!
//! Guests are reached through QEMU's user-mode port forward on 127.0.0.1 with a
//! per-VM ed25519 key. A real `ssh` gives a proper terminal (resizing, job
//! control, signals) on every host OS for free.

use crate::vm::Vm;
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
        self.command()
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"])
            .arg(self.target())
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
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
