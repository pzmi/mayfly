//! Start-up: composition opens the state directory and hands the journal adapter its reader.

use std::fs;
use std::path::Path;

pub struct Site {
    pub entries: usize,
    pub reader: fn(&Path) -> std::io::Result<Vec<u8>>,
}

pub fn open_root(path: &Path) -> std::io::Result<Site> {
    let entries = fs::read_dir(path)?.count();
    Ok(Site {
        entries,
        reader: mayfly_adapter_sqlite::read,
    })
}
