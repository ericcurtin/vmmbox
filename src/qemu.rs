//! Locating QEMU, building its command line, and launching it detached.
//!
//! Guests are always hardware-accelerated (KVM / HVF / WHPX) and always the
//! host's own architecture; there is no TCG fallback.

use crate::host::{Accel, Arch, Os, Platform};
use crate::paths::Paths;
use crate::qmp::Endpoint;
use crate::share::HOME_TAG;
use crate::tools::{exe_name, find_binary};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Escape a value for QEMU's `key=value,key=value` option syntax,
/// where a literal comma is written `,,`.
pub fn escape(s: &str) -> String {
    s.replace(',', ",,")
}

/// Where package managers put QEMU when it isn't on PATH.
fn well_known_dirs(os: Os) -> Vec<PathBuf> {
    match os {
        Os::Mac => ["/opt/homebrew/bin", "/opt/local/bin"]
            .map(PathBuf::from)
            .to_vec(),
        Os::Linux => ["/usr/bin", "/usr/local/bin", "/usr/libexec"]
            .map(PathBuf::from)
            .to_vec(),
        Os::Windows => {
            let mut dirs = Vec::new();
            for var in ["ProgramFiles", "ProgramW6432"] {
                if let Some(p) = std::env::var_os(var) {
                    dirs.push(PathBuf::from(p).join("qemu"));
                }
            }
            if let Some(p) = std::env::var_os("LOCALAPPDATA") {
                dirs.push(PathBuf::from(p).join("Programs").join("qemu"));
            }
            if let Some(p) = std::env::var_os("USERPROFILE") {
                dirs.push(PathBuf::from(p).join("scoop/apps/qemu/current"));
            }
            dirs
        }
    }
}

fn install_hint(os: Os, arch: Arch) -> String {
    match os {
        Os::Mac => "brew install qemu".into(),
        Os::Linux => match arch {
            Arch::X86_64 => "install QEMU, e.g. `sudo apt install qemu-system-x86 qemu-utils` \
                             or `sudo dnf install qemu-system-x86-core qemu-img`"
                .into(),
            Arch::Aarch64 => "install qemu-system-aarch64 and qemu-img".into(),
        },
        Os::Windows => "winget install SoftwareFreedomConservancy.QEMU".into(),
    }
}

pub struct Qemu {
    pub system: PathBuf,
    pub img: PathBuf,
    devices: std::cell::OnceCell<String>,
}

/// How guest audio reaches the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Audio {
    /// QEMU `-audiodev` options without the id, e.g. `coreaudio`.
    pub backend: String,
    /// virtio-sound if the QEMU build has it, otherwise an Intel HDA codec.
    pub virtio: bool,
}

/// A paravirtual GPU backed by the host's real one (virtio-gpu with
/// virglrenderer). Linux hosts only: elsewhere upstream QEMU cannot give a
/// Linux guest an accelerated GPU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gpu {
    /// The DRM render node QEMU draws on, e.g. `/dev/dri/renderD128`.
    pub render_node: PathBuf,
    /// Vulkan (Venus) as well as OpenGL (virgl).
    pub vulkan: bool,
}

impl Gpu {
    pub fn describe(&self) -> String {
        format!(
            "{} via {}",
            if self.vulkan {
                "Vulkan and OpenGL"
            } else {
                "OpenGL"
            },
            self.render_node.display()
        )
    }
}

/// What the host and its QEMU can do for a GPU; see [`plan_gpu`].
#[derive(Debug, Default)]
struct GpuHost {
    /// `virtio-gpu-gl-pci` exists (QEMU was built with virglrenderer).
    gl_device: bool,
    /// The `egl-headless` display exists.
    egl_headless: bool,
    /// The GL device has the `venus` option (virglrenderer 1.0 or newer).
    venus_option: bool,
    /// Host kernel (major, minor).
    kernel: Option<(u32, u32)>,
    /// Render nodes this user can open for reading and writing.
    render_nodes: Vec<PathBuf>,
}

/// The oldest kernel on which virglrenderer can pass Vulkan through (host
/// blob memory), per the QEMU documentation.
const VENUS_MIN_KERNEL: (u32, u32) = (6, 13);

/// How to treat the GPU, from `VMMBOX_GPU`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GpuMode {
    Auto,
    /// `none`: no GPU device.
    Off,
    /// `opengl`: no Vulkan, e.g. if Venus misbehaves on this host.
    OpenGl,
}

fn gpu_mode(value: Option<&str>) -> GpuMode {
    match value {
        Some("none") => GpuMode::Off,
        Some("opengl") => GpuMode::OpenGl,
        _ => GpuMode::Auto,
    }
}

