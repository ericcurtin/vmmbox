//! Performance-core detection.
//!
//! On heterogeneous CPUs (Apple Silicon, Intel hybrid, big.LITTLE) the guest
//! gets only the performance cores; efficiency cores would just make vCPUs
//! unpredictably slow. On homogeneous CPUs it gets every logical CPU.

/// Number of vCPUs to give a guest. Always at least 1.
pub fn guest_cpus() -> usize {
    detect_performance_cores()
        .filter(|&n| n > 0)
        .unwrap_or_else(logical_cores)
        .max(1)
}

pub fn logical_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Count the CPUs in a Linux cpulist such as `0-7,16,18-19`.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn count_cpulist(s: &str) -> Option<usize> {
    let mut total = 0;
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (usize, usize) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
                total += b.checked_sub(a)? + 1;
            }
            None => {
                part.trim().parse::<usize>().ok()?;
                total += 1;
            }
        }
    }
    Some(total)
}

/// Whether a macOS perf level is an efficiency level. Apple names levels
/// ("Performance"/"Efficiency", or "Super"/"Performance" on chips without
/// efficiency cores), so exclude by name rather than assuming level 0 only.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn is_efficiency_level(name: &str) -> bool {
    name.to_ascii_lowercase().contains("efficiency")
}

#[cfg(target_os = "macos")]
fn detect_performance_cores() -> Option<usize> {
    fn sysctl(name: &str) -> Option<String> {
        let out = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", name])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    let levels: usize = sysctl("hw.nperflevels")?.parse().ok()?;
    if levels < 2 {
        return None; // homogeneous
    }
    let mut total = 0;
    for i in 0..levels {
        let cpus: usize = sysctl(&format!("hw.perflevel{i}.logicalcpu"))?
            .parse()
            .ok()?;
        let is_perf = match sysctl(&format!("hw.perflevel{i}.name")) {
            Some(name) => !is_efficiency_level(&name),
            None => i == 0, // no names: level 0 is the highest-performance level
        };
        if is_perf {
            total += cpus;
        }
    }
    Some(total)
}

#[cfg(target_os = "linux")]
fn detect_performance_cores() -> Option<usize> {
    // Intel hybrid exposes the P-core logical CPUs directly.
    if let Ok(list) = std::fs::read_to_string("/sys/devices/cpu_core/cpus") {
        return count_cpulist(&list);
    }
    // Arm (and RISC-V) expose a relative capacity per CPU; keep the top tier.
    let mut caps = Vec::new();
    for entry in std::fs::read_dir("/sys/devices/system/cpu").ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_cpu = name
            .strip_prefix("cpu")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !is_cpu {
            continue;
        }
        if let Ok(s) = std::fs::read_to_string(entry.path().join("cpu_capacity"))
            && let Ok(c) = s.trim().parse::<u32>()
        {
            caps.push(c);
        }
    }
    let (&max, &min) = (caps.iter().max()?, caps.iter().min()?);
    (max != min).then(|| caps.iter().filter(|&&c| c == max).count())
}

#[cfg(windows)]
fn detect_performance_cores() -> Option<usize> {
    use windows_sys::Win32::System::SystemInformation::{
        CpuSetInformation, GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: standard two-call pattern. The first call reports the required
    // size, the second fills a buffer of at least that size. The buffer is
    // u64-backed so the 8-byte-aligned entries can be read in place, and each
    // entry's own `Size` field is used to step to the next one.
    unsafe {
        let process = GetCurrentProcess();
        let mut needed: u32 = 0;
        GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut needed, process, 0);
        if needed == 0 {
            return None;
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let info = buf.as_mut_ptr() as *mut SYSTEM_CPU_SET_INFORMATION;
        let mut returned: u32 = 0;
        if GetSystemCpuSetInformation(info, needed, &mut returned, process, 0) == 0 {
            return None;
        }

        // Higher EfficiencyClass = higher performance. Keep the top class.
        let mut classes = Vec::new();
        let base = buf.as_ptr() as *const u8;
        let mut offset = 0usize;
        while offset + std::mem::size_of::<u32>() * 2 <= returned as usize {
            let entry = &*(base.add(offset) as *const SYSTEM_CPU_SET_INFORMATION);
            if entry.Size == 0 {
                break;
            }
            if entry.Type == CpuSetInformation {
                classes.push(entry.Anonymous.CpuSet.EfficiencyClass);
            }
            offset += entry.Size as usize;
        }
        let (&max, &min) = (classes.iter().max()?, classes.iter().min()?);
        (max != min).then(|| classes.iter().filter(|&&c| c == max).count())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn detect_performance_cores() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpulists() {
        assert_eq!(count_cpulist("0-7"), Some(8));
        assert_eq!(count_cpulist("0-7,16,18-19\n"), Some(11));
        assert_eq!(count_cpulist("3"), Some(1));
        assert_eq!(count_cpulist(""), Some(0));
        assert_eq!(count_cpulist("a-b"), None);
        assert_eq!(count_cpulist("7-3"), None);
    }

    #[test]
    fn efficiency_levels() {
        assert!(is_efficiency_level("Efficiency"));
        assert!(!is_efficiency_level("Performance"));
        assert!(!is_efficiency_level("Super"));
    }

    #[test]
    fn at_least_one() {
        assert!(guest_cpus() >= 1);
        assert!(guest_cpus() <= logical_cores());
    }
}
