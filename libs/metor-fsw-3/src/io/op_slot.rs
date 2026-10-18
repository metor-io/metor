//! The state machine of one op slot in a system's queue.
//!
//! [`OpSlot::apply`] is the only transition. It returns the slot's next state and
//! the op, if any, the queue must write for it. The queue applies both, and
//! reports a write the SQ refused back as [`Event::Refused`].

use super::{IoStatus, RawHandle};

/// Where a slot's op is.
///
/// `cancel` on an op in flight is a cancel the SQ refused. The next pump
/// writes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum SlotState {
    #[default]
    Free,
    Waiting {
        opens: bool,
        cancel: bool,
    },
    /// Its `Pending` was dropped. The completion frees it, closing any handle
    /// the op opened first.
    Orphaned {
        opens: bool,
        cancel: bool,
    },
    /// A close of the handle an orphaned op opened, waiting for SQ room.
    CloseQueued(RawHandle),
    /// Closing the handle an orphaned op opened, under the op's own token.
    /// The close's completion frees it.
    Closing(RawHandle),
    Done {
        status: IoStatus,
        value: u32,
        opens: bool,
    },
}

/// One slot of a system's op table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct OpSlot {
    pub(super) generation: u16,
    pub(super) state: SlotState,
}

/// Something that happens to a slot. Each event but `Submitted`, `Pumped`,
/// and `Refused` carries the generation of the op it is about, and a slot
/// ignores an event for another generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Event {
    /// An op was written to the SQ under this slot.
    Submitted { generation: u16, opens: bool },
    /// The op's completion arrived.
    Completed {
        generation: u16,
        status: IoStatus,
        value: u32,
    },
    /// The op's `Pending` was dropped.
    Dropped { generation: u16 },
    /// The op's `Pending` asked to cancel it.
    Canceled { generation: u16 },
    /// The op's result was returned.
    Returned { generation: u16 },
    /// The queue pumped: a write the SQ refused may go out now.
    Pumped,
    /// The SQ refused a write this slot asked for.
    Refused(Write),
}

/// An op a slot asks the queue to write under its token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Write {
    Cancel,
    Close(RawHandle),
}

impl OpSlot {
    /// The slot after `event`, and the op to write for it.
    pub(super) fn apply(self, event: Event) -> (Self, Option<Write>) {
        let ours = |generation: u16| generation == self.generation;
        match event {
            Event::Submitted { generation, opens } => {
                let state = SlotState::Waiting {
                    opens,
                    cancel: false,
                };
                (Self { generation, state }, None)
            }
            Event::Completed {
                generation,
                status,
                value,
            } if ours(generation) => self.completed(status, value),
            Event::Dropped { generation } if ours(generation) => self.dropped(),
            Event::Canceled { generation } if ours(generation) && self.in_flight() => {
                (self.with_cancel(false), Some(Write::Cancel))
            }
            Event::Returned { generation } if ours(generation) && self.done() => {
                (self.with(SlotState::Free), None)
            }
            Event::Pumped => self.pumped(),
            Event::Refused(Write::Cancel) if self.in_flight() => (self.with_cancel(true), None),
            Event::Refused(Write::Close(handle)) => {
                (self.with(SlotState::CloseQueued(handle)), None)
            }
            _ => (self, None),
        }
    }

    /// The op's result, if it has finished as generation `generation`.
    pub(super) fn result(self, generation: u16) -> Option<(IoStatus, u32)> {
        match self.state {
            SlotState::Done { status, value, .. } if generation == self.generation => {
                Some((status, value))
            }
            _ => None,
        }
    }

    pub(super) fn is_free(self) -> bool {
        self.state == SlotState::Free
    }

    fn in_flight(self) -> bool {
        matches!(
            self.state,
            SlotState::Waiting { .. } | SlotState::Orphaned { .. }
        )
    }

    fn done(self) -> bool {
        matches!(self.state, SlotState::Done { .. })
    }

    fn with(self, state: SlotState) -> Self {
        Self { state, ..self }
    }

    /// The slot with its op's refused cancel set to `cancel`. Only an op in
    /// flight has one.
    fn with_cancel(self, cancel: bool) -> Self {
        match self.state {
            SlotState::Waiting { opens, .. } => self.with(SlotState::Waiting { opens, cancel }),
            SlotState::Orphaned { opens, .. } => self.with(SlotState::Orphaned { opens, cancel }),
            _ => self,
        }
    }

