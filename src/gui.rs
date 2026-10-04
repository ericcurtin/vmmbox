#![cfg_attr(not(unix), allow(dead_code, unused_imports))]
//! Seamless GUI apps: the guest app speaks Wayland, `waypipe` carries the
//! protocol over the SSH connection, and a compositor on the host turns each
//! guest window into a real host window.
//!
//! * macOS: Cocoa-Way (a native Wayland compositor) in rootless mode, so each
//!   window is a separate macOS window. vmmbox starts it on first use.
//! * Linux: the user's own Wayland session.
//! * Windows: not supported yet.
//!
//! Cocoa-Way and waypipe-darwin are GPL-3.0 and vmmbox is GPL-2.0-only, so they
//! are only ever run as separate programs, never linked or bundled.

use crate::host::{Os, Platform};
use crate::paths::Paths;
use crate::ssh::{self, Ssh};
use crate::tools::find_binary;
use crate::util::{sh_quote, tail_lines};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for a freshly started compositor to open its socket.
const COMPOSITOR_TIMEOUT: Duration = Duration::from_secs(20);

/// Environment that steers common toolkits to Wayland. waypipe sets
/// `WAYLAND_DISPLAY` itself; these cover apps that otherwise prefer X11.
const GUEST_ENV: &[(&str, &str)] = &[
    ("XDG_SESSION_TYPE", "wayland"),
    ("MOZ_ENABLE_WAYLAND", "1"),
    ("QT_QPA_PLATFORM", "wayland"),
    ("ELECTRON_OZONE_PLATFORM_HINT", "auto"),
];

#[derive(Debug)]
pub struct Gui {
    waypipe: PathBuf,
    backend: Backend,
}

#[derive(Debug)]
enum Backend {
    /// Cocoa-Way, started on demand.
    CocoaWay(PathBuf),
    /// The compositor of the user's current Wayland session.
    Session,
}

/// A host Wayland socket to forward into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandSocket {
    pub runtime_dir: PathBuf,
    pub display: String,
}

