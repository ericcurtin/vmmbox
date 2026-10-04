//! Implementations of the `vmmbox` subcommands.

use crate::bundle;
use crate::distro::{self, ImageRef};
use crate::fsd;
use crate::gui::{self, Gui};
use crate::host::{self, Arch, Platform};
use crate::http::Http;
use crate::image::{Images, Pulled};
use crate::paths::Paths;
use crate::qemu::{self, Gpu, Launch, Qemu};
use crate::qmp::Endpoint;
use crate::resources;
use crate::share::{self, Transport};
use crate::ssh::Ssh;
use crate::util::{
    dir_size, format_ago, format_bytes, format_duration, now_secs, sh_quote, table, tail_lines,
};
use crate::vm::{self, Vm};
use anyhow::{Context, Result, bail};
use std::io::IsTerminal;
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long to wait for a guest to accept SSH after QEMU starts.
const BOOT_TIMEOUT: Duration = Duration::from_secs(300);
/// How long to give a guest to power off after an ACPI shutdown request.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);

/// `vmmbox setup`: install the QEMU build vmmbox ships for this machine.
pub fn setup() -> Result<()> {
    let platform = Platform::current()?;
    let paths = Paths::discover()?;
    if bundle::target_for(platform).is_none() {
        println!("{}", bundle::why_none(platform));
        return Ok(());
    }
    match bundle::ensure(&paths, platform, &Http::new()?)? {
        bundle::Outcome::Installed => println!("Installed into {}", paths.bin().display()),
        _ => println!("Already up to date ({})", paths.bin().display()),
    }
    Ok(())
}

/// Install vmmbox's own QEMU if one is published for this machine and ours is
/// missing or old. This is a convenience, never a requirement: if it fails
/// the QEMU already on the system is used.
fn ensure_bundle(paths: &Paths, platform: Platform) {
    if bundle::target_for(platform).is_none() {
        return;
    }
    if let Err(e) = Http::new().and_then(|http| bundle::ensure(paths, platform, &http)) {
        eprintln!(
            "warning: could not install vmmbox's QEMU ({e:#}); using the QEMU on this system"
        );
    }
}

pub fn pull(image: &ImageRef) -> Result<()> {
    let platform = Platform::current()?;
    let paths = Paths::discover()?;
    let version = image.version_or_default();
    let http = Http::new()?;
    match Images::new(&paths).pull(&http, image.distro, &version, platform.arch)? {
        Pulled::Downloaded(m) => println!(
            "Pulled {} ({}, {})",
            m.reference(),
            m.arch,
            format_bytes(m.size)
        ),
        Pulled::UpToDate(m) => println!("{} is up to date ({})", m.reference(), m.arch),
    }
    Ok(())
}

pub fn images() -> Result<()> {
    let paths = Paths::discover()?;
    let images = Images::new(&paths).list()?;
    if images.is_empty() {
        println!(
            "No images. Pull one with `vmmbox pull <distro>` (available: {}).",
            distro::known_names()
        );
        return Ok(());
    }
    let mut rows = vec![vec![
        "IMAGE".into(),
        "ARCH".into(),
        "SIZE".into(),
        "PULLED".into(),
    ]];
    for m in images {
        rows.push(vec![
            m.reference(),
            m.arch.clone(),
            format_bytes(m.size),
            format_ago(m.pulled_at),
        ]);
    }
    print!("{}", table(&rows));
    Ok(())
}

pub fn rmi(r: &ImageRef) -> Result<()> {
    let platform = Platform::current()?;
    let paths = Paths::discover()?;
    let freed = remove_image(&paths, r, platform.arch)?;
    println!(
        "{}:{} removed ({} freed)",
        r.distro.name,
        r.version_or_default(),
        format_bytes(freed)
    );
    Ok(())
}

/// Delete a pulled image, returning the bytes freed. VMs made from it keep
/// working: each has its own copy of the disk.
fn remove_image(paths: &Paths, r: &ImageRef, arch: Arch) -> Result<u64> {
    let images = Images::new(paths);
    let version = r.version_or_default();
    match images.remove(r.distro.name, &version, arch)? {
        Some(freed) => Ok(freed),
        None => {
            let name = r.distro.name;
            let others: Vec<String> = images
                .list()?
                .into_iter()
                .filter(|m| m.distro == name && m.arch == arch.as_str())
                .map(|m| m.version)
                .collect();
            if others.is_empty() {
                bail!("no image {name}:{version}; see `vmmbox images`")
            }
            bail!(
                "no image {name}:{version} (pulled: {}); name the version to remove, \
                 e.g. `vmmbox rmi {name}:{}`",
                others.join(", "),
                others[0]
            )
        }
    }
}

