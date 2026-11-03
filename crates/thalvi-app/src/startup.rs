//! Whether Thalvi starts when the user logs in: the OS registration behind the Settings switch.

use std::fmt;

use tauri::{AppHandle, Runtime};
use tauri_plugin_autostart::ManagerExt;

/// The argument the login entry passes, so that starting at login stays in the tray.
pub const LOGIN_ARG: &str = "--login";

/// The current user's startup entry.
pub trait Registration {
    /// # Errors
    /// Why the OS could not be asked.
    fn is_enabled(&self) -> Result<bool, String>;
    /// # Errors
    /// Why the entry could not be added.
    fn enable(&self) -> Result<(), String>;
    /// # Errors
    /// Why the entry could not be removed.
    fn disable(&self) -> Result<(), String>;
}

/// The plugin's entry in the user's XDG autostart folder or Windows `Run` key.
impl<R: Runtime> Registration for AppHandle<R> {
    fn is_enabled(&self) -> Result<bool, String> {
        self.autolaunch()
            .is_enabled()
            .map_err(|error| error.to_string())
    }

    fn enable(&self) -> Result<(), String> {
        self.autolaunch()
            .enable()
            .map_err(|error| error.to_string())
    }

    fn disable(&self) -> Result<(), String> {
        self.autolaunch()
            .disable()
            .map_err(|error| error.to_string())
    }
}

/// Why the switch could not be read or changed; the text is what the user sees.
#[derive(Debug, PartialEq, Eq)]
pub enum StartupError {
    Read(String),
    Change {
        on: bool,
        reason: String,
    },
    /// The OS took the change but still reports the old state, for example because the user
    /// turned Thalvi off in the system's startup list.
    NotKept {
        on: bool,
    },
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(reason) => write!(f, "Thalvi could not check its startup entry: {reason}"),
            Self::Change { on: true, reason } => {
                write!(f, "Thalvi could not be set to start at login: {reason}")
            }
            Self::Change { on: false, reason } => {
                write!(
                    f,
                    "Thalvi could not be stopped from starting at login: {reason}"
                )
            }
            Self::NotKept { on: true } => write!(
                f,
                "Thalvi added its startup entry, but the system reports it as off. Check the system's list of startup apps."
            ),
            Self::NotKept { on: false } => write!(
                f,
                "Thalvi removed its startup entry, but the system still reports it as on. Check the system's list of startup apps."
            ),
        }
    }
}

/// Whether the OS will start Thalvi at login.
///
/// # Errors
/// [`StartupError::Read`] when the OS cannot be asked.
pub fn state(registration: &impl Registration) -> Result<bool, StartupError> {
    registration.is_enabled().map_err(StartupError::Read)
}

/// Adds or removes the startup entry, then checks that the OS holds it as asked.
///
/// # Errors
/// [`StartupError::Change`] when the OS refuses, [`StartupError::NotKept`] when it does not keep the
/// change; the entry is then as the OS reports it, which [`state`] tells.
pub fn set(registration: &impl Registration, on: bool) -> Result<(), StartupError> {
    if on {
        registration.enable()
    } else {
        registration.disable()
    }
    .map_err(|reason| StartupError::Change { on, reason })?;
    if state(registration)? == on {
        Ok(())
    } else {
        Err(StartupError::NotKept { on })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// An OS whose startup entry the test controls.
    #[derive(Default)]
    struct Fake {
        entry: Cell<bool>,
        refuse: Option<&'static str>,
        /// Reports the entry as off however it is set.
        ignores_changes: bool,
        unreadable: bool,
    }

    impl Registration for Fake {
        fn is_enabled(&self) -> Result<bool, String> {
            if self.unreadable {
                return Err("the registry is locked".to_owned());
            }
            Ok(self.entry.get())
        }

        fn enable(&self) -> Result<(), String> {
            if let Some(reason) = self.refuse {
                return Err(reason.to_owned());
            }
            self.entry.set(!self.ignores_changes);
            Ok(())
        }

        fn disable(&self) -> Result<(), String> {
            if let Some(reason) = self.refuse {
                return Err(reason.to_owned());
            }
            self.entry.set(self.ignores_changes);
            Ok(())
        }
    }

    #[test]
    fn the_entry_is_off_until_enabled_and_each_change_is_read_back() {
        let os = Fake::default();
        assert_eq!(state(&os), Ok(false));
        assert_eq!(set(&os, true), Ok(()));
        assert_eq!(state(&os), Ok(true), "a later read, such as on relaunch");
        assert_eq!(set(&os, false), Ok(()));
        assert_eq!(state(&os), Ok(false));
    }

    #[test]
    fn a_refusal_names_the_change_and_leaves_the_entry_as_it_was() {
        let os = Fake {
            refuse: Some("permission denied"),
            ..Fake::default()
        };
        assert_eq!(
            set(&os, true).unwrap_err().to_string(),
            "Thalvi could not be set to start at login: permission denied"
        );
        assert_eq!(state(&os), Ok(false));

        os.entry.set(true);
        assert_eq!(
            set(&os, false).unwrap_err().to_string(),
            "Thalvi could not be stopped from starting at login: permission denied"
        );
        assert_eq!(state(&os), Ok(true));
    }

    #[test]
    fn a_change_the_os_does_not_keep_is_an_error_not_a_success() {
        let os = Fake {
            ignores_changes: true,
            ..Fake::default()
        };
        assert_eq!(set(&os, true), Err(StartupError::NotKept { on: true }));
        assert_eq!(state(&os), Ok(false));

        os.entry.set(true);
        assert_eq!(set(&os, false), Err(StartupError::NotKept { on: false }));
        assert_eq!(state(&os), Ok(true));
    }

    #[test]
    fn an_unreadable_entry_is_reported_on_load_and_after_a_change() {
        let os = Fake {
            unreadable: true,
            ..Fake::default()
        };
        let reason = "the registry is locked".to_owned();
        assert_eq!(state(&os), Err(StartupError::Read(reason.clone())));
        assert_eq!(set(&os, true), Err(StartupError::Read(reason)));
    }
}
