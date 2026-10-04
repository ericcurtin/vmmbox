//! Sizing policy: how much of the host a VM gets.
//!
//! Memory and (virtual) disk are the largest power of two that is strictly
//! smaller than what the host has, so the host always keeps some:
//!
//! | host   | VM     |
//! |--------|--------|
//! | 16 GiB | 8 GiB  |
//! | 32 GiB | 16 GiB |
//! | 36 GiB | 32 GiB |
//! | 64 GiB | 32 GiB |

use anyhow::{Context, Result};
use std::path::Path;

const MIB: u64 = 1 << 20;
/// Never size a guest below this, however small the host reports.
const MIN_MEMORY: u64 = 512 * MIB;

/// Largest power of two strictly less than `n` (0 if there is none).
pub fn pow2_below(n: u64) -> u64 {
    if n < 2 {
        0
    } else {
        1u64 << (63 - (n - 1).leading_zeros())
    }
}

pub fn memory_for(host_total: u64) -> u64 {
    pow2_below(host_total).max(MIN_MEMORY)
}

pub fn disk_for(volume_total: u64) -> u64 {
    pow2_below(volume_total)
}

pub fn host_memory() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory()
}

/// Total capacity of the filesystem holding `path`.
pub fn volume_capacity(path: &Path) -> Result<u64> {
    fs4::total_space(path).with_context(|| format!("reading capacity of {}", path.display()))
}

#[derive(Clone, Copy, Debug)]
pub struct Compute {
    pub cpus: u32,
    pub memory_bytes: u64,
}

/// vCPU count and memory for a guest started right now.
pub fn compute() -> Compute {
    Compute {
        cpus: crate::cpu::guest_cpus() as u32,
        memory_bytes: memory_for(host_memory()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const GIB: u64 = 1 << 30;

    #[test]
    fn power_of_two_below() {
        assert_eq!(pow2_below(0), 0);
        assert_eq!(pow2_below(1), 0);
        assert_eq!(pow2_below(2), 1);
        assert_eq!(pow2_below(3), 2);
        assert_eq!(pow2_below(4), 2);
        assert_eq!(pow2_below(5), 4);
        assert_eq!(pow2_below(u64::MAX), 1 << 63);
    }

    #[test]
    fn spec_examples() {
        // "32GB memory machine should use 16GB, 36GB machine should use 32GB"
        assert_eq!(memory_for(32 * GIB), 16 * GIB);
        assert_eq!(memory_for(36 * GIB), 32 * GIB);
    }

    #[test]
    fn more_memory_sizes() {
        assert_eq!(memory_for(8 * GIB), 4 * GIB);
        assert_eq!(memory_for(16 * GIB), 8 * GIB);
        assert_eq!(memory_for(64 * GIB), 32 * GIB);
        assert_eq!(memory_for(128 * GIB), 64 * GIB);
        // Kernels report slightly less than the nominal size.
        assert_eq!(memory_for(32 * GIB - 600 * MIB), 16 * GIB);
        assert_eq!(memory_for(16 * GIB - 300 * MIB), 8 * GIB);
        // Tiny hosts still get a bootable guest.
        assert_eq!(memory_for(GIB), 512 * MIB);
        assert_eq!(memory_for(512 * MIB), 512 * MIB);
    }

    #[test]
    fn disk_sizes() {
        // A "1 TB" disk is ~931 GiB: 512 GiB virtual.
        assert_eq!(disk_for(931 * GIB), 512 * GIB);
        assert_eq!(disk_for(512 * GIB), 256 * GIB);
        assert_eq!(disk_for(2000 * GIB), 1024 * GIB);
    }
}
