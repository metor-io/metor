//! The ISA-18.1 sequence-A state machine, shared by every surface that shows a
//! condition an operator has to dismiss.
//!
//! Two callers with different authority fold the same rules: an annunciator
//! tile derives its condition locally and keeps the latch per view, while the
//! alarm store folds declared alarms whose acks are wire events. Writing the
//! transitions once — pure, with no gpui or DB reach — is what lets those two
//! agree on what "cleared but nobody looked" means.

use metor_proto::types::Timestamp;

/// The four states a latched condition can be in.
///
/// `ClearedUnacked` is the ringback state that makes latching worth having: the
/// condition went away on its own, and the tile stays lit until someone says
/// they saw it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TileState {
    #[default]
    Normal,
    AlarmUnacked,
    AlarmAcked,
    ClearedUnacked,
}

/// Edge tracker behind one latched condition.
///
/// `since` is the trip time, retained through a clear so first-out ordering
/// survives a condition that came and went.
#[derive(Clone, Copy, Debug, Default)]
pub struct Latch {
    active: bool,
    acked: bool,
    since: Option<Timestamp>,
}

impl Latch {
    /// Fold one condition sample, stamped `now`.
    ///
    /// A rising edge re-arms the latch: it takes a fresh `since` and drops any
    /// earlier acknowledgment, so an alarm that returns has to be dismissed
    /// again. A falling edge only forgets the trip once it has been acked.
    pub fn condition(&mut self, active: bool, now: Timestamp) {
        if active && !self.active {
            self.since = Some(now);
            self.acked = false;
        }
        self.active = active;
        if !active && self.acked {
            self.since = None;
        }
    }

    /// Record the operator's acknowledgment, retiring the latch outright when
    /// the condition has already cleared.
    pub fn ack(&mut self) {
        self.acked = true;
        if !self.active {
            self.since = None;
        }
    }

    /// When the current trip began, or `None` once nothing is pending. Feeds
    /// the first-out rule: the oldest pending trip is the one that caused the
    /// rest.
    pub fn since(&self) -> Option<Timestamp> {
        self.since
    }

    /// The state to render. With `latching` off the latch collapses to a plain
    /// live readout, which is what an unlatched annunciator wants.
    pub fn state(&self, latching: bool) -> TileState {
        if !latching {
            return match self.active {
                true => TileState::AlarmUnacked,
                false => TileState::Normal,
            };
        }
        match (self.active, self.acked, self.since.is_some()) {
            (true, false, _) => TileState::AlarmUnacked,
            (true, true, _) => TileState::AlarmAcked,
            (false, false, true) => TileState::ClearedUnacked,
            _ => TileState::Normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T1: Timestamp = Timestamp(1);
    const T2: Timestamp = Timestamp(2);
    const T3: Timestamp = Timestamp(3);

    #[test]
    fn a_rise_trips_and_stamps() {
        let mut latch = Latch::default();
        assert_eq!(latch.state(true), TileState::Normal);
        assert_eq!(latch.since(), None);

        latch.condition(true, T1);
        assert_eq!(latch.state(true), TileState::AlarmUnacked);
        assert_eq!(latch.since(), Some(T1));
    }

    #[test]
    fn a_fall_while_unacked_rings_back() {
        let mut latch = Latch::default();
        latch.condition(true, T1);
        latch.condition(false, T2);

        assert_eq!(latch.state(true), TileState::ClearedUnacked);
        assert_eq!(latch.since(), Some(T1));

        latch.ack();
        assert_eq!(latch.state(true), TileState::Normal);
        assert_eq!(latch.since(), None);
    }

    #[test]
    fn an_ack_while_active_leaves_the_alarm_up() {
        let mut latch = Latch::default();
        latch.condition(true, T1);
        latch.ack();
        assert_eq!(latch.state(true), TileState::AlarmAcked);
        assert_eq!(latch.since(), Some(T1));

        latch.condition(false, T2);
        assert_eq!(latch.state(true), TileState::Normal);
    }

    /// An acked alarm that trips again has to be dismissed again, and dates
    /// from the new trip rather than the old one.
    #[test]
    fn a_second_trip_rearms_the_latch() {
        let mut latch = Latch::default();
        latch.condition(true, T1);
        latch.ack();
        latch.condition(false, T2);
        latch.condition(true, T3);

        assert_eq!(latch.state(true), TileState::AlarmUnacked);
        assert_eq!(latch.since(), Some(T3));
    }

    /// Unlatched, the two ack-bearing states are unreachable — this is the
    /// pre-annunciator grid's behaviour, and both polarities feed the same
    /// pre-inverted condition.
    #[test]
    fn latching_off_collapses_to_two_states() {
        let mut latch = Latch::default();
        latch.condition(true, T1);
        assert_eq!(latch.state(false), TileState::AlarmUnacked);

        latch.condition(false, T2);
        assert_eq!(latch.state(false), TileState::Normal);

        latch.condition(true, T3);
        latch.ack();
        assert_eq!(latch.state(false), TileState::AlarmUnacked);
    }

    /// A healthy flag reads `false` when sick, so the caller inverts before
    /// folding; the latch itself never learns about polarity.
    #[test]
    fn an_inverted_condition_latches_the_same_way() {
        let healthy = [true, false, true];
        let mut latch = Latch::default();
        for (i, sample) in healthy.into_iter().enumerate() {
            latch.condition(!sample, Timestamp(i as i64));
        }
        assert_eq!(latch.state(true), TileState::ClearedUnacked);
        assert_eq!(latch.since(), Some(Timestamp(1)));
    }
}