/// `6.13.0-generic` -> (6, 13).
fn parse_kernel(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor: String = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some((major, minor.parse().ok()?))
}

/// Decide whether the guest gets a GPU. `Err` carries the reason it does not,
/// which `vmmbox start` shows.
///
/// A GPU QEMU cannot use is worse than none: without a usable render node QEMU
/// refuses to start at all. So every requirement is checked up front.
fn plan_gpu(os: Os, mode: GpuMode, host: &GpuHost) -> std::result::Result<Gpu, String> {
    if os != Os::Linux {
        return Err(format!(
            "none (QEMU cannot give a Linux guest an accelerated GPU on {})",
            os.name()
        ));
    }
    if mode == GpuMode::Off {
        return Err("none (disabled by VMMBOX_GPU)".into());
    }
    if !host.gl_device {
        return Err("none (this QEMU was built without virglrenderer)".into());
    }
    if !host.egl_headless {
        return Err("none (this QEMU has no egl-headless display)".into());
    }
    let Some(node) = host.render_nodes.first() else {
        return Err(
            "none (no usable /dev/dri/renderD*; is your user in the 'render' group?)".into(),
        );
    };
    let vulkan = mode == GpuMode::Auto
        && host.venus_option
        && host.kernel.is_some_and(|k| k >= VENUS_MIN_KERNEL);
    Ok(Gpu {
        render_node: node.clone(),
        vulkan,
    })
}

fn usable_render_nodes() -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    let mut nodes: Vec<PathBuf> = dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("renderD"))
        })
        .filter(|p| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(p)
                .is_ok()
        })
        .collect();
    nodes.sort();
    nodes
}

pub struct Firmware {
    pub code: PathBuf,
    pub vars_template: PathBuf,
}

impl Qemu {
    /// Find QEMU: vmmbox's own tools prefix first (where a GPU-enabled build
    /// lives), then PATH, then the usual package-manager locations.
    pub fn locate(platform: Platform, paths: &Paths) -> Result<Self> {
        let own = [paths.bin()];
        let extra = well_known_dirs(platform.os);
        let sys_name = format!("qemu-system-{}", platform.arch.as_str());
        let system = find_binary(&sys_name, &own, &extra).with_context(|| {
            format!(
                "{sys_name} not found on PATH. Install QEMU: {}",
                install_hint(platform.os, platform.arch)
            )
        })?;
        // Prefer the qemu-img shipped next to the system emulator.
        let img = system
            .parent()
            .map(|d| d.join(exe_name("qemu-img")))
            .filter(|p| p.is_file())
            .or_else(|| find_binary("qemu-img", &own, &extra))
            .with_context(|| {
                format!(
                    "qemu-img not found. Install QEMU: {}",
                    install_hint(platform.os, platform.arch)
                )
            })?;
        Ok(Self {
            system,
            img,
            devices: std::cell::OnceCell::new(),
        })
    }

    fn run_help(&self, args: &[&str]) -> String {
        Command::new(&self.system)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map(|o| {
                let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                s.push_str(&String::from_utf8_lossy(&o.stderr));
                s
            })
            .unwrap_or_default()
    }

    /// Ensure this QEMU build can use the host's hardware accelerator.
    pub fn require_accel(&self, accel: Accel) -> Result<()> {
        let out = self.run_help(&["-accel", "help"]);
        if out.lines().any(|l| l.trim() == accel.name()) {
            Ok(())
        } else {
            bail!(
                "this QEMU build ({}) does not support the {} accelerator",
                self.system.display(),
                accel.name().to_uppercase()
            )
        }
    }

    fn has_device(&self, name: &str) -> bool {
        self.devices
            .get_or_init(|| self.run_help(&["-device", "help"]))
            .lines()
            .any(|l| l.starts_with(&format!("name \"{name}\"")))
    }

    /// Whether this build can share a host directory with the guest (9p).
    pub fn supports_9p(&self) -> bool {
        self.has_device("virtio-9p-pci")
    }

    /// Whether this build can attach a virtio-fs device, which needs vhost-user
    /// and a RAM backend that can be shared with the server. Distribution QEMUs
    /// on Linux have both; Homebrew's has neither on macOS, and the build vmmbox
    /// ships has both.
    pub fn supports_virtiofs(&self, os: Os) -> bool {
        let backend = ram_backend(os);
        self.has_device("vhost-user-fs-pci")
            && self
                .run_help(&["-object", "help"])
                .lines()
                .any(|l| l.trim() == backend)
    }

