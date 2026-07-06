//! The wiring [`LoadError`] — every load/resolve failure with source context
//! (wiring.md §5.2), split out of `wiring/mod.rs` (C6).

use miette::{Diagnostic, SourceSpan};
use thiserror::Error;

use crate::coordinator::WireError;
use crate::dl::DlError;

// ---------------------------------------------------------------------------
// Errors — every failure with source context (wiring.md §5.2)
// ---------------------------------------------------------------------------

/// A wiring-document load failure, each carrying the offending KDL source span.
/// Mirrors `metor-proto-kdl`'s `KdlSchematicError`: same `thiserror`+`miette`
/// derive, same `#[source_code]`/`#[label]` shape.
#[derive(Error, Debug, Diagnostic)]
pub enum LoadError {
    #[error("KDL parse error")]
    #[diagnostic(code(fsw_wiring::parse_error))]
    Parse {
        #[source]
        source: kdl::KdlError,
        #[source_code]
        src: String,
        #[label("here")]
        span: SourceSpan,
    },

    #[error("missing the single `coordinator` node")]
    #[diagnostic(code(fsw_wiring::missing_coordinator))]
    MissingCoordinator,

    #[error("more than one `coordinator` node (exactly one is required)")]
    #[diagnostic(code(fsw_wiring::multiple_coordinators))]
    MultipleCoordinators {
        #[source_code]
        src: String,
        #[label("extra coordinator here")]
        span: SourceSpan,
    },

    #[error("`system` node is missing its instance name (the first argument)")]
    #[diagnostic(code(fsw_wiring::missing_instance_name))]
    MissingInstanceName {
        #[source_code]
        src: String,
        #[label("name this system, e.g. `system \"imu\" ...`")]
        span: SourceSpan,
    },

    #[error("`system \"{name}\"` is missing its `type=` property")]
    #[diagnostic(code(fsw_wiring::missing_type))]
    MissingType {
        name: String,
        #[source_code]
        src: String,
        #[label("add a registered `type=\"...\"`")]
        span: SourceSpan,
    },

    #[error("unknown system type `{ty}` (not in the registry)")]
    #[diagnostic(code(fsw_wiring::unknown_type))]
    UnknownType {
        ty: String,
        #[source_code]
        src: String,
        #[label("register this type before loading")]
        span: SourceSpan,
    },

    #[error("duplicate instance name `{name}`")]
    #[diagnostic(code(fsw_wiring::duplicate_instance))]
    DuplicateInstance {
        name: String,
        #[source_code]
        src: String,
        #[label("instance names must be unique")]
        span: SourceSpan,
    },

    /// A required params field (per the typed `Params` struct or the dl `Params`
    /// schema) has no matching property/child on the node.
    #[error("missing required param `{property}` for system `{system}`")]
    #[diagnostic(code(fsw_wiring::missing_param))]
    MissingParam {
        property: String,
        system: String,
        #[source_code]
        src: String,
        #[label("this node is missing the param")]
        span: SourceSpan,
    },

    /// A params value that does not decode as the field's type (or a params
    /// surface shape error: a stray positional argument, a repeated property, …).
    #[error("invalid value for `{property}` on system `{system}`: expected {expected}")]
    #[diagnostic(code(fsw_wiring::invalid_param))]
    InvalidParam {
        property: String,
        system: String,
        expected: String,
        #[source_code]
        src: String,
        #[label("invalid value here")]
        span: SourceSpan,
    },

    /// A property/child with no matching params field — a typo or a stale config.
    /// Raised uniformly by the static path (the serde deserializer) and the dl
    /// path (the `Params`-schema validation).
    #[error("unknown param `{property}` for system `{system}`")]
    #[diagnostic(code(fsw_wiring::unknown_param))]
    UnknownParam {
        property: String,
        system: String,
        #[source_code]
        src: String,
        #[label("no params field is named this")]
        span: SourceSpan,
    },

    #[error("`connect` is missing required property `{property}`")]
    #[diagnostic(code(fsw_wiring::missing_edge_field))]
    MissingEdgeField {
        property: String,
        #[source_code]
        src: String,
        #[label("this edge is incomplete")]
        span: SourceSpan,
    },

    #[error("unknown instance `{name}` referenced in a `connect`")]
    #[diagnostic(code(fsw_wiring::unknown_instance))]
    UnknownInstance {
        name: String,
        #[source_code]
        src: String,
        #[label("no `system` declares this instance")]
        span: SourceSpan,
    },

