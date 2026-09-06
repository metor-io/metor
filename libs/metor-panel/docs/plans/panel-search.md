# Panel search and filtering

Status (2026-08-29): WP1 and WP3 landed — `src/query.rs`,
`src/views/filter_bar.rs`, the `ColumnBrowser` toolbar slot, and the bar
toggle on the palette / right-click menu / Cmd-F. WP2, WP4, WP5 are open.

Every exploration panel today either has no filter (component table,
sequences, alarms) or a bespoke one (browser's saved glob filters, logs'
level chips + exact source). This plan gives each a search affordance built
from one shared query model and one shared bar, so the glob syntax operators
already know from the browser works everywhere a component name appears.

## Shared foundation (WP1)

**`src/query.rs` — one `Query` type.** Parses the bar text into:

- free-text terms, and
- `field:value` atoms (`source:imu`, `level:warn`, `state:running`,
  `severity:critical`). Unknown fields are treated as free text.

Matching a name against a free-text term picks the mode from the term itself:
a term containing `*` or `?` is an anchored glob (move `glob_to_regex` out of
`component_browser/mod.rs` here); anything else is a case-insensitive
substring on dotted names and nucleo fuzzy on prose (log messages). That
keeps `*.health` doing what it does in the browser without making operators
type wildcards for `imu` to find `cube_sat.imu.temp`. One regex/matcher per
query, compiled once per edit, not per row.

Both fuzzy scorers in tree (`inspector/completion.rs` and
`connections/picker.rs::fuzzy_scores`) collapse into this module.

**`src/views/filter_bar.rs` — the in-pane search strip.** A header row that
wraps `inspector::rows::text_field::TextField` (the same widget the
connection picker embeds) with: a search icon, placeholder, live filtering
on every keystroke, `✕` to clear, Esc to clear-then-blur, and a trailing
`shown / total` count. It also owns the chip vocabulary logs already draws by
hand (`level_chip`, the source pill, Follow): a `chip(label, active,
on_click)` helper so sequences and alarms get identical chrome. Clicking a
chip toggles its atom into the query text, so chips and typed atoms are one
state, not two.

Focus: a `ToggleFilterBar` gpui action bound to Cmd-F on the pane's root
`key_context` (predicated bindings need one — see the gpui context-predicate
note) that focuses the bar of the focused pane. Keystrokes route through
`TextField::handle_key_down` exactly as the picker does.

Persistence: query text lands in each `*PanelConfig` as a `#[serde(default)]`
`String`, so a saved layout reopens filtered. No layout-version bump unless
the serial tests pin the shape.

## Component table (WP2)

The clearest win: it lists every component with no way to narrow. Add the
bar above the table; `ComponentTableDelegate` keeps `metas` intact and
filters into a `visible: Vec<usize>` rebuilt when the query or `vtable_gen`
changes. Sorting sorts `visible`, so the existing `sort_column` arms move
one indirection. Match on the full dotted name only; value matching would
have to re-format every row per keystroke.

## Component browser and data table (WP3)

The browser already has *saved* filters (label = glob, shown as synthetic
roots). What it lacks is the transient one — type, see the tree shrink,
move on. Give `ColumnBrowser` an optional toolbar slot
(`ColumnBrowserDelegate::render_toolbar`) and have `ComponentBrowserDelegate`
put the bar there. A live query becomes a transient `FilterEntry` built with
the existing `prune_to_matches`, and the selection root switches to it while
text is present (`SelectionRoot::Filter` already models this). Enter on a
non-empty query promotes it to a saved filter via `add_filter` — which is the
current "Add filter…" flow with a first-class entry point. The detail column
filters its `detail_components` by the same query.

`DataTable` rides the same toolbar slot: group and instance names filter
through `Query`; the grid underneath is untouched.

## Logs (WP4)

Closest to done. Keep the level floor and source filter but fold them into
`Query` atoms (`level:`, `source:`) and add free text over `message`, `span`
and `key=value` fields. `LogDelegate::refresh` already keys on
`(pushed, floor, source)`; the key becomes `(pushed, query)`. A full scan of
the bounded history ring per edit is fine. Source cells keep click-to-filter,
now by appending `source:<name>`. Follow stays orthogonal.

## Sequences and alarms (WP5)

Both panels render hand-built rows from a global store with header count
chips and a Channels/History (Active/History/Shelved) tab — the same shape
as logs, and where `filter_bar` pays for itself. In each, the bar sits under
the chips and the row builders take a `&Query`:

- **Sequence panel**: channel rows match on channel name (glob/substring)
  and loaded sequence name, with `state:running|failed|…` atoms; the chips
  toggle those atoms. History rows match on channel and label.
- **Sequence grid**: same query, non-matching tiles are hidden rather than
  dimmed so a filtered grid stays a compact monitor. The bar is collapsed
  until Cmd-F or a saved query, since the grid's point is density.
- **Alarm panel**: pending/shelved/history rows match on the def's name and
  its target component name, plus `detail`; `severity:` atoms map to the
  existing severity chips.

## Left alone

Plots, dashboards, 3D, traffic lights, value strips and monitors bind through
pickers whose `ExpressionRow` already searches via `metor_expr::complete`;
the inspector/palette is already fuzzy. Nothing to add there.

## Order and size

WP1 first (it's what makes the rest small), then WP2 → WP4 → WP5 → WP3 —
the table and logs are the highest-traffic panels; the browser is last
because the toolbar slot touches the generic `ColumnBrowser`. Each WP is a
self-contained commit; WP1 and WP2 together are a day, the rest a day or
two more.
