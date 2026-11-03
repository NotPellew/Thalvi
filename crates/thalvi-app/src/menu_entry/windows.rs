//! The Windows app menu entry, which the installer owns.

use super::MenuEntry;
use crate::switch::Unavailable;

const INSTALLER_OWNS_IT: &str = "Added by the installer; uninstalling removes it.";

/// The Start menu shortcut.
pub struct System;

impl System {
    pub fn from_env() -> Self {
        Self
    }
}

fn installer_owns_it() -> String {
    INSTALLER_OWNS_IT.to_owned()
}

impl MenuEntry for System {
    fn unavailable(&self) -> Option<Unavailable> {
        Some(Unavailable {
            reason: installer_owns_it(),
            line: None,
        })
    }

    fn is_shown(&self) -> Result<bool, String> {
        Err(installer_owns_it())
    }

    fn show(&self) -> Result<(), String> {
        Err(installer_owns_it())
    }

    fn hide(&self) -> Result<(), String> {
        Err(installer_owns_it())
    }
}