impl Gui {
    /// Check that this host can show guest windows. The error says what is
    /// missing and how to get it.
    pub fn detect(platform: Platform, paths: &Paths) -> Result<Self> {
        let own = [paths.tools_bin()];
        match platform.os {
            Os::Mac => {
                let fallback = ["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from);
                let waypipe = find_binary("waypipe", &own, &fallback)
                    .or_else(|| find_binary("waypipe-darwin", &own, &fallback));
                let compositor = find_binary("cocoa-way", &own, &fallback);
                match (waypipe, compositor) {
                    (Some(waypipe), Some(bin)) => Ok(Self {
                        waypipe,
                        backend: Backend::CocoaWay(bin),
                    }),
                    (w, c) => {
                        let missing: Vec<&str> =
                            [(w.is_none(), "waypipe"), (c.is_none(), "cocoa-way")]
                                .iter()
                                .filter(|(m, _)| *m)
                                .map(|(_, n)| *n)
                                .collect();
                        bail!(
                            "GUI apps need {} on the host: brew install J-x-Z/tap/cocoa-way J-x-Z/tap/waypipe-darwin",
                            missing.join(" and ")
                        )
                    }
                }
            }
            Os::Linux => {
                let waypipe = find_binary("waypipe", &own, &["/usr/bin".into()])
                    .context("GUI apps need waypipe on the host (install the waypipe package)")?;
                if session_socket().is_none() {
                    bail!("GUI apps need a running Wayland session (WAYLAND_DISPLAY is not set)");
                }
                Ok(Self {
                    waypipe,
                    backend: Backend::Session,
                })
            }
            Os::Windows => bail!("GUI apps are not supported on Windows yet"),
        }
    }

    /// Where the host compositor listens, starting it first if needed.
    pub fn socket(&self, paths: &Paths) -> Result<WaylandSocket> {
        match &self.backend {
            Backend::Session => session_socket().context("no Wayland session socket"),
            Backend::CocoaWay(bin) => {
                if let Some(s) = live_socket(&cocoa_way_dirs()) {
                    return Ok(s);
                }
                start_cocoa_way(bin, &paths.root().join("cocoa-way.log"))
            }
        }
    }

    /// Run `command` in the guest with its Wayland connection forwarded to the
    /// host compositor, and return its exit status.
    ///
    /// This is what `waypipe ssh` does, done by hand so vmmbox owns the SSH
    /// process: a one-shot waypipe *client* listens on a private host socket,
    /// ssh forwards the guest's socket to it, and a waypipe *server* in the
    /// guest runs the command. The exit status is then plain ssh's. (waypipe's
    /// own ssh mode loses it on macOS: waypipe-darwin reaps its ssh child and
    /// then waits on it a second time.)
    #[cfg(unix)]
    pub fn run(
        &self,
        paths: &Paths,
        ssh: &Ssh,
        tty: bool,
        cwd: Option<&str>,
        command: &[String],
    ) -> Result<i32> {
        let wayland = self.socket(paths)?;
        let id = format!("{}-{}", std::process::id(), crate::util::now_secs());
        let host_sock = paths.runtime_dir()?.join(format!("wp-{id}.sock"));
        let guest_sock = format!("/tmp/vmmbox-wp-{id}.sock");
        let _ = std::fs::remove_file(&host_sock);

        let mut client = Command::new(&self.waypipe)
            .arg("--socket")
            .arg(&host_sock)
            .args(["--oneshot", "client"])
            .env("XDG_RUNTIME_DIR", &wayland.runtime_dir)
            .env("WAYLAND_DISPLAY", &wayland.display)
            .stdin(Stdio::null())
            // Never hand waypipe our own stderr: waypipe-darwin calls isatty()
            // on it and aborts if it is a socket (as it is under some IDEs and
            // process supervisors). A pipe is always safe; forward it ourselves.
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", self.waypipe.display()))?;
        if let Some(mut err) = client.stderr.take() {
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut err, &mut std::io::stderr());
            });
        }

        // The forward must have somewhere to connect to by the time the guest
        // opens its first Wayland connection.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !host_sock.exists() {
            if let Some(status) = client.try_wait()? {
                bail!("waypipe exited during start-up ({status})");
            }
            if Instant::now() > deadline {
                let _ = client.kill();
                bail!("waypipe did not open its socket in time");
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let rc_file = format!("/tmp/vmmbox-rc-{id}");
        let remote = gui_server_command(&guest_sock, &gui_script(cwd, command, &rc_file), &rc_file);
        let mut cmd = ssh.command();
        cmd.args(["-o", "StreamLocalBindUnlink=yes"])
            .arg("-R")
            .arg(format!("{guest_sock}:{}", host_sock.display()));
        if tty {
            cmd.arg("-t");
        }
        let result = cmd.arg(ssh.target()).arg(remote).status();

        // The client lingers if the command never opened a Wayland connection.
        let _ = client.kill();
        let _ = client.wait();
        let _ = std::fs::remove_file(&host_sock);
        Ok(ssh::exit_code(result.context("running ssh")?))
    }

    #[cfg(not(unix))]
    pub fn run(
        &self,
        _paths: &Paths,
        _ssh: &Ssh,
        _tty: bool,
        _cwd: Option<&str>,
        _command: &[String],
    ) -> Result<i32> {
        bail!("GUI apps are not supported on this host yet")
    }
}

/// The current Wayland session's socket, if there is a live one.
fn session_socket() -> Option<WaylandSocket> {
    let display = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|d| !d.is_empty())?;
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    let path = dir.join(&display);
    is_live_socket(&path).then_some(WaylandSocket {
        runtime_dir: dir,
        display,
    })
}

/// Directories where Cocoa-Way may have put its `wayland-*` socket.
fn cocoa_way_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for var in ["COCOA_WAY_RUNTIME_DIR", "XDG_RUNTIME_DIR"] {
        if let Some(d) = std::env::var_os(var).filter(|d| !d.is_empty()) {
            dirs.push(PathBuf::from(d));
        }
    }
    if let Some(t) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) {
        dirs.push(PathBuf::from(t).join("cocoa-way"));
    }
    dirs.push(PathBuf::from("/tmp/cocoa-way"));
    dirs
}

/// The first connectable `wayland-*` socket in `dirs`.
fn live_socket(dirs: &[PathBuf]) -> Option<WaylandSocket> {
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut names: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
            .collect();
        names.sort();
        for name in names {
            if is_live_socket(&dir.join(&name)) {
                return Some(WaylandSocket {
                    runtime_dir: dir.clone(),
                    display: name,
                });
            }
        }
    }
    None
}

