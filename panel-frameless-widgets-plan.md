# Panel: frameless dashboard widgets + StateChip → value strip

Two changes to `metor-panel`, requested 2026-08-30:

1. Dashboard widgets can drop their background and outline (P&ID / schematic
   dashboards want widgets to read as bare readouts on the canvas).
2. StateChip's bespoke rendering is deleted; the chip becomes a bare
   `ComponentValueStrip` (boxes preset) carrying a per-strip state table
   (value → label + accent colour). No label, no Monitor chrome — revised
   2026-08-30 after review: the first cut hosted it on `Monitor`, which looks
   nothing like a value strip.

## A. Frameless widgets

- `DashboardWidget` gains `frame: bool`, `#[serde(default = "default_true")]`.
  Old layouts deserialize with `frame: true`; no layout version bump (additive
  field with default).
- `render_widget` (`dashboard/interaction.rs`): apply `.border_1() .border_color()
  .rounded() .bg(bg_primary)` only when `widget.frame`. In edit mode a frameless
  widget still paints a faint border (`border_primary.opacity(0.35)`) so it stays
  findable/selectable; never a background.
- Edit surface: edit-mode right-click already opens the widget inspector via the
  blocker. `open_widget_inspector` stops dispatching `InspectEntity` and instead
  builds the same reflect rows itself (`reflect::rows_for_any_entity`) plus a
  dynamic `BoolRow` "Frame" that toggles `widget.frame`, opened through the
  `open_inspector` global callback.
- Leaf views stop painting their own `bg(theme.bg_primary)` root fill — both the
  dashboard container (when framed) and the tile pane content
  (`tiles/pane.rs` "pane-content") already paint bg_primary behind them, so the
  leaf fill is redundant today and is what would defeat framelessness.
  Files: monitor.rs, meter.rs, gauge.rs, attitude.rs, traffic_light.rs,
  annunciator.rs, component_text.rs, sequence_control.rs. Inner chrome
  (annunciator tiles, gauge arcs, etc.) is untouched.
- Python preset API (`metor_config`) does not grow a `frame` knob in this pass —
  follow-up if wanted.

## B. StateChip → value strip

Kept surfaces (no on-disk or Python breakage):
- widget kind `"state_chip"`, tile serialization key `"state_chip"`,
  `StateChipConfig` / `StateEntryConfig` JSON shapes, the Python
  `StateChip`/`State` builders, the add-widget wizards.

Changes:
- `StripStyle` gains `states: Option<StateTable>` (entries + `unknown_label`).
  `ComponentValueStrip::render` maps each cell's raw numeric value through the
  table (±0.5 tolerance, as today): match → label + accent chip styling
  (dim(accent, 0.18) bg, accent border), no match → `unknown_label` or the raw
  number. Precedence: pending edit > stale tint > state accent.
- `views/state_chip.rs` becomes a slim host: it owns a `ComponentValueStrip`
  in `StripStyle::boxes()` plus the editable state table
  (`states: Vec<Entity<StateEntry>>`, `unknown_label`), and renders nothing but
  the centered strip — no label, no background. The binding (`component_id`)
  stays inspector-editable and rebinding keeps the table. Monitor is reverted
  to exactly its pre-change shape.
- `StateChipConfig.element != 0` no longer has a bespoke code path: the binding
  is rewritten to the expression tier (`=comp[el]`, or `=(body)[el]` when the
  binding is already an expression) at construction, so element selection rides
  the standard `=` path. `element` serializes back as 0 with the expression as
  the component text (round-trips through `binding_text`); `label` is carried
  only for tab titles and the config round trip.
- match-tolerance unit tests move alongside the table-matching code in
  `value_strip.rs`.
