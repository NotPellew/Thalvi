//! Files and folders only their owner can read.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "windows")]
use windows as os;

/// Makes `path` and every missing folder above it, which only their owner can enter.
///
/// # Errors
/// Why the folder could not be made.
pub fn create_dir(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    os::dir_mode(&mut builder);
    builder.create(path)
}

/// Opens `path` to append to, making it as a file only its owner can read.
///
/// # Errors
/// Why the file could not be opened.
pub fn open_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    os::file_mode(&mut options);
    options.open(path)
}

/// Replaces `path` with `contents` in one step, as a file only its owner can read; the folder
/// holding it must exist.
///
/// # Errors
/// Why the file could not be written.
pub fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let written = write_new(&temporary, contents).and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// A leftover from an earlier crash must not lend its permissions to the new file.
fn write_new(path: &Path, contents: &[u8]) -> io::Result<()> {
    let _ = std::fs::remove_file(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    os::file_mode(&mut options);
    options.open(path)?.write_all(contents)
}
