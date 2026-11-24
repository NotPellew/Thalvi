//! Whether Thalvi starts when the user logs in: the OS registration behind the Settings switch.

use std::fmt;

use tauri::{AppHandle, Runtime};
use tauri_plugin_autostart::ManagerExt;

use crate::switch::{SwitchState, Unavailable};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "windows")]
use windows as os;

/// The name of the login entry, which the system's startup list shows.
pub const APP_NAME: &str = "Thalvi";

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
    /// Whether the user turned Thalvi off in the system's own startup list, which an entry written
    /// by [`Registration::enable`] would silently override.
    fn turned_off_by_user(&self) -> bool;
    /// Why this session would never run the entry, when it would not.
    fn unavailable(&self) -> Option<Unavailable>;
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

    fn turned_off_by_user(&self) -> bool {
        os::turned_off_by_user()
    }

    fn unavailable(&self) -> Option<Unavailable> {
        os::unavailable()
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
    /// The OS took the change but the choice could not be written to Thalvi's config.
    Remember {
        reason: String,
    },
    /// The user turned Thalvi off in the system's startup list, so Thalvi left the entry alone.
    TurnedOff,
    /// Nothing in this session would run an entry, so none was written.
    Unavailable(String),
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
            Self::Remember { reason } => write!(
                f,
                "Thalvi changed its startup entry but could not save that choice, so it may be undone when Thalvi next starts: {reason}"
            ),
            Self::TurnedOff => write!(
                f,
                "Thalvi is turned off in the system's list of startup apps. Turn it on there, or switch it off and on here."
            ),
            Self::Unavailable(reason) => f.write_str(reason),
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

/// What the Settings switch shows: the OS's state, or why this session offers no switch.
///
/// # Errors
/// [`StartupError::Read`] when the OS cannot be asked.
pub fn switch(
    registration: &impl Registration,
    restore_problem: Option<String>,
) -> Result<SwitchState, StartupError> {
    if let Some(unavailable) = registration.unavailable() {
        return Ok(SwitchState::Unavailable(unavailable));
    }
    Ok(SwitchState::available(
        state(registration)?,
        restore_problem,
    ))
}

/// Adds or removes the startup entry, checks that the OS holds it as asked, then has `remember`
/// save the choice as Thalvi's login preference. A change the OS did not take is not remembered.
///
/// # Errors
/// [`StartupError::Unavailable`] when the session would never run the entry,
/// [`StartupError::Change`] when the OS refuses, [`StartupError::NotKept`] when it does not keep the
/// change; the entry is then as the OS reports it, which [`state`] tells. [`StartupError::Remember`]
/// when the entry changed but `remember` failed.
pub fn set(
    registration: &impl Registration,
    on: bool,
    remember: impl FnOnce() -> Result<(), String>,
) -> Result<(), StartupError> {
    if let Some(unavailable) = registration.unavailable() {
        return Err(StartupError::Unavailable(unavailable.reason));
    }
    if on {
        registration.enable()
    } else {
        registration.disable()
    }
    .map_err(|reason| StartupError::Change { on, reason })?;
    if state(registration)? != on {
        return Err(StartupError::NotKept { on });
    }
    remember().map_err(|reason| StartupError::Remember { reason })
}

/// Re-adds the startup entry on start when the user's preference is on: a Windows upgrade removes
/// it, and an `AppImage` that moved needs its path refreshed. Does nothing when the preference is
/// off, or when the session would never run the entry; the switch then says why.
///
/// # Errors
/// [`StartupError::TurnedOff`] when the user turned Thalvi off in the system's startup list,
/// otherwise as [`set`].
pub fn restore(registration: &impl Registration, preferred: bool) -> Result<(), StartupError> {
    if !preferred || registration.unavailable().is_some() {
        return Ok(());
    }
    if registration.turned_off_by_user() {
        return Err(StartupError::TurnedOff);
    }
    set(registration, true, || Ok(()))
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
        turned_off_in_system: bool,
        unavailable: Option<Unavailable>,
        enables: Cell<u32>,
        disables: Cell<u32>,
    }

    impl Registration for Fake {
        fn is_enabled(&self) -> Result<bool, String> {
            if self.unreadable {
                return Err("the registry is locked".to_owned());
            }
            Ok(self.entry.get())
        }

        fn enable(&self) -> Result<(), String> {
            self.enables.set(self.enables.get() + 1);
            if let Some(reason) = self.refuse {
                return Err(reason.to_owned());
            }
            self.entry.set(!self.ignores_changes);
            Ok(())
        }

        fn disable(&self) -> Result<(), String> {
            self.disables.set(self.disables.get() + 1);
            if let Some(reason) = self.refuse {
                return Err(reason.to_owned());
            }
            self.entry.set(self.ignores_changes);
            Ok(())
        }

        fn turned_off_by_user(&self) -> bool {
            self.turned_off_in_system
        }

        fn unavailable(&self) -> Option<Unavailable> {
            self.unavailable.clone()
        }
    }

    #[test]
    fn the_entry_is_off_until_enabled_and_each_change_is_read_back() {
        let os = Fake::default();
        assert_eq!(state(&os), Ok(false));
        assert_eq!(set(&os, true, || Ok(())), Ok(()));
        assert_eq!(state(&os), Ok(true), "a later read, such as on relaunch");
        assert_eq!(set(&os, false, || Ok(())), Ok(()));
        assert_eq!(state(&os), Ok(false));
    }

    #[test]
    fn a_refusal_names_the_change_and_leaves_the_entry_as_it_was() {
        let os = Fake {
            refuse: Some("permission denied"),
            ..Fake::default()
        };
        assert_eq!(
            set(&os, true, || Ok(())).unwrap_err().to_string(),
            "Thalvi could not be set to start at login: permission denied"
        );
        assert_eq!(state(&os), Ok(false));

        os.entry.set(true);
        assert_eq!(
            set(&os, false, || Ok(())).unwrap_err().to_string(),
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
        assert_eq!(
            set(&os, true, || Ok(())),
            Err(StartupError::NotKept { on: true })
        );
        assert_eq!(state(&os), Ok(false));

        os.entry.set(true);
        assert_eq!(
            set(&os, false, || Ok(())),
            Err(StartupError::NotKept { on: false })
        );
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
        assert_eq!(set(&os, true, || Ok(())), Err(StartupError::Read(reason)));
    }

    #[test]
    fn a_change_is_remembered_only_when_the_os_keeps_it() {
        let remembered = Cell::new(0);
        let remember = || {
            remembered.set(remembered.get() + 1);
            Ok(())
        };
        let os = Fake::default();
        assert_eq!(set(&os, true, remember), Ok(()));
        assert_eq!(remembered.get(), 1);

        for os in [
            Fake {
                refuse: Some("permission denied"),
                ..Fake::default()
            },
            Fake {
                ignores_changes: true,
                ..Fake::default()
            },
            Fake {
                unreadable: true,
                ..Fake::default()
            },
        ] {
            assert!(set(&os, true, remember).is_err());
        }
        assert_eq!(
            remembered.get(),
            1,
            "a change the OS did not take is not remembered"
        );

        let error = set(&Fake::default(), true, || Err("disk full".to_owned())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Thalvi changed its startup entry but could not save that choice, so it may be undone when Thalvi next starts: disk full"
        );
    }

    #[test]
    fn a_wanted_entry_is_put_back_on_start_and_an_unwanted_one_is_left_alone() {
        let missing = Fake::default();
        assert_eq!(restore(&missing, true), Ok(()));
        assert_eq!(state(&missing), Ok(true));

        let moved = Fake::default();
        moved.entry.set(true);
        assert_eq!(restore(&moved, true), Ok(()));
        assert_eq!(
            moved.enables.get(),
            1,
            "the entry is rewritten with the current path"
        );

        for entry in [false, true] {
            let untouched = Fake::default();
            untouched.entry.set(entry);
            assert_eq!(restore(&untouched, false), Ok(()));
            assert_eq!(
                (
                    untouched.entry.get(),
                    untouched.enables.get(),
                    untouched.disables.get()
                ),
                (entry, 0, 0)
            );
        }
    }

    #[test]
    fn a_restore_that_fails_says_why_and_never_overrides_the_systems_startup_list() {
        let refused = Fake {
            refuse: Some("permission denied"),
            ..Fake::default()
        };
        assert_eq!(
            restore(&refused, true).unwrap_err().to_string(),
            "Thalvi could not be set to start at login: permission denied"
        );

        let ignored = Fake {
            ignores_changes: true,
            ..Fake::default()
        };
        assert_eq!(
            restore(&ignored, true),
            Err(StartupError::NotKept { on: true })
        );

        let off_in_system = Fake {
            turned_off_in_system: true,
            ..Fake::default()
        };
        assert_eq!(restore(&off_in_system, true), Err(StartupError::TurnedOff));
        assert_eq!(off_in_system.enables.get(), 0);
        assert_eq!(restore(&off_in_system, false), Ok(()));
    }

    #[test]
    fn a_session_that_runs_no_entry_gets_a_reason_instead_of_a_switch_or_an_entry() {
        let reason = Unavailable {
            reason: "Nothing runs it.".to_owned(),
            line: Some("exec-once = thalvi --login".to_owned()),
        };
        let os = Fake {
            unavailable: Some(reason.clone()),
            ..Fake::default()
        };
        assert_eq!(switch(&os, None), Ok(SwitchState::Unavailable(reason)));
        assert_eq!(
            set(&os, true, || Ok(())),
            Err(StartupError::Unavailable("Nothing runs it.".to_owned()))
        );
        assert_eq!(restore(&os, true), Ok(()));
        assert_eq!((os.enables.get(), os.disables.get()), (0, 0));
    }

    #[test]
    fn the_switch_shows_the_os_state_and_a_restore_problem_only_while_off() {
        let os = Fake::default();
        let problem = Some("Thalvi could not be set to start at login: busy".to_owned());
        assert_eq!(
            switch(&os, problem.clone()),
            Ok(SwitchState::Available {
                on: false,
                problem: problem.clone()
            })
        );
        os.entry.set(true);
        assert_eq!(
            switch(&os, problem),
            Ok(SwitchState::Available {
                on: true,
                problem: None
            })
        );
    }
}
