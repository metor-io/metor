# Component Browser — filter simplification plan

## What the feature does today

Right-click in the component browser → "Add filter…" → type `label = pattern`
(glob with `*` and `?`). A synthetic top-level node named `label` appears as a
sibling of the real top-level prefixes. Drilling into it shows the real tree
pruned to branches containing a match: matching nodes carry their full
subtree, non-matching intermediates with one surviving child collapse so a
deep single-chain renders as one row. Filters disappear when the browser is
rerooted, and rerooting *into* a filter is rejected. Right-clicking a
synthetic filter root offers "Remove filter".

## What's making it complicated

1. **Two compression implementations.** `component_tree::compress_subtree`
   already collapses single-child non-component chains. `prune_to_matches`
   in `mod.rs` re-implements the same collapse while pruning.
2. **Selection lookup is a fallback chain.** `relative_selection`
   (`mod.rs:128–172`) walks the real tree first; if that returns nothing it
   re-tries the first segment as a filter label and walks a fresh synth
   tree. The code's own comment notes this is fragile when a filter label
   coincides with a real segment.
3. **Synth trees are rebuilt on every render.** `relative_selection` calls
   `synth_filter_node` each time, which recurses the entire real tree —
   even though filters only change on add/remove and the tree only changes
   on `vtable_gen`.
4. **`is_filter_node` is a shape heuristic.** Filter-ness is detected via
   `find_filter(seg).is_some() && segment == full_name`. Used in
   `set_root_override` and indirectly in context-row construction.
5. **`ComponentPreview` over-caches callbacks.** It stores `click`,
   `right_click`, and `plot_all` Arcs alongside the strip, even though
   `render_detail` overwrites the strip behavior every frame anyway.
6. **Special-cases scattered across delegate methods.** `root_items`,
   `column_label`, `detail_label`, `set_root_override`, `remove_filter`,
   `relative_selection` each branch on filter logic. There's no single
   place that answers "which tree am I navigating right now?"

## Plan

### Step 1 — model the selection root explicitly

Replace

```rust
selection_path: SmallVec<[SharedString; 8]>,
root_override_path: Option<SmallVec<[SharedString; 8]>>,
```

with

```rust
enum SelectionRoot { Real, Filter(SharedString) }

struct Selection {
    root: SelectionRoot,
    path: SmallVec<[SharedString; 8]>,           // segments below the root
    root_override: Option<SmallVec<[SharedString; 8]>>,  // only valid when root == Real
}
```

Now `relative_selection` is one uniform walk: pick the root tree (real
override-rooted node, or the cached synth root for the named filter),
then resolve `path` against it. The dual-phase fallback and its
fragility comment go away.

Touched: most delegate methods — they already read these two fields, so
the change is mostly mechanical (replace field accesses with selection
helpers).

### Step 2 — cache synth trees on `FilterEntry`, unify compression

Extend `FilterEntry`:

```rust
struct FilterEntry {
    label: SharedString,
    regex: regex::Regex,
    synth: Arc<ComponentNode>,   // mirrors real tree, pruned to matches
}
```

Refresh `synth` in two places:
- `add_filter`: build once after compiling the regex.
- `spawn_watcher`: after assigning `self.tree`, recompute every filter's
  `synth`.

In `prune_to_matches`, drop the in-place collapse branch (the
`pruned_children.len() == 1` case). Build the raw pruned tree and run
the existing `component_tree::compress_subtree` on it. One compression
implementation, used in both contexts.

### Step 3 — drop the `is_filter_node` heuristic

With Step 1, callers always know whether they're inside a filter
because `SelectionRoot::Filter(_)` carries that information.

- `set_root_override`: reject when `selection.root == Filter(_)`. The
  ancestors-walk no longer needs the per-node check.
- `build_context_rows`: show "Remove filter" when
  `column_ix == 0 && selection.root == Filter(label)`.

`is_filter_node` and `find_filter` get deleted.

### Step 4 — slim `ComponentPreview`

Keep only `{ component_id, full_name, strip }`. Build the click /
right-click / plot-all callbacks inline in `reconcile_previews` for the
strip constructor and in `render_detail` / `render_preview_entry` for
the row. Each callback is just a `db.clone()` + `id` capture; the
caching wasn't buying anything since `render_detail` rewrites
`StripBehavior` on every paint anyway.

### Step 5 — minor cleanups

- Inline `effective_root`/`override_depth` use into the new
  `Selection` helpers.
- Reorder `mod.rs` so the public delegate impl sits above the
  filter-tree internals (right now they're interleaved).

## Out of scope

- The "Add filter…" inspector UX (NavRow → DefaultActionRow parsing
  `label=pattern`). It's awkward but it's UX, not internal complexity.
  Worth a separate pass once an input row that can capture two fields
  exists.
- Filter persistence across sessions.
- Glob → regex rules (`*` and `?` only). Fine as-is.

## Files touched

- `libs/metor-panel/src/views/component_browser/mod.rs` — all steps.
- `libs/metor-panel/src/views/component_browser/component_tree.rs` —
  no changes; `compress_subtree` is already `pub(crate)`.

## Suggested commit shape

One commit per step keeps each diff reviewable; Step 1 is the largest
because it reshapes the selection state. Steps 2–5 are independent and
could land in any order after Step 1.
