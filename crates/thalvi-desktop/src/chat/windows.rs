//! Starts the local chat runtime without a console window on Windows.

use crate::models::{Asset, ModelKind};
use std::os::windows::process::CommandExt;
use std::process::Command;

pub(super) fn no_window(command: &mut Command) {
    command.creation_flags(0x0800_0000);
}

pub(super) const CPU_ARCHIVE: Asset<'static> = archive!(
    "llama-b12255-bin-win-cpu-x64.zip",
    24_931_718,
    "ec06d7b2f3fc4f7cff3e395e3e85c0de5f928b6a62a8647bbf85776351385597"
);
pub(super) const VULKAN_ARCHIVE: Asset<'static> = archive!(
    "llama-b12255-bin-win-vulkan-x64.zip",
    37_897_090,
    "12460d097486a06eb1c3b745194eb1436d315bcc23003a74a40a11e2721167c8"
);
