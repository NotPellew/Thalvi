//! The Windows backend: it reports that recognition runs on the CPU here, and starts no GPU session.

use std::path::Path;

use ort::session::Session;
use ort::session::builder::SessionBuilder;

use super::{Adapter, Fallback, Nodes, Part};

pub fn adapter() -> Adapter {
    Adapter::Unsupported
}

pub fn open(
    _builder: SessionBuilder,
    _path: &Path,
    _part: Part,
) -> Result<(Session, Nodes), Fallback> {
    Err(Fallback::WindowsUnsupported)
}
