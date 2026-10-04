//! The server loop: accept QEMU's vhost-user connection, then serve the guest's
//! FUSE requests from the host directory until QEMU goes away.
//!
//! One thread does everything, as libkrun's own virtio-fs device does: wait on
//! QEMU's socket and on each queue's kick descriptor, handle what arrived,
//! repeat. `poll(2)` stands in for `epoll`, which macOS does not have.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};

use super::LOG_LEVEL;
use super::descriptor_utils::{Reader, Writer};
use super::inode_alloc::InodeAllocator;
use super::passthrough::{CachePolicy, Config, PassthroughFs, PermissionSemantics};
use super::server::Server;
use super::vhost_user::{self, Backend, Outcome};

/// How long a guest may cache what the server told it about a directory entry
/// or a file's attributes. The host's own programs change files behind the
/// guest's back (an editor saving, git checking out), so this is short.
const CACHE_TIMEOUT: Duration = Duration::from_secs(1);

/// Serve `root` to the guest of the QEMU that connects to `socket`.
///
/// Returns when QEMU hangs up. The socket is removed once QEMU has connected,
/// and on the way out if it never does.
pub fn serve(socket: &Path, root: &Path, debug: bool) -> Result<()> {
    LOG_LEVEL.store(debug as u8, Ordering::Relaxed);
    raise_open_file_limit();

    let root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    if !root.is_dir() {
        anyhow::bail!("{} is not a directory", root.display());
    }

    let _ = std::fs::remove_file(socket);
    let listener =
        UnixListener::bind(socket).with_context(|| format!("listening on {}", socket.display()))?;
    // Only this user may connect.
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;

    let server = make_server(&root)?;
    let accepted = listener.accept();
    let _ = std::fs::remove_file(socket);
    let (conn, _) = accepted.context("waiting for QEMU to connect")?;
    drop(listener);
    debug!("QEMU connected; serving {}", root.display());

    run(conn, &server).context("serving the guest")
}

fn make_server(root: &Path) -> Result<Server<PassthroughFs>> {
    let cfg = Config {
        root_dir: root.to_string_lossy().into_owned(),
        entry_timeout: CACHE_TIMEOUT,
        attr_timeout: CACHE_TIMEOUT,
        // Close-to-open consistency: the guest revalidates when it opens a file.
        cache_policy: CachePolicy::Auto,
        // The host changes files without telling the guest, so the guest must
        // not hold writes back.
        writeback: false,
        // The default keeps the guest's idea of owner and mode in a hidden
        // extended attribute and leaves the real file alone, which is right for
        // a container image and wrong for a home directory: `chmod +x` in the
        // guest must make the file executable on the host too, and the host's
        // files must not each grow an attribute. This keeps the real mode bits.
        // The price is no extended attributes, and ownership reported as the
        // caller's; the guest user has the host user's uid, so that is who owns
        // the files anyway.
        semantics: PermissionSemantics::LinuxSimplified,
        xattr: false,
        ..Config::default()
    };
    let fs = PassthroughFs::new(cfg, Arc::new(InodeAllocator::new()))
        .context("opening the shared directory")?;
    Ok(Server::new(fs))
}

/// The server keeps a descriptor open for every file the guest has looked at,
/// and macOS starts a process with only 256.
fn raise_open_file_limit() {
    // SAFETY: plain getrlimit/setrlimit calls on a local struct.
    unsafe {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return;
        }
        // The hard limit is often "unlimited", which the kernel will not
        // accept as a soft one; work down to what it will.
        for want in [limit.rlim_max, 1 << 20, 1 << 16, 10240] {
            let want = want.min(limit.rlim_max);
            let new = libc::rlimit {
                rlim_cur: want,
                rlim_max: limit.rlim_max,
            };
            if want > limit.rlim_cur && libc::setrlimit(libc::RLIMIT_NOFILE, &new) == 0 {
                return;
            }
        }
    }
}

fn run(conn: UnixStream, server: &Server<PassthroughFs>) -> io::Result<()> {
    let mut backend = Backend::new();
    let exit_code = Arc::new(AtomicI32::new(0));
    // Whether a queue has been drained since it last started: buffers the
    // guest added before we had its kick descriptor would never be announced.
    let mut primed = [false; vhost_user::NUM_QUEUES];

    loop {
        let mut fds = vec![libc::pollfd {
            fd: conn.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // For each descriptor after the first, the queue it belongs to.
        let mut owner = Vec::new();
        for (i, v) in backend.vrings.iter().enumerate() {
            match &v.kick {
                Some(kick) if v.running() => {
                    fds.push(libc::pollfd {
                        fd: kick.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    });
                    owner.push(i);
                }
                _ => primed[i] = false,
            }
        }

        // A queue that has just started may already have buffers in it.
        let mut worked = false;
        for &q in &owner {
            if !primed[q] {
                primed[q] = true;
                debug!("queue {q} started");
                process_queue(&mut backend, q, server, &exit_code);
                worked = true;
            }
        }
        if worked {
            continue;
        }

        // SAFETY: `fds` is a valid array of the length given.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }

        // QEMU first: a message can change which queues exist.
        if fds[0].revents & libc::POLLIN != 0 {
            match vhost_user::recv(&conn)? {
                None => return Ok(()),
                Some(msg) => {
                    if backend.handle(&conn, msg)? == Outcome::Stop {
                        return Ok(());
                    }
                }
            }
            continue;
        }
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            return Ok(());
        }

        for (j, &q) in owner.iter().enumerate() {
            let revents = fds[j + 1].revents;
            if revents & libc::POLLIN != 0 {
                debug!("queue {q} kicked");
                if let Some(kick) = &backend.vrings[q].kick {
                    drain(kick.as_raw_fd());
                }
                process_queue(&mut backend, q, server, &exit_code);
            } else if revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                // QEMU closed its end: the queue is gone.
                backend.vrings[q].kick = None;
            }
        }
    }
}

/// Read a kick descriptor dry. It is an eventfd or a pipe, so what is read
/// does not matter, only that it is no longer readable.
fn drain(fd: i32) {
    let mut buf = [0u8; 64];
    loop {
        // SAFETY: `buf` is valid for its length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            return;
        }
    }
}

/// Answer every request the guest has queued.
fn process_queue(
    backend: &mut Backend,
    index: usize,
    server: &Server<PassthroughFs>,
    exit_code: &Arc<AtomicI32>,
) {
    let Some(mem) = backend.mem.as_ref() else {
        return;
    };
    let vring = &mut backend.vrings[index];
    loop {
        if let Err(e) = vring.queue.disable_notification(mem) {
            error!("queue {index}: {e:?}");
            return;
        }
        while let Some(head) = vring.queue.pop(mem) {
            let len = match (
                Reader::new(mem, head.clone()),
                Writer::new(mem, head.clone()),
            ) {
                (Ok(r), Ok(w)) => server
                    .handle_message(r, w, false, &None, exit_code, &None)
                    .unwrap_or_else(|e| {
                        error!("handling a request: {e:?}");
                        0
                    }),
                _ => {
                    error!("queue {index}: a request with unreadable buffers");
                    0
                }
            };
            if let Err(e) = vring.queue.add_used(mem, head.index, len as u32) {
                error!("queue {index}: marking a request done: {e:?}");
            }
            if vring.queue.needs_notification(mem).unwrap_or(true) {
                vring.signal();
            }
        }
        // If more arrived while we were finishing, go round again.
        match vring.queue.enable_notification(mem) {
            Ok(true) => continue,
            _ => return,
        }
    }
}
