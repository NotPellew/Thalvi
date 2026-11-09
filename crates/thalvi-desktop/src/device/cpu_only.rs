//! The backend of an OS that recognizes on the CPU only: it reports that, and starts no GPU session.

use std::path::Path;

use ort::session::Session;
use ort::session::builder::SessionBuilder;

use super::{Adapter, Fallback};

pub fn adapter() -> Adapter {
    Adapter::Unsupported
}

pub fn open(_builder: SessionBuilder, _path: &Path) -> Result<Session, Fallback> {
    Err(Fallback::WindowsUnsupported)
}