    #[error("instance `{instance}` has no port for frame `{frame}`")]
    #[diagnostic(code(fsw_wiring::unknown_frame))]
    UnknownFrame {
        instance: String,
        frame: String,
        #[source_code]
        src: String,
        #[label("misspelled or wrong-direction frame")]
        span: SourceSpan,
    },

    #[error("instance `{instance}` has no message port for `{msg}`")]
    #[diagnostic(code(fsw_wiring::unknown_msg))]
    UnknownMsg {
        instance: String,
        msg: String,
        #[source_code]
        src: String,
        #[label("misspelled or wrong-direction message type")]
        span: SourceSpan,
    },

    #[error("wiring error: {source}")]
    #[diagnostic(code(fsw_wiring::wire))]
    Wire {
        #[source]
        source: WireError,
        #[source_code]
        src: String,
        #[label("introduced here")]
        span: SourceSpan,
    },

    // --- dl-open resolution -------------------------------------------------
    #[error("system `{system}` references unknown artifact `{artifact}`")]
    #[diagnostic(code(fsw_wiring::unknown_artifact))]
    UnknownArtifact {
        system: String,
        artifact: String,
        #[source_code]
        src: String,
        #[label("declare an `artifact \"{artifact}\" ...` node, or fix the `artifact=` ref")]
        span: SourceSpan,
    },

    #[error("artifact `{artifact}` has no resolved path (run the build driver first)")]
    #[diagnostic(code(fsw_wiring::artifact_not_built))]
    ArtifactNotBuilt {
        artifact: String,
        #[source_code]
        src: String,
        #[label("`build_artifacts` must set this artifact's `path` before `resolve`")]
        span: SourceSpan,
    },

    #[error("failed to load the `.so` for system `{system}` (artifact `{artifact}`): {source}")]
    #[diagnostic(code(fsw_wiring::dl_open))]
    DlOpen {
        system: String,
        artifact: String,
        // Boxed: a `DlError` carries a `libloading::Error`, which would otherwise bloat
        // every `LoadError` (the `result_large_err` lint).
        #[source]
        source: Box<DlError>,
        #[source_code]
        src: String,
        #[label("this dl system failed to load")]
        span: SourceSpan,
    },

    /// The dl system's `Params` schema could not be encoded from its KDL config (an
    /// unsupported schema shape, or the dynamic encoder rejected the value).
    #[error("dl system `{system}` params could not be schema-encoded: {reason}")]
    #[diagnostic(code(fsw_wiring::dl_param_encode))]
    DlParamEncode {
        system: String,
        reason: String,
        #[source_code]
        src: String,
        #[label("these params could not be encoded against the `Params` schema")]
        span: SourceSpan,
    },

