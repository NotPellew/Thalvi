//! Windows files and folders need no mode: they inherit the private ACL of the per-user data
//! directory.

use std::fs::{DirBuilder, OpenOptions};

/// Leaves `options` as Windows makes it.
pub fn file_mode(_: &mut OpenOptions) {}

/// Leaves `builder` as Windows makes it.
pub fn dir_mode(_: &mut DirBuilder) {}