    fn completed(self, status: IoStatus, value: u32) -> (Self, Option<Write>) {
        let state = match self.state {
            SlotState::Waiting { opens, .. } => SlotState::Done {
                status,
                value,
                opens,
            },
            SlotState::Orphaned { opens: true, .. } => {
                return self.close_opened(status.result().map(|()| value));
            }
            done @ SlotState::Done { .. } => done,
            SlotState::Orphaned { .. }
            | SlotState::CloseQueued(_)
            | SlotState::Closing(_)
            | SlotState::Free => SlotState::Free,
        };
        (self.with(state), None)
    }

    fn dropped(self) -> (Self, Option<Write>) {
        match self.state {
            SlotState::Waiting { opens, cancel } => {
                (self.with(SlotState::Orphaned { opens, cancel }), None)
            }
            SlotState::Done {
                status,
                value,
                opens: true,
            } => self.close_opened(status.result().map(|()| value)),
            _ => (self.with(SlotState::Free), None),
        }
    }

    fn pumped(self) -> (Self, Option<Write>) {
        match self.state {
            SlotState::CloseQueued(handle) => (
                self.with(SlotState::Closing(handle)),
                Some(Write::Close(handle)),
            ),
            SlotState::Waiting { cancel: true, .. } | SlotState::Orphaned { cancel: true, .. } => {
                (self.with_cancel(false), Some(Write::Cancel))
            }
            _ => (self, None),
        }
    }

