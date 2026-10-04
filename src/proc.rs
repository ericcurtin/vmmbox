//! Process liveness and termination, by pid.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Start `cmd` detached from this process: it must outlive vmmbox and must not
/// die with the terminal. Its output goes to `log`.
pub fn spawn_detached(cmd: &mut Command, log: &Path) -> Result<Child> {
    let out = std::fs::File::create(log).with_context(|| format!("creating {}", log.display()))?;
    let err = out.try_clone()?;
    cmd.stdin(Stdio::null()).stdout(out).stderr(err);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and the closure touches no
        // shared state, as required between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    cmd.spawn()
        .with_context(|| format!("starting {}", cmd.get_program().to_string_lossy()))
}

fn with_process<T>(pid: u32, f: impl FnOnce(&sysinfo::Process) -> T) -> Option<T> {
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid).map(f)
}

/// Terminate `pid` if it is a `vmmbox virtiofsd`. The command line is checked,
/// not just the name, so a pid reused by some other `vmmbox` invocation (a
/// concurrent `vmmbox run`, say) is left alone.
pub fn kill_virtiofsd(pid: u32) -> bool {
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    sys.process(pid).is_some_and(|p| {
        p.name().to_string_lossy().contains("vmmbox")
            && p.cmd().iter().any(|a| a == "virtiofsd")
            && p.kill()
    })
}

/// Whether `pid` is a live QEMU process. Checking the name guards against pid
/// reuse after a crash: a stale pid must not make us think the VM is running
/// (or kill something unrelated).
pub fn is_qemu(pid: u32) -> bool {
    with_process(pid, |p| {
        p.name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("qemu")
    })
    .unwrap_or(false)
}

/// Forcefully terminate `pid` if it is a QEMU process.
pub fn kill_qemu(pid: u32) -> bool {
    with_process(pid, |p| {
        p.name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("qemu")
            && p.kill()
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_not_qemu() {
        assert!(!is_qemu(std::process::id()));
        assert!(!kill_qemu(std::process::id()));
    }

    #[test]
    fn dead_pid_is_not_qemu() {
        assert!(!is_qemu(u32::MAX - 1));
    }
}
