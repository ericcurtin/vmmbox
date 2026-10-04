//! Small formatting and quoting helpers shared across the crate.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Total size of the regular files under `dir`, not following symlinks.
pub fn dir_size(dir: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Format a byte count using binary units: `32 GiB`, `901.7 MiB`.
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if (v - v.round()).abs() < 0.05 {
        format!("{:.0} {}", v.round(), UNITS[unit])
    } else {
        format!("{:.1} {}", v, UNITS[unit])
    }
}

fn plural(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("{n} {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// Coarse human duration: `5 seconds`, `3 hours`, `2 days`.
pub fn format_duration(secs: u64) -> String {
    match secs {
        0..=59 => plural(secs, "second"),
        60..=3599 => plural(secs / 60, "minute"),
        3600..=86399 => plural(secs / 3600, "hour"),
        86400..=1_209_599 => plural(secs / 86400, "day"),
        1_209_600..=5_183_999 => plural(secs / 604_800, "week"),
        _ => plural(secs / 2_592_000, "month"),
    }
}

pub fn format_ago(then: u64) -> String {
    format!("{} ago", format_duration(now_secs().saturating_sub(then)))
}

/// Quote one word for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Render rows as an aligned, space-separated table. The last column is not padded.
pub fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0; cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        let last = row.len().saturating_sub(1);
        for (i, cell) in row.iter().enumerate() {
            if i == last {
                out.push_str(cell);
            } else {
                out.push_str(cell);
                out.extend(std::iter::repeat_n(
                    ' ',
                    widths[i] - cell.chars().count() + 3,
                ));
            }
        }
        out.push('\n');
    }
    out
}

/// Last `n` lines of a text file, for surfacing QEMU / console errors.
pub fn tail_lines(path: &std::path::Path, n: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(32 << 30), "32 GiB");
        assert_eq!(format_bytes(512 << 30), "512 GiB");
        assert_eq!(format_bytes(945_530_880), "901.7 MiB");
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(1), "1 second");
        assert_eq!(format_duration(125), "2 minutes");
        assert_eq!(format_duration(7200), "2 hours");
        assert_eq!(format_duration(86400 * 3), "3 days");
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("bash"), "bash");
        assert_eq!(sh_quote("/Users/me/dir"), "/Users/me/dir");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn tables_align() {
        let t = table(&[
            vec!["A".into(), "BB".into(), "C".into()],
            vec!["aaa".into(), "b".into(), "c".into()],
        ]);
        assert_eq!(t, "A     BB   C\naaa   b    c\n");
    }
}
