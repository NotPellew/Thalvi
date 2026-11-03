//! Whether Thalvi is in the desktop's app menu: the Settings switch for an `AppImage`'s menu entry.

use crate::switch::{SwitchState, Unavailable};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::System;
#[cfg(target_os = "windows")]
pub use windows::System;

/// The app menu entry for the current user.
pub trait MenuEntry {
    /// Why Thalvi cannot add or remove the entry here, when it cannot.
    fn unavailable(&self) -> Option<Unavailable>;
    /// # Errors
    /// Why the entry could not be checked.
    fn is_shown(&self) -> Result<bool, String>;
    /// Writes the entry for where Thalvi runs from now.
    ///
    /// # Errors
    /// Why the entry could not be written.
    fn show(&self) -> Result<(), String>;
    /// # Errors
    /// Why the entry could not be removed.
    fn hide(&self) -> Result<(), String>;
}

/// What the Settings switch shows: whether the entry exists, or why Thalvi offers no switch.
///
/// # Errors
/// Why the entry could not be checked.
pub fn switch(
    entry: &impl MenuEntry,
    restore_problem: Option<String>,
) -> Result<SwitchState, String> {
    if let Some(unavailable) = entry.unavailable() {
        return Ok(SwitchState::Unavailable(unavailable));
    }
    Ok(SwitchState::available(entry.is_shown()?, restore_problem))
}

/// Adds or removes the entry, then has `remember` save the choice as Thalvi's preference.
///
/// # Errors
/// The reason in plain words: this session offers no entry, the entry could not be changed, or the
/// choice could not be saved.
pub fn set(
    entry: &impl MenuEntry,
    on: bool,
    remember: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if let Some(unavailable) = entry.unavailable() {
        return Err(unavailable.reason);
    }
    if on { entry.show() } else { entry.hide() }.map_err(|reason| {
        let change = if on {
            "add Thalvi to"
        } else {
            "remove Thalvi from"
        };
        format!("Thalvi could not {change} the app menu: {reason}")
    })?;
    remember().map_err(|reason| {
        format!(
            "Thalvi changed its app menu entry but could not save that choice, so it may be undone when Thalvi next starts: {reason}"
        )
    })
}

/// Rewrites the entry on start when the user's preference is on, so an `AppImage` that moved stays
/// right. Does nothing when the preference is off or this session offers no entry.
///
/// # Errors
/// Why the entry could not be written.
pub fn restore(entry: &impl MenuEntry, preferred: bool) -> Result<(), String> {
    if !preferred || entry.unavailable().is_some() {
        return Ok(());
    }
    set(entry, true, || Ok(()))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[derive(Default)]
    struct Fake {
        shown: Cell<bool>,
        refuse: Option<&'static str>,
        unavailable: Option<Unavailable>,
        writes: Cell<u32>,
    }

    impl MenuEntry for Fake {
        fn unavailable(&self) -> Option<Unavailable> {
            self.unavailable.clone()
        }

        fn is_shown(&self) -> Result<bool, String> {
            Ok(self.shown.get())
        }

        fn show(&self) -> Result<(), String> {
            self.writes.set(self.writes.get() + 1);
            if let Some(reason) = self.refuse {
                return Err(reason.to_owned());
            }
            self.shown.set(true);
            Ok(())
        }

        fn hide(&self) -> Result<(), String> {
            if let Some(reason) = self.refuse {
                return Err(reason.to_owned());
            }
            self.shown.set(false);
            Ok(())
        }
    }

    #[test]
    fn the_entry_is_added_and_removed_and_remembered_only_when_that_worked() {
        let remembered = Cell::new(0);
        let remember = || {
            remembered.set(remembered.get() + 1);
            Ok(())
        };
        let os = Fake::default();
        assert_eq!(switch(&os, None), Ok(SwitchState::available(false, None)));
        assert_eq!(set(&os, true, remember), Ok(()));
        assert_eq!(switch(&os, None), Ok(SwitchState::available(true, None)));
        assert_eq!(set(&os, false, remember), Ok(()));
        assert_eq!(switch(&os, None), Ok(SwitchState::available(false, None)));
        assert_eq!(remembered.get(), 2);

        let refusing = Fake {
            refuse: Some("read-only folder"),
            ..Fake::default()
        };
        assert_eq!(
            set(&refusing, true, remember),
            Err("Thalvi could not add Thalvi to the app menu: read-only folder".to_owned())
        );
        refusing.shown.set(true);
        assert_eq!(
            set(&refusing, false, remember),
            Err("Thalvi could not remove Thalvi from the app menu: read-only folder".to_owned())
        );
        assert_eq!(remembered.get(), 2);

        assert_eq!(
            set(&Fake::default(), true, || Err("disk full".to_owned())),
            Err("Thalvi changed its app menu entry but could not save that choice, so it may be undone when Thalvi next starts: disk full".to_owned())
        );
    }

    #[test]
    fn a_wanted_entry_is_rewritten_on_start_and_an_unwanted_one_is_left_alone() {
        let moved = Fake::default();
        moved.shown.set(true);
        assert_eq!(restore(&moved, true), Ok(()));
        assert_eq!(moved.writes.get(), 1, "rewritten for the current path");

        for shown in [false, true] {
            let untouched = Fake::default();
            untouched.shown.set(shown);
            assert_eq!(restore(&untouched, false), Ok(()));
            assert_eq!((untouched.shown.get(), untouched.writes.get()), (shown, 0));
        }

        let refusing = Fake {
            refuse: Some("read-only folder"),
            ..Fake::default()
        };
        assert!(restore(&refusing, true).is_err());
    }

    #[test]
    fn a_session_without_an_entry_gets_its_reason_and_nothing_is_written() {
        let reason = Unavailable {
            reason: "Your package manager adds Thalvi to the app menu.".to_owned(),
            line: None,
        };
        let os = Fake {
            unavailable: Some(reason.clone()),
            ..Fake::default()
        };
        assert_eq!(
            switch(&os, None),
            Ok(SwitchState::Unavailable(reason.clone()))
        );
        assert_eq!(set(&os, true, || Ok(())), Err(reason.reason));
        assert_eq!(restore(&os, true), Ok(()));
        assert_eq!(os.writes.get(), 0);
    }
}
