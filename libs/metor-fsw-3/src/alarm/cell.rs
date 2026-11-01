//! The alarm state machine: raise, debounce, recover.

use metor_proto::types::Timestamp;

use super::AlarmSpec;

/// A raise or a clear of the occurrence raised at `raised_at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Raised { raised_at: Timestamp },
    Cleared { raised_at: Timestamp },
}

/// One alarm's debounce counts and active occurrence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlarmCell {
    breach: u16,
    clear: u16,
    /// When the active occurrence was raised.
    active: Option<Timestamp>,
}

impl AlarmCell {
    /// Advances the cell by one cycle and returns any edge. `breached` is
    /// `None` when the condition cannot tell, which keeps the counts.
    pub fn step(
        &mut self,
        spec: &AlarmSpec,
        breached: Option<bool>,
        now: Timestamp,
    ) -> Option<Edge> {
        match breached? {
            true => self.breach(spec, now),
            false => self.recover(spec),
        }
    }

    /// Resets the counts and clears an active alarm.
    pub fn disarm(&mut self) -> Option<Edge> {
        self.breach = 0;
        self.clear = 0;
        let raised_at = self.active.take()?;
        Some(Edge::Cleared { raised_at })
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    fn breach(&mut self, spec: &AlarmSpec, now: Timestamp) -> Option<Edge> {
        self.clear = 0;
        if self.active.is_some() {
            return None;
        }
        self.breach = self.breach.saturating_add(1);
        if self.breach < spec.debounce.get() {
            return None;
        }
        self.breach = 0;
        self.active = Some(now);
        Some(Edge::Raised { raised_at: now })
    }

    fn recover(&mut self, spec: &AlarmSpec) -> Option<Edge> {
        self.breach = 0;
        let raised_at = self.active?;
        self.clear = self.clear.saturating_add(1);
        if self.clear < spec.debounce.get() {
            return None;
        }
        self.clear = 0;
        self.active = None;
        Some(Edge::Cleared { raised_at })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Option<bool> = Some(true);
    const C: Option<bool> = Some(false);
    const H: Option<bool> = None;
    const SPEC: AlarmSpec = AlarmSpec::warning("a").debounce(3);

    /// Steps `cell` through `levels` at times 1, 2, ..., and returns the edges.
    fn run(cell: &mut AlarmCell, spec: &AlarmSpec, levels: &[Option<bool>]) -> Vec<Option<Edge>> {
        levels
            .iter()
            .enumerate()
            .map(|(i, l)| cell.step(spec, *l, Timestamp(i as i64 + 1)))
            .collect()
    }

    fn raised(t: i64) -> Option<Edge> {
        Some(Edge::Raised {
            raised_at: Timestamp(t),
        })
    }

    fn cleared(t: i64) -> Option<Edge> {
        Some(Edge::Cleared {
            raised_at: Timestamp(t),
        })
    }

    #[test]
    fn raises_after_debounce_and_clears_after_debounce() {
        let mut cell = AlarmCell::default();
        let edges = run(&mut cell, &SPEC, &[B, B, B, C, C, C]);
        assert_eq!(edges, [None, None, raised(3), None, None, cleared(3)]);
        assert!(!cell.is_active());
    }

    #[test]
    fn breach_while_active_is_no_edge() {
        let mut cell = AlarmCell::default();
        let edges = run(&mut cell, &SPEC, &[B, B, B, B]);
        assert_eq!(edges[3], None);
    }

    #[test]
    fn re_raise_carries_a_new_raised_at() {
        let spec = AlarmSpec::warning("a");
        let mut cell = AlarmCell::default();
        let edges = run(&mut cell, &spec, &[B, C, B]);
        assert_eq!(edges, [raised(1), cleared(1), raised(3)]);
    }

    #[test]
    fn hold_keeps_partial_breach_count() {
        let mut cell = AlarmCell::default();
        let edges = run(&mut cell, &SPEC, &[B, B, H, B]);
        assert_eq!(edges[3], raised(4));
    }

    #[test]
    fn hold_keeps_partial_clear_count_and_active() {
        let mut cell = AlarmCell::default();
        let edges = run(&mut cell, &SPEC, &[B, B, B, C, C, H, C]);
        assert_eq!(edges[5], None);
        assert_eq!(edges[6], cleared(3));
    }

    #[test]
    fn breach_while_recovering_resets_clear_count() {
        let mut cell = AlarmCell::default();
        let edges = run(&mut cell, &SPEC, &[B, B, B, C, C, B, C, C]);
        assert_eq!(edges[7], None, "the clear count restarted");
        assert!(cell.is_active());
    }

    #[test]
    fn debounce_one_raises_on_first_breach() {
        let mut cell = AlarmCell::default();
        let edges = run(&mut cell, &AlarmSpec::critical("a"), &[B]);
        assert_eq!(edges, [raised(1)]);
    }

    #[test]
    fn disarm_while_active_clears() {
        let mut cell = AlarmCell::default();
        run(&mut cell, &SPEC, &[B, B, B]);
        assert_eq!(cell.disarm(), cleared(3));
        assert_eq!(cell, AlarmCell::default());
    }

    #[test]
    fn disarm_while_inactive_resets_counts() {
        let mut cell = AlarmCell::default();
        run(&mut cell, &SPEC, &[B, B]);
        assert_eq!(cell.disarm(), None);
        assert_eq!(cell, AlarmCell::default());
    }

    #[test]
    fn max_debounce_saturates() {
        let spec = AlarmSpec::warning("a").debounce(u16::MAX);
        let mut cell = AlarmCell::default();
        let edges = (0..u32::from(u16::MAX) + 2)
            .filter_map(|t| cell.step(&spec, B, Timestamp(t.into())))
            .collect::<Vec<_>>();
        assert_eq!(edges, [raised(i64::from(u16::MAX) - 1).unwrap()]);
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;
    use crate::alarm::Severity;

    const STEPS: usize = 8;

    fn any_spec() -> AlarmSpec {
        AlarmSpec {
            id: "a",
            severity: Severity::Warning,
            debounce: kani::any(),
        }
    }

    #[kani::proof]
    #[kani::unwind(9)]
    fn counts_stay_below_debounce() {
        let spec = any_spec();
        kani::assume(usize::from(spec.debounce.get()) <= STEPS);
        let mut cell = AlarmCell::default();
        for _ in 0..STEPS {
            cell.step(&spec, kani::any(), Timestamp(kani::any()));
            assert!(cell.breach < spec.debounce.get());
            assert!(cell.clear < spec.debounce.get());
        }
    }

    #[kani::proof]
    #[kani::unwind(9)]
    fn clear_follows_raise() {
        let spec = any_spec();
        let mut cell = AlarmCell::default();
        let mut last_raise: Option<Timestamp> = None;
        for _ in 0..STEPS {
            let was_active = cell.is_active();
            match cell.step(&spec, kani::any(), Timestamp(kani::any())) {
                Some(Edge::Raised { raised_at }) => {
                    assert!(!was_active);
                    last_raise = Some(raised_at);
                }
                Some(Edge::Cleared { raised_at }) => {
                    assert!(was_active);
                    assert_eq!(Some(raised_at), last_raise);
                    last_raise = None;
                }
                None => {}
            }
            assert_eq!(cell.is_active(), last_raise.is_some());
        }
    }
}
