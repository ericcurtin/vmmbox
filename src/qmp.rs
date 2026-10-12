//! A minimal QMP (QEMU Machine Protocol) client, used for graceful shutdown.
//!
//! The channel is a Unix socket in a private directory on macOS and Linux. On
//! Windows there is none: QEMU's pipe chardev blocks at start-up until a client
//! connects and serves only that one, so a VM is stopped through the guest
//! instead (see `Ssh::power_off`).

use crate::paths::Paths;
use anyhow::{Result, bail};
use std::time::Duration;
#[cfg(unix)]
use {
    anyhow::Context,
    serde_json::{Value, json},
    std::io::{BufRead, BufReader, Read, Write},
    std::sync::mpsc,
};

#[derive(Clone, Debug)]
pub enum Endpoint {
    #[cfg(unix)]
    Unix(std::path::PathBuf),
    /// No control channel: QEMU is not given one.
    #[cfg_attr(unix, allow(dead_code))] // Windows uses it; unix tests do too
    Unavailable,
}

impl Endpoint {
    #[cfg(unix)]
    pub fn for_vm(paths: &Paths, name: &str) -> Result<Self> {
        Ok(Endpoint::Unix(
            paths.runtime_dir()?.join(format!("{name}.sock")),
        ))
    }

    #[cfg(windows)]
    pub fn for_vm(_paths: &Paths, _name: &str) -> Result<Self> {
        Ok(Endpoint::Unavailable)
    }

    /// Whether QEMU gets a control channel for this endpoint.
    pub fn available(&self) -> bool {
        !matches!(self, Endpoint::Unavailable)
    }

    /// The QEMU `-chardev` specification (with id `qmp`) for this endpoint, or
    /// `None` if QEMU is to have no control channel.
    pub fn chardev(&self) -> Option<String> {
        match self {
            #[cfg(unix)]
            Endpoint::Unix(p) => Some(format!(
                "socket,id=qmp,path={},server=on,wait=off",
                crate::qemu::escape(&p.to_string_lossy())
            )),
            Endpoint::Unavailable => None,
        }
    }

    /// Remove any stale socket left by a previous crashed run.
    pub fn cleanup(&self) {
        #[cfg(unix)]
        if let Endpoint::Unix(p) = self {
            let _ = std::fs::remove_file(p);
        }
    }

    #[cfg(unix)]
    fn connect(&self) -> std::io::Result<std::os::unix::net::UnixStream> {
        match self {
            Endpoint::Unix(p) => std::os::unix::net::UnixStream::connect(p),
            Endpoint::Unavailable => Err(std::io::ErrorKind::Unsupported.into()),
        }
    }

    /// Run one QMP command, giving up after `timeout`. The exchange runs on its
    /// own thread so that a QEMU that never answers cannot hold us up.
    #[cfg_attr(not(unix), allow(unused_variables))]
    pub fn execute(&self, command: &str, timeout: Duration) -> Result<()> {
        #[cfg(unix)]
        if self.available() {
            let endpoint = self.clone();
            let command = command.to_string();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(endpoint.exchange(&command));
            });
            return match rx.recv_timeout(timeout) {
                Ok(result) => result,
                Err(_) => bail!("timed out waiting for QEMU to answer"),
            };
        }
        bail!("this VM has no QEMU control channel")
    }

    #[cfg(unix)]
    fn exchange(&self, command: &str) -> Result<()> {
        let conn = self
            .connect()
            .context("connecting to the QEMU control channel")?;
        let mut conn = BufReader::new(conn);

        read_message(&mut conn)?; // greeting
        send(&mut conn, "qmp_capabilities")?;
        read_reply(&mut conn)?.context("QEMU closed the control channel during negotiation")?;
        send(&mut conn, command)?;
        // `quit` may close the channel before replying; that counts as success.
        read_reply(&mut conn)?;
        Ok(())
    }
}

#[cfg(unix)]
fn send<S: Read + Write>(conn: &mut BufReader<S>, command: &str) -> Result<()> {
    let msg = json!({ "execute": command }).to_string() + "\n";
    let stream = conn.get_mut();
    stream.write_all(msg.as_bytes())?;
    stream.flush()?;
    Ok(())
}

#[cfg(unix)]
/// Read one JSON message; `None` on EOF.
fn read_message<R: Read>(r: &mut BufReader<R>) -> Result<Option<Value>> {
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line.trim().is_empty() {
            continue;
        }
        return Ok(Some(
            serde_json::from_str(&line).context("malformed QMP message")?,
        ));
    }
}

#[cfg(unix)]
/// Read until a command reply, skipping asynchronous events.
/// Returns `None` if the channel closed first.
fn read_reply<R: Read>(r: &mut BufReader<R>) -> Result<Option<Value>> {
    while let Some(msg) = read_message(r)? {
        if let Some(err) = msg.get("error") {
            bail!("QEMU rejected the command: {err}");
        }
        if let Some(ret) = msg.get("return") {
            return Ok(Some(ret.clone()));
        }
    }
    Ok(None)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A fake QEMU that speaks just enough QMP.
    fn fake_qemu(
        path: std::path::PathBuf,
        answer_command: bool,
    ) -> std::thread::JoinHandle<Vec<String>> {
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut r = BufReader::new(stream);
            let mut seen = Vec::new();
            r.get_mut()
                .write_all(b"{\"QMP\": {\"version\": {}, \"capabilities\": []}}\n")
                .unwrap();
            for i in 0..2 {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let v: Value = serde_json::from_str(&line).unwrap();
                seen.push(v["execute"].as_str().unwrap().to_string());
                if i == 1 && !answer_command {
                    break; // close without replying, like `quit`
                }
                // An async event must be skipped by the client.
                r.get_mut().write_all(b"{\"event\": \"RESET\"}\n").unwrap();
                r.get_mut().write_all(b"{\"return\": {}}\n").unwrap();
            }
            seen
        })
    }

    fn sock(tag: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("/tmp/vmmbox-qmp-{}-{tag}.sock", std::process::id()))
    }

    #[test]
    fn negotiates_and_runs_command() {
        let p = sock("ok");
        let h = fake_qemu(p.clone(), true);
        Endpoint::Unix(p.clone())
            .execute("system_powerdown", Duration::from_secs(5))
            .unwrap();
        assert_eq!(h.join().unwrap(), ["qmp_capabilities", "system_powerdown"]);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn eof_after_command_is_success() {
        let p = sock("quit");
        let h = fake_qemu(p.clone(), false);
        Endpoint::Unix(p.clone())
            .execute("quit", Duration::from_secs(5))
            .unwrap();
        assert_eq!(h.join().unwrap(), ["qmp_capabilities", "quit"]);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn missing_socket_is_an_error() {
        let p = sock("missing");
        assert!(
            Endpoint::Unix(p)
                .execute("quit", Duration::from_secs(2))
                .is_err()
        );
    }
}
