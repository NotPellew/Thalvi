//! What a Settings switch for an OS setting shows: the OS's state, or why Thalvi offers no switch.

use serde::Serialize;

/// Why a switch is not offered, and a line the user can copy to do it by hand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Unavailable {
    pub reason: String,
    pub line: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SwitchState {
    /// The OS state, read each time, and why Thalvi could not restore it on start.
    Available {
        on: bool,
        problem: Option<String>,
    },
    Unavailable(Unavailable),
}

impl SwitchState {
    /// The `problem` is shown only while the setting is off, since it explains why it is not on.
    #[must_use]
    pub fn available(on: bool, problem: Option<String>) -> Self {
        Self::Available {
            on,
            problem: problem.filter(|_| !on),
        }
    }
}
