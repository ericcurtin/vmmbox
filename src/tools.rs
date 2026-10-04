//! Locating external programs vmmbox drives (QEMU, waypipe, a compositor).

use std::path::PathBuf;

/// `name` with the platform's executable suffix.
pub fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// The first `name` found in `priority_dirs`, then on `PATH`, then in
/// `fallback_dirs` (where package managers install things that may not be on
/// `PATH`, such as Homebrew's prefix for a GUI-launched process).
pub fn find_binary(
    name: &str,
    priority_dirs: &[PathBuf],
    fallback_dirs: &[PathBuf],
) -> Option<PathBuf> {
    let file = exe_name(name);
    let path_dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    priority_dirs
        .iter()
        .chain(&path_dirs)
        .chain(fallback_dirs)
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_dir_beats_fallback() {
        let root = std::env::temp_dir().join(format!("vmmbox-tools-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let name = "vmmbox-test-tool-xyz";
        std::fs::write(a.join(exe_name(name)), b"").unwrap();
        std::fs::write(b.join(exe_name(name)), b"").unwrap();

        let found = find_binary(name, std::slice::from_ref(&a), std::slice::from_ref(&b));
        assert_eq!(found, Some(a.join(exe_name(name))));
        let found = find_binary(name, &[], std::slice::from_ref(&b));
        assert_eq!(found, Some(b.join(exe_name(name))));
        assert_eq!(find_binary("vmmbox-no-such-tool", &[], &[]), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
