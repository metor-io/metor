# Fast fades, tab movement, and live split gaps

Status: core implementation complete, 2026-09-06. Scope: overlay fades, short
tab position transitions, live split previews, and tab/window updates on existing GPUI 0.2.1.
Dependencies are unchanged. Optional polish and manual visual validation remain.

Make opening palettes, dismissing menus, and changing tabs feel continuous
without delaying commands or obscuring telemetry. Start with short, bounded
transitions and one shared motion policy.

“Live updating for tab and window updates” is interpreted to include live tab
metadata and palette entries, immediate layout feedback during manipulation,
and short position transitions for tab chrome. Tab layout commits immediately;
surviving headers move to their new positions. Native window movement and resizing take effect immediately. The dragged
header follows the pointer directly while neighbouring headers animate. Palette/menu zoom is outside this scope.

## Research and compatibility

**Zed:** its UI animation helpers define 50/150/300 ms duration tiers. The
entrance helper uses 150 ms with an ease-out quint curve, optionally combining
opacity and positional motion. The locally inspected `workspace/toast_layer.rs`
uses that helper for toast entry. Adopt its short duration and shared policy
for metor-panel's opacity transitions. This is evidence about the helper and
toast, not a claim about Zed's command palette behavior.
[Zed animation source](https://github.com/zed-industries/zed/blob/main/crates/ui/src/styles/animation.rs).
Local Zed reference: `87cf32a6245f4a6b99805e28f0845d0e2e06b519` at
`/Users/sphw/code/os/zed`.

**gpui-component:** its motion design separates product timing from retained
transition mechanics. Stable IDs preserve state; changing targets resumes
from the sampled value; presence keeps exiting content mounted; reduced motion
snaps values to their targets. Its drag guidance keeps movement attached to the
pointer. These are useful patterns for metor-panel's own small helper, without
adopting an entire component toolkit. This source describes a component
library's current design, not the behavior of every application using it.
[Motion design](https://github.com/longbridge/gpui-component/blob/main/docs/STYLING-AND-MOTION.md).

**Native GPUI:** the installed 0.2.1 source has `AnimationExt::with_animation`,
easing functions, element-keyed start times, and frame requests that stop at
completion. It has no completion callback in that wrapper. Reusing an animation
ID does not automatically restart its clock when the application changes a
target. Use the wrapper for simple entrances; use explicit retained state for
interruptible transitions and exit lifecycle. The upstream example also shows
newer APIs, so examples must be checked against the selected dependency.
[GPUI animation example](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/animation.rs).

**Dependency decision:** `Cargo.toml` currently uses `gpui = "*"` for normal
and test dependencies; the workspace lockfile resolves 0.2.1. That version
already supports the opacity and frame scheduling needed here. Keep the current
dependency resolution. A GPUI upgrade or transform-support fork is not required
for this plan.

## Proposed motion

These timings are starting values for metor-panel, to tune in the running app.
Use ease-out cubic for entry and linear opacity for exits. Avoid bounce,
overshoot, staggered rows, and repeating decorative effects.

| Interaction | Treatment | Duration |
| --- | --- | --- |
| Centered command palette opens | Opacity 0 → 1 at its final size and position | 120 ms |
| Centered palette closes | Opacity → 0 | 80 ms |
| Right-click inspector opens | Small opacity entrance at its final anchor | 70 ms |
| Right-click inspector closes | Opacity → 0, retaining its last placement | 90 ms |
| Active tab changes | Update highlight and content immediately | 0 ms |
| Tab inserted | New header appears opaque; existing neighbours move to their new positions | 120 ms |
| Tab removed or reordered | Remove closed header immediately; surviving headers move to their new positions | 120 ms |
| Tab dragging | Floating header follows pointer; source closes and neighbours preview the insertion slot | 0 / 120 ms |
| Split edge hovered while dragging | Open destination gap; surrounding panes resize into place | 120 ms |
| Split committed; manual resizing | Commit previewed geometry; follow resize pointer immediately | 0 ms |
| Transient chord menu | Fade in and out at its final position | 100 / 80 ms |
| Inspector page navigation | Optional short content fade, keeping search and frame stable | 70 ms |

Keep hover, pressed states, search results, plot samples, alarm transitions,
crosshairs, and keyboard selection immediate. Avoid crossfading whole plots or
3D views during tab switches. Let the OS own native window open/close behavior;
window contents must be ready and live when displayed.

## Phase 1: verify overlay fading on the current dependency

Build a small example exercising opacity over nested controls, text, clipping,
shadows, and a preview image. Verify frame completion and noninteractive exit
rendering on GPUI 0.2.1. Keep the panel at its final bounds throughout the fade,
with its normal input behavior available immediately on entry.

Check that fading the overlay also fades its nested content and shadow cleanly.
Confirm that the implementation does not resize plot/3D surfaces or require
per-frame image capture. The main integration work is retaining a dismissed
visual while disabling its input, not adding new rendering primitives.

## Phase 2: shared timing and overlay presence

Add `src/motion.rs` with duration tokens, pure elapsed-time sampling, and a small
retained transition holding start value, target, start time, and duration.
Retarget from the currently sampled value. Use wall-clock elapsed time rather
than fixed per-frame increments; bypass animation entirely at zero duration.
Request frames only while a transition is active, from its owning view. Avoid
a process-wide animation timer or invalidating every window.

Add a persisted motion preference to `PanelConfig` with a facet default so old
files load: `Reduced` and `Full` (shown as “Standard motion”). Reduced mode
applies state immediately. Changing the preference settles active transitions
immediately. Automatic system preference detection is deferred; this version
provides an explicit persisted override in the command palette. Colors remain
in `theme.rs`.

Introduce `Entering → Open → Exiting → Closed` presence for overlays. In
`src/inspector/mod.rs`, dismissal currently sets `dismissed` and rendering
immediately returns an empty element. `AppRoot::render` also drops dismissed
entities, and `toggle_palette` bypasses dismissal by clearing the slot. Route
these paths through one lifecycle instead.

On logical dismissal, run the chosen action exactly once, release focus when
appropriate, and remove the overlay from input routing. Retain only the visual
until exit completes. Child buttons, text input, scrolling, drag handlers,
occlusion, and accessibility focus must all become inactive; removing just the
outer `.occlude()` is insufficient. Prove a subtree input-suppression mechanism
in Phase 1 or render a dedicated passive exit representation.

Have completion explicitly notify the owner, which removes the matching
overlay by entity ID/generation. Cleanup must never refocus the root over a
newly opened palette or delete its replacement. Reopening reverses a transition
when retaining the same overlay; replacing it creates a new generation and
retires the old visual without interfering with new input. Bound retained exits
so repeated right clicks cannot accumulate a stack of fading menus.

Apply policy by `InspectorMode`: centered gets the palette entrance, anchored
gets menu fades, and inline inspectors keep their host's layout contract.
Audit connection picker's nested inspectors and shift-hover previews separately;
they must not inherit full-window modality or delayed dismissal accidentally.
Preserve immediate type-to-search and Enter/Escape behavior throughout entry.

## Phase 3: live tab and window state

`Pane::add_item`, `remove_item`, `activate_item`, and resize handlers already
notify their own entities. Start by reproducing stale behavior; don't add a
polling loop to cover missing dependencies. The current pane/group events carry
inspection and structural requests, not a complete metadata change stream.

Use GPUI entity-read dependencies for tab membership, order, active tab and
title invalidation. Compare a metadata snapshot during palette rendering before
rebuilding rows, so telemetry updates cannot rebuild the entries unnecessarily.
Window registration and root release refresh windows; no polling loop or new
per-item subscription layer is needed.

`ItemRegistry::root_rows` builds a snapshot when the palette opens. Make the
palette's dynamic root sources refresh on relevant changes, preserving query,
page stack, scroll and selected item identity. Existing row `query_revision`
logic is not a substitute for refreshing that root snapshot. Bind sources to
the palette's owning window, so a focus change elsewhere cannot redirect them.
Use stable entity/command keys rather than labels, which can change or repeat.

Keep `workspace::panel_windows` authoritative. Add a lifecycle signal around
window registration and actual release so consumers refresh when windows open
or close; don't create another strong-reference registry that keeps windows
alive. Check tear-out, close-last-tab, restored layouts, and cross-window moves.

Coalesce relevant changes into one refresh per frame. Reconcile before applying
visual transitions so disabling animations still gives fully live behavior.

## Phase 4: tab chrome movement

Replace index-based tab element IDs such as `("tab", ix)` with item identity
scoped to the pane. Resolve click, close, drag/drop and tear-out mutations by
identity, since captured indices can become stale during live updates.

Commit tab mutations and layout immediately. Retain each surviving tab's previous
position in GPUI element state and ease it toward its new layout position over
120 ms. A custom element offsets child prepaint, moving both painting and pointer
hitboxes together. Retarget an interrupted transition from its displayed
position. Tabs remain fully opaque and selection changes immediately. Newly
inserted headers appear at their final positions; removed tabs release immediately.

Measure positions relative to the tab rail so scrolling and pane movement do not
become animations. Snap when the rail size, viewport or orientation changes, and
during resizing or reduced motion. Neighbours may animate during tab dragging;
the floating header always follows the pointer directly. Support horizontal and vertical
bars. Coalesce frame requests per rail and stop at the exact target. No motion
state enters workspace/layout serialization.

Drag previews temporarily reorder the rendered headers without mutating the
saved item order. Lifting a tab immediately closes its source slot; hovering
a different slot or another pane opens an insertion gap. Determine targets from
committed slot centres rather than moving hitboxes to avoid oscillation. Cancel
or leave a destination to discard its preview; release commits by item identity.
The floating header shares normal tab geometry, captures the inherited font
style, and preserves the measured source size.

Cross-window tear-out transfers the existing item once and immediately renders
it in the destination. A tab closed during a drag cannot be resurrected by a
stale drop payload.

## Live split gaps

Hovering a content edge opens a gap using a temporary clone of the committed
split tree. Placeholder panes contain no items and do not enter the pane registry,
palette, subscriptions, or serialized layout. The normal split-insertion algorithm
chooses the destination; short transitions grow its flex weight. When moving the
last tab, the source slot shrinks as the new gap opens. Divider widths follow
slot presence so settled preview bounds match the actual drop.

Capture content drop regions before reflow and route drops through those fixed
regions. This prevents edge oscillation and preserves center drops while an old
gap closes. Show five detached miniature-layout cards around the original pane
center, with space between them and no shared backplate. Highlight the matching
card for the pointer's current destination. Cards use the app's 1 px outlines,
4 px outer / 3 px inner corners, and theme surfaces; the active destination has
a `control_active` border with the same faint accent fill used by connection
actions. The whole content region remains
droppable: outer quarters split, the center adds a tab, and each guide card is
also an explicit target. The guides stay fixed while content reflows, so users
can merge tabs without chasing a moving tab strip. Ordinary tab-strip insertion
still works; there is no tab-hover freeze or pinned-strip treatment.

Changing edges retargets from current progress; cancellation restores
the committed layout. Release transfers the existing item exactly once and clears
preview state. Preview layout uses actual pane sizing, so manual validation must
include frame time and render-target allocation in busy plot/3D workspaces.

## Phase 5: polish and validation

Reuse presence for the transient chord menu after inspector behavior is solid.
Then try the optional page-content fade. Evaluate connection-picker entrance
for consistency. Keep each extra only if it improves continuity in ordinary use.

Automated checks should exercise behavior rather than encode visual constants:

- Pure transition sampling at different frame intervals, interruption/reversal,
  zero duration and reduced motion.
- Dismiss during entry, immediate command execution, repeated Escape, outside
  clicks during exit, and replacement before completion. Old cleanup cannot
  steal focus or remove the new overlay.
- Palette refresh after rename/add/remove/move, including duplicate labels and
  another window changing focus. Preserve query and stable selection.
- Tab reorder/close during drag, last-tab removal, tear-out, and window release.
- Frame requests cease after settling, and overlay subscriptions are released.

For implementation, run focused tests followed by `cargo test -p metor-panel`
and `cargo clippy -p metor-panel`; run
`cargo check -p metor-panel --all-targets` as well. Check platform builds through
available CI and record any untested platform.

Manually exercise a disconnected/idle workspace and a busy plot/3D workspace
at 60 and 120 Hz where available. Record frame time and render-target allocations
before/after. Budgets are approximately 16.7 and 8.3 ms per frame respectively;
motion should not introduce an extra visible frame of input latency or ongoing
idle repainting. Check fractional DPI, long palettes, anchored menus at window
edges, drag-and-drop between monitors, reduced motion, and resizing while an
overlay is open.

Delivery order: opacity example → shared timing/presence → palette/menu fades
→ live metadata and window notifications → tab movement → optional polish.

## Implementation notes

Core changes include shared fades, passive inspector exit rendering, transient
menu presence, live palette entries bound to their owning window, stable tab
identities, tab position transitions, and live split gaps. Inspector exits with arbitrary preview
views/accessories or custom rows that have not opted into passive rendering are
immediate. Inline inspectors and passive hover previews remain immediate.

Page-content and connection-picker polish, automatic OS reduced-motion detection,
and manual frame-time/DPI/multi-monitor checks remain follow-up work. Automated
checks cover interrupted transitions, passive exit input and focus ownership,
duplicate-title selection during live refresh, moving tab hitboxes, and stale
drag payloads.

Validation: `cargo test -p metor-panel --offline` passes all 447 tests, plus
binary and documentation targets. `cargo clippy -p metor-panel --all-targets
--offline` passes with existing warnings. Drag integration tests cover horizontal
and vertical bars, source-slot collapse, unchanged floating-header dimensions,
cancellation, cross-pane insertion without duplicate transfer, all four split
edges, last-tab source collapse, split retargeting, and center drops during
reflow. Split tests compare preview bounds with the committed result and verify
that guides stay fixed during reflow and whole-pane drops agree with their
highlighted destination. Manual visual
and performance validation has not been performed.
