//! The virtio-fs server for a VM: `vmmbox virtiofsd`, run as a separate
//! process for as long as the VM's QEMU is up.
//!
//! QEMU connects to it over a vhost-user socket; when QEMU goes away the server
//! sees the socket close and exits. This module starts it, waits until it is
//! listening, and makes sure it does not outlive a failed start.

use crate::paths::Paths;
use crate::vm::Vm;
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// Start the server for `vm`, sharing `root`, and wait until QEMU can connect.
/// Returns its pid and the socket QEMU must be pointed at.
#[cfg(unix)]
pub fn start(paths: &Paths, vm: &Vm, root: &Path) -> Result<(u32, PathBuf)> {
    use anyhow::Context;
    use std::process::Command;
    use std::time::{Duration, Instant};

    let socket = paths
        .runtime_dir()?
        .join(format!("{}.vfs.sock", vm.state.name));
    let _ = std::fs::remove_file(&socket);

    let mut cmd = Command::new(std::env::current_exe().context("finding vmmbox itself")?);
    cmd.arg("virtiofsd")
        .arg("--socket")
        .arg(&socket)
        .arg("--root")
        .arg(root);
    if std::env::var_os("VMMBOX_DEBUG_VIRTIOFS").is_some() {
        cmd.arg("--debug");
    }
    let log = vm.dir.join("virtiofsd.log");
    let mut child = crate::proc::spawn_detached(&mut cmd, &log)?;

    // The server removes the socket once QEMU has connected, so its being
    // there means it is listening and nobody has connected yet.
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
pub fn start(_paths: &Paths, _vm: &Vm, _root: &Path) -> Result<(u32, PathBuf)> {
    bail!("virtio-fs is not supported on this host")
}

/// Stop the server with this pid, if it is still running. It normally has
/// already exited because QEMU did; this covers a start that failed before QEMU
/// connected, and a QEMU that was killed.
pub fn stop(pid: u32) {
    crate::proc::kill_virtiofsd(pid);
}
