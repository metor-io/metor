# Universal data bindings

Implemented in `src/data_binding.rs` and the common inspector picker.

## The shared contract

Every independently editable widget input owns a `Binding`. Its private state
keeps the authoritative `BindingSpec`, resolved component ID, expression owner,
and resolution error together. Inspector edits replace the whole value, so
expression ownership is acquired before the picker releases its temporary.
Cloning a binding shares computation but does not link subsequent edits.

`BindingSpec` distinguishes unbound, component, and expression sources. The
component variant preserves an explicitly selected ID even if the display name
is different. Expressions preserve their original `=` syntax independently of
labels and metadata. Invalid or unavailable expressions retain that syntax and
expose an error; they are never hashed as component names. Missing inputs retry
when the DB registration generation changes.

The implementation uses an owned value instead of the originally proposed GPUI
entity. This keeps context-free public trace constructors usable. Resolving a
binding requires an App context; raw-ID constructors resolve synchronously when
they are attached to a plot, so an immediate save retains expression provenance.

## Creation, editing, and rendering

The `Binding` inspector field uses the same `binding_picker_rows` as dashboard
and tile creation. It prefills the current expression, and supports editing and
clearing without switching to a separate optional-ID editor. Empty widgets stay
real, inspectable widgets after saving and loading.

Instruments, Text, Monitor, Traffic Light, Map, Samples, Attitude and its markers,
all plot traces, both XY axes, both model transforms, and explicit annunciator
conditions use this contract. Expression-specific inspector edit hooks and
host-owned expression wrappers have been removed.

The existing `views/binding.rs` helpers continue to provide seeded streams,
metadata, freshness and scalar/vector/on-off decoding. Widgets retain their
presentation-specific stream reset and decoding behavior. Element indexes,
quaternion offsets and vector lengths remain their existing typed fields; the
migration preserves those selection conventions rather than encoding them as
new expression syntax.

`InputChanges` observes non-rendered child inputs and invalidates plot trackers
when their source or selection changes. Each XY axis has its own binding editor.
Registration watchers wake unresolved consumers when producers appear.

## History

`BoundHistory` discovers the available range from the output and expression
inputs, requests remote input/output spans, and schedules missing output through
the existing Backfiller/replay engine. Time-series plots, spectrograms, Map and
Samples use the same replay request path. Input hydration wakes consumers to
retry replay even before output history exists.

Map trails and sample indexes include the manifest generation in their cache
keys, so installing older history invalidates them without requiring a newer
live sample. Map requests its time window. Samples initially requests a minute
of history and provides a Load older samples action to extend that range in
bounded steps; expressions that have not emitted a first sample can still request
history using the input extent.

This retains the replay engine's existing treatment of stateful/windowed
expressions and its boundary around the live writer. It does not change replay
semantics or implement a new expression engine.

## Annunciator

Component mode supports explicit `conditions` alongside its existing glob.
Each condition is a child with a normal binding editor and owns its computation.
Duplicate component IDs produce one tile. Rebinding a condition changes its tile
identity and clears the previous condition's latch. Hidden expression outputs
are not globally exposed to glob matching. Alarm-store mode retains its existing
semantics.

## Persistence and API migration

Existing layout kinds and component-string / plot-ID configuration fields still
load. Shared boundary adapters normalize them to bindings. Writers obtain source
text from the binding, never from a display label. Plot expression fields now
come from the owned binding even when saving before the next render.

3D ModelConfig adds optional `position_expression` and `orientation_expression`
fields. AnnunciatorConfig adds an optional `conditions` list. Old layouts omit
these fields and deserialize with their previous behavior. Old ID-only
expressions can recover provenance from DB metadata when that metadata exists;
missing original source text cannot be reconstructed from an ID alone.

Rust callers using live fields should migrate `component_id` to `source`, and
`x_component_id` / `y_component_id` to `x_source` / `y_source`. Read the resolved
ID with `.id()` and replace an input with a complete `Binding`. Constructors
accepting component IDs remain available. Persisted config fields retain their
old names. Model live transform fields now hold a Binding; Unbound replaces None.

## Adding another widget

1. Expose each data input as a Binding field and register the view as inspectable.
2. Normalize config text/IDs through `Binding::from_text` / `from_legacy`.
3. Use the existing typed stream helpers for decoding; resolve and reset stream
   state when an input changes. Keep a registration watcher for delayed sources.
4. Snapshot binding text or its legacy ID/expression pair through the shared API.
5. For history, use BoundHistory; for child inputs use InputChanges to notify and
   invalidate the parent. Keep rendering and LoD decisions local to the view.
6. Extend the shared editor and snapshot integration tables.

## Verification

The integration table exercises all 18 current editable inputs through their
real reflected editors: commit without rendering, retain the expression, reopen
on its source, and clear/release it. Snapshot tests cover valid, empty and
unavailable expression sources across widget builders. Additional checks cover
cold model-expression restoration, delayed producer registration, shared
computation lifetime, discovery/backfill of existing history, and automatic
plot tracker invalidation after an input edit.


## Global display time

Instantaneous views now lease `temporal::samples::SelectedReader` through the shared
binding adapters. A selected sample owns its bytes and carries its actual timestamp,
availability, freshness, and reconstruction status. It is the last eligible sample at
or before the controller's view time. Scalar readouts, copy actions, vector plots,
map markers, and 3D transforms use that same selection. Ingestion and operational
alarm/sequence state continue live while display time is paused.

Historical stateless expressions can be reconstructed in a bounded point query
without writing output history or mutating the live evaluator. Missing stateful
outputs require a checkpoint policy and display an unavailable status. See
[global time and replay](global-time-replay.md) for the anchor model, shared inspector
workflow, and persistence compatibility.
