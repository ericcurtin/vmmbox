//! The virtio-fs server for a VM, run as a separate process for as long as the
//! VM's QEMU is up.
//!
//! * macOS: `vmmbox virtiofsd`, vmmbox's own (see `virtiofs/`), since nothing
//!   else serves virtio-fs there.
//! * Linux: the standard `virtiofsd`, which the distributions package.
//!
//! QEMU connects to it over a vhost-user socket; when QEMU goes away the server
//! sees the socket close and exits. This module starts it, waits until it is
//! listening, and makes sure it does not outlive a failed start.

use crate::host::{Os, Platform};
use crate::paths::Paths;
use crate::vm::Vm;
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// Where distributions put `virtiofsd`, besides `PATH`: Fedora and most others
/// in libexec, Debian and Ubuntu under /usr/lib/qemu, plus Homebrew and cargo.
const VIRTIOFSD_DIRS: &[&str] = &[
    "/usr/libexec",
    "/usr/lib/qemu",
    "/usr/lib/virtiofsd",
    "/usr/local/libexec",
    "/usr/local/bin",
    "/home/linuxbrew/.linuxbrew/bin",
];

/// The `virtiofsd` binary on this machine, if any (Linux only).
fn find_virtiofsd() -> Option<PathBuf> {
    let own: [PathBuf; 0] = [];
    let mut extra: Vec<PathBuf> = VIRTIOFSD_DIRS.iter().map(PathBuf::from).collect();
    if let Some(home) = dirs::home_dir() {
        extra.push(home.join(".cargo/bin"));
    }
    crate::tools::find_binary("virtiofsd", &own, &extra)
}

/// Whether this machine can serve virtio-fs: macOS always can (the server is
/// part of vmmbox), Linux needs `virtiofsd` installed.
pub fn available(platform: Platform) -> bool {
    match platform.os {
        Os::Mac => true,
        Os::Linux => find_virtiofsd().is_some(),
        Os::Windows => false,
    }
}

/// What to tell someone whose machine cannot serve virtio-fs.
pub fn install_hint(os: Os) -> &'static str {
    match os {
        Os::Mac => "run `vmmbox setup` to install the QEMU vmmbox ships",
        Os::Linux => {
            "install virtiofsd (Fedora: `dnf install virtiofsd`; Debian and Ubuntu: \
             `apt install virtiofsd`) and a QEMU that has vhost-user-fs"
        }
        Os::Windows => "this is not supported on Windows",
    }
}

/// The command that serves `root` on `socket`.
#[cfg(unix)]
fn server_command(root: &Path, socket: &Path, os: Os) -> Result<std::process::Command> {
    use anyhow::Context;
    use std::process::Command;

    if os == Os::Linux {
        let bin = find_virtiofsd().context("virtiofsd is not installed")?;
        // SAFETY: getuid and getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        return Ok(virtiofsd_command(&bin, root, socket, uid, gid));
    }
    let mut cmd = Command::new(std::env::current_exe().context("finding vmmbox itself")?);
    cmd.arg("virtiofsd")
        .arg("--socket")
        .arg(socket)
        .arg("--root")
        .arg(root);
    if std::env::var_os("VMMBOX_DEBUG_VIRTIOFS").is_some() {
        cmd.arg("--debug");
    }
    Ok(cmd)
}

/// The standard `virtiofsd`, run unprivileged as `uid`:`gid`.
#[cfg(unix)]
fn virtiofsd_command(
    bin: &Path,
    root: &Path,
    socket: &Path,
    uid: u32,
    gid: u32,
) -> std::process::Command {
    {
        let mut cmd = std::process::Command::new(bin);
        cmd.arg("--socket-path")
            .arg(socket)
            .arg("--shared-dir")
            .arg(root)
            .args(["--cache", "auto", "--announce-submounts"])
            // Not the default namespace sandbox: that maps the user to root
            // inside it, and it is unavailable where unprivileged user
            // namespaces are restricted (Ubuntu 24.04, containers).
            .args(["--sandbox", "none"])
            // Unprivileged, virtiofsd cannot give files any owner but its own,
            // so a guest user other than ours, root included, could not create
            // anything. Squash every guest uid and gid onto ours. The guest
            // user already has our uid, so our files look the same to it.
            .arg("--translate-uid")
            .arg(format!("squash-guest:0:{uid}:4294967295"))
            .arg("--translate-gid")
            .arg(format!("squash-guest:0:{gid}:4294967295"));
        cmd
    }
}

/// Start the server for `vm`, sharing `root`, and wait until QEMU can connect.
/// Returns its pid and the socket QEMU must be pointed at.
#[cfg(unix)]
pub fn start(paths: &Paths, vm: &Vm, root: &Path, os: Os) -> Result<(u32, PathBuf)> {
    use std::time::{Duration, Instant};

    let socket = paths
        .runtime_dir()?
        .join(format!("{}.vfs.sock", vm.state.name));
    let _ = std::fs::remove_file(&socket);

    let mut cmd = server_command(root, &socket, os)?;
    let log = vm.dir.join("virtiofsd.log");
    let mut child = crate::proc::spawn_detached(&mut cmd, &log)?;

    // The server's socket being there means it is listening.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        if let Some(status) = child.try_wait()? {
            bail!(
                "the virtio-fs server exited during start-up ({status}):\n{}\nlog: {}",
                crate::util::tail_lines(&log, 10),
                log.display()
            );
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the virtio-fs server did not start listening; see {}",
                log.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok((child.id(), socket))
}

#[cfg(not(unix))]
pub fn start(_paths: &Paths, _vm: &Vm, _root: &Path, _os: Os) -> Result<(u32, PathBuf)> {
    bail!("virtio-fs is not supported on this host")
}

/// Stop the server with this pid, if it is still running. It normally has
/// already exited because QEMU did; this covers a start that failed before QEMU
/// connected, and a QEMU that was killed.
pub fn stop(pid: u32) {
    crate::proc::kill_virtiofsd(pid);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn args(cmd: &std::process::Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn virtiofsd_runs_unprivileged_with_every_guest_user_squashed_onto_ours() {
        let cmd = virtiofsd_command(
            Path::new("/usr/lib/qemu/virtiofsd"),
            Path::new("/home/me"),
            Path::new("/run/user/1000/vmmbox/ubuntu.vfs.sock"),
            1000,
            1001,
        );
        assert_eq!(cmd.get_program(), "/usr/lib/qemu/virtiofsd");
        let a = args(&cmd);
        let has = |w: &[&str]| a.windows(w.len()).any(|x| x == w);
        assert!(has(&[
            "--socket-path",
            "/run/user/1000/vmmbox/ubuntu.vfs.sock"
        ]));
        assert!(has(&["--shared-dir", "/home/me"]));
        // Without squashing, a guest user other than ours (root included) could
        // not create files: virtiofsd cannot chown as an ordinary user.
        assert!(has(&["--translate-uid", "squash-guest:0:1000:4294967295"]));
        assert!(has(&["--translate-gid", "squash-guest:0:1001:4294967295"]));
        // The default namespace sandbox is unavailable where unprivileged user
        // namespaces are restricted.
        assert!(has(&["--sandbox", "none"]));
    }
}
