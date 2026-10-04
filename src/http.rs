//! HTTP downloads with checksumming and a terminal progress bar.

use crate::checksum::{Algo, Hasher};
use crate::util::format_bytes;
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{BufWriter, IsTerminal, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub struct Http {
    agent: ureq::Agent,
}

pub struct Downloaded {
    pub size: u64,
    pub digest: String,
}

impl Http {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(concat!("vmmbox/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(30)))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }

    /// GET a small text resource (directory listings, checksum files).
    pub fn get_text(&self, url: &str) -> Result<String> {
        let mut resp = self
            .agent
            .get(url)
            .call()
            .with_context(|| format!("GET {url}"))?;
        resp.body_mut()
            .with_config()
            .limit(16 << 20)
            .lossy_utf8(true)
            .read_to_string()
            .with_context(|| format!("reading {url}"))
    }

    /// Download `url` to `dest`, hashing as it streams.
    pub fn download(&self, url: &str, dest: &Path, algo: Algo, label: &str) -> Result<Downloaded> {
        let resp = self
            .agent
            .get(url)
            .call()
            .with_context(|| format!("GET {url}"))?;
        let total = resp.body().content_length();
        let mut reader = resp.into_body().into_reader();

        let file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        let mut hasher = Hasher::new(algo);
        let mut progress = Progress::new(label, total);
        let mut buf = vec![0u8; 256 << 10];
        let mut done = 0u64;
        loop {
            let n = reader.read(&mut buf).context("download interrupted")?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])
                .with_context(|| format!("writing {}", dest.display()))?;
            hasher.update(&buf[..n]);
            done += n as u64;
            progress.update(done);
        }
        out.flush()?;
        progress.finish();
        if let Some(t) = total
            && t != done
        {
            bail!("download truncated: got {done} of {t} bytes");
        }
        Ok(Downloaded {
            size: done,
            digest: hasher.finish_hex(),
        })
    }
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
