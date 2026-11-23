//! Linux file modes: only the owner may read a private file or enter a private folder.

use std::fs::{DirBuilder, OpenOptions};

/// Gives `options` the mode of a file only its owner can read.
pub fn file_mode(options: &mut OpenOptions) {
    std::os::unix::fs::OpenOptionsExt::mode(options, 0o600);
}

/// Gives `builder` the mode of a folder only its owner can enter.
pub fn dir_mode(builder: &mut DirBuilder) {
    std::os::unix::fs::DirBuilderExt::mode(builder, 0o700);
}
