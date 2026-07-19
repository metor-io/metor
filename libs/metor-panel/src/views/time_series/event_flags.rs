//! Event-flag annotations: vertical flags in a gutter across the top of a
//! time-series plot, one per clustered burst of [`PlotEvent`]s.
//!
//! The plot resolves its enabled [`EventSource`](crate::plot_events::EventSource)s
//! each frame, queries the events in the visible window, and clusters those
//! that would paint within a few pixels of each other into one flag. The line
//! and label chip are painted from a snapshot ([`ClusterPaint`]); the full
//! events stay on the plot ([`EventCluster`]) for hit-testing and the
//! popovers. Chips truncate to the gap before the next flag and collapse to a
//! bare nub when even a couple of characters won't fit.

use gpui::{Bounds, Hsla, PathBuilder, Pixels, SharedString, TextRun, Window, point, px};
use metor_proto::types::Timestamp;

use super::{PlotView, plot_area};
use crate::plot_events::PlotEvent;
use crate::theme::Theme;

/// Height of the flag gutter band inside the top of the plot area. Chips hang
/// from here, and hit-testing for hover/pin is confined to it so the trace
/// hover/pan/zoom below stays untouched.
pub(super) const GUTTER_H: f32 = 16.0;

/// Chip geometry: height, corner radius, horizontal text padding, label font
/// size, and the per-character width estimate used both to truncate a label
/// to its gap and to hit-test without a text system.
const CHIP_H: f32 = 13.0;
const CHIP_RADIUS: f32 = 3.0;
const CHIP_PAD_X: f32 = 4.0;
const CHIP_FONT_SIZE: f32 = 9.0;
const CHIP_CHAR_W: f32 = 5.6;
/// Widest a chip may grow regardless of the gap to its neighbor.
const CHIP_MAX_W: f32 = 120.0;
/// A gap too small for this many label characters paints a bare nub instead.
const CHIP_MIN_CHARS: usize = 3;

/// Events whose screen x lands within this many pixels of a cluster's anchor
/// merge into it, so a dense burst reads as one flag rather than a smear.
const CLUSTER_PX: f32 = 6.0;

/// Pointer-to-flag distance treated as a hit left of the rule; to the right
/// the whole estimated chip width hits.
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

    /// The chip text: the first event's short name, plus a `+N` tail when
    /// more merged in.
    pub fn chip_label(&self) -> String {
        let first = self.events.first().map(|e| e.short.as_ref()).unwrap_or("");
        if self.events.len() > 1 {
            format!("{first} +{}", self.events.len() - 1)
        } else {
            first.to_string()
        }
    }

    /// Estimated on-screen chip width for `label`, char-count based so
    /// hit-testing needs no text system (the readout sizing idiom).
    pub fn chip_width(label_chars: usize) -> Pixels {
        px((label_chars as f32 * CHIP_CHAR_W + CHIP_PAD_X * 2.0).min(CHIP_MAX_W))
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
    /// The chip text ([`EventCluster::chip_label`]), truncated at paint time
    /// to the gap before the next flag.
    pub label: String,
}

/// Truncate `label` to what fits in `avail` pixels of chip, ellipsized. An
/// answer under [`CHIP_MIN_CHARS`] characters means "no room" (`None`), which
/// paints a bare nub instead.
fn fit_label(label: &str, avail: Pixels) -> Option<String> {
    let budget = (f32::from(avail).min(CHIP_MAX_W) - CHIP_PAD_X * 2.0).max(0.0);
    let fit = (budget / CHIP_CHAR_W) as usize;
    let chars = label.chars().count();
    if chars <= fit {
        return Some(label.to_string());
    }
    if fit < CHIP_MIN_CHARS + 1 {
        return None;
    }
    let mut out: String = label.chars().take(fit - 1).collect();
    out.push('…');
    Some(out)
}

/// Paint the flag gutter: a faint full-height rule at each cluster plus a
/// rounded label chip at its top — tinted background, hairline border, the
/// event's short name in its color. Chips truncate to the gap before the
/// next flag (they never overlap) and collapse to a bare nub when even a few
/// characters won't fit.
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
    let chip_font_size = px(CHIP_FONT_SIZE);
    let font = window.text_style().font();
    let top = pb.origin.y;
    let bottom = pb.origin.y + pb.size.height;
    let right = pb.origin.x + pb.size.width;

    for (i, cluster) in clusters.iter().enumerate() {
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

        // The chip claims at most the gap to the next flag (so neighbors
        // never overlap) and the plot's right edge.
        let next_x = clusters
            .get(i + 1)
            .map(|c| c.x)
            .unwrap_or(right)
            .min(right);
        let avail = next_x - x - px(2.0);
        let chip_y = top + px(1.0);
        match fit_label(&cluster.label, avail) {
            Some(text) if !text.is_empty() => {
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
                    chip_font_size,
                    &[run],
                    None,
                );
                let chip = Bounds::new(
                    point(x, chip_y),
                    gpui::size(shaped.width + px(CHIP_PAD_X * 2.0), px(CHIP_H)),
                );
                window.paint_quad(gpui::quad(
                    chip,
                    px(CHIP_RADIUS),
                    Hsla {
                        a: 0.16,
                        ..cluster.color
                    },
                    px(1.0),
                    Hsla {
                        a: 0.55,
                        ..cluster.color
                    },
                    gpui::BorderStyle::Solid,
                ));
                let origin = point(
                    x + px(CHIP_PAD_X),
                    chip_y + (px(CHIP_H) - chip_font_size) / 2.0,
                );
                let _ = shaped.paint(origin, chip_font_size, window, cx);
            }
            _ => {
                // No room for text: a bare rounded nub marks the flag.
                let nub = Bounds::new(point(x, chip_y), gpui::size(px(7.0), px(7.0)));
                window.paint_quad(gpui::quad(
                    nub,
                    px(2.0),
                    cluster.color,
                    px(0.0),
                    cluster.color,
                    gpui::BorderStyle::Solid,
                ));
            }
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
            short: SharedString::new_static("e"),
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
    fn chip_labels_fit_or_collapse() {
        // Plenty of room: untouched.
        assert_eq!(fit_label("WARN nav", px(120.0)).as_deref(), Some("WARN nav"));
        // Tight: ellipsized to the budget.
        let tight = fit_label("WARN navigation", px(60.0)).unwrap();
        assert!(tight.ends_with('…') && tight.chars().count() < 15);
        // No room: nub.
        assert_eq!(fit_label("WARN nav", px(12.0)), None);
    }

    #[test]
    fn chip_label_counts_extras() {
        let clusters = cluster_events(vec![
            (px(0.0), event(0)),
            (px(2.0), event(1)),
            (px(4.0), event(2)),
        ]);
        assert_eq!(clusters[0].chip_label(), "e +2");
        let single = cluster_events(vec![(px(0.0), event(0))]);
        assert_eq!(single[0].chip_label(), "e");
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
