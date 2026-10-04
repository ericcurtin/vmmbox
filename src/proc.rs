//! Process liveness and termination, by pid.

use sysinfo::{Pid, ProcessesToUpdate, System};

fn with_process<T>(pid: u32, f: impl FnOnce(&sysinfo::Process) -> T) -> Option<T> {
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid).map(f)
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
