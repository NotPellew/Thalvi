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
    fn a_later_configure_replaces_an_earlier_one() {
        assert_eq!(
            coalesce([Configure::Ready, Configure::Closed]),
            Some(Configure::Closed)
        );
        assert_eq!(
            coalesce([Configure::Closed, Configure::Ready]),
            Some(Configure::Ready)
        );
        assert_eq!(coalesce([]), None);
    }

    #[test]
    fn a_configure_that_arrives_before_the_retry_places_the_surface_on_that_retry() {
        let (seat, created) = show_step(Seat::None, [Configure::Ready]);
        assert_eq!(
            (seat, created),
            (
                Seat::Pending {
                    waited: Duration::ZERO
                },
                Step::Create
            ),
            "a configure left from a dropped surface is discarded"
        );
        let (_, waiting) = show_step(
            Seat::Pending {
                waited: Duration::from_millis(10),
            },
            None,
        );
        assert_eq!(waiting, Step::NotYet);
        let (seat, placed) = show_step(
            Seat::Pending {
                waited: Duration::from_millis(50),
            },
            [Configure::Ready],
        );
        assert_eq!(
            (seat, placed),
            (Seat::Placed, Step::Attach { gave_up: false }),
            "the configure that arrived for this surface places it"
        );
    }

    #[test]
    fn placement_follows_the_configure_the_seat_and_how_long_it_has_waited() {
        let within = Duration::from_millis(50);
        let past = CONFIGURE_WITHIN;
        let cases = [
            (
                "nothing queued yet leaves the surface unplaced",
                Seat::Pending { waited: within },
                None,
                Seat::Pending { waited: within },
                Step::NotYet,
            ),
            (
                "ready while still inside the wait attaches the frame",
                Seat::Pending { waited: within },
                Some(Configure::Ready),
                Seat::Placed,
                Step::Attach { gave_up: false },
            ),
            (
                "ready after the wait attaches without counting it as a failure",
                Seat::Pending { waited: past },
                Some(Configure::Ready),
                Seat::Placed,
                Step::Attach { gave_up: false },
            ),
            (
                "a close drops the surface",
                Seat::Pending { waited: within },
                Some(Configure::Closed),
                Seat::None,
                Step::Closed,
            ),
            (
                "a close of a placed surface drops it",
                Seat::Placed,
                Some(Configure::Closed),
                Seat::None,
                Step::Closed,
            ),
            (
                "no configure within the wait keeps the surface and reports the failure",
                Seat::Pending { waited: past },
                None,
                Seat::Placed,
                Step::Attach { gave_up: true },
            ),
            (
                "a placed surface is drawn on again",
                Seat::Placed,
                None,
                Seat::Placed,
                Step::Attach { gave_up: false },
            ),
            (
                "a show with no surface makes one",
                Seat::None,
                None,
                Seat::Pending {
                    waited: Duration::ZERO,
                },
                Step::Create,
            ),
            (
                "a show with no surface discards a leftover close",
                Seat::None,
                Some(Configure::Closed),
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
