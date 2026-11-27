//! Hyprland's field probe: replaced by the AT-SPI reader, which is built separately.

use super::Window;
use crate::learn::{FieldLimit, FieldProbe};

/// Arms a watch of the focused field in `window`.
pub fn probe(_window: &Window) -> Result<Box<dyn FieldProbe>, FieldLimit> {
    Err(FieldLimit::NoAccessibility)
}
