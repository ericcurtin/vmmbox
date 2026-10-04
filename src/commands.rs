//! Implementations of the `vmmbox` subcommands.

use crate::distro::{self, ImageRef};
use crate::host::{self, Arch, Platform};
use crate::http::Http;
use crate::image::{Images, Pulled};
use crate::paths::Paths;
use crate::qemu::{self, Launch, Qemu};
use crate::qmp::Endpoint;
use crate::resources;
use crate::ssh::Ssh;
use crate::util::{
    format_ago, format_bytes, format_duration, now_secs, sh_quote, table, tail_lines,
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
            println!("No running VMs. Use `vmmbox ps -a` to list stopped ones.");
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
            "the '{}' VM is {}, not {}:{v}. Only one VM per distro is supported; to use a \
             different version, stop it and delete {} first",
            vm.state.name,
            vm.reference(),
            r.distro.name,
            vm.dir.display()
        );
    }
    Ok(())
}

pub fn start(r: &ImageRef) -> Result<()> {
    let platform = Platform::current()?;
    let paths = Paths::discover()?;
    let name = r.distro.name;

    let existing = vm::load(&paths, name)?;
    if let Some(vm) = &existing {
        check_version(vm, r)?;
        if let Some(pid) = vm.running_pid() {
            println!("{name} is already running (pid {pid})");
            return Ok(());
        }
    }

    // Check every prerequisite up front, before downloading gigabytes.
    platform.check_accel()?;
    let qemu = Qemu::locate(platform)?;
    qemu.require_accel(platform.accel())?;
    Ssh::require_client()?;

    let mut vm = match existing {
        Some(vm) => vm,
        None => {
            let user = host::current_user()?;
            let version = r.version_or_default();
            let images = Images::new(&paths);
            let image = images.ensure(&Http::new()?, r.distro, &version, platform.arch)?;
            eprintln!("Creating VM '{name}' from {}...", image.reference());
            let base = images.disk_path(&image.distro, &image.version, platform.arch);
            vm::create(&paths, platform, &qemu, &user, &image, &base)?
        }
    };
    boot(&paths, platform, &qemu, &mut vm)
}

fn free_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("finding a free local port")?;
    Ok(listener.local_addr()?.port())
}

fn boot(paths: &Paths, platform: Platform, qemu: &Qemu, vm: &mut Vm) -> Result<()> {
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
    let share = vm
        .state
        .home_shared
        .then(|| PathBuf::from(&vm.state.host_home));
    let audio = qemu.audio(platform.os);

    let args = qemu::build_args(&Launch {
        name: &name,
        platform,
        cpus: compute.cpus,
        memory_mib: compute.memory_bytes >> 20,
        disk: &disk,
        seed: &seed,
        efi: efi.as_deref().map(|code| (code, efi_vars.as_path())),
        ssh_port,
        share: share.as_deref(),
        console_log: &console_log,
        qmp: &endpoint,
        audio: audio.as_ref(),
    })?;

    let first_boot = vm.state.started_at.is_none();
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
    if vm.state.home_shared {
        let probe = format!("mountpoint -q {}", sh_quote(&vm.state.guest_home));
        if ssh.run_quiet(&probe)? != 0 {
            eprintln!(
                "warning: the host home is not mounted at {} in the guest; the guest kernel may \
                 lack 9p support (see {})",
                vm.state.guest_home,
                vm.console_log().display()
            );
        }
    }

    let s = &vm.state;
    println!(
        "Started {name} ({}) in {}s",
        vm.reference(),
        started.elapsed().as_secs()
    );
    println!("  CPUs:   {}", s.cpus);
    println!("  Memory: {}", format_bytes(s.memory_bytes));
    println!("  Disk:   {} (grows on demand)", format_bytes(s.disk_bytes));
    println!("  User:   {} (uid {}, gid {})", s.user, s.uid, s.gid);
    if s.home_shared {
        println!("  Home:   {} (shared with the host)", s.guest_home);
    } else {
        println!("  Home:   {}", s.guest_home);
    }
    println!("Run `vmmbox exec {name} bash` for a shell.");
    Ok(())
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

pub fn stop(r: &ImageRef) -> Result<()> {
    let paths = Paths::discover()?;
    let mut vm = existing_vm(&paths, r)?;
    let name = vm.state.name.clone();

    let Some(pid) = vm.running_pid() else {
        if vm.state.pid.is_some() {
            vm.state.pid = None;
            vm.state.started_at = None;
            vm.save()?;
        }
        println!("{name} is not running");
        return Ok(());
    };

    let endpoint = Endpoint::for_vm(&paths, &name)?;
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
    vm.state.pid = None;
    vm.state.started_at = None;
    vm.save()?;
    println!("{name} stopped");
    Ok(())
}

pub fn exec(r: &ImageRef, command: &[String]) -> Result<i32> {
    let paths = Paths::discover()?;
    let vm = existing_vm(&paths, r)?;
    let name = &vm.state.name;
    if !vm.is_running() {
        bail!("{name} is not running; start it with `vmmbox start {name}`");
    }
    let ssh = Ssh::for_vm(&vm)?;
    let remote = remote_command(guest_cwd(&vm).as_deref(), command);
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    ssh.run_interactive(&remote, tty)
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

    #[test]
    fn free_port_is_usable() {
        let p = free_port().unwrap();
        assert!(p > 0);
        assert!(TcpListener::bind(("127.0.0.1", p)).is_ok());
    }
}