pub fn ps(all: bool) -> Result<()> {
    let paths = Paths::discover()?;
    let vms = vm::list(&paths)?;
    let mut rows = vec![
        ["NAME", "IMAGE", "STATUS", "CPUS", "MEMORY", "DISK", "SSH"]
            .map(String::from)
            .to_vec(),
    ];
    let mut shown = 0;
    for vm in &vms {
        let running = vm.is_running();
        if !running && !all {
            continue;
        }
        shown += 1;
        let s = &vm.state;
        let status = match (running, s.started_at) {
            (true, Some(t)) => format!("Up {}", format_duration(now_secs().saturating_sub(t))),
            (true, None) => "Up".into(),
            (false, _) => "Stopped".into(),
        };
        let used = std::fs::metadata(vm.disk()).map(|m| m.len()).unwrap_or(0);
        let started = s.started_at.is_some();
        rows.push(vec![
            s.name.clone(),
            vm.reference(),
            status,
            if started {
                s.cpus.to_string()
            } else {
                "-".into()
            },
            if started {
                format_bytes(s.memory_bytes)
            } else {
                "-".into()
            },
            format!("{} / {}", format_bytes(used), format_bytes(s.disk_bytes)),
            if running {
                format!("127.0.0.1:{}", s.ssh_port)
            } else {
                "-".into()
            },
        ]);
    }
    if shown == 0 {
        if vms.is_empty() {
            println!("No VMs. Create one with `vmmbox start <distro>`.");
        } else {
            println!("No running VMs. Use `vmmbox ls` to list stopped ones.");
        }
        return Ok(());
    }
    print!("{}", table(&rows));
    Ok(())
}

/// Resolve a reference to an existing VM, checking any version given.
fn existing_vm(paths: &Paths, r: &ImageRef) -> Result<Vm> {
    let name = r.distro.name;
    let vm = vm::load(paths, name)?
        .with_context(|| format!("no VM named '{name}'; create it with `vmmbox start {name}`"))?;
    check_version(&vm, r)?;
    Ok(vm)
}

fn check_version(vm: &Vm, r: &ImageRef) -> Result<()> {
    if let Some(v) = &r.version
        && *v != vm.state.version
    {
        bail!(
            "the '{name}' VM is {}, not {}:{v}. Only one VM per distro is supported; to use a \
             different version, remove it first with `vmmbox rm {name}`",
            vm.reference(),
            r.distro.name,
            name = vm.state.name
        );
    }
    Ok(())
}

pub fn start(r: &ImageRef) -> Result<()> {
    let platform = Platform::current()?;
    let paths = Paths::discover()?;
    let (vm, booted) = ensure_running(&paths, platform, r)?;
    match booted {
        Some(b) => print_summary(&paths, platform, &vm, &b),
        None => {
            if let Some(pid) = vm.running_pid() {
                println!("{} is already running (pid {pid})", vm.state.name);
            }
        }
    }
    Ok(())
}

/// What booting a VM did, for the summary `start` prints.
struct Booted {
    elapsed: Duration,
    gpu: std::result::Result<Gpu, String>,
}

/// Make sure the VM for `r` is running: pull its image, create the VM and boot
/// it, whichever of those is still to do. The boot report is `None` if it was
/// already running. Everything this prints goes to stderr, so a command run
/// through `run` keeps its stdout to itself.
fn ensure_running(paths: &Paths, platform: Platform, r: &ImageRef) -> Result<(Vm, Option<Booted>)> {
    let name = r.distro.name;

    let existing = match vm::load(paths, name)? {
        Some(vm) => {
            check_version(&vm, r)?;
            if vm.running_pid().is_some() {
                return Ok((vm, None));
            }
            Some(vm)
        }
        None => None,
    };

    // Check every prerequisite up front, before downloading gigabytes.
    platform.check_accel()?;
    ensure_bundle(paths, platform);
    let qemu = Qemu::locate(platform, paths)?;
    qemu.require_accel(platform.accel())?;
    Ssh::require_client()?;

    let mut vm = match existing {
        Some(vm) => vm,
        None => {
            let user = host::current_user()?;
            let version = r.version_or_default();
            let images = Images::new(paths);
            let image = images.ensure(&Http::new()?, r.distro, &version, platform.arch)?;
            eprintln!("Creating VM '{name}' from {}...", image.reference());
            let base = images.disk_path(&image.distro, &image.version, platform.arch);
            vm::create(paths, platform, &qemu, &user, &image, &base)?
        }
    };
    let booted = boot(paths, platform, &qemu, &mut vm)?;
    Ok((vm, Some(booted)))
}

fn free_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("finding a free local port")?;
    Ok(listener.local_addr()?.port())
}

