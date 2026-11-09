//! Reading a tree of golden fixtures, for a crate whose own tests may not touch the file system
//! (a pure crate's `clippy.toml` forbids it, tests included).
//!
//! The reader only reads; what a fixture must contain is the calling harness's rule (ver §4).

use std::fs;
use std::io;
use std::path::Path;

/// Every file under `root`, as its path relative to `root` (components joined by `/`) and its
/// UTF-8 text, sorted by path. A file that is not UTF-8 is an error, so a harness never skips
/// a fixture it cannot read. Pinned by `should_list_every_file_sorted_when_a_tree_is_read`.
///
/// # Errors
/// Any I/O error, and [`io::ErrorKind::InvalidData`] for a path or text that is not UTF-8.
pub fn read_tree(root: &Path) -> io::Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    walk(root, "", &mut files)?;
    files.sort();
    Ok(files)
}

fn walk(dir: &Path, prefix: &str, files: &mut Vec<(String, String)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a file name is not UTF-8"))?;
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type()?.is_dir() {
            walk(&entry.path(), &relative, files)?;
        } else {
            files.push((relative, fs::read_to_string(entry.path())?));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_list_every_file_sorted_when_a_tree_is_read() {
        // given
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));

        // when
        let files = read_tree(root).expect("the crate's own tree is readable");

        // then
        let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"Cargo.toml"));
        assert!(paths.contains(&"src/fixtures.rs"));
        assert!(paths.windows(2).all(|w| w[0] < w[1]));
        let (_, text) = files
            .iter()
            .find(|(p, _)| p == "src/lib.rs")
            .expect("lib.rs is listed");
        assert!(text.contains("pub mod fixtures;"));
    }

    #[test]
    fn should_fail_when_the_root_does_not_exist() {
        // given
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-directory");

        // when
        let read = read_tree(&root);

        // then
        assert_eq!(read.map_err(|e| e.kind()), Err(io::ErrorKind::NotFound));
    }
}
