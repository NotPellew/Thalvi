//! Whether a Windows install can replace itself: the per-user installer's folder always can.

/// The per-user installer's folder is its user's to write, so the app always updates itself.
pub fn unsupported() -> Option<String> {
    None
}