fn boot(paths: &Paths, platform: Platform, qemu: &Qemu, vm: &mut Vm) -> Result<Booted> {
    let name = vm.state.name.clone();
    let compute = resources::compute();
    let ssh_port = free_port()?;
    let endpoint = Endpoint::for_vm(paths, &name)?;
    endpoint.cleanup();

    let efi = if platform.arch == Arch::Aarch64 {
        Some(qemu.firmware(platform.os)?.code)
    } else {
        None
    };
    let efi_vars = vm.efi_vars();
    let disk = vm.disk();
    let seed = vm.seed();
    let console_log = vm.console_log();
    let host_home = PathBuf::from(&vm.state.host_home);
    // For a virtio-fs share the server must be listening before QEMU starts, and
    // must not be left behind if the start fails.
    let mut fsd = FsdGuard::new();
    let vfs_socket: PathBuf;
    let share = if !vm.state.home_shared {
        None
    } else {
        if !qemu.supports_virtiofs(platform.os) || !fsd::available(platform) {
            bail!(
                "{name} shares your home over virtio-fs, which this machine cannot serve \
                 with {}: {}",
                qemu.system.display(),
                fsd::install_hint(platform.os)
            );
        }
        let (pid, socket) = fsd::start(paths, vm, &host_home, platform.os)?;
        fsd.started = Some((pid, socket.clone()));
        vm.state.fsd_pid = Some(pid);
        vfs_socket = socket;
        Some(vfs_socket.as_path())
    };
    let audio = qemu.audio(platform.os);
    let mut gpu = qemu.gpu(platform.os);

    let args_for = |gpu: Option<&Gpu>| {
        qemu::build_args(&Launch {
            name: &name,
            platform,
            cpus: compute.cpus,
            memory_mib: compute.memory_bytes >> 20,
            disk: &disk,
            seed: &seed,
            efi: efi.as_deref().map(|code| (code, efi_vars.as_path())),
            ssh_port,
            share,
            console_log: &console_log,
            qmp: &endpoint,
            audio: audio.as_ref(),
            gpu,
        })
    };
    let args = args_for(gpu.as_ref().ok())?;

    let first_boot = !vm.state.booted_before;
    let started = Instant::now();
    eprintln!(
        "Starting {name}: {} CPUs, {} memory, {} accelerated",
        compute.cpus,
        format_bytes(compute.memory_bytes),
        platform.accel().name().to_uppercase()
    );
    let mut child = qemu::spawn(qemu, &args, &vm.qemu_log())?;

    vm.state.pid = Some(child.id());
    vm.state.ssh_port = ssh_port;
    vm.state.started_at = Some(now_secs());
    vm.state.cpus = compute.cpus;
    vm.state.memory_bytes = compute.memory_bytes;
    vm.save()?;

    let ssh = Ssh::for_vm(vm)?;
    eprintln!("Waiting for {name} to boot...");
    loop {
        if let Some(status) = child.try_wait()? {
            // A GPU this host cannot actually drive must not stop the VM from
            // booting: say why, and start again without it.
            if let Ok(g) =
                std::mem::replace(&mut gpu, Err("none (QEMU would not start with it)".into()))
            {
                let why = tail_lines(&vm.qemu_log(), 1);
                let why = why.trim();
                eprintln!(
                    "warning: QEMU could not start with the GPU ({}){}; starting without it",
                    g.describe(),
                    if why.is_empty() {
                        String::new()
                    } else {
                        format!(": {why}")
                    }
                );
                endpoint.cleanup();
                child = qemu::spawn(qemu, &args_for(None)?, &vm.qemu_log())?;
                vm.state.pid = Some(child.id());
                vm.save()?;
                continue;
            }
            vm.state.pid = None;
            vm.state.started_at = None;
            vm.save()?;
            let log = tail_lines(&vm.qemu_log(), 15);
            bail!(
                "QEMU exited during start-up ({status}){}\nlog: {}",
                if log.is_empty() {
                    String::new()
                } else {
                    format!(":\n{log}")
                },
                vm.qemu_log().display()
            );
        }
        if ssh.probe() {
            break;
        }
        if started.elapsed() > BOOT_TIMEOUT {
            bail!(
                "{name} is running but did not accept SSH within {}s; see {}",
                BOOT_TIMEOUT.as_secs(),
                vm.console_log().display()
            );
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    // SSH is up as soon as the user exists; first-boot configuration (growing
    // the filesystem, installing the audio stack) may still be finishing.
    // Run as root: cloud-init's status command can't read its own state as an
    // ordinary user on some distros (Fedora), and then never sees completion.
    if first_boot {
        eprintln!("First boot: installing packages and configuring the guest...");
    }
    let _ = ssh.run_quiet("sudo -n timeout 300 cloud-init status --wait");
    vm.state.booted_before = true;
    vm.save()?;
    // A VM made when 9p was offered has a 9p line in its fstab: it was just
    // started with a virtio-fs device instead, so switch the line over.
    if needs_home_migration(&vm.state)
        && let Err(e) = migrate_home(vm, &ssh)
    {
        eprintln!("warning: could not switch the shared home to virtio-fs: {e:#}");
    }
    if let Some(path) = vm.state.shared_home_path() {
        let probe = format!("mountpoint -q {}", sh_quote(path));
        if ssh.run_quiet(&probe)? != 0 {
            eprintln!(
                "warning: the host home is not mounted at {path} in the guest; the guest kernel \
                 may lack virtio-fs support (see {})",
                vm.console_log().display()
            );
        }
    }

    fsd.disarm();
    Ok(Booted {
        elapsed: started.elapsed(),
        gpu,
    })
}

/// Whether the guest still mounts the shared home with 9p, which vmmbox no
/// longer offers: its VM was made before virtio-fs.
fn needs_home_migration(state: &vm::VmState) -> bool {
    state.home_shared && state.home_transport == Transport::NineP
}

/// Switch a running guest's fstab entry for the shared home from 9p to virtio-fs,
/// and mount it. Once it has worked, the VM is recorded as switched.
fn migrate_home(vm: &mut Vm, ssh: &Ssh) -> Result<()> {
    let Some(mount_point) = vm.state.shared_home_path() else {
        return Ok(());
    };
    eprintln!(
        "Switching {}'s shared home from 9p to virtio-fs (once)...",
        vm.state.name
    );
    let script = share::migration_script(mount_point);
    let code = ssh.run_quiet(&format!("sudo -n sh -c {}", sh_quote(&script)))?;
    if code != 0 {
        bail!("the guest could not mount the share with virtio-fs (exit status {code})");
    }
    vm.state.home_transport = Transport::VirtioFs;
    vm.save()
}

fn print_summary(paths: &Paths, platform: Platform, vm: &Vm, booted: &Booted) {
    let s = &vm.state;
    let name = &s.name;
    println!(
        "Started {name} ({}) in {}s",
        vm.reference(),
        booted.elapsed.as_secs()
    );
    println!("  CPUs:   {}", s.cpus);
    println!("  Memory: {}", format_bytes(s.memory_bytes));
    println!("  Disk:   {} (grows on demand)", format_bytes(s.disk_bytes));
    match &booted.gpu {
        Ok(g) => println!("  GPU:    {}", g.describe()),
        Err(why) => println!("  GPU:    {why}"),
    }
    println!("  User:   {} (uid {}, gid {})", s.user, s.uid, s.gid);
    if s.home_shared && s.guest_home == s.host_home {
        println!(
            "  Home:   {} (shared with the host over virtio-fs)",
            s.guest_home
        );
    } else if s.home_shared {
        println!(
            "  Home:   {} (on the VM); your host home is mounted at {} over virtio-fs",
            s.guest_home, s.host_home
        );
    } else {
        println!("  Home:   {}", s.guest_home);
    }
    match Gui::detect(platform, paths) {
        Ok(_) => println!("  GUI:    ready (windows open on your desktop)"),
        Err(e) => println!("  GUI:    unavailable: {e}"),
    }
    println!("Run `vmmbox run {name} bash` for a shell.");
}

/// The virtio-fs server started for a boot, until QEMU has connected to it: if
/// the boot fails before that, dropping this stops the server instead of
/// leaving it to wait for a connection that will never come.
struct FsdGuard {
    /// The server's pid and socket, until QEMU has connected.
    started: Option<(u32, PathBuf)>,
}

impl FsdGuard {
    fn new() -> Self {
        Self { started: None }
    }

    /// QEMU has the server's socket now, and the server ends with QEMU.
    fn disarm(&mut self) {
        self.started = None;
    }
}

impl Drop for FsdGuard {
    fn drop(&mut self) {
        if let Some((pid, socket)) = self.started.take() {
            fsd::stop(pid);
            // Killed before it could remove its own socket.
            let _ = std::fs::remove_file(socket);
        }
    }
}

fn wait_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !crate::proc::is_qemu(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    !crate::proc::is_qemu(pid)
}

/// Shut a VM down, gracefully if the guest cooperates. Returns whether it was
/// running.
fn stop_vm(paths: &Paths, vm: &mut Vm) -> Result<bool> {
    let name = vm.state.name.clone();
    let Some(pid) = vm.running_pid() else {
        if vm.state.pid.is_some() {
            vm.state.pid = None;
            vm.state.started_at = None;
            vm.save()?;
        }
        return Ok(false);
    };

    let endpoint = Endpoint::for_vm(paths, &name)?;
    eprintln!("Stopping {name}...");
    // Ask politely first (ACPI power button) so the guest unmounts and syncs.
    let graceful = match endpoint.execute("system_powerdown", Duration::from_secs(10)) {
        Ok(()) => wait_exit(pid, SHUTDOWN_TIMEOUT),
        Err(e) => {
            eprintln!("warning: could not ask the guest to shut down: {e:#}");
            false
        }
    };
    if !graceful {
        eprintln!("The guest did not shut down in time; powering it off");
        let _ = endpoint.execute("quit", Duration::from_secs(5));
        if !wait_exit(pid, Duration::from_secs(5)) {
            crate::proc::kill_qemu(pid);
            if !wait_exit(pid, Duration::from_secs(5)) {
                bail!("could not stop QEMU (pid {pid})");
            }
        }
    }

    endpoint.cleanup();
    // The virtio-fs server ends when QEMU does; make sure of it.
    if let Some(pid) = vm.state.fsd_pid.take() {
        fsd::stop(pid);
    }
    vm.state.pid = None;
    vm.state.started_at = None;
    vm.save()?;
    Ok(true)
}

pub fn stop(r: &ImageRef) -> Result<()> {
    let paths = Paths::discover()?;
    let mut vm = existing_vm(&paths, r)?;
    let name = vm.state.name.clone();
    if stop_vm(&paths, &mut vm)? {
        println!("{name} stopped");
    } else {
        println!("{name} is not running");
    }
    Ok(())
}

pub fn rm(r: &ImageRef, force: bool) -> Result<()> {
    let paths = Paths::discover()?;
    let freed = remove_vm(&paths, r, force)?;
    println!("{} removed ({} freed)", r.distro.name, format_bytes(freed));
    Ok(())
}

/// Delete a VM and its disk, returning the bytes freed. The pulled image it was
/// made from is kept, and so is everything in the shared host home: that is
/// mounted from the host, not stored in the VM directory.
fn remove_vm(paths: &Paths, r: &ImageRef, force: bool) -> Result<u64> {
    let name = r.distro.name;
    let dir = paths.vm_dir(name);
    let mut vm = match vm::load(paths, name) {
        Ok(Some(vm)) => {
            check_version(&vm, r)?;
            vm
        }
        Ok(None) => bail!("no VM named '{name}'"),
        // A VM whose record is unreadable cannot be inspected, but it can still
        // be removed on request.
        Err(e) if force && dir.is_dir() => {
            eprintln!("warning: {e:#}; removing it anyway");
            return delete_vm_dir(paths, &dir);
        }
        Err(e) => return Err(e.context(format!("use `vmmbox rm -f {name}` to remove it anyway"))),
    };
    if vm.is_running() {
        if !force {
            bail!(
                "{name} is running; stop it first with `vmmbox stop {name}`, or use `vmmbox rm -f {name}`"
            );
        }
        stop_vm(paths, &mut vm)?;
    }
    delete_vm_dir(paths, &vm.dir)
}

/// Remove one VM directory, refusing anything that is not a real directory
/// directly inside the VM store (a symlink there must not lead the delete
/// somewhere else).
fn delete_vm_dir(paths: &Paths, dir: &std::path::Path) -> Result<u64> {
    let meta =
        std::fs::symlink_metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
    if !meta.is_dir() || dir.parent() != Some(paths.vms().as_path()) {
        bail!("refusing to remove {}: not a VM directory", dir.display());
    }
    let size = dir_size(dir);
    std::fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
    Ok(size)
}

/// Whether `run` forwards windows for a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuiMode {
    /// Forward windows when the command is a GUI application.
    Auto,
    /// Always forward windows (`--gui`).
    Always,
    /// Never (`--no-gui`).
    Never,
}

pub fn run(r: &ImageRef, command: &[String], mode: GuiMode, x11: bool) -> Result<i32> {
    let paths = Paths::discover()?;
    let platform = Platform::current()?;
    // The VM is pulled, created and started first if that is still to do.
    let (vm, booted) = ensure_running(&paths, platform, r)?;
    let name = &vm.state.name;
    let ssh = Ssh::for_vm(&vm)?;
    match booted {
        Some(b) => eprintln!("Started {name} in {}s", b.elapsed.as_secs()),
        None => await_ssh(&ssh, &vm, BOOT_TIMEOUT.as_secs())?,
    }
    let cwd = guest_cwd(&vm);
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    // GUI apps go through waypipe. Plain commands do not, so terminal use stays
    // as fast and as simple as ssh: a GUI-ness check (one short ssh call) only
    // runs for commands that could plausibly be GUI apps.
    let gui = Gui::detect(platform, &paths);
    // What this command would actually run: itself, or what is inside a
    // `bash -c "..."` or behind `env`/`sudo`.
    let candidates = gui::gui_candidates(command);
    // Software with no Wayland support needs an X server as well; it is a GUI
    // app whether or not it has a launcher file.
    let x11_only = gui::needs_x11(&candidates);
    let use_gui = match (&mode, &gui) {
        (GuiMode::Never, _) => false,
        (GuiMode::Always, Err(e)) => bail!("{e}"),
        (GuiMode::Always, Ok(_)) => true,
        (GuiMode::Auto, Ok(_)) => {
            x11_only
                || (!candidates.is_empty()
                    && ssh.run_quiet(&gui::gui_check_script(&candidates))? == 0)
        }
        (GuiMode::Auto, Err(_)) => false,
    };

    if use_gui && let Ok(gui) = &gui {
        return gui.run(&paths, &ssh, tty, cwd.as_deref(), command, x11 || x11_only);
    }

    let remote = remote_command(cwd.as_deref(), command);
    let code = ssh.run_interactive(&remote, tty)?;
    // The command failed and this host can't show windows: if it was a GUI app,
    // say why it did not open.
    if code != 0
        && mode == GuiMode::Auto
        && let Err(reason) = &gui
        && !candidates.is_empty()
        && ssh
            .run_quiet(&gui::gui_check_script(&candidates))
            .unwrap_or(1)
            == 0
    {
        eprintln!("vmmbox: this looks like a GUI app, but {reason}");
    }
    Ok(code)
}

/// Another `vmmbox` may be booting this VM right now, which makes it "running"
/// before it answers on SSH. If it was started recently, wait for it rather
/// than fail. A VM that has been up a while is not probed: that would cost a
/// connection on every command.
fn await_ssh(ssh: &Ssh, vm: &Vm, limit: u64) -> Result<()> {
    let Some(since) = vm
        .state
        .started_at
        .filter(|&t| now_secs().saturating_sub(t) < limit)
    else {
        return Ok(());
    };
    if ssh.probe() {
        return Ok(());
    }
    eprintln!("Waiting for {} to finish booting...", vm.state.name);
    while !ssh.probe() {
        if now_secs().saturating_sub(since) >= limit {
            bail!(
                "{} is running but did not accept SSH within {limit}s; see {}",
                vm.state.name,
                vm.console_log().display()
            );
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

/// The host working directory as seen from the guest, if it is inside the
/// shared home (the same absolute path there).
fn guest_cwd(vm: &Vm) -> Option<String> {
    if !vm.state.home_shared {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    if !cwd.starts_with(&vm.state.host_home) {
        return None;
    }
    cwd.to_str().map(String::from)
}

/// Build the shell line the guest runs: enter the working directory (best
/// effort), then replace the shell with the command so its exit status and
/// signals pass straight through.
fn remote_command(cwd: Option<&str>, command: &[String]) -> String {
    let mut s = String::new();
    if let Some(dir) = cwd {
        s.push_str(&format!("cd {} 2>/dev/null; ", sh_quote(dir)));
    }
    s.push_str("exec");
    for word in command {
        s.push(' ');
        s.push_str(&sh_quote(word));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_command_quotes_every_word() {
        let cmd = ["bash".to_string()];
        assert_eq!(remote_command(None, &cmd), "exec bash");
        let cmd = ["sh", "-c", "echo $HOME; ls 'x y'"].map(String::from);
        assert_eq!(
            remote_command(Some("/Users/me/my proj"), &cmd),
            "cd '/Users/me/my proj' 2>/dev/null; exec sh -c 'echo $HOME; ls '\\''x y'\\'''"
        );
    }

    use crate::vm::VmState;

    fn scratch(tag: &str) -> (Paths, PathBuf) {
        let root = std::env::temp_dir().join(format!("vmmbox-rm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("vms")).unwrap();
        (Paths::with_root(root.clone()), root)
    }

    /// A stopped VM on disk: a record plus a disk file of `disk` bytes.
    fn fake_vm(paths: &Paths, name: &str, version: &str, disk: usize, pid: Option<u32>) -> PathBuf {
        let dir = paths.vm_dir(name);
        std::fs::create_dir_all(&dir).unwrap();
        let state = VmState {
            name: name.into(),
            distro: name.into(),
            version: version.into(),
            arch: "aarch64".into(),
            created_at: 1,
            disk_bytes: 1 << 30,
            user: "me".into(),
            uid: 501,
            gid: 20,
            guest_home: "/home/me".into(),
            host_home: "/Users/me".into(),
            home_shared: true,
            home_transport: Transport::VirtioFs,
            fsd_pid: None,
            booted_before: true,
            pid,
            ssh_port: 2222,
            started_at: None,
            cpus: 4,
            memory_bytes: 1 << 30,
        };
        std::fs::write(dir.join("vm.json"), serde_json::to_vec(&state).unwrap()).unwrap();
        std::fs::write(dir.join("disk.qcow2"), vec![0u8; disk]).unwrap();
        dir
    }

    fn rref(s: &str) -> ImageRef {
        s.parse().unwrap()
    }

    /// A pulled image on disk: a record plus a disk file of `size` bytes.
    fn fake_image(paths: &Paths, distro: &str, version: &str, size: usize) -> PathBuf {
        let dir = paths.image_dir(distro, version, "aarch64");
        std::fs::create_dir_all(&dir).unwrap();
        let meta = crate::image::ImageMeta {
            distro: distro.into(),
            version: version.into(),
            arch: "aarch64".into(),
            file_name: "disk.img".into(),
            url: "https://example.invalid/disk.img".into(),
            algo: crate::checksum::Algo::Sha256,
            sha: "0".repeat(64),
            size: size as u64,
            pulled_at: 1,
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        std::fs::write(dir.join("disk.qcow2"), vec![0u8; size]).unwrap();
        dir
    }

    /// A stopped VM loaded back from disk, whose SSH port nothing listens on.
    fn closed_port_vm(paths: &Paths, started_secs_ago: u64) -> Vm {
        fake_vm(paths, "ubuntu", "26.04", 10, None);
        let mut vm = vm::load(paths, "ubuntu").unwrap().unwrap();
        vm.state.ssh_port = free_port().unwrap();
        vm.state.started_at = Some(now_secs() - started_secs_ago);
        vm
    }

    #[test]
    fn only_a_shared_vm_from_the_9p_days_needs_its_home_switched() {
        let (paths, root) = scratch("migrate");
        fake_vm(&paths, "ubuntu", "26.04", 10, None);
        let mut vm = vm::load(&paths, "ubuntu").unwrap().unwrap();
        assert!(
            !needs_home_migration(&vm.state),
            "a new VM is already virtio-fs"
        );
        vm.state.home_transport = Transport::NineP;
        assert!(needs_home_migration(&vm.state));
        // No share, nothing to switch.
        vm.state.home_shared = false;
        assert!(!needs_home_migration(&vm.state));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn run_does_not_probe_a_vm_that_has_been_up_a_while() {
        let (paths, root) = scratch("await-old");
        let vm = closed_port_vm(&paths, 10_000);
        let Ok(ssh) = Ssh::for_vm(&vm) else { return };
        // Nothing is listening, so a probe would fail; it must not even try.
        let began = Instant::now();
        assert!(await_ssh(&ssh, &vm, 300).is_ok());
        assert!(began.elapsed() < Duration::from_secs(3));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn run_waits_for_a_vm_that_is_still_booting_then_gives_up() {
        let (paths, root) = scratch("await-new");
        let vm = closed_port_vm(&paths, 1);
        let Ok(ssh) = Ssh::for_vm(&vm) else { return };
        let began = Instant::now();
        let err = await_ssh(&ssh, &vm, 2).unwrap_err().to_string();
        assert!(err.contains("did not accept SSH within 2s"), "{err}");
        // The limit is in whole seconds, so how long this took depends on where in
        // a second it started; all that can be said is that it gave up.
        assert!(began.elapsed() < Duration::from_secs(10));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn records_from_before_booted_before_existed_count_as_booted() {
        let (paths, root) = scratch("booted-default");
        let dir = fake_vm(&paths, "ubuntu", "26.04", 10, None);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("vm.json")).unwrap()).unwrap();
        json.as_object_mut().unwrap().remove("booted_before");
        std::fs::write(dir.join("vm.json"), serde_json::to_vec(&json).unwrap()).unwrap();
        let vm = vm::load(&paths, "ubuntu").unwrap().unwrap();
        assert!(
            vm.state.booted_before,
            "an old VM must not claim a first boot"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rmi_deletes_the_image_and_reports_the_space() {
        let (paths, root) = scratch("rmi-ok");
        let image = fake_image(&paths, "ubuntu", "24.04", 7000);
        let other_version = fake_image(&paths, "ubuntu", "26.04", 10);
        let other_distro = fake_image(&paths, "debian", "13", 10);
        // A VM made from the image has its own copy of the disk.
        let vm = fake_vm(&paths, "ubuntu", "24.04", 20, None);

        let freed = remove_image(&paths, &rref("ubuntu:24.04"), Arch::Aarch64).unwrap();
        assert!(freed >= 7000, "freed {freed}");
        assert!(!image.exists());
        assert!(other_version.join("disk.qcow2").exists());
        assert!(other_distro.join("disk.qcow2").exists());
        assert!(vm.join("disk.qcow2").exists(), "VMs are untouched");
        // The version directory went with it; the distro's other version stays.
        assert!(!paths.images().join("ubuntu/24.04").exists());
        assert!(paths.images().join("ubuntu/26.04").is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rmi_prunes_directories_left_empty_but_not_others() {
        let (paths, root) = scratch("rmi-prune");
        fake_image(&paths, "debian", "13", 10);
        remove_image(&paths, &rref("debian"), Arch::Aarch64).unwrap();
        assert!(!paths.images().join("debian").exists());
        assert!(paths.images().is_dir(), "the store itself stays");
        assert!(Images::new(&paths).list().unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rmi_without_a_version_means_the_default_one() {
        let (paths, root) = scratch("rmi-default");
        let default = rref("ubuntu").version_or_default();
        let other = if default == "24.04" { "26.04" } else { "24.04" };
        let image = fake_image(&paths, "ubuntu", &default, 10);
        let kept = fake_image(&paths, "ubuntu", other, 10);
        remove_image(&paths, &rref("ubuntu"), Arch::Aarch64).unwrap();
        assert!(!image.exists());
        assert!(kept.join("disk.qcow2").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rmi_of_an_image_that_is_not_there_says_what_is() {
        let (paths, root) = scratch("rmi-missing");
        let err = remove_image(&paths, &rref("fedora:44"), Arch::Aarch64)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no image fedora:44") && err.contains("vmmbox images"),
            "{err}"
        );

        fake_image(&paths, "fedora", "43", 10);
        let err = remove_image(&paths, &rref("fedora:44"), Arch::Aarch64)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no image fedora:44")
                && err.contains("pulled: 43")
                && err.contains("rmi fedora:43"),
            "{err}"
        );
        assert!(paths.image_dir("fedora", "43", "aarch64").is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rmi_removes_a_partial_download() {
        let (paths, root) = scratch("rmi-partial");
        let dir = paths.image_dir("ubuntu", "24.04", "aarch64");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("disk.qcow2.part"), vec![0u8; 3000]).unwrap();
        let freed = remove_image(&paths, &rref("ubuntu:24.04"), Arch::Aarch64).unwrap();
        assert!(freed >= 3000, "freed {freed}");
        assert!(!dir.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rmi_never_follows_a_symlink_out_of_the_store() {
        let (paths, root) = scratch("rmi-symlink");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("24.04/aarch64")).unwrap();
        std::fs::write(elsewhere.join("24.04/aarch64/precious.txt"), b"keep me").unwrap();
        std::fs::create_dir_all(paths.images()).unwrap();

        // The distro directory is a symlink...
        std::os::unix::fs::symlink(&elsewhere, paths.images().join("ubuntu")).unwrap();
        let err = remove_image(&paths, &rref("ubuntu:24.04"), Arch::Aarch64)
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing"), "{err}");
        assert!(elsewhere.join("24.04/aarch64/precious.txt").exists());
        std::fs::remove_file(paths.images().join("ubuntu")).unwrap();

        // ...and so is the image directory itself.
        std::fs::create_dir_all(paths.images().join("debian/13")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.join("24.04/aarch64"),
            paths.images().join("debian/13/aarch64"),
        )
        .unwrap();
        assert!(remove_image(&paths, &rref("debian:13"), Arch::Aarch64).is_err());
        assert!(elsewhere.join("24.04/aarch64/precious.txt").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rm_deletes_the_vm_and_reports_the_space() {
        let (paths, root) = scratch("ok");
        let dir = fake_vm(&paths, "ubuntu", "26.04", 5000, None);
        // A pulled image and a neighbouring VM must survive.
        let image = paths.image_dir("ubuntu", "26.04", "aarch64");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("disk.qcow2"), b"image").unwrap();
        let other = fake_vm(&paths, "debian", "13", 10, None);

        let freed = remove_vm(&paths, &rref("ubuntu"), false).unwrap();
        assert!(freed >= 5000, "freed {freed}");
        assert!(!dir.exists());
        assert!(image.join("disk.qcow2").exists(), "the image is kept");
        assert!(other.join("vm.json").exists(), "other VMs are untouched");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rm_unknown_vm_and_wrong_version() {
        let (paths, root) = scratch("unknown");
        let err = remove_vm(&paths, &rref("ubuntu"), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no VM named 'ubuntu'"), "{err}");

        let dir = fake_vm(&paths, "ubuntu", "24.04", 10, None);
        let err = remove_vm(&paths, &rref("ubuntu:26.04"), false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ubuntu:24.04") && err.contains("vmmbox rm ubuntu"),
            "{err}"
        );
        assert!(dir.exists(), "a version mismatch must not delete anything");
        // Naming the right version works.
        remove_vm(&paths, &rref("ubuntu:24.04"), false).unwrap();
        assert!(!dir.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rm_unreadable_record_needs_force() {
        let (paths, root) = scratch("corrupt");
        let dir = paths.vm_dir("fedora");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vm.json"), b"{ not json").unwrap();
        std::fs::write(dir.join("disk.qcow2"), vec![0u8; 100]).unwrap();

        let err = format!(
            "{:#}",
            remove_vm(&paths, &rref("fedora"), false).unwrap_err()
        );
        assert!(err.contains("rm -f fedora"), "{err}");
        assert!(dir.exists());

        assert!(remove_vm(&paths, &rref("fedora"), true).unwrap() >= 100);
        assert!(!dir.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Run only by `fake_qemu`: a process that just sits there.
    #[cfg(unix)]
    #[test]
    #[ignore = "helper process for the rm tests, not a test"]
    fn fake_qemu_sleeper() {
        std::thread::sleep(Duration::from_secs(60));
    }

    /// A long-running process whose name contains "qemu", so vmmbox takes it
    /// for the VM. It is a copy of this test executable (a system binary such as
    /// `sleep` cannot be copied and run on macOS), sent to sleep.
    #[cfg(unix)]
    fn fake_qemu(root: &std::path::Path) -> std::process::Child {
        use std::process::Stdio;
        let bin = root.join("qemu-fake");
        std::fs::copy(std::env::current_exe().unwrap(), &bin).unwrap();
        let mut cmd = std::process::Command::new(&bin);
        cmd.args(["--ignored", "--exact", "commands::tests::fake_qemu_sleeper"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Another test forking while the copy was still open for writing makes
        // the first exec fail with "Text file busy". That passes in moments.
        let mut tries = 0;
        let mut child = loop {
            match cmd.spawn() {
                Ok(child) => break child,
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 200 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => panic!("starting the fake qemu: {e}"),
            }
        };
        // Wait until vmmbox itself recognises it.
        for _ in 0..100 {
            if crate::proc::is_qemu(child.id()) {
                return child;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("the fake qemu never showed up as a qemu process");
    }

    #[cfg(unix)]
    #[test]
    fn rm_refuses_a_running_vm_without_force() {
        let (paths, root) = scratch("running");
        let mut child = fake_qemu(&root);
        let dir = fake_vm(&paths, "ubuntu", "26.04", 10, Some(child.id()));

        let err = remove_vm(&paths, &rref("ubuntu"), false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("is running") && err.contains("rm -f ubuntu"),
            "{err}"
        );
        assert!(
            dir.join("disk.qcow2").exists(),
            "a running VM must not be touched"
        );
        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rm_never_follows_a_symlink_out_of_the_store() {
        let (paths, root) = scratch("symlink");
        // vms/ubuntu is a symlink to a directory elsewhere that has a valid
        // record, so loading succeeds; deleting must still be refused.
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let real = fake_vm(&paths, "debian", "13", 10, None);
        std::fs::copy(real.join("vm.json"), elsewhere.join("vm.json")).unwrap();
        std::fs::write(elsewhere.join("precious.txt"), b"keep me").unwrap();
        std::fs::remove_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&elsewhere, paths.vm_dir("debian")).unwrap();

        let err = remove_vm(&paths, &rref("debian"), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a VM directory"), "{err}");
        assert!(
            elsewhere.join("precious.txt").exists(),
            "target of the symlink survived"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn free_port_is_usable() {
        // The port is released before it is returned, so another test binding
        // port 0 can take it in between. That race is inherent in picking a free
        // port; a few tries make the test about the function, not the race.
        let usable = (0..10).any(|_| {
            let p = free_port().unwrap();
            p > 0 && TcpListener::bind(("127.0.0.1", p)).is_ok()
        });
        assert!(usable);
    }
}
