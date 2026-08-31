//! Persisted execution-timeline state.
//!
//! The row list is never persisted: it is derived from whatever wiring the
//! target is broadcasting now, so saving it would just pin a stale topology.
//! What a layout does carry is the operator's edits to that derivation — which
//! kinds of row to show, and which instances to hide.

use std::sync::Arc;

use gpui::{App, Context};
use metor_db::DB;
use serde::{Deserialize, Serialize};

use super::ExecTimeline;
use crate::views::time_series::Override;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecTimelineConfig {
    pub label: String,
    /// [`TimeRangeBehavior`](crate::views::time_series::TimeRangeBehavior) as
    /// text; empty means follow the app-wide range.
    pub x_range: String,
    pub show_slots: bool,
    /// The coordinator's own lane. Its whole-cycle band paints behind every
    /// row regardless — this is only the dedicated row.
    pub show_coordinator_row: bool,
    /// Scope-trigger mode: pin the window to the newest cycle. Overrides
    /// `x_range` while on.
    pub trigger: bool,
    /// Instance names filtered out of the derived row list. A hidden row still
    /// contributes its duration to the prefix sum; only the lane is dropped.
    pub hidden_rows: Vec<String>,
}

impl Default for ExecTimelineConfig {
    fn default() -> Self {
        Self {
            label: String::new(),
            x_range: String::new(),
            show_slots: true,
            show_coordinator_row: true,
            trigger: false,
            hidden_rows: Vec::new(),
        }
    }
}

impl ExecTimeline {
    pub fn from_config(config: ExecTimelineConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let mut timeline = Self::new(db, cx);
        timeline.label = config.label.into();
        if let Ok(range) = config.x_range.parse() {
            timeline.x_range = Override::Custom(range);
        }
        timeline.show_slots = config.show_slots;
        timeline.show_coordinator_row = config.show_coordinator_row;
        timeline.trigger = config.trigger;
        timeline.hidden = config.hidden_rows.into_iter().map(Into::into).collect();
        timeline
    }

    pub fn to_config(&self, _cx: &App) -> ExecTimelineConfig {
        let mut hidden: Vec<String> = self.hidden.iter().map(ToString::to_string).collect();
        hidden.sort_unstable();
        ExecTimelineConfig {
            label: self.label.to_string(),
            x_range: self
                .x_range
                .as_custom()
                .map(ToString::to_string)
                .unwrap_or_default(),
            show_slots: self.show_slots,
            show_coordinator_row: self.show_coordinator_row,
            trigger: self.trigger,
            hidden_rows: hidden,
        }
    }
}
