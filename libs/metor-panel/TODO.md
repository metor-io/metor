# metor-panel TODO

Features from metor-ui parity (excluding 3D rendering) and new ideas for industrial controls.

## Panel Types

- [x] Time Series Plot — line plots with pan/zoom/multi-trace
- [x] Component Table — sortable table with sparklines
- [x] Component Text — single value display
- [ ] **Monitor** — large numeric value cards for multiple components, with element labels and card grid layout
- [ ] **Video Stream** — H264/H265 decoded video display
- [ ] **Map** — geographic map with markers driven by component data (lat/lon from EQL)
- [ ] **Hierarchy** — entity/component tree browser with fuzzy search and expand/collapse
- [ ] **Inspector** — component property editor with type-aware widgets

## Plot Features

- [x] Line plots with multiple traces
- [x] Pan (drag) and zoom (scroll wheel)
- [x] Double-click to reset view
- [x] Auto-ranging Y bounds
- [x] Axis labels and grid lines
- [x] **Point/scatter plots**
- [x] **Bar plots**
- [x] **Line width and color control** — adjustable stroke width and color per trace
- [x] **Line visibility toggles** — enable/disable individual traces
  Note the above four features should be part of a single inspectable section. You should be able to select a trace, and manually toggle its visibilty and color. This might require a rethink of the inspectable value logic since it is nested.
  Maybe a new type of InspectableValue where you can select a new Box<InspectableValue>
- [x] **Legend** — trace labels with color indicators
- [x] **Manual set x and y bounds** - With the inspector you should be able to select times for the X axis using the same syntax as metor-ui, and be able to set y bounds
- [x] **Axis zoom and pan**. Scrolling on each axis should just zoom that axis, same for panning

## Timeline & Playback
TBD whether this is good / we want this

- [ ] **Timeline bar** — scrubber showing time range with current position
- [ ] **Play/pause** — toggle simulation playback
- [ ] **Frame step** — advance forward/backward by one tick
- [ ] **Jump to start/end** — skip to earliest/latest timestamp

## Command & Interaction

- [x] Command palette with fuzzy search
- [x] Multi-page palette navigation with breadcrumb pills
- [ ] **Tab cycling** (Ctrl+Tab / Shift+Ctrl+Tab)

## Layout & Persistence

- [x] Tile splits (horizontal/vertical)
- [x] Tab containers with drag-and-drop
- [x] Resize handles between splits
- [x] Serialization to JSON
- [ ] **Schematic file I/O** — load/save layouts to disk (KDL or TOML format)
- [ ] **Live reload** — watch schematic file for changes and hot-reload layout
- [ ] **Default layouts** — e.g. "sidebar" layout with hierarchy + center + inspector

## Inspector & Editing

- [x] Inspectable trait for runtime field configuration
- [x] Trace picker with component/element selection
- [ ] **In-place value editing** — edit component values with type-aware input widgets
- [ ] **Color picker** — visual color selection

## Theming & Polish

- [x] Dark theme with configurable color palette
- [ ] **Icons** — toolbar and panel type icons (play, pause, add, close, search, etc.)
- [ ] **Custom button widgets** — styled buttons with icon + text support

---

## New Ideas

### Alarms & Limits

- [ ] **Limit lines on plots** — horizontal threshold lines (warning/critical) with configurable color and label, defined per-component or per-trace
- [ ] **Alarm panel** — live list of active limit violations with severity, timestamp, component name, and value. Sortable by severity/time. Click to navigate to the relevant plot.
- [ ] **Visual limit indicators** — plot background changes color when a trace is out of bounds (e.g., red tint above critical threshold)
- [ ] **Alarm history** — scrollable log of past alarm events with acknowledgment tracking

### Data Export & Annotation

- [ ] **CSV/Parquet export** — export visible plot data or query results to file
- [ ] **Screenshot panel** — capture a single panel or the full layout as PNG
- [ ] **Events / Annotations** — click on a plot to place a timestamped text marker (persisted as a message in the TB). It should allow you to display any message as an event


### Operational UX

- [ ] **Status bar** — User customizable status bar, inspired by Zed / nvim that allows you to assign certain components as health options
- [ ] **Component search** — global Cmd+K search that finds components by name across all panels, jumping to or creating a view for the result
- [ ] **Favorites / pinned components** — star components for quick access in the palette and hierarchy
- [ ] **Panel templates** — save a configured panel (e.g., a plot with specific traces and limits) as a reusable template that can be stamped into any layout
- [ ] **Linked time axes** — option to synchronize the X-axis (time range) across all visible plots so panning one pans all

### Sessions / Files

- [ ] **Open DB** - Allow users to open databases
- [ ] **Multi DB** — overlay two DBs on the same plot with aligned time axes for comparison

### Procedures & Sequences

- [ ] **Procedure panel** — checklist-style panel with steps that can be marked done, with optional EQL conditions for auto-completion (e.g., "pressure > 100 psi")
- [ ] **Sequence editor** — define and trigger timed sequences of actions (command at T+0, wait for condition, command at T+5, etc.)