    /// A **static** (`from_static`) system carries typed builder params
    /// ([`ParamSource::Postcard`]). The static path has no postcard decoder: the
    /// [`Registry`] factory constructs the system by deserializing its params off the
    /// KDL `system` node ([`Registry::register`]), so the postcard bytes would be
    /// silently dropped and the system would run on defaults.
    #[error(
        "static system `{system}` (type `{ty}`) has typed builder params, but a static \
         system takes params through its registered KDL-deserializing factory — express \
         them as KDL config properties (`system \"{system}\" type=\"{ty}\" key=value`), or \
         resolve the system `from_artifact` (only the dl path decodes postcard params)"
    )]
    #[diagnostic(code(fsw_wiring::static_postcard_params))]
    StaticPostcardParams {
        system: String,
        ty: String,
        #[source_code]
        src: String,
        #[label("typed `params(...)` cannot reach a static system")]
        span: SourceSpan,
    },

    #[error("`artifact` node is missing required property `{property}`")]
    #[diagnostic(code(fsw_wiring::missing_artifact_field))]
    MissingArtifactField {
        property: &'static str,
        #[source_code]
        src: String,
        #[label("this artifact node is incomplete")]
        span: SourceSpan,
    },

    /// A top-level node name the wiring schema does not know — a typo would
    /// otherwise be silently skipped.
    #[error("unknown top-level node `{node}`")]
    #[diagnostic(code(fsw_wiring::unknown_top_level_node))]
    UnknownTopLevelNode {
        node: String,
        #[source_code]
        src: String,
        #[label("expected `coordinator`/`artifact`/`system`/`slot`/`connect`")]
        span: SourceSpan,
    },

    /// The pre-normalization `telemetry { … }` / `uplink { … }` nodes — a guidance
    /// error carrying the ordinary `system` spelling that replaced them.
    #[error("the `{node}` node was replaced by an ordinary `system` declaration")]
    #[diagnostic(
        code(fsw_wiring::legacy_link_node),
        help("declare it as a system, e.g. {example}")
    )]
    LegacyLinkNode {
        node: String,
        example: String,
        #[source_code]
        src: String,
        #[label("no longer a dedicated node")]
        span: SourceSpan,
    },

    /// `lib=` on a `system` node — renamed to `artifact=` (a guidance error for the
    /// pre-rename spelling; `lib=` on `artifact` nodes still means the library stem).
    #[error(
        "`lib=` on `system` nodes was renamed to `artifact=` (it references an artifact \
         id; `lib=` on `artifact` nodes still means the library stem)"
    )]
    #[diagnostic(code(fsw_wiring::system_lib_renamed))]
    SystemLibRenamed {
        #[source_code]
        src: String,
        #[label("write `artifact=` here")]
        span: SourceSpan,
    },

    /// A dl system's explicit `type=` contradicts the `.so` type its artifact
    /// exports (`type=` is optional on dl systems; when given it must match).
    #[error(
        "system `{system}` declares type `{ty}` but its artifact exports `{artifact_type}`"
    )]
    #[diagnostic(code(fsw_wiring::type_mismatches_artifact))]
    TypeMismatchesArtifact {
        system: String,
        ty: String,
        artifact_type: String,
        #[source_code]
        src: String,
        #[label("drop the `type=` or make it `{artifact_type}`")]
        span: SourceSpan,
    },

    // --- slots (sequences-slots.md §5) --------------------------------------
    #[error("`slot` node has an unknown child `{child}` (expected `input`/`output`/`allow`/`initial`)")]
    #[diagnostic(code(fsw_wiring::unknown_slot_child))]
    UnknownSlotChild {
        child: String,
        #[source_code]
        src: String,
        #[label("remove or rename this child")]
        span: SourceSpan,
    },

    #[error("`slot \"{slot}\"` has no `allow` occupant (a slot needs at least one)")]
    #[diagnostic(code(fsw_wiring::empty_slot))]
    EmptySlot {
        slot: String,
        #[source_code]
        src: String,
        #[label("add an `allow occupant=\"...\"` child")]
        span: SourceSpan,
    },

    #[error("`slot \"{slot}\"` has an invalid initial `state=\"{state}\"` (expected `empty`/`loaded`/`running`)")]
    #[diagnostic(code(fsw_wiring::bad_slot_state))]
    BadSlotState {
        slot: String,
        state: String,
        #[source_code]
        src: String,
        #[label("use `empty`, `loaded`, or `running`")]
        span: SourceSpan,
    },

    /// A `slot`'s declared `input`/`output frame="…"` contract names a frame that the
    /// slot's resolved (registered) descriptor has no matching user port for — a typo or
    /// a stale contract (the explicit-contract validation, Resolved Q4).
    #[error("`slot \"{slot}\"` declares {dir} frame `{frame}` but its occupants have no such user port")]
    #[diagnostic(code(fsw_wiring::slot_contract_mismatch))]
    SlotContractMismatch {
        slot: String,
        dir: &'static str,
        frame: String,
        #[source_code]
        src: String,
        #[label("no occupant {dir} port is named this")]
        span: SourceSpan,
    },

    /// A `slot`'s `initial occupant="…"` names an occupant that is not in the slot's
    /// `allow` set — a typo the slot would otherwise boot `Empty` on with no diagnostic
    /// (the runtime `Load` path validates the same set).
    #[error(
        "`slot \"{slot}\"` initial occupant `{occupant}` is not in the allowed set \
         (allowed: {allowed})"
    )]
    #[diagnostic(code(fsw_wiring::unknown_initial_occupant))]
    UnknownInitialOccupant {
        slot: String,
        occupant: String,
        /// The comma-joined allowed occupant names, for the message/label.
        allowed: String,
        #[source_code]
        src: String,
        #[label("no `allow occupant=` names this (allowed: {allowed})")]
        span: SourceSpan,
    },

    /// Two allowed occupants of one `slot` have incompatible descriptors — the slot derives
    /// its single contract from the allowed set's shared shape (v1 holds sequence occupants
    /// only, so they must agree). A clean error in place of `add_slot`'s build-time panic.
    #[error("`slot \"{slot}\"` occupant `{occupant}` is incompatible with the slot contract")]
    #[diagnostic(code(fsw_wiring::slot_occupant_mismatch))]
    SlotOccupantMismatch {
        slot: String,
        occupant: String,
        #[source_code]
        src: String,
        #[label("this occupant's ports differ from the first allowed occupant's")]
        span: SourceSpan,
    },
}