    /// How to give the guest a host-accelerated GPU, or why it gets none.
    /// `VMMBOX_GPU=none` turns it off; `VMMBOX_GPU=opengl` leaves out Vulkan.
    pub fn gpu(&self, os: Os) -> std::result::Result<Gpu, String> {
        let mode = gpu_mode(std::env::var("VMMBOX_GPU").ok().as_deref());
        // Don't ask QEMU anything on hosts that can never have one.
        if os != Os::Linux || mode == GpuMode::Off {
            return plan_gpu(os, mode, &GpuHost::default());
        }
        let gl_device = self.has_device("virtio-gpu-gl-pci");
        let host = GpuHost {
            gl_device,
            egl_headless: self
                .run_help(&["-display", "help"])
                .lines()
                .any(|l| l.trim() == "egl-headless"),
            venus_option: gl_device
                && self
                    .run_help(&["-device", "virtio-gpu-gl-pci,help"])
                    .contains("venus="),
            kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .ok()
                .and_then(|r| parse_kernel(&r)),
            render_nodes: usable_render_nodes(),
        };
        plan_gpu(os, mode, &host)
    }

    /// Pick how to give the guest sound: the host OS's native audio backend
    /// that this QEMU build includes, with `VMMBOX_AUDIO` as an override
    /// (`none` disables sound; anything else is raw `-audiodev` options such as
    /// `wav,path=/tmp/out.wav`).
    pub fn audio(&self, os: Os) -> Option<Audio> {
        let backend = match std::env::var("VMMBOX_AUDIO") {
            Ok(v) if v == "none" => return None,
            Ok(v) if !v.is_empty() => v,
            _ => {
                let available = self.run_help(&["-audiodev", "help"]);
                let available: Vec<&str> = available.lines().map(str::trim).collect();
                let preferred: &[&str] = match os {
                    Os::Mac => &["coreaudio"],
                    Os::Linux => &["pipewire", "pa", "alsa"],
                    Os::Windows => &["wasapi", "dsound"],
                };
                preferred
                    .iter()
                    .find(|b| available.contains(b))?
                    .to_string()
            }
        };
        let virtio = self.has_device("virtio-sound-pci");
        if !virtio && !self.has_device("hda-duplex") {
            return None;
        }
        Some(Audio { backend, virtio })
    }

