//! Linux process setup for the local chat runtime.

use crate::models::{Asset, ModelKind};
use std::process::Command;

pub(super) fn no_window(_command: &mut Command) {}

pub(super) const CPU_ARCHIVE: Asset<'static> = archive!(
    "llama-b12255-bin-ubuntu-x64.tar.gz",
    23_155_724,
    "e49f3492de6ca193766f84fa954490a9e32e733be8ac3ec53d08749775461aa7"
);
pub(super) const VULKAN_ARCHIVE: Asset<'static> = archive!(
    "llama-b12255-bin-ubuntu-vulkan-x64.tar.gz",
    35_981_697,
    "82443355c8893a15516e760da40c32c99f5fd39825812424dbcadeb6d67baf61"
);