/// Whether something is listening on the Unix socket at `path`. A stale socket
/// file left by a crashed compositor refuses the connection.
#[cfg(unix)]
fn is_live_socket(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(not(unix))]
fn is_live_socket(_path: &Path) -> bool {
    false
}

/// Launch Cocoa-Way detached and wait for it to open its Wayland socket. It is
/// started in rootless mode: one native window per guest window.
fn start_cocoa_way(bin: &Path, log: &Path) -> Result<WaylandSocket> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    eprintln!("Starting the Cocoa-Way compositor...");
    let mut cmd = Command::new(bin);
    cmd.env("COCOA_WAY_PRESENTATION", "rootless");
    let mut child = crate::proc::spawn_detached(&mut cmd, log)?;

    let start = Instant::now();
    loop {
        if let Some(socket) = live_socket(&cocoa_way_dirs()) {
            return Ok(socket);
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "Cocoa-Way exited during start-up ({status}):\n{}\nlog: {}",
                tail_lines(log, 10),
                log.display()
            );
        }
        if start.elapsed() > COMPOSITOR_TIMEOUT {
            bail!(
                "Cocoa-Way did not open a Wayland socket within {}s; see {}",
                COMPOSITOR_TIMEOUT.as_secs(),
                log.display()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Commands that are never GUI apps; skip the guest check for them so plain
/// shells start without an extra round trip.
const NEVER_GUI: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh"];

/// Whether `command` is worth checking in the guest at all.
pub fn worth_checking(command: &[String]) -> bool {
    let Some(first) = command.first() else {
        return false;
    };
    let base = first.rsplit('/').next().unwrap_or(first);
    !NEVER_GUI.contains(&base)
}

/// A guest script that exits 0 when `command` resolves to the program behind
/// some installed, non-terminal `.desktop` launcher, i.e. a GUI application.
/// Matching goes through `readlink -f` so wrappers and `alternatives` links
/// (`google-chrome` -> `google-chrome-stable`) resolve to the same target.
pub fn gui_check_script(command: &str) -> String {
    let script = format!(
        r#"c={cmd}
p=$(command -v -- "$c" 2>/dev/null) || exit 1
r=$(readlink -f -- "$p" 2>/dev/null) || r=$p
files=$(ls /usr/share/applications/*.desktop /usr/local/share/applications/*.desktop "$HOME"/.local/share/applications/*.desktop /var/lib/flatpak/exports/share/applications/*.desktop 2>/dev/null)
[ -n "$files" ] || exit 1
paths=""
for e in $(grep -L '^Terminal=true' $files 2>/dev/null | xargs -r sed -n 's/^Exec=\([^ ]*\).*/\1/p' | sort -u); do
  q=$(command -v -- "$e" 2>/dev/null) && paths="$paths $q"
done
[ -n "$paths" ] || exit 1
printf '%s\n' $paths | xargs -r readlink -f -- 2>/dev/null | grep -qxF -- "$r"
"#,
        cmd = sh_quote(command)
    );
    format!("sh -c {}", sh_quote(&script))
}

/// The script the guest shell runs for a GUI command: enter the working
/// directory (best effort), set the Wayland-preferring environment, run the
/// command, and record its exit status in `rc_file`.
///
/// The status goes through a file because waypipe's `server` mode returns 0
/// whatever its command did, so it cannot be read off the SSH exit status.
pub fn gui_script(cwd: Option<&str>, command: &[String], rc_file: &str) -> String {
    let mut script = String::new();
    if let Some(dir) = cwd {
        script.push_str(&format!("cd {} 2>/dev/null; ", sh_quote(dir)));
    }
    script.push_str("env");
    for (k, v) in GUEST_ENV {
        script.push_str(&format!(" {k}={v}"));
    }
    for word in command {
        script.push(' ');
        script.push_str(&sh_quote(word));
    }
    script.push_str(&format!(
        "; rc=$?; echo \"$rc\" > {}; exit \"$rc\"",
        sh_quote(rc_file)
    ));
    script
}

/// The line the guest's login shell runs. The script travels as one quoted
/// word to `sh -c`, where a `;` cannot split it; afterwards the outer shell
/// exits with the status the script recorded (or waypipe's own, if waypipe
/// failed before the command ran).
///
/// `--no-gpu`: waypipe's dmabuf path cannot cross a VM boundary, and the host
/// side is built without it.
pub fn gui_server_command(guest_socket: &str, script: &str, rc_file: &str) -> String {
    let rc = sh_quote(rc_file);
    format!(
        "waypipe --no-gpu --unlink-socket --socket {} server -- sh -c {}; \
         w=$?; rc=$(cat {rc} 2>/dev/null); rm -f {rc}; exit ${{rc:-$w}}",
        sh_quote(guest_socket),
        sh_quote(script),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vmmbox-gui-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn finds_only_live_sockets() {
        let dir = temp("sock");
        // A stale socket (nobody listening) and a lock file must be ignored.
        let stale = dir.join("wayland-0");
        drop(UnixListener::bind(&stale).unwrap());
        std::fs::write(dir.join("wayland-1.lock"), b"").unwrap();
        // Other tests fork children, and a child forked while the listener was
        // still open briefly keeps the socket alive until it execs. Wait that
        // out; a genuinely dead compositor never has this window.
        let mut dead = false;
        for _ in 0..300 {
            if live_socket(std::slice::from_ref(&dir)).is_none() {
                dead = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(dead, "a stale socket must not be treated as live");

        let live = dir.join("wayland-2");
        let _listener = UnixListener::bind(&live).unwrap();
        let got = live_socket(std::slice::from_ref(&dir)).unwrap();
        assert_eq!(got.display, "wayland-2");
        assert_eq!(got.runtime_dir, dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn searches_directories_in_order() {
        let (a, b) = (temp("ord-a"), temp("ord-b"));
        let _la = UnixListener::bind(a.join("wayland-5")).unwrap();
        let _lb = UnixListener::bind(b.join("wayland-1")).unwrap();
        let got = live_socket(&[PathBuf::from("/nonexistent"), a.clone(), b.clone()]).unwrap();
        assert_eq!(got.runtime_dir, a);
        std::fs::remove_dir_all(&a).unwrap();
        std::fs::remove_dir_all(&b).unwrap();
    }
}

#[cfg(all(test, unix))]
mod script_tests {
    use super::*;

    #[test]
    fn shells_are_not_checked() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(!worth_checking(&v(&["bash"])));
        assert!(!worth_checking(&v(&["/bin/zsh", "-l"])));
        assert!(!worth_checking(&[]));
        assert!(worth_checking(&v(&["google-chrome"])));
        assert!(worth_checking(&v(&["ls", "-la"])));
    }

    #[test]
    fn script_survives_a_real_shell() {
        let rc = std::env::temp_dir().join(format!("vmmbox-rc-a-{}", std::process::id()));
        let cmd = ["printf", "[%s]", "a b", "it's", "$HOME;x"].map(String::from);
        let script = gui_script(Some("/tmp"), &cmd, &rc.to_string_lossy());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "[a b][it's][$HOME;x]");
        assert_eq!(std::fs::read_to_string(&rc).unwrap().trim(), "0");
        std::fs::remove_file(&rc).unwrap();
    }

    #[test]
    fn script_sets_the_wayland_environment() {
        let rc = std::env::temp_dir().join(format!("vmmbox-rc-b-{}", std::process::id()));
        let script = gui_script(None, &["env".to_string()], &rc.to_string_lossy());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("XDG_SESSION_TYPE=wayland"), "{text}");
        assert!(text.contains("QT_QPA_PLATFORM=wayland"), "{text}");
        assert!(text.contains("MOZ_ENABLE_WAYLAND=1"), "{text}");
        let _ = std::fs::remove_file(&rc);
    }

    /// Run the full guest line with a fake `waypipe` that behaves like the
    /// real one: runs the command and always exits 0.
    fn run_guest_line(command: &[&str], tag: &str) -> (Option<i32>, String) {
        let rc = std::env::temp_dir().join(format!("vmmbox-rc-{tag}-{}", std::process::id()));
        let rc = rc.to_string_lossy().into_owned();
        let cmd: Vec<String> = command.iter().map(|s| s.to_string()).collect();
        let line = gui_server_command("/tmp/s.sock", &gui_script(None, &cmd, &rc), &rc);
        let fake = format!(
            "waypipe() {{ while [ \"$1\" != -- ]; do shift; done; shift; \"$@\"; return 0; }}; {line}"
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(fake)
            .output()
            .unwrap();
        assert!(
            !std::path::Path::new(&rc).exists(),
            "rc file must be cleaned up"
        );
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    #[test]
    fn exit_status_survives_waypipe_returning_zero() {
        assert_eq!(run_guest_line(&["sh", "-c", "exit 7"], "x7").0, Some(7));
        assert_eq!(run_guest_line(&["false"], "xf").0, Some(1));
        assert_eq!(run_guest_line(&["true"], "xt").0, Some(0));
        let (code, out) = run_guest_line(&["printf", "[%s]", "a b", "it's"], "xp");
        assert_eq!(code, Some(0));
        assert_eq!(out, "[a b][it's]");
    }

    #[test]
    fn waypipe_failure_is_not_masked() {
        // If waypipe dies before running the command there is no status file,
        // so waypipe's own non-zero status must come through.
        let rc = std::env::temp_dir().join(format!("vmmbox-rc-w-{}", std::process::id()));
        let rc = rc.to_string_lossy().into_owned();
        let line = gui_server_command("/tmp/s.sock", "true", &rc);
        let fake = format!("waypipe() {{ return 3; }}; {line}");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(fake)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(3));
    }

    #[test]
    fn server_command_is_one_shell_line() {
        let script = gui_script(
            Some("/Users/me/my proj"),
            &["foot".to_string(), "-e".into(), "ls".into()],
            "/tmp/rc",
        );
        let line = gui_server_command("/tmp/s.sock", &script, "/tmp/rc");
        assert!(line.starts_with(
            "waypipe --no-gpu --unlink-socket --socket /tmp/s.sock server -- sh -c '"
        ));
        // Parsed the way the guest's login shell would, the script is exactly
        // one word. A fake `waypipe` that prints its arguments shows that.
        let fake = format!(
            "waypipe() {{ for a in \"$@\"; do printf '<%s>' \"$a\"; done; return 0; }}; {line}"
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(fake)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            text.starts_with(
                "<--no-gpu><--unlink-socket><--socket></tmp/s.sock><server><--><sh><-c><"
            ),
            "{text}"
        );
        assert_eq!(
            text.matches('<').count(),
            9,
            "script must be a single word: {text}"
        );
    }

    #[test]
    fn check_script_quotes_the_command() {
        let s = gui_check_script("it's weird");
        assert!(s.starts_with("sh -c '"));
        // The command must arrive in the script as a single quoted word.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&s)
            .output()
            .unwrap();
        // `it's weird` is not an installed program: the check fails cleanly.
        assert_eq!(out.status.code(), Some(1));
    }

    #[test]
    fn check_script_recognises_a_desktop_app_through_symlinks() {
        // Build a fake "guest": a launcher whose Exec points at a symlink that
        // resolves to the same file as the command the user types.
        let root = std::env::temp_dir().join(format!("vmmbox-guisc-{}", std::process::id()));
        let apps = root.join("share/applications");
        let bin = root.join("bin");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let real = bin.join("realapp");
        std::fs::write(&real, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
            symlink(&real, bin.join("app")).unwrap();
            symlink(&real, bin.join("app-stable")).unwrap();
        }
        std::fs::write(
            apps.join("app.desktop"),
            format!(
                "[Desktop Entry]\nExec={}/app-stable %U\nName=App\n",
                bin.display()
            ),
        )
        .unwrap();
        std::fs::write(
            apps.join("tui.desktop"),
            format!(
                "[Desktop Entry]\nExec={}/realapp\nTerminal=true\n",
                bin.display()
            ),
        )
        .unwrap();

        // Run the check with HOME pointing at the fake share dir's parent, and
        // the system dirs hidden by rewriting them to the fake location.
        let script = gui_check_script(&format!("{}/app", bin.display()));
        let patched = script.replace("/usr/share/applications", &apps.to_string_lossy());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&patched)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // A program with no launcher is not a GUI app.
        std::fs::write(bin.join("tool"), "#!/bin/sh\n").unwrap();
        let script = gui_check_script(&format!("{}/tool", bin.display()));
        let patched = script.replace("/usr/share/applications", &apps.to_string_lossy());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&patched)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
