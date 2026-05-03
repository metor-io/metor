# metor-panel TODO

Features from metor-ui parity (excluding 3D rendering) and new ideas for industrial controls.

## Panel Types

- [x] Time Series Plot — line plots with pan/zoom/multi-trace
- [x] Component Table — sortable table with sparklines
- [x] Component Text — single value display
- [X] **Monitor** — large numeric value cards for multiple components, with element labels and card grid layout
- [ ] **Video Stream** — H264/H265 decoded video display
- [ ] **Map** — geographic map with markers driven by component data (lat/lon from EQL)
- [X] **Hierarchy** — entity/component tree browser with fuzzy search and expand/collapse
- [X] **Inspector** — component property editor with type-aware widgets
- [x] **XY Plot** — phase / correlation plot pairing two `(component, element)` axes
- [x] **List Plot** — index-vs-value of a vector component's latest sample (FFT/spectrum visualization), with Line / Scatter / Bar styles

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

## Dynamic Nodes

The node editor exposes a runtime graph that produces components from clocks, signals, derivations, and DB sources. Each op compiles to a `DynamicNode` and is reused across reconciliations.

### Existing ops

- [x] Clocks: Fixed Rate, Clock Of
- [x] Generators: Waveform (sin/cos/square/sawtooth), Random, Constant
- [x] Derive: Scale, Offset, Abs, Neg, Log, **Window** (sliding `[f64; N]`), **FFT** (one-sided magnitude spectrum)
- [x] Compose: Add, Sub, Mul, Mean, Pack
- [x] Resample: Zero-Order Hold, Linear, Latest At
- [x] DB: From DB, Persist

### Proposed: math primitives

- [ ] **Sqrt / Pow / Exp** — round out the f64 math toolkit (Log already exists)
- [ ] **Sin / Cos / Atan2** — trig (Atan2 takes two scalars; useful for angle-from-(x,y))
- [ ] **Sign / Clamp** — `Clamp { min, max }` saturates a value to a range
- [ ] **Linear Map** — single-node `a*x + b` (convenience over Scale → Offset chains)
- [ ] **Lerp** — three-input `a*(1-t) + b*t` (mix two streams by a third)

### Proposed: sliding statistics over the last N samples

A natural follow-up to Window. Each is `f64 in → f64 out` with one `size` arg.

- [ ] **Sliding Mean** — moving average (smoothing); distinct from the existing co-clocked `Mean` over multiple inputs
- [ ] **Sliding Min / Max** — running envelope (alarm flooring/ceiling)
- [ ] **Sliding StdDev / Variance** — noise / jitter quantification
- [ ] **Differentiate** — discrete derivative (e.g. velocity from position)
- [ ] **Integrate** — running sum, with optional reset trigger

### Proposed: thresholds and triggers

- [ ] **Threshold** — `input > k` → 0/1 f64 (downstream sees a binary signal)
- [ ] **Hysteresis** — two-threshold debounced version of Threshold (prevents chatter near the edge)
- [ ] **Edge Detect** — pulse (1 for one tick) on rising / falling / either edge
- [ ] **Latch** — sample-and-hold; freezes the input value when a trigger goes high

### Proposed: vector ops (operate on vector schemas like Pack/Window/FFT outputs)

- [ ] **Slice / Index** — pull element `[i]` of a vector → scalar (the inverse of Pack)
- [ ] **Magnitude / L2 Norm** — `sqrt(sum(x_i²))` of a vector input → scalar; useful for speed-from-velocity
- [ ] **Dot Product** — two vectors → scalar
- [ ] **Concatenate** — two vectors of length M, N → one vector of length M+N
- [ ] **Window Function** — multiply a vector by Hann / Hamming / Blackman before FFT (real-world FFT users always want this)

### Proposed: control & sequencing

- [ ] **PID Controller** — `setpoint, measured → output` with P/I/D gains; useful for closed-loop sim demos
- [ ] **Counter** — increment on each tick (or on a trigger edge)
- [ ] **Time Since Trigger** — seconds elapsed since input last crossed a threshold; useful for cooldowns and timeouts
- [ ] **Decimate** — emit every k-th sample (cheap downsample without filtering)

## Timeline & Playback
TBD whether this is good / we want this

- [ ] **Timeline bar** — scrubber showing time range with current position
- [ ] **Play/pause** — toggle simulation playback
- [ ] **Frame step** — advance forward/backward by one tick
- [ ] **Jump to start/end** — skip to earliest/latest timestamp

## Command & Interaction

- [x] Command palette with fuzzy search
- [x] Multi-page palette navigation with breadcrumb pills
- [x] **Tab cycling** (Ctrl+Tab / Shift+Ctrl+Tab)

## Layout & Persistence

- [x] Tile splits (horizontal/vertical)
- [x] Tab containers with drag-and-drop
- [x] Resize handles between splits
- [x] Serialization to JSON
- [X] **Schematic file I/O** — load/save layouts to disk (KDL or TOML format)

## Inspector & Editing

- [x] Inspectable trait for runtime field configuration
- [x] Trace picker with component/element selection
- [X] **In-place value editing** — edit component values with type-aware input widgets
- [X] **Color picker** — visual color selection

## Theming & Polish

- [x] Dark theme with configurable color palette
- [x] **Icons** — toolbar and panel type icons (play, pause, add, close, search, etc.)
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
