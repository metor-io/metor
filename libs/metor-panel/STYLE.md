# metor-panel Style Guide

## Comments

- **Doc comments (`///`)** go on every public type and trait. They should explain design intent and how the type fits into the system, not restate what the code does.
- **Inline comments** are reserved for non-obvious logic — things a reader couldn't deduce from the code itself (e.g., borrow checker workarounds, invariants, algorithm constraints).
- **Do not comment obvious code.** No `// Click to activate` before `on_click`, no `// Remove from source` before `remove_item()`, no `// Recurse` before a recursive call.
- **Do not use section divider comments** (`// ── Section ──────`) to organize functions within a file. If code needs logical sections, split it into separate files/modules.

## Naming

- Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/naming.html).
- Getters omit `get_` prefix: `fn items()` not `fn get_items()`.
- Alternative constructors read like enum variants: `PlotPanel::empty()` not `PlotPanel::new_empty()`.
- Cyclic state transitions use `cycle()` not `next()`.
- Name functions for intent, not implementation: `drop_tab()` not `move_or_insert_tab()`.
- Avoid weak verbs: `apply_sort()` not `perform_sort()`.

## Module Organization

- Submodules that are internal implementation should be `pub(crate)`, not `pub`.
- Re-export public API from the parent module without aliases — types are already namespaced (e.g., `tiles::Pane` not `tiles::TilePane`).
- Logically distinct subsystems get their own file (e.g., `trace_picker.rs` is separate from `inspectable.rs`).

## Colors and Theming

- All colors go in `theme.rs` via the `Theme` struct. No hardcoded `Hsla` literals outside of `theme.rs`.
- Reference colors as `DARK.selection_bg`, not local `const` blocks.

## Allocations

- Use `SmallVec` for small, frequently-cloned vectors with predictable upper bounds (e.g., element indices, tree paths). Define type aliases near their usage.
- Use `SharedString` instead of `String` for display text that gets cloned during rendering.
- Use `&[T]` in function parameters when the callee only reads the data — don't require the caller to allocate a `Vec`.
- `Arc` and `Entity` clones are cheap (ref-count bumps) — don't contort code to avoid them.
- Clones forced by gpui's `'static` closure requirements (canvas, event handlers) are acceptable.

## Structural

- Avoid dead code: no `#[allow(dead_code)]` as a permanent state, no event variants that are never emitted.
- Reduce parameter threading in recursive functions by bundling shared immutable context into a struct.
- Extract shared logic into helpers (e.g., `list_components()`) rather than duplicating query patterns.
- Keep wrapper types thin — if a wrapper only delegates to an inner entity, make sure it earns its existence (e.g., by implementing a trait the inner type can't).