    /// Closes the handle an orphaned op opened. An op that failed opened
    /// nothing, so its slot is free.
    fn close_opened(self, opened: Result<u32, super::IoError>) -> (Self, Option<Write>) {
        match opened {
            Ok(raw) => {
                let handle = RawHandle::from_raw(raw);
                (
                    self.with(SlotState::Closing(handle)),
                    Some(Write::Close(handle)),
                )
            }
            Err(_) => (self.with(SlotState::Free), None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::IoError;
    use super::*;

    const GEN: u16 = 7;

    fn waiting(opens: bool) -> OpSlot {
        OpSlot::default()
            .apply(Event::Submitted {
                generation: GEN,
                opens,
            })
            .0
    }

    fn completed(status: IoStatus, value: u32) -> Event {
        Event::Completed {
            generation: GEN,
            status,
            value,
        }
    }

    #[test]
    fn test_op_completes_and_is_taken() {
        let slot = waiting(false).apply(completed(IoStatus::OK, 5)).0;
        assert_eq!(slot.result(GEN), Some((IoStatus::OK, 5)));
        assert_eq!(slot.result(GEN + 1), None, "another generation's result");
        let (slot, write) = slot.apply(Event::Returned { generation: GEN });
        assert!(slot.is_free() && write.is_none());
    }

    #[test]
    fn test_other_generation_is_ignored() {
        let slot = waiting(true);
        for event in [
            Event::Completed {
                generation: GEN + 1,
                status: IoStatus::OK,
                value: 1,
            },
            Event::Dropped {
                generation: GEN + 1,
            },
            Event::Canceled {
                generation: GEN + 1,
            },
            Event::Returned {
                generation: GEN + 1,
            },
        ] {
            assert_eq!(slot.apply(event), (slot, None), "{event:?}");
        }
    }

    #[test]
    fn test_dropped_open_closes_what_it_opened() {
        let orphan = waiting(true).apply(Event::Dropped { generation: GEN }).0;
        let orphaned = SlotState::Orphaned {
            opens: true,
            cancel: false,
        };
        assert_eq!(orphan.state, orphaned);
        let (closing, write) = orphan.apply(completed(IoStatus::OK, 42));
        let handle = RawHandle::from_raw(42);
        assert_eq!(write, Some(Write::Close(handle)));
        assert_eq!(closing.state, SlotState::Closing(handle));
        let (free, write) = closing.apply(completed(IoStatus::OK, 0));
        assert!(free.is_free() && write.is_none());
    }

    #[test]
    fn test_dropped_after_open_landed_closes_it() {
        let done = waiting(true).apply(completed(IoStatus::OK, 9)).0;
        let (closing, write) = done.apply(Event::Dropped { generation: GEN });
        assert_eq!(write, Some(Write::Close(RawHandle::from_raw(9))));
        assert!(matches!(closing.state, SlotState::Closing(_)));
    }

    #[test]
    fn test_failed_orphan_frees_its_slot() {
        let orphan = waiting(true).apply(Event::Dropped { generation: GEN }).0;
        let failed = IoStatus::err(IoError::Canceled);
        let (slot, write) = orphan.apply(completed(failed, 0));
        assert!(slot.is_free() && write.is_none());
        let (slot, _) = waiting(false).apply(Event::Dropped { generation: GEN });
        let (slot, write) = slot.apply(completed(IoStatus::OK, 3));
        assert!(slot.is_free() && write.is_none(), "nothing to close");
    }

    #[test]
    fn test_refused_close_is_resent_on_pump() {
        let orphan = waiting(true).apply(Event::Dropped { generation: GEN }).0;
        let (closing, write) = orphan.apply(completed(IoStatus::OK, 4));
        let Some(write) = write else {
            panic!("a close")
        };
        let held = closing.apply(Event::Refused(write)).0;
        let handle = RawHandle::from_raw(4);
        assert_eq!(held.state, SlotState::CloseQueued(handle));
        let (sent, write) = held.apply(Event::Pumped);
        assert_eq!(write, Some(Write::Close(handle)));
        assert_eq!(sent.apply(Event::Pumped), (sent, None), "sent once");
    }

    #[test]
    fn test_refused_cancel_waits_for_pump_while_in_flight() {
        let slot = waiting(false);
        let (asked, write) = slot.apply(Event::Canceled { generation: GEN });
        assert_eq!(write, Some(Write::Cancel));
        let held = asked.apply(Event::Refused(Write::Cancel)).0;
        let (resent, write) = held.apply(Event::Pumped);
        assert_eq!(write, Some(Write::Cancel));
        assert_eq!(resent.apply(Event::Pumped), (resent, None));
        let landed = held.apply(completed(IoStatus::OK, 0)).0;
        assert_eq!(
            landed.apply(Event::Pumped),
            (landed, None),
            "a finished op is not canceled"
        );
    }

    #[test]
    fn test_free_slot_ignores_everything_but_submit() {
        let free = OpSlot::default();
        assert_eq!(free.apply(Event::Pumped), (free, None));
        assert_eq!(free.apply(Event::Refused(Write::Cancel)), (free, None));
        assert_eq!(free.apply(Event::Returned { generation: 0 }), (free, None));
        assert_eq!(free.apply(Event::Canceled { generation: 0 }), (free, None));
    }
}

#[cfg(kani)]
mod proofs {
    use zerocopy::FromBytes;

    use super::*;

    fn any_status() -> IoStatus {
        let raw: u32 = kani::any();
        // PANIC Safety: any four bytes are an `IoStatus`.
        IoStatus::read_from_bytes(&raw.to_le_bytes()).unwrap()
    }

    fn any_state() -> SlotState {
        match kani::any::<u8>() % 6 {
            0 => SlotState::Free,
            1 => SlotState::Waiting {
                opens: kani::any(),
                cancel: kani::any(),
            },
            2 => SlotState::Orphaned {
                opens: kani::any(),
                cancel: kani::any(),
            },
            3 => SlotState::CloseQueued(RawHandle::from_raw(kani::any())),
            4 => SlotState::Closing(RawHandle::from_raw(kani::any())),
            _ => SlotState::Done {
                status: any_status(),
                value: kani::any(),
                opens: kani::any(),
            },
        }
    }

    fn any_slot() -> OpSlot {
        OpSlot {
            generation: kani::any(),
            state: any_state(),
        }
    }

    fn any_event() -> Event {
        let generation = kani::any();
        match kani::any::<u8>() % 8 {
            0 => Event::Submitted {
                generation,
                opens: kani::any(),
            },
            1 => Event::Completed {
                generation,
                status: any_status(),
                value: kani::any(),
            },
            2 => Event::Dropped { generation },
            3 => Event::Canceled { generation },
            4 => Event::Returned { generation },
            5 => Event::Pumped,
            6 => Event::Refused(Write::Cancel),
            _ => Event::Refused(Write::Close(RawHandle::from_raw(kani::any()))),
        }
    }

    /// No event but a submit changes the generation, and only a submit takes
    /// a free slot out of `Free`, so a stale token never reaches a new op.
    #[kani::proof]
    fn generation_changes_only_on_submit() {
        let (slot, event) = (any_slot(), any_event());
        let (next, _) = slot.apply(event);
        if !matches!(event, Event::Submitted { .. }) {
            assert_eq!(next.generation, slot.generation);
            if slot.is_free() && !matches!(event, Event::Refused(Write::Close(_))) {
                assert!(next.is_free());
            }
        }
    }

    /// A close is only ever written for a slot left closing that handle, and
    /// a cancel only for an op still in flight.
    #[kani::proof]
    fn writes_match_the_state_left_behind() {
        let (slot, event) = (any_slot(), any_event());
        match slot.apply(event) {
            (next, Some(Write::Close(handle))) => {
                assert_eq!(next.state, SlotState::Closing(handle));
            }
            (next, Some(Write::Cancel)) => assert!(matches!(
                next.state,
                SlotState::Waiting { cancel: false, .. }
                    | SlotState::Orphaned { cancel: false, .. }
            )),
            (_, None) => {}
        }
    }
}
