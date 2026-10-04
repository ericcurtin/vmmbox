//! HTTP downloads through the system `curl`, with checksumming and a terminal
//! progress bar.
//!
//! vmmbox is GPL-2.0 licensed, which cannot be combined with the Apache-2.0
//! code in the common Rust TLS stacks (`ring`, `aws-lc`, the `openssl` crate),
//! so it links no TLS library at all. curl ships with macOS and Windows 10+,
//! is on practically every Linux machine, and uses the operating system's own
//! TLS and certificate store, so corporate root CAs and proxies just work.

use crate::checksum::{Algo, Hasher};
use crate::util::format_bytes;
use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const USER_AGENT: &str = concat!("vmmbox/", env!("CARGO_PKG_VERSION"));

pub struct Http {
    curl: PathBuf,
}

#[derive(Debug)]
pub struct Downloaded {
    pub size: u64,
    pub digest: String,
}

fn find_curl() -> Result<PathBuf> {
    let file = if cfg!(windows) { "curl.exe" } else { "curl" };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        dirs.push(PathBuf::from(root).join("System32"));
    }
    dirs.iter()
        .map(|d| d.join(file))
        .find(|p| p.is_file())
        .context(
            "curl not found. vmmbox uses curl for downloads (it ships with macOS and Windows; \
             on Linux install the curl package)",
        )
}

impl Http {
    pub fn new() -> Result<Self> {
        Ok(Self { curl: find_curl()? })
    }

    /// A curl command with the options every request shares: fail on HTTP
    /// errors, follow redirects, quiet apart from error messages.
    fn curl(&self) -> Command {
        let mut c = Command::new(&self.curl);
        c.args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=http,https",
            "--proto-redir",
            "=http,https",
            "--connect-timeout",
            "30",
            "--user-agent",
            USER_AGENT,
        ])
        .stdin(Stdio::null());
        c
    }

    /// GET a small text resource (directory listings, checksum files).
    pub fn get_text(&self, url: &str) -> Result<String> {
        let out = self
            .curl()
            .args(["--max-time", "120", "--max-filesize", "16777216"])
            .arg(url)
            .output()
            .context("running curl")?;
        if !out.status.success() {
            bail!("GET {url}: {}", curl_error(&out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Size of the resource at `url` from a HEAD request, if the server says.
    fn content_length(&self, url: &str) -> Option<u64> {
        let out = self
            .curl()
            .args(["--head", "--include", "--max-time", "30"])
            .arg(url)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| final_content_length(&String::from_utf8_lossy(&out.stdout)))
            .flatten()
    }

    /// Download `url` to `dest`, then hash it.
    pub fn download(&self, url: &str, dest: &Path, algo: Algo, label: &str) -> Result<Downloaded> {
        let total = self.content_length(url);
        let mut progress = Progress::new(label, total);

        let mut child = self
            .curl()
            .args(["--retry", "3", "--output"])
            .arg(dest)
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("running curl")?;

        // curl does the transfer; watch the file grow to draw progress.
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            progress.update(std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0));
            std::thread::sleep(Duration::from_millis(100));
        };
        progress.finish();

        if !status.success() {
            let mut stderr = Vec::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_end(&mut stderr);
            }
            bail!("GET {url}: {}", curl_error(&stderr));
        }

        let size = std::fs::metadata(dest)
            .with_context(|| format!("reading {}", dest.display()))?
            .len();
        if let Some(t) = total
            && t != size
        {
            bail!("download truncated: got {size} of {t} bytes");
        }
        Ok(Downloaded {
            size,
            digest: hash_file(dest, algo)?,
        })
    }
}

fn curl_error(stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    let s = s.trim();
    if s.is_empty() {
        "curl failed".to_string()
    } else {
        s.to_string()
    }
}

fn hash_file(path: &Path, algo: Algo) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Hasher::new(algo);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.finish_hex());
        }
        hasher.update(&buf[..n]);
    }
}