    /// Directories QEMU searches for firmware and data files.
    fn data_dirs(&self, os: Os) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self
            .run_help(&["-L", "help"])
            .lines()
            .map(|l| PathBuf::from(l.trim()))
            .filter(|p| p.is_dir())
            .collect();
        if let Some(bin) = self
            .system
            .canonicalize()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
        {
            dirs.push(bin.join("../share/qemu"));
            dirs.push(bin.join("share")); // Windows installer layout
        }
        match os {
            Os::Mac => dirs.extend(["/opt/homebrew/share/qemu"].map(PathBuf::from)),
            Os::Linux => dirs.extend(
                [
                    "/usr/share/qemu",
                    "/usr/share/AAVMF",
                    "/usr/share/edk2/aarch64",
                    "/usr/share/qemu-efi-aarch64",
                ]
                .map(PathBuf::from),
            ),
            Os::Windows => {}
        }
        dirs
    }

    /// UEFI firmware (code image + variable-store template) for aarch64 guests.
    /// x86_64 guests boot with QEMU's bundled SeaBIOS and need none.
    pub fn firmware(&self, os: Os) -> Result<Firmware> {
        const PAIRS: &[(&str, &str)] = &[
            ("edk2-aarch64-code.fd", "edk2-arm-vars.fd"),
            ("AAVMF_CODE.fd", "AAVMF_VARS.fd"),
            ("QEMU_EFI-pflash.raw", "vars-template-pflash.raw"),
        ];
        for dir in self.data_dirs(os) {
            for (code, vars) in PAIRS {
                let (c, v) = (dir.join(code), dir.join(vars));
                if c.is_file() && v.is_file() {
                    return Ok(Firmware {
                        code: c,
                        vars_template: v,
                    });
                }
            }
        }
        bail!(
            "UEFI firmware for aarch64 guests (edk2-aarch64-code.fd) was not found; is QEMU fully installed?"
        )
    }

    /// Grow a qcow2 image's virtual size. The file itself stays small: qcow2
    /// only allocates clusters as the guest writes them.
    pub fn resize(&self, disk: &Path, bytes: u64) -> Result<()> {
        let out = Command::new(&self.img)
            .args(["resize", "-f", "qcow2"])
            .arg(disk)
            .arg(bytes.to_string())
            .output()
            .context("running qemu-img")?;
        if !out.status.success() {
            bail!(
                "qemu-img resize failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

/// Copy a UEFI variable-store template, padding it to the 64 MiB flash size
/// that the aarch64 `virt` machine requires.
pub fn prepare_efi_vars(template: &Path, dest: &Path) -> Result<()> {
    const FLASH_SIZE: u64 = 64 << 20;
    std::fs::copy(template, dest)
        .with_context(|| format!("copying {} to {}", template.display(), dest.display()))?;
    let f = std::fs::OpenOptions::new().write(true).open(dest)?;
    if f.metadata()?.len() < FLASH_SIZE {
        f.set_len(FLASH_SIZE)?;
    }
    Ok(())
}

/// The QEMU object that makes guest RAM shareable with the virtio-fs server.
fn ram_backend(os: Os) -> &'static str {
    match os {
        Os::Linux => "memory-backend-memfd",
        _ => "memory-backend-shm",
    }
}

/// How the host home is attached to the guest. See `share.rs`.
#[derive(Clone, Copy, Debug)]
pub enum HomeShare<'a> {
    /// QEMU serves `dir` itself, over virtio-9p.
    NineP { dir: &'a Path },
    /// A separate server, already listening on `socket`, serves it over virtio-fs.
    VirtioFs { socket: &'a Path },
}

/// Everything needed to describe one VM boot.
pub struct Launch<'a> {
    pub name: &'a str,
    pub platform: Platform,
    pub cpus: u32,
    pub memory_mib: u64,
    pub disk: &'a Path,
    pub seed: &'a Path,
    /// (code, vars) pflash images; required for aarch64 guests.
    pub efi: Option<(&'a Path, &'a Path)>,
    pub ssh_port: u16,
    /// Host directory to expose to the guest over 9p.
    pub share: Option<HomeShare<'a>>,
    pub console_log: &'a Path,
    pub qmp: &'a Endpoint,
    pub audio: Option<&'a Audio>,
    pub gpu: Option<&'a Gpu>,
}

#[derive(Default)]
struct Args(Vec<String>);

impl Args {
    fn flag(&mut self, flag: &str) {
        self.0.push(flag.to_string());
    }

    fn kv(&mut self, flag: &str, value: String) {
        self.0.push(flag.to_string());
        self.0.push(value);
    }
}

/// Build the QEMU argument list.
pub fn build_args(l: &Launch) -> Result<Vec<String>> {
    let mut a = Args::default();
    let path = |p: &Path| escape(&p.to_string_lossy());

    a.kv("-name", format!("vmmbox-{}", l.name));

    let (machine, accel, cpu) = match (l.platform.arch, l.platform.accel()) {
        (Arch::Aarch64, Accel::Hvf) => ("virt", "hvf", "host"),
        (Arch::X86_64, Accel::Kvm) => ("q35", "kvm", "host"),
        // `host` isn't dependable under WHPX; `max` exposes what it supports.
        // kernel-irqchip=off avoids known WHPX interrupt-controller issues.
        (Arch::X86_64, Accel::Whpx) => ("q35", "whpx,kernel-irqchip=off", "max"),
        (arch, accel) => bail!(
            "no accelerated machine configuration for {} guests under {}",
            arch.as_str(),
            accel.name()
        ),
    };
    // virtio-fs's server reaches guest RAM through a shared mapping of it, so
    // the RAM has to be a shareable object. POSIX shm, as macOS has no memfd; on
    // HVF it boots as fast as plain RAM and copies at the same speed.
    let shared_ram = matches!(l.share, Some(HomeShare::VirtioFs { .. }));
    a.kv(
        "-machine",
        if shared_ram {
            format!("{machine},memory-backend=mem0")
        } else {
            machine.to_string()
        },
    );
    a.kv("-accel", accel.to_string());
    a.kv("-cpu", cpu.to_string());
    a.kv("-smp", l.cpus.to_string());
    a.kv("-m", l.memory_mib.to_string());

    // No default devices: everything the guest sees is listed here.
    a.flag("-nodefaults");
    match l.gpu {
        // Headless, but with an EGL context on the host GPU for virglrenderer.
        Some(gpu) => a.kv(
            "-display",
            format!("egl-headless,rendernode={}", path(&gpu.render_node)),
        ),
        None => a.kv("-display", "none".into()),
    }

    if let Some((code, vars)) = l.efi {
        a.kv(
            "-drive",
            format!(
                "if=pflash,format=raw,unit=0,readonly=on,file={}",
                path(code)
            ),
        );
        a.kv(
            "-drive",
            format!("if=pflash,format=raw,unit=1,file={}", path(vars)),
        );
    }

    a.kv(
        "-drive",
        format!(
            "file={},if=none,id=disk0,format=qcow2,discard=unmap",
            path(l.disk)
        ),
    );
    a.kv("-device", "virtio-blk-pci,drive=disk0,bootindex=0".into());
    a.kv(
        "-drive",
        format!(
            "file={},if=none,id=seed0,format=raw,readonly=on",
            path(l.seed)
        ),
    );
    a.kv("-device", "virtio-blk-pci,drive=seed0".into());

    a.kv(
        "-netdev",
        format!("user,id=net0,hostfwd=tcp:127.0.0.1:{}-:22", l.ssh_port),
    );
    a.kv("-device", "virtio-net-pci,netdev=net0".into());
    a.kv("-device", "virtio-rng-pci".into());

    match l.share {
        Some(HomeShare::NineP { dir }) => {
            // security_model=none: the guest sees real host ownership and modes,
            // which is correct because the guest user has the host user's
            // uid/gid.
            a.kv(
                "-fsdev",
                format!(
                    "local,id=fsdev0,path={},security_model=none,multidevs=remap",
                    path(dir)
                ),
            );
            a.kv(
                "-device",
                format!("virtio-9p-pci,fsdev=fsdev0,mount_tag={HOME_TAG}"),
            );
        }
        Some(HomeShare::VirtioFs { socket }) => {
            // The server is already listening on `socket`. Its RAM is shared as
            // memfd on Linux and POSIX shm on macOS, which has no memfd.
            a.kv(
                "-object",
                format!(
                    "{},id=mem0,size={}M,share=on",
                    ram_backend(l.platform.os),
                    l.memory_mib
                ),
            );
            a.kv("-chardev", format!("socket,id=vfs,path={}", path(socket)));
            a.kv(
                "-device",
                format!("vhost-user-fs-pci,chardev=vfs,tag={HOME_TAG}"),
            );
        }
        None => {}
    }

    if let Some(audio) = l.audio {
        a.kv("-audiodev", format!("{},id=snd0", audio.backend));
        if audio.virtio {
            a.kv("-device", "virtio-sound-pci,audiodev=snd0".into());
        } else {
            a.kv("-device", "intel-hda".into());
            a.kv("-device", "hda-duplex,audiodev=snd0".into());
        }
    }

    if let Some(gpu) = l.gpu {
        let mut device = String::from("virtio-gpu-gl-pci");
        if gpu.vulkan {
            // Host-visible memory and blob resources are what Venus needs; the
            // window is address space, not memory taken from the host.
            device.push_str(",hostmem=4G,blob=true,venus=true");
        }
        a.kv("-device", device);
    }

    a.kv(
        "-chardev",
        format!("file,id=console,path={}", path(l.console_log)),
    );
    a.kv("-serial", "chardev:console".into());

    a.kv("-chardev", l.qmp.chardev());
    a.kv("-qmp", "chardev:qmp".into());

    Ok(a.0)
}

/// Start QEMU detached from this process: it must outlive `vmmbox start`, and
/// must not die with the terminal. Output goes to `log`.
pub fn spawn(qemu: &Qemu, args: &[String], log: &Path) -> Result<Child> {
    let mut cmd = Command::new(&qemu.system);
    cmd.args(args);
    crate::proc::spawn_detached(&mut cmd, log)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn launch<'a>(
        platform: Platform,
        endpoint: &'a Endpoint,
        share: Option<HomeShare<'a>>,
    ) -> Launch<'a> {
        Launch {
            name: "ubuntu",
            platform,
            cpus: 10,
            memory_mib: 32768,
            disk: Path::new("/data/vms/ubuntu/disk.qcow2"),
            seed: Path::new("/data/vms/ubuntu/seed.img"),
            efi: Some((
                Path::new("/fw/code.fd"),
                Path::new("/data/vms/ubuntu/efi-vars.fd"),
            )),
            ssh_port: 2222,
            share,
            console_log: Path::new("/data/vms/ubuntu/console.log"),
            qmp: endpoint,
            audio: None,
            gpu: None,
        }
    }

    #[cfg(unix)]
    fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[cfg(unix)]
    #[test]
    fn macos_arm_args() {
        let ep = Endpoint::Unix("/tmp/vmmbox-501/ubuntu.sock".into());
        let platform = Platform {
            os: Os::Mac,
            arch: Arch::Aarch64,
        };
        let share = HomeShare::NineP {
            dir: Path::new("/Users/me"),
        };
        let args = build_args(&launch(platform, &ep, Some(share))).unwrap();
        assert!(has_pair(&args, "-machine", "virt"));
        assert!(has_pair(&args, "-accel", "hvf"));
        assert!(has_pair(&args, "-cpu", "host"));
        assert!(has_pair(&args, "-qmp", "chardev:qmp"));
        assert!(has_pair(&args, "-smp", "10"));
        assert!(has_pair(&args, "-m", "32768"));
        assert!(has_pair(
            &args,
            "-netdev",
            "user,id=net0,hostfwd=tcp:127.0.0.1:2222-:22"
        ));
        assert!(has_pair(
            &args,
            "-fsdev",
            "local,id=fsdev0,path=/Users/me,security_model=none,multidevs=remap"
        ));
        assert!(has_pair(
            &args,
            "-device",
            "virtio-9p-pci,fsdev=fsdev0,mount_tag=vmmhome"
        ));
        assert!(
            args.iter()
                .any(|a| a.starts_with("if=pflash,format=raw,unit=0,readonly=on"))
        );
        // Hardware acceleration only: never TCG.
        assert!(!args.iter().any(|a| a.contains("tcg")));
    }

    #[cfg(unix)]
    #[test]
    fn x86_args_use_accel_and_no_firmware_args_needed() {
        let ep = Endpoint::Unix("/tmp/s.sock".into());
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        let mut l = launch(platform, &ep, None);
        l.efi = None;
        let args = build_args(&l).unwrap();
        assert!(has_pair(&args, "-machine", "q35"));
        assert!(has_pair(&args, "-accel", "kvm"));
        assert!(!args.iter().any(|a| a.contains("pflash")));
        assert!(!args.iter().any(|a| a.contains("9p")));
    }

    #[cfg(unix)]
    #[test]
    fn audio_devices() {
        let ep = Endpoint::Unix("/tmp/s.sock".into());
        let platform = Platform {
            os: Os::Mac,
            arch: Arch::Aarch64,
        };
        let virtio = Audio {
            backend: "coreaudio".into(),
            virtio: true,
        };
        let mut l = launch(platform, &ep, None);
        l.audio = Some(&virtio);
        let args = build_args(&l).unwrap();
        assert!(has_pair(&args, "-audiodev", "coreaudio,id=snd0"));
        assert!(has_pair(&args, "-device", "virtio-sound-pci,audiodev=snd0"));

        let hda = Audio {
            backend: "wav,path=/tmp/o.wav".into(),
            virtio: false,
        };
        l.audio = Some(&hda);
        let args = build_args(&l).unwrap();
        assert!(has_pair(&args, "-audiodev", "wav,path=/tmp/o.wav,id=snd0"));
        assert!(has_pair(&args, "-device", "hda-duplex,audiodev=snd0"));
        assert!(args.iter().any(|a| a == "intel-hda"));

        // No audio configured: no audio devices at all.
        assert!(
            !build_args(&launch(platform, &ep, None))
                .unwrap()
                .iter()
                .any(|a| a.contains("audiodev"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_virtiofs_share_uses_shared_ram_and_a_vhost_user_device() {
        let ep = Endpoint::Unix("/tmp/s.sock".into());
        let platform = Platform {
            os: Os::Mac,
            arch: Arch::Aarch64,
        };
        let share = HomeShare::VirtioFs {
            socket: Path::new("/tmp/vmmbox-501/ubuntu.vfs.sock"),
        };
        let args = build_args(&launch(platform, &ep, Some(share))).unwrap();
        // The server maps guest RAM, so the RAM must be a shareable object, and
        // it must be the machine's RAM, of the same size as -m.
        assert!(has_pair(&args, "-machine", "virt,memory-backend=mem0"));
        assert!(has_pair(
            &args,
            "-object",
            "memory-backend-shm,id=mem0,size=32768M,share=on"
        ));
        assert!(has_pair(
            &args,
            "-chardev",
            "socket,id=vfs,path=/tmp/vmmbox-501/ubuntu.vfs.sock"
        ));
        assert!(has_pair(
            &args,
            "-device",
            "vhost-user-fs-pci,chardev=vfs,tag=vmmhome"
        ));
        assert!(has_pair(&args, "-m", "32768"));
        // And none of 9p.
        assert!(!args.iter().any(|a| a.contains("9p") || a.contains("fsdev")));
    }

    #[cfg(unix)]
    #[test]
    fn on_linux_the_shared_ram_is_memfd() {
        let ep = Endpoint::Unix("/tmp/s.sock".into());
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        let share = HomeShare::VirtioFs {
            socket: Path::new("/run/user/1000/vmmbox/ubuntu.vfs.sock"),
        };
        let mut l = launch(platform, &ep, Some(share));
        l.efi = None;
        let args = build_args(&l).unwrap();
        assert!(has_pair(&args, "-machine", "q35,memory-backend=mem0"));
        assert!(has_pair(
            &args,
            "-object",
            "memory-backend-memfd,id=mem0,size=32768M,share=on"
        ));
        assert!(!args.iter().any(|a| a.contains("memory-backend-shm")));
    }

    #[cfg(unix)]
    #[test]
    fn a_9p_share_and_no_share_leave_ram_alone() {
        let ep = Endpoint::Unix("/tmp/s.sock".into());
        let platform = Platform {
            os: Os::Mac,
            arch: Arch::Aarch64,
        };
        for share in [
            Some(HomeShare::NineP {
                dir: Path::new("/Users/me"),
            }),
            None,
        ] {
            let args = build_args(&launch(platform, &ep, share)).unwrap();
            assert!(has_pair(&args, "-machine", "virt"));
            assert!(!args.iter().any(|a| a.contains("memory-backend")));
            assert!(!args.iter().any(|a| a.contains("vhost-user")));
        }
    }

    #[cfg(unix)]
    #[test]
    fn no_gpu_means_no_gpu_device() {
        let ep = Endpoint::Unix("/tmp/s.sock".into());
        let platform = Platform {
            os: Os::Mac,
            arch: Arch::Aarch64,
        };
        let args = build_args(&launch(platform, &ep, None)).unwrap();
        assert!(has_pair(&args, "-display", "none"));
        assert!(!args.iter().any(|a| a.contains("virtio-gpu")));
    }

    #[cfg(unix)]
    #[test]
    fn vulkan_gpu_args() {
        let ep = Endpoint::Unix("/tmp/s.sock".into());
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        let gpu = Gpu {
            render_node: "/dev/dri/renderD128".into(),
            vulkan: true,
        };
        let mut l = launch(platform, &ep, None);
        l.gpu = Some(&gpu);
        let args = build_args(&l).unwrap();
        assert!(has_pair(
            &args,
            "-display",
            "egl-headless,rendernode=/dev/dri/renderD128"
        ));
        assert!(has_pair(
            &args,
            "-device",
            "virtio-gpu-gl-pci,hostmem=4G,blob=true,venus=true"
        ));
        // One display, not two.
        assert_eq!(args.iter().filter(|a| *a == "-display").count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn opengl_gpu_args_leave_out_venus() {
        let ep = Endpoint::Unix("/tmp/s.sock".into());
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        let gpu = Gpu {
            render_node: "/dev/dri/renderD129".into(),
            vulkan: false,
        };
        let mut l = launch(platform, &ep, None);
        l.gpu = Some(&gpu);
        let args = build_args(&l).unwrap();
        assert!(has_pair(&args, "-device", "virtio-gpu-gl-pci"));
        assert!(!args.iter().any(|a| a.contains("venus")));
    }

    #[cfg(unix)]
    #[test]
    fn whpx_args() {
        let ep = Endpoint::Unix("/tmp/s.sock".into());
        let platform = Platform {
            os: Os::Windows,
            arch: Arch::X86_64,
        };
        let args = build_args(&launch(platform, &ep, None)).unwrap();
        assert!(has_pair(&args, "-accel", "whpx,kernel-irqchip=off"));
        assert!(has_pair(&args, "-cpu", "max"));
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_combo_is_rejected() {
        let ep = Endpoint::Unix("/tmp/s.sock".into());
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::Aarch64,
        };
        assert!(build_args(&launch(platform, &ep, None)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn intel_macs_are_not_supported() {
        let ep = Endpoint::Unix("/tmp/s.sock".into());
        let platform = Platform {
            os: Os::Mac,
            arch: Arch::X86_64,
        };
        let err = build_args(&launch(platform, &ep, None)).unwrap_err();
        assert!(err.to_string().contains("no accelerated machine"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn commas_in_paths_are_escaped() {
        let ep = Endpoint::Unix("/tmp/s.sock".into());
        let platform = Platform {
            os: Os::Mac,
            arch: Arch::Aarch64,
        };
        let mut l = launch(platform, &ep, None);
        l.disk = Path::new("/data/a,b/disk.qcow2");
        let args = build_args(&l).unwrap();
        assert!(
            args.iter()
                .any(|a| a.starts_with("file=/data/a,,b/disk.qcow2,"))
        );
    }
}

#[cfg(test)]
mod gpu_tests {
    use super::*;

    fn capable() -> GpuHost {
        GpuHost {
            gl_device: true,
            egl_headless: true,
            venus_option: true,
            kernel: Some((6, 14)),
            render_nodes: vec!["/dev/dri/renderD128".into(), "/dev/dri/renderD129".into()],
        }
    }

    #[test]
    fn a_capable_linux_host_gets_vulkan_on_its_first_render_node() {
        let gpu = plan_gpu(Os::Linux, GpuMode::Auto, &capable()).unwrap();
        assert_eq!(gpu.render_node, Path::new("/dev/dri/renderD128"));
        assert!(gpu.vulkan);
        assert_eq!(gpu.describe(), "Vulkan and OpenGL via /dev/dri/renderD128");
    }

    #[test]
    fn macos_and_windows_never_get_one() {
        for os in [Os::Mac, Os::Windows] {
            let why = plan_gpu(os, GpuMode::Auto, &capable()).unwrap_err();
            assert!(why.contains(os.name()), "{why}");
        }
    }

    #[test]
    fn an_old_kernel_or_old_virglrenderer_falls_back_to_opengl() {
        let old_kernel = GpuHost {
            kernel: Some((6, 12)),
            ..capable()
        };
        assert!(
            !plan_gpu(Os::Linux, GpuMode::Auto, &old_kernel)
                .unwrap()
                .vulkan
        );
        let unknown_kernel = GpuHost {
            kernel: None,
            ..capable()
        };
        assert!(
            !plan_gpu(Os::Linux, GpuMode::Auto, &unknown_kernel)
                .unwrap()
                .vulkan
        );
        let no_venus = GpuHost {
            venus_option: false,
            ..capable()
        };
        let gpu = plan_gpu(Os::Linux, GpuMode::Auto, &no_venus).unwrap();
        assert!(!gpu.vulkan);
        assert_eq!(gpu.describe(), "OpenGL via /dev/dri/renderD128");
        // Exactly 6.13 is new enough; a newer major is too.
        for k in [(6, 13), (7, 0)] {
            let h = GpuHost {
                kernel: Some(k),
                ..capable()
            };
            assert!(
                plan_gpu(Os::Linux, GpuMode::Auto, &h).unwrap().vulkan,
                "{k:?}"
            );
        }
    }

    #[test]
    fn missing_pieces_are_reported_not_half_enabled() {
        // QEMU refuses to start with a GL GPU and no render node, so none of
        // these may produce a device.
        for (host, needle) in [
            (
                GpuHost {
                    gl_device: false,
                    ..capable()
                },
                "virglrenderer",
            ),
            (
                GpuHost {
                    egl_headless: false,
                    ..capable()
                },
                "egl-headless",
            ),
            (
                GpuHost {
                    render_nodes: vec![],
                    ..capable()
                },
                "render",
            ),
        ] {
            let why = plan_gpu(Os::Linux, GpuMode::Auto, &host).unwrap_err();
            assert!(why.starts_with("none"), "{why}");
            assert!(why.contains(needle), "{why}");
        }
    }

    #[test]
    fn the_environment_can_turn_it_off_or_drop_vulkan() {
        assert_eq!(gpu_mode(None), GpuMode::Auto);
        assert_eq!(gpu_mode(Some("")), GpuMode::Auto);
        assert_eq!(gpu_mode(Some("none")), GpuMode::Off);
        assert_eq!(gpu_mode(Some("opengl")), GpuMode::OpenGl);

        let why = plan_gpu(Os::Linux, GpuMode::Off, &capable()).unwrap_err();
        assert!(why.contains("VMMBOX_GPU"), "{why}");
        let gpu = plan_gpu(Os::Linux, GpuMode::OpenGl, &capable()).unwrap();
        assert!(!gpu.vulkan);
    }

    #[test]
    fn kernel_release_strings() {
        assert_eq!(parse_kernel("6.13.0-generic\n"), Some((6, 13)));
        assert_eq!(parse_kernel("6.8.0-45-generic"), Some((6, 8)));
        assert_eq!(parse_kernel("6.14-rc2"), Some((6, 14)));
        assert_eq!(
            parse_kernel("5.15.167.4-microsoft-standard-WSL2"),
            Some((5, 15))
        );
        assert_eq!(parse_kernel(""), None);
        assert_eq!(parse_kernel("garbage"), None);
    }
}
