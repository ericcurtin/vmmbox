//! VM records on disk, and VM creation.
//!
//! A VM is named after its distro (one VM per distro) and lives in
//! `<root>/vms/<name>/`. Its disk is an independent copy of the pulled image,
//! so re-pulling or deleting the image never affects an existing VM.

use crate::cloudinit::Seed;
use crate::host::{HostUser, Platform};
use crate::image::ImageMeta;
use crate::paths::Paths;
use crate::qemu::{self, Qemu};
use crate::resources;
use crate::ssh;
use crate::util::now_secs;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VmState {
    pub name: String,
    pub distro: String,
    pub version: String,
    pub arch: String,
    pub created_at: u64,
    /// Virtual size of the disk. The qcow2 file grows on demand up to this.
    pub disk_bytes: u64,
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    /// The user's home directory in the guest.
    pub guest_home: String,
    /// The host's home directory path. When shared it is mounted at this same
    /// path in the guest. That mount is `guest_home` itself only if the two
    /// match; otherwise `guest_home` is a plain directory on the VM's disk.
    pub host_home: String,
    pub home_shared: bool,
    /// Whether the VM has been up before. First boot installs packages, so it
    /// is slower and says so. VMs recorded before this field existed have run.
    #[serde(default = "yes")]
    pub booted_before: bool,

    // Runtime state, updated on start/stop.
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub ssh_port: u16,
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub cpus: u32,
    #[serde(default)]
    pub memory_bytes: u64,
}

fn yes() -> bool {
    true
}

impl VmState {
    /// Where the shared host home is mounted in the guest (its host path), or
    /// `None` if the home is not shared.
    pub fn shared_home_path(&self) -> Option<&str> {
        self.home_shared.then_some(self.host_home.as_str())
    }
}

pub struct Vm {
    pub dir: PathBuf,
    pub state: VmState,
}

impl Vm {
    pub fn disk(&self) -> PathBuf {
        self.dir.join("disk.qcow2")
    }
    pub fn seed(&self) -> PathBuf {
        self.dir.join("seed.img")
    }
    pub fn efi_vars(&self) -> PathBuf {
        self.dir.join("efi-vars.fd")
    }
    pub fn key(&self) -> PathBuf {
        self.dir.join("id_ed25519")
    }
    pub fn known_hosts(&self) -> PathBuf {
        self.dir.join("known_hosts")
    }
    pub fn console_log(&self) -> PathBuf {
        self.dir.join("console.log")
    }
    pub fn qemu_log(&self) -> PathBuf {
        self.dir.join("qemu.log")
    }

    /// `ubuntu:24.04`
    pub fn reference(&self) -> String {
        format!("{}:{}", self.state.distro, self.state.version)
    }

    /// The pid of the running QEMU process, if the VM is up.
    pub fn running_pid(&self) -> Option<u32> {
        self.state.pid.filter(|&p| crate::proc::is_qemu(p))
    }

    pub fn is_running(&self) -> bool {
        self.running_pid().is_some()
    }

    pub fn save(&self) -> Result<()> {
        let tmp = self.dir.join("vm.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.state)?)?;
        std::fs::rename(&tmp, self.dir.join("vm.json"))
            .with_context(|| format!("saving {}", self.dir.display()))
    }
}

pub fn load(paths: &Paths, name: &str) -> Result<Option<Vm>> {
    let dir = paths.vm_dir(name);
    let bytes = match std::fs::read(dir.join("vm.json")) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let state = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is corrupt", dir.join("vm.json").display()))?;
    Ok(Some(Vm { dir, state }))
}

pub fn list(paths: &Paths) -> Result<Vec<Vm>> {
    let mut vms = Vec::new();
    let Ok(rd) = std::fs::read_dir(paths.vms()) else {
        return Ok(vms);
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skip in-progress creations (".name.creating").
        if name.starts_with('.') {
            continue;
        }
        if let Some(vm) = load(paths, &name)? {
            vms.push(vm);
        }
    }
    vms.sort_by(|a, b| a.state.name.cmp(&b.state.name));
    Ok(vms)
}

/// Create the VM for `image`, built in a scratch directory and moved into place
/// only when complete so a failure never leaves a half-made VM behind.
pub fn create(
    paths: &Paths,
    platform: Platform,
    qemu: &Qemu,
    user: &HostUser,
    image: &ImageMeta,
    base_disk: &Path,
) -> Result<Vm> {
    let name = image.distro.clone();
    let final_dir = paths.vm_dir(&name);
    let scratch = paths.vms().join(format!(".{name}.creating"));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;

    let built = build(paths, platform, qemu, user, image, base_disk, &scratch);
    let state = match built {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(e);
        }
    };
    if let Err(e) = std::fs::rename(&scratch, &final_dir) {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(e).with_context(|| format!("creating {}", final_dir.display()));
    }
    Ok(Vm {
        dir: final_dir,
        state,
    })
}

