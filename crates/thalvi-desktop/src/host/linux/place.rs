//! Whether a layer surface may be drawn on: the compositor's latest configure, the surface still
//! being placed, and how long that wait has run.

use std::time::Duration;

/// How long a new layer surface may wait for the compositor's first configure.
pub(super) const CONFIGURE_WITHIN: Duration = Duration::from_secs(1);

/// What the compositor said about the layer surface last made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Configure {
    Ready,
    Closed,
}

/// The latest configure still in force: a later one replaces an earlier one, so a `Ready` queued
/// ahead of a `Closed` cannot hide the closure.
pub(super) fn coalesce(queued: impl IntoIterator<Item = Configure>) -> Option<Configure> {
    queued.into_iter().last()
}

/// Where the one layer surface is between draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Seat {
    /// No surface. The next show makes one.
    None,
    /// A surface committed and not yet placed, waiting since the show that made it.
    Pending { waited: Duration },
    /// A surface the compositor placed, or one kept after the wait ran out.
    Placed,
}

/// What one show or present does with the surface, before any pixels are attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// Make a new surface. Configures from a dropped one were discarded on the way here.
    Create,
    /// Attach the frame. `gave_up` is a compositor that never configured within the wait.
    Attach { gave_up: bool },
    /// The compositor has not placed the surface yet.
    NotYet,
    /// The compositor closed the surface.
    Closed,
}

/// One show: the configures queued since the last one, and where the surface is.
///
/// A show that finds no surface discards them, because they belong to a surface already dropped,
/// and makes a new one. A show for a surface that exists keeps them, so a configure that arrived
/// after it was made still places it.
pub(super) fn show_step(seat: Seat, queued: impl IntoIterator<Item = Configure>) -> (Seat, Step) {
    // No surface means the queued configures belong to one already dropped.
    let configure = match seat {
        Seat::None => None,
        Seat::Pending { .. } | Seat::Placed => coalesce(queued),
    };
    step(seat, configure)
}

/// Decides the next step from the seat and the configure in force for it.
fn step(seat: Seat, configure: Option<Configure>) -> (Seat, Step) {
    if matches!(seat, Seat::None) {
        return (
            Seat::Pending {
                waited: Duration::ZERO,
            },
            Step::Create,
        );
    }
    if configure == Some(Configure::Closed) {
        return (Seat::None, Step::Closed);
    }
    let seat = if matches!(seat, Seat::Pending { .. }) && configure == Some(Configure::Ready) {
        Seat::Placed
    } else {
        seat
    };
    match seat {
        Seat::Placed => (Seat::Placed, Step::Attach { gave_up: false }),
        Seat::Pending { waited } if waited >= CONFIGURE_WITHIN => {
            (Seat::Placed, Step::Attach { gave_up: true })
        }
        Seat::Pending { .. } | Seat::None => (seat, Step::NotYet),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_follows_the_configure_the_seat_and_how_long_it_has_waited() {
        let within = Duration::from_millis(50);
        let past = CONFIGURE_WITHIN;
        let cases = [
            (
                "nothing queued yet leaves the surface unplaced",
                Seat::Pending { waited: within },
                vec![],
                Seat::Pending { waited: within },
                Step::NotYet,
            ),
            (
                "ready while still inside the wait attaches the frame",
                Seat::Pending { waited: within },
                vec![Configure::Ready],
                Seat::Placed,
                Step::Attach { gave_up: false },
            ),
            (
                "ready after the wait attaches without counting it as a failure",
                Seat::Pending { waited: past },
                vec![Configure::Ready],
                Seat::Placed,
                Step::Attach { gave_up: false },
            ),
            (
                "a close drops the surface",
                Seat::Pending { waited: within },
                vec![Configure::Closed],
                Seat::None,
                Step::Closed,
            ),
            (
                "a close of a placed surface drops it",
                Seat::Placed,
                vec![Configure::Closed],
                Seat::None,
                Step::Closed,
            ),
            (
                "no configure within the wait keeps the surface and reports the failure",
                Seat::Pending { waited: past },
                vec![],
                Seat::Placed,
                Step::Attach { gave_up: true },
            ),
            (
                "a placed surface is drawn on again",
                Seat::Placed,
                vec![],
                Seat::Placed,
                Step::Attach { gave_up: false },
            ),
            (
                "a show with no surface makes one",
                Seat::None,
                vec![],
                Seat::Pending {
                    waited: Duration::ZERO,
                },
                Step::Create,
            ),
            (
                "a show with no surface discards a leftover ready",
                Seat::None,
                vec![Configure::Ready],
                Seat::Pending {
                    waited: Duration::ZERO,
                },
                Step::Create,
            ),
            (
                "a later close replaces an earlier ready",
                Seat::Pending { waited: within },
                vec![Configure::Ready, Configure::Closed],
                Seat::None,
                Step::Closed,
            ),
            (
                "a later ready replaces an earlier close",
                Seat::Pending { waited: within },
                vec![Configure::Closed, Configure::Ready],
                Seat::Placed,
                Step::Attach { gave_up: false },
            ),
            (
                "a show with no surface discards a leftover close",
                Seat::None,
                vec![Configure::Closed],
                Seat::Pending {
                    waited: Duration::ZERO,
                },
                Step::Create,
            ),
        ];
        for (label, seat, configure, next, outcome) in cases {
            assert_eq!(show_step(seat, configure), (next, outcome), "{label}");
        }
    }
}
