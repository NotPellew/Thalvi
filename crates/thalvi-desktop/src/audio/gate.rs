//! The switch that makes the capture callback drop microphone audio.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Shared on/off switch for microphone input, readable without locks on the capture thread.
///
/// Every change bumps an epoch, so the callback also discards the chunk and partial frame that
/// were in flight when input came back on.
#[derive(Clone, Debug)]
pub struct InputGate(Arc<AtomicU64>);

impl Default for InputGate {
    fn default() -> Self {
        Self(Arc::new(AtomicU64::new(1)))
    }
}

impl InputGate {
    /// Turns input on or off. Audio captured before this call is never delivered after it.
    pub fn set_enabled(&self, enabled: bool) {
        self.0
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                Some((((state >> 1) + 1) << 1) | u64::from(enabled))
            })
            .unwrap_or_else(|state| state);
    }

    /// Whether input is currently on.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.state().0
    }

    pub(crate) fn state(&self) -> (bool, u64) {
        let state = self.0.load(Ordering::SeqCst);
        (state & 1 == 1, state >> 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_change_moves_the_epoch_and_keeps_the_flag() {
        let gate = InputGate::default();
        let (enabled, start) = gate.state();
        assert!(enabled);
        let mut last = start;
        for target in [false, false, true, true] {
            gate.set_enabled(target);
            let (enabled, epoch) = gate.state();
            assert_eq!(enabled, target);
            assert!(epoch > last, "epoch must move on every set");
            last = epoch;
        }
    }

    #[test]
    fn clones_share_one_switch() {
        let gate = InputGate::default();
        gate.clone().set_enabled(false);
        assert!(!gate.is_enabled());
    }
}