/// `Content-Length` of the *final* response in `curl --head --include`
/// output. Redirects print one header block each, and a redirect's own length
/// must not be mistaken for the file's.
fn final_content_length(headers: &str) -> Option<u64> {
    let normalized = headers.replace("\r\n", "\n");
    let last = normalized
        .split("\n\n")
        .map(str::trim)
        .filter(|block| !block.is_empty())
        .last()?;
    last.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

/// Single-line progress bar on stderr; silent when stderr is not a terminal.
struct Progress {
    label: String,
    total: Option<u64>,
    enabled: bool,
    start: Instant,
    last_draw: Instant,
}

impl Progress {
    fn new(label: &str, total: Option<u64>) -> Self {
        let enabled = std::io::stderr().is_terminal();
        if !enabled {
            eprintln!("Downloading {label}...");
        }
        let now = Instant::now();
        Self {
            label: label.to_string(),
            total,
            enabled,
            start: now,
            last_draw: now - Duration::from_secs(1),
        }
    }

    fn update(&mut self, done: u64) {
        if !self.enabled || self.last_draw.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_draw = Instant::now();
        let rate = done as f64 / self.start.elapsed().as_secs_f64().max(0.001);
        let line = match self.total {
            Some(total) if total > 0 => {
                let frac = (done as f64 / total as f64).clamp(0.0, 1.0);
                let width = 24;
                let filled = (frac * width as f64) as usize;
                format!(
                    "{} [{}{}] {:>3.0}%  {} / {}  {}/s",
                    self.label,
                    "=".repeat(filled),
                    " ".repeat(width - filled),
                    frac * 100.0,
                    format_bytes(done),
                    format_bytes(total),
                    format_bytes(rate as u64),
                )
            }
            _ => format!(
                "{}  {}  {}/s",
                self.label,
                format_bytes(done),
                format_bytes(rate as u64)
            ),
        };
        eprint!("\r\x1b[2K{line}");
        let _ = std::io::stderr().flush();
    }

    fn finish(&mut self) {
        if self.enabled {
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    #[test]
    fn content_length_of_final_response_only() {
        let direct = "HTTP/1.1 200 OK\r\nContent-Length: 1234\r\n\r\n";
        assert_eq!(final_content_length(direct), Some(1234));

        // A redirect's own (tiny) length must not leak through.
        let redirected = "HTTP/1.1 302 Found\r\nlocation: /x\r\ncontent-length: 5\r\n\r\n\
                          HTTP/2 200\r\ncontent-length: 900000\r\n\r\n";
        assert_eq!(final_content_length(redirected), Some(900_000));

        // Final response without a length: unknown, not the redirect's.
        let unknown = "HTTP/1.1 302 Found\r\ncontent-length: 5\r\n\r\nHTTP/1.1 200 OK\r\n\r\n";
        assert_eq!(final_content_length(unknown), None);
        assert_eq!(final_content_length(""), None);
    }

    /// Serve `/file`, `/redir` -> `/file`, and 404 for everything else.
    fn serve(body: Vec<u8>) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                let mut parts = request_line.split_whitespace();
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let out = reader.get_mut();
                match path {
                    "/file" => {
                        let _ = write!(
                            out,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        if method != "HEAD" {
                            let _ = out.write_all(&body);
                        }
                    }
                    "/redir" => {
                        let _ = write!(
                            out,
                            "HTTP/1.1 302 Found\r\nLocation: /file\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    }
                    _ => {
                        let _ = write!(
                            out,
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    }
                }
            }
        });
        port
    }

    fn sha256_hex(data: &[u8]) -> String {
        let mut h = Hasher::new(Algo::Sha256);
        h.update(data);
        h.finish_hex()
    }

    #[test]
    fn downloads_follows_redirects_and_hashes() {
        let Ok(http) = Http::new() else {
            eprintln!("curl not installed; skipping");
            return;
        };
        let body: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let port = serve(body.clone());
        let dir = std::env::temp_dir().join(format!("vmmbox-http-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        for path in ["file", "redir"] {
            let dest = dir.join(path);
            let got = http
                .download(
                    &format!("http://127.0.0.1:{port}/{path}"),
                    &dest,
                    Algo::Sha256,
                    "test",
                )
                .unwrap();
            assert_eq!(got.size, body.len() as u64);
            assert_eq!(got.digest, sha256_hex(&body));
            assert_eq!(std::fs::read(&dest).unwrap(), body);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn http_errors_are_reported() {
        let Ok(http) = Http::new() else {
            return;
        };
        let port = serve(Vec::new());
        let err = http
            .get_text(&format!("http://127.0.0.1:{port}/missing"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("404"), "unexpected error: {err}");

        let dest = std::env::temp_dir().join(format!("vmmbox-http-404-{}", std::process::id()));
        let err = http
            .download(
                &format!("http://127.0.0.1:{port}/missing"),
                &dest,
                Algo::Sha256,
                "t",
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("404"), "unexpected error: {err}");
        let _ = std::fs::remove_file(dest);
    }

    #[test]
    fn get_text_returns_body() {
        let Ok(http) = Http::new() else {
            return;
        };
        let port = serve(b"hello checksums\n".to_vec());
        assert_eq!(
            http.get_text(&format!("http://127.0.0.1:{port}/file"))
                .unwrap(),
            "hello checksums\n"
        );
    }
}
