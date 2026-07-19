//! Event-flag annotations: vertical flags in a gutter across the top of a
//! time-series plot, one per clustered burst of [`PlotEvent`]s.
//!
//! The plot resolves its enabled [`EventSource`](crate::plot_events::EventSource)s
//! each frame, queries the events in the visible window, and clusters those
//! that would paint within a few pixels of each other into one flag. The line
//! and pennant are painted from a snapshot ([`ClusterPaint`]); the full events
//! stay on the plot ([`EventCluster`]) for hit-testing and the popovers.

use gpui::{Bounds, Hsla, PathBuilder, Pixels, SharedString, TextRun, Window, point, px};
use metor_proto::types::Timestamp;

use super::{PlotView, plot_area};
use crate::plot_events::PlotEvent;
use crate::theme::Theme;

/// Height of the flag gutter band inside the top of the plot area. Flags hang
/// from here, and hit-testing for hover/pin is confined to it so the trace
/// hover/pan/zoom below stays untouched.
pub(super) const GUTTER_H: f32 = 14.0;

/// Events whose screen x lands within this many pixels of a cluster's anchor
/// merge into it, so a dense burst reads as one flag rather than a smear.
const CLUSTER_PX: f32 = 6.0;

/// Pointer-to-flag distance treated as a hit in the gutter.
pub(super) const FLAG_HIT_PX: f32 = 8.0;

/// Exact-equality check for two colors — [`Hsla`] has no `PartialEq`, and a
/// cluster only needs to know whether every event shares one resolved color.
fn hsla_eq(a: Hsla, b: Hsla) -> bool {
    a.h == b.h && a.s == b.s && a.l == b.l && a.a == b.a
}

/// One flag: a screen x plus the events that merged into it, ascending by
/// time. Rebuilt each frame (screen x depends on the view), so consumers that
/// outlive a frame key on [`Self::ts`] rather than a list index.
pub(super) struct EventCluster {
    pub x: Pixels,
    pub events: Vec<PlotEvent>,
}

impl EventCluster {
    /// Time of the first (earliest) event — the cluster's stable identity
    /// across frames.
    pub fn ts(&self) -> Timestamp {
        self.events.first().map(|e| e.ts).unwrap_or(Timestamp(0))
    }

    /// Flag color: the shared event color, or `text_secondary` when the
    /// cluster mixes colors (a burst spanning severities or sources).
    pub fn color(&self, theme: &Theme) -> Hsla {
        let Some(first) = self.events.first() else {
            return theme.text_secondary;
        };
        if self.events.iter().all(|e| hsla_eq(e.color, first.color)) {
            first.color
        } else {
            theme.text_secondary
        }
    }
}

/// Merge screen-positioned events into flags left-to-right, folding any within
/// [`CLUSTER_PX`] of the running cluster's anchor x into it. `positioned` must
/// be ascending by x (guaranteed by sorting the source events by time, since
/// the x transform is monotonic).
pub(super) fn cluster_events(positioned: Vec<(Pixels, PlotEvent)>) -> Vec<EventCluster> {
    let mut clusters: Vec<EventCluster> = Vec::new();
    for (x, event) in positioned {
        match clusters.last_mut() {
            Some(last) if f32::from(x - last.x).abs() <= CLUSTER_PX => {
                last.events.push(event);
            }
            _ => clusters.push(EventCluster {
                x,
                events: vec![event],
            }),
        }
    }
    clusters
}

/// One flag reduced to what paint needs, kept free of any `App`/entity access
/// so the paint closure stays a plain `Fn` (mirrors `CursorPaint`).
pub(super) struct ClusterPaint {
    pub x: Pixels,
    pub color: Hsla,
    /// Number of events merged; a count label is drawn when this exceeds one.
    pub count: usize,
}

