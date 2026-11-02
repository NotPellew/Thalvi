//! Owns the process-wide ONNX Runtime environment that every model session is built in.

use std::sync::OnceLock;

use ort::environment::Environment;

/// The shared environment, created on first use.
///
/// # Errors
/// Fails with the reason when ONNX Runtime cannot start.
pub fn environment() -> Result<&'static Environment, String> {
    static ENVIRONMENT: OnceLock<Result<Environment, String>> = OnceLock::new();
    ENVIRONMENT
        .get_or_init(|| ort::init().build().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(Clone::clone)
}