fn build(
    paths: &Paths,
    platform: Platform,
    qemu: &Qemu,
    user: &HostUser,
    image: &ImageMeta,
    base_disk: &Path,
    dir: &Path,
) -> Result<VmState> {
    let name = &image.distro;
    let created_at = now_secs();

    // Disk: the largest power of two below the host volume's capacity, sparse.
    let disk_bytes = resources::disk_for(resources::volume_capacity(paths.root())?);
    let disk = dir.join("disk.qcow2");
    std::fs::copy(base_disk, &disk)
        .with_context(|| format!("copying {} to {}", base_disk.display(), disk.display()))?;
    qemu.resize(&disk, disk_bytes)?;

    // Share the host home only where QEMU can (9p), and warn rather than fail
    // where it can't: the VM is still useful without it.
    let home_shared = if !platform.supports_home_share() {
        eprintln!(
            "warning: sharing the host home directory is not supported on {} (QEMU has no \
             9p/virtiofs there); the VM will have its own /home/{}",
            platform.os.name(),
            user.name
        );
        false
    } else if !qemu.supports_9p() {
        eprintln!(
            "warning: this QEMU build lacks virtio-9p; the host home directory will not be shared"
        );
        false
    } else {
        true
    };
    // The account's home is /home/<user>, whatever the host's layout. The host
    // home is shared at its own host path, and is the guest home only when that
    // path is the same (see cloudinit.rs).
    let guest_home = format!("/home/{}", user.name);
    let host_home = user
        .home
        .to_str()
        .with_context(|| format!("home directory {} is not valid UTF-8", user.home.display()))?
        .to_string();

    let public_key = ssh::generate_keypair(dir)?;

    if platform.arch == crate::host::Arch::Aarch64 {
        // aarch64 guests need UEFI; each VM gets its own variable store.
        let fw = qemu.firmware(platform.os)?;
        qemu::prepare_efi_vars(&fw.vars_template, &dir.join("efi-vars.fd"))?;
    }

    let family = crate::distro::lookup(&image.distro)
        .map(|d| d.family)
        .with_context(|| format!("unknown distro '{}'", image.distro))?;
    let packages: Vec<&str> = family
        .audio_packages()
        .iter()
        .chain(family.gui_packages())
        .copied()
        .collect();
    let instance_id = format!("vmmbox-{name}-{created_at}");
    Seed {
        hostname: name,
        instance_id: &instance_id,
        user,
        ssh_public_key: &public_key,
        guest_home: &guest_home,
        host_home: &host_home,
        share_home: home_shared,
        packages: &packages,
        kernel_modules_cmd: family.kernel_modules_cmd(),
    }
    .write_image(&dir.join("seed.img"))?;

    let state = VmState {
        name: name.clone(),
        distro: image.distro.clone(),
        version: image.version.clone(),
        arch: image.arch.clone(),
        created_at,
        disk_bytes,
        user: user.name.clone(),
        uid: user.uid,
        gid: user.gid,
        guest_home,
        host_home,
        home_shared,
        pid: None,
        ssh_port: 0,
        booted_before: false,
        started_at: None,
        cpus: 0,
        memory_bytes: 0,
    };
    std::fs::write(dir.join("vm.json"), serde_json::to_vec_pretty(&state)?)?;
    Ok(state)
}