/// Paint the flag gutter: a faint full-height rule at each cluster plus a
/// pennant hanging from the top gutter, with a count label on multi-event
/// clusters. Transposes the alarm-limit-line rule and the axis-triangle fill.
pub(super) fn paint_event_flags(
    outer_bounds: Bounds<Pixels>,
    view: &PlotView,
    clusters: &[ClusterPaint],
    window: &mut Window,
    cx: &mut gpui::App,
) {
    if clusters.is_empty() {
        return;
    }
    let pb = plot_area(outer_bounds, view.axis_count());
    let label_font_size = px(super::LABEL_FONT_SIZE);
    let font = window.text_style().font();
    let top = pb.origin.y;
    let bottom = pb.origin.y + pb.size.height;
    let right = pb.origin.x + pb.size.width;

    for cluster in clusters {
        let x = cluster.x;
        if x < pb.origin.x || x > right {
            continue;
        }
        // Full-height rule at reduced alpha so traces stay legible under it.
        let rule_color = Hsla {
            a: 0.5,
            ..cluster.color
        };
        let mut rule = PathBuilder::stroke(px(1.0));
        rule.move_to(point(x, top));
        rule.line_to(point(x, bottom));
        if let Ok(path) = rule.build() {
            window.paint_path(path, rule_color);
        }

        // Pennant hanging from the top of the plot: a right-pointing flag with
        // its staff on the rule.
        let mut flag = PathBuilder::fill();
        flag.move_to(point(x, top));
        flag.line_to(point(x + px(8.0), top + px(4.0)));
        flag.line_to(point(x, top + px(8.0)));
        flag.line_to(point(x, top));
        if let Ok(path) = flag.build() {
            window.paint_path(path, cluster.color);
        }

        if cluster.count > 1 {
            let text = format!("{}", cluster.count);
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color: cluster.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(text),
                label_font_size,
                &[run],
                None,
            );
            // Just right of the pennant, pulled inside the plot's right edge.
            let label_x = (x + px(10.0)).min(right - shaped.width - px(2.0));
            let origin = point(label_x, top + px(GUTTER_H) - label_font_size - px(1.0));
            let _ = shaped.paint(origin, label_font_size, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plot_events::EventDetail;

    fn event(ts: i64) -> PlotEvent {
        PlotEvent {
            ts: Timestamp(ts),
            color: Hsla::default(),
            label: SharedString::new_static("e"),
            detail: EventDetail::Raw(0),
        }
    }

    fn colored(ts: i64, color: Hsla) -> PlotEvent {
        PlotEvent {
            color,
            ..event(ts)
        }
    }

    #[test]
    fn clusters_merge_within_threshold() {
        // 0,3,5 all within 6px of the anchor at 0; 20,22 form a second flag.
        let positioned = vec![
            (px(0.0), event(0)),
            (px(3.0), event(1)),
            (px(5.0), event(2)),
            (px(20.0), event(3)),
            (px(22.0), event(4)),
        ];
        let clusters = cluster_events(positioned);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].events.len(), 3);
        assert_eq!(clusters[1].events.len(), 2);
        // Anchor x is the first event's x, and ts is the earliest.
        assert_eq!(clusters[0].x, px(0.0));
        assert_eq!(clusters[0].ts(), Timestamp(0));
        assert_eq!(clusters[1].x, px(20.0));
    }

    #[test]
    fn cluster_stays_bounded_to_threshold() {
        // A drifting chain must not snowball: 0,5,10 — 10 is 10px from the
        // anchor at 0, so it starts a new cluster despite being 5px from 5.
        let positioned = vec![(px(0.0), event(0)), (px(5.0), event(1)), (px(10.0), event(2))];
        let clusters = cluster_events(positioned);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].events.len(), 2);
        assert_eq!(clusters[1].events.len(), 1);
    }

    #[test]
    fn cluster_color_is_shared_or_secondary() {
        let theme = crate::theme::DARK.clone();
        let red = Hsla {
            h: 0.0,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        };
        let blue = Hsla {
            h: 0.6,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        };
        let uniform = cluster_events(vec![(px(0.0), colored(0, red)), (px(2.0), colored(1, red))]);
        assert!(hsla_eq(uniform[0].color(&theme), red));

        let mixed = cluster_events(vec![(px(0.0), colored(0, red)), (px(2.0), colored(1, blue))]);
        assert!(hsla_eq(mixed[0].color(&theme), theme.text_secondary));
    }
}
