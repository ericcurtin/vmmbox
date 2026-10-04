//! Streaming hashing and parsing of distro checksum files.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Algo {
    Sha256,
    Sha512,
}

impl Algo {
    pub fn hex_len(self) -> usize {
        match self {
            Algo::Sha256 => 64,
            Algo::Sha512 => 128,
        }
    }
}

pub enum Hasher {
    Sha256(Sha256),
    Sha512(Sha512),
}

impl Hasher {
    pub fn new(algo: Algo) -> Self {
        match algo {
            Algo::Sha256 => Hasher::Sha256(Sha256::new()),
            Algo::Sha512 => Hasher::Sha512(Sha512::new()),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha256(h) => h.update(data),
            Hasher::Sha512(h) => h.update(data),
        }
    }

    pub fn finish_hex(self) -> String {
        let bytes: Vec<u8> = match self {
            Hasher::Sha256(h) => h.finalize().iter().copied().collect(),
            Hasher::Sha512(h) => h.finalize().iter().copied().collect(),
        };
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Find the hex digest for `file_name` in a checksum file.
///
/// Understands both layouts distros publish:
/// * coreutils: `<hex>  <name>` / `<hex> *<name>` (Ubuntu, Debian, Alma)
/// * BSD:       `SHA256 (<name>) = <hex>` (Fedora, Rocky, CentOS)
///
/// Comment lines and PGP clear-sign armour are ignored.
pub fn find_checksum(text: &str, file_name: &str, algo: Algo) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parsed = parse_bsd(line).or_else(|| parse_coreutils(line));
        if let Some((name, hex)) = parsed {
            let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
            if base == file_name
                && hex.len() == algo.hex_len()
                && hex.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Some(hex.to_ascii_lowercase());
            }
        }
    }
    None
}

fn parse_bsd(line: &str) -> Option<(&str, &str)> {
    let (_algo, rest) = line.split_once(" (")?;
    let (name, hex) = rest.rsplit_once(") = ")?;
    Some((name, hex.trim()))
}

fn parse_coreutils(line: &str) -> Option<(&str, &str)> {
    let (hex, name) = line.split_once(char::is_whitespace)?;
    Some((name.trim().trim_start_matches('*'), hex))
}

#[cfg(test)]
mod tests {
    use super::*;

    const H256: &str = "63a93bd5a8d76e33b15ceb5daa3657bd79be804748051ab178e643b0f5da22e7";

    #[test]
    fn hashes_known_vector() {
        let mut h = Hasher::new(Algo::Sha256);
        h.update(b"ab");
        h.update(b"c");
        assert_eq!(
            h.finish_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut h = Hasher::new(Algo::Sha512);
        h.update(b"abc");
        assert!(h.finish_hex().starts_with("ddaf35a193617aba"));
    }

    #[test]
    fn coreutils_star_format() {
        let text = format!("deadbeef *other.img\n{H256} *ubuntu-26.04-server-cloudimg-arm64.img\n");
        assert_eq!(
            find_checksum(
                &text,
                "ubuntu-26.04-server-cloudimg-arm64.img",
                Algo::Sha256
            )
            .as_deref(),
            Some(H256)
        );
    }

    #[test]
    fn coreutils_plain_format() {
        let text = format!("{H256}  AlmaLinux-10-GenericCloud-latest.aarch64.qcow2\n");
        assert!(
            find_checksum(
                &text,
                "AlmaLinux-10-GenericCloud-latest.aarch64.qcow2",
                Algo::Sha256
            )
            .is_some()
        );
    }

    #[test]
    fn bsd_format_with_comments_and_pgp_armour() {
        let text = format!(
            "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\n# Fedora-Cloud: 123 bytes\nSHA256 (Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2) = {H256}\n-----BEGIN PGP SIGNATURE-----\n"
        );
        assert_eq!(
            find_checksum(
                &text,
                "Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2",
                Algo::Sha256
            )
            .as_deref(),
            Some(H256)
        );
    }

    #[test]
    fn rejects_wrong_name_length_or_algo() {
        let text = format!("{H256}  a.img\n");
        assert_eq!(find_checksum(&text, "b.img", Algo::Sha256), None);
        assert_eq!(find_checksum(&text, "a.img", Algo::Sha512), None);
        assert_eq!(find_checksum("abc  a.img\n", "a.img", Algo::Sha256), None);
    }
}
