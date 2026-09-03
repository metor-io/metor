//! Errors raised while loading and resolving a wiring document.

use metor_fsw_2_core::params::{ParamError, ParamErrorKind};
use miette::Diagnostic;
use thiserror::Error;

use crate::coordinator::WireError;
use crate::dl::DlError;

/// A reason a wiring failed to validate or resolve into a runnable system
/// graph, from missing params through graph-level [`WireError`]s and
/// shared-library loading failures ([`DlError`]).
#[derive(Debug)]
pub struct LoadError {
    pub kind: LoadErrorKind,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.kind, f)
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.kind.source()
    }
}

impl Diagnostic for LoadError {}

impl From<ParamError> for LoadError {
    fn from(e: ParamError) -> Self {
        LoadError {
            kind: LoadErrorKind::Params(e.kind),
        }
    }
}

impl LoadErrorKind {
    /// Wrap this error kind for the resolver's public error surface.
    pub fn bare(self) -> LoadError {
        LoadError { kind: self }
    }
}

/// The payload and display string behind a [`LoadError`].
#[derive(Error, Debug)]
pub enum LoadErrorKind {
    /// A [`Wiring`](super::Wiring) stamped with a different
    /// [`IR_VERSION`](super::IR_VERSION) than this build's. Spanless: version
    /// skew is producer/host drift, not a mistake in the document text.
    #[error(
        "wiring IR version mismatch: the wiring carries v{found}, this host speaks v{expected} \
         (regenerate the wiring)"
    )]
    IrVersionMismatch { found: u32, expected: u32 },

    /// A serialized simulated-clock step cannot be represented as a positive
    /// [`Duration`](std::time::Duration).
    #[error(
        "invalid simulated clock step {dt_secs}: dt_secs must be finite, positive, and representable"
    )]
    InvalidSimulatedStep { dt_secs: f64 },

    /// A scope index (a spec's `scope` or a scope's `parent`) outside the
    /// wiring's scope table. The table is front-end metadata, so a bad index
    /// is a front-end bug rather than a document mistake; spanless.
    #[error("{owner} references scope index {index}, but the scope table has {len} entries")]
    BadScopeRef {
        owner: String,
        index: usize,
        len: usize,
    },

    /// A static system spec carries no `type`, which the registry needs to
    /// select a factory. Only a builder-origin spec can omit it.
    #[error("system `{name}` is missing its `type`")]
    MissingType { name: String },

    /// Two [`Artifact`](super::Artifact)s share an `id`. The id is how a
    /// system's `artifact=` and a slot's `allow` reference their pack, so a
    /// duplicate would silently shadow.
    #[error("duplicate artifact id `{id}`")]
    DuplicateArtifact { id: String },

    #[error("unknown system type `{ty}` (not in the registry)")]
    UnknownType { ty: String },

    /// Two declarations of the wiring's Python program share a name. Decls
    /// are addressed by name (a program-built pack entry references its
    /// function this way), so a duplicate would silently shadow.
    #[error("program declaration `{name}` appears more than once")]
    DuplicateProgramDecl { name: String },

    /// A system loading from the program artifact names an entry the wiring's
    /// program never declares. The program *is* that artifact's source, so a
    /// spec without a matching `@system` declaration is front-end drift, not
    /// a document mistake.
    #[error("Python system `{name}`: the wiring's program declares no `{name}`")]
    ProgramUnknownDecl { name: String },

    /// A cdylib artifact without a crate name or lib stem: nothing could
    /// build or locate it.
    #[error("artifact `{id}` is a cdylib but names no crate or lib stem")]
    ArtifactMissingCrate { id: String },

    /// A program-built wasm artifact in a wiring that captured no program:
    /// there is no source to compile it from.
    #[error("artifact `{id}` compiles from the target program, but the wiring carries none")]
    ProgramArtifactWithoutProgram { id: String },

    #[error("system `{name}` sets `process` without an `artifact`")]
    ProcessNeedsArtifact { name: String },

    /// A `process=#true` system or slot on a target without process-worker
    /// support. `name` is the instance name of either.
    #[error("`{name}` sets `process=#true`, unsupported on this target")]
    ProcessUnsupported { name: String },

    #[error("wasm_fuel_per_poll must be greater than zero")]
    InvalidWasmFuel,

    #[error("wasm_memory_limit_bytes must be nonzero and fit this host (got {bytes})")]
    InvalidWasmMemory { bytes: u64 },

    /// A resolve-time describe worker failed. `system` names the owning
    /// `system` or `slot` instance; for a slot, `artifact` is the allowed
    /// occupant whose describe failed.
    #[error("describe worker for `{system}` (artifact `{artifact}`) failed: {detail}")]
    ProcDescribe {
        system: String,
        artifact: String,
        detail: String,
    },

    #[error("duplicate instance name `{name}`")]
    DuplicateInstance { name: String },

    /// A params surface that did not decode or encode as the system's typed
    /// `Params`. The codec is `metor-fsw-2-core`'s, since a pack entry decodes
    /// its own params with no host in the loop; its diagnostic code and label
    /// come straight off the kind.
    #[error(transparent)]
    Params(ParamErrorKind),

    #[error("unknown instance `{name}` referenced in a `connect`")]
    UnknownInstance { name: String },

    #[error("instance `{instance}` has no port for frame `{frame}`")]
    UnknownFrame { instance: String, frame: String },

    #[error("instance `{instance}` has no message port for `{msg}`")]
    UnknownMsg { instance: String, msg: String },

    #[error("system `{system}` names unknown msg `{msg}` (registered: {available})")]
    UnknownMsgName {
        system: String,
        msg: String,
        available: String,
    },

    #[error("wiring error: {source}")]
    Wire {
        #[source]
        source: WireError,
    },

    // --- dl-open resolution -------------------------------------------------
    #[error("system `{system}` references unknown artifact `{artifact}`")]
    UnknownArtifact { system: String, artifact: String },

    /// The generated pack module for this artifact was produced against a
    /// different pack manifest than the one now built: its params, ports, or
    /// entries have changed. Fails before any dlopen, naming the one command
    /// that fixes it. Only a generated artifact (whose `ARTIFACT` constant
    /// carries the recorded hash) can trigger this; a builder-authored
    /// artifact records no hash and skips the check.
    #[error(
        "generated module for artifact `{artifact}` is stale (the pack manifest changed since \
         it was generated); regenerate with `uv sync`"
    )]
    StaleStubs {
        /// The artifact whose recorded and live manifest hashes disagree.
        artifact: String,
    },

    #[error("artifact `{artifact}` has no resolved path (run the build driver first)")]
    ArtifactNotBuilt { artifact: String },

    #[error("failed to load the `.so` for system `{system}` (artifact `{artifact}`): {source}")]
    DlOpen {
        system: String,
        artifact: String,
        // Boxed because `DlError` carries a `libloading::Error`, which would
        // otherwise bloat every `LoadError` (the `result_large_err` lint).
        #[source]
        source: Box<DlError>,
    },

    /// A wasm occupant could not be read, described, or matched to an entry.
    ///
    /// Carries one boxed message rather than the slot, occupant, artifact and
    /// cause as separate fields: four `String`s would make this the largest
    /// variant and bloat every `LoadError`, the same `result_large_err` reason
    /// [`DlOpen`](Self::DlOpen) boxes its source.
    #[error("{0}")]
    WasmOccupant(Box<str>),

    /// A wired wasm system could not be read, described, matched to an
    /// entry, or its synthesized edges did not line up with its descriptor.
    /// One boxed message, for the same size reason as
    /// [`WasmOccupant`](Self::WasmOccupant).
    #[error("{0}")]
    WasmSystem(Box<str>),

    /// A static system was given typed builder params. The static path has no
    /// postcard decoder; its registered factory deserializes params from a
    /// value tree, so the postcard bytes would be silently dropped and the
    /// system would run on defaults. Only the dl path decodes postcard params.
    #[error(
        "static system `{system}` (type `{ty}`) has typed builder params, but a static \
         system takes params through its registered value-tree-deserializing factory — \
         give it `params_value(...)`, or resolve the system `from_artifact` (only the dl \
         path decodes postcard params)"
    )]
    StaticPostcardParams { system: String, ty: String },

    /// A pack entry's create phase failed for a non-params reason (a
    /// moved-in state instantiated twice, a configure failure); a params
    /// failure surfaces as its own parameter error instead.
    #[error("system `{system}` failed to create: {message}")]
    PackCreate { system: String, message: String },

    /// A `state` spec's `type=` matches no shared state a registered pack
    /// declared.
    #[error(
        "state `{name}`: no registered pack declares shared state type `{ty}`{}",
        if available.is_empty() { String::new() } else { format!(" (declared: {available})") }
    )]
    UnknownStateType {
        name: String,
        ty: String,
        /// The comma-joined declared state types, for the message.
        available: String,
    },

    /// Two `state` specs share one instance name (or one state type, which
    /// has exactly one instance).
    #[error("state `{name}` is declared more than once")]
    DuplicateState { name: String },

    /// A shared state's own init fn failed, such as a resource acquisition
    /// like a listener bind, or its params did not decode.
    #[error("state `{name}` (type `{ty}`) failed to construct: {message}")]
    StateInit {
        name: String,
        ty: String,
        message: String,
    },

    /// A declared state no created system attached to: the state would run
    /// (its lifecycle serves attached systems only) for nobody, which is a
    /// config defect, not a quiet default.
    #[error("state `{name}` (type `{ty}`) is declared but no system in this wiring attaches to it")]
    StateUnused { name: String, ty: String },

    /// A system's `attach` named a state no `state` declaration provides.
    #[error("system `{system}` attaches to state `{attach}`, which no `state` declares")]
    AttachUnknownState { system: String, attach: String },

    /// A system set `attach`, but its type declares no pack-shared state to
    /// attach to.
    #[error("system `{system}` sets attach=`{attach}`, but its type is not a shared-state system")]
    AttachOnNonSharedSystem { system: String, attach: String },

    /// A shared-state system named no state to attach to.
    #[error("system `{system}` is a shared-state system but names no `attach` state")]
    MissingAttach { system: String },

    /// A system's `attach` named a state whose type is not the concrete
    /// shared type the system binds.
    #[error("system `{system}` cannot attach to state `{attach}`: incompatible shared-state type")]
    AttachTypeMismatch { system: String, attach: String },

    /// A loaded pack has no entry under the requested name, or the entry
    /// selection failed (the wrapped [`DlError`](crate::dl::DlError) says
    /// which).
    #[error("system `{system}`: {source}")]
    PackSystem {
        system: String,
        #[source]
        source: Box<crate::dl::DlError>,
    },

    /// A `system` node over a multi-entry pack omitted `type=`.
    #[error(
        "system `{system}`: artifact `{artifact}` exports several systems ({available}); \
         pick one with `type=`"
    )]
    PackTypeRequired {
        system: String,
        artifact: String,
        available: String,
    },

    /// An `allow` without `artifact=` matched no artifact, or more than one.
    #[error(
        "slot `{slot}`: occupant `{occupant}` {}; name the pack with `artifact=`",
        if matches.is_empty() { "matches no artifact's exports".to_string() }
        else { format!("is exported by more than one artifact ({})", matches.join(", ")) }
    )]
    OccupantAmbiguous {
        slot: String,
        occupant: String,
        matches: Vec<String>,
    },

    /// A `.state(...)` entry (instantiable once) was allowed as a slot
    /// occupant, which must be reloadable.
    #[error(
        "slot `{slot}`: occupant `{occupant}` holds moved-in state (`.state(...)`) and cannot \
         be reloaded; slot occupants must be reloadable"
    )]
    OccupantNotReloadable { slot: String, occupant: String },

    // --- slots ---------------------------------------------------------------
    #[error("`slot \"{slot}\"` has no `allow` occupant (a slot needs at least one)")]
    EmptySlot { slot: String },

    /// A slot's declared `input`/`output frame="…"` contract names a frame
    /// that none of its resolved occupants expose as a user port, usually a
    /// typo or a stale contract.
    #[error(
        "`slot \"{slot}\"` declares {dir} frame `{frame}` but its occupants have no such user port"
    )]
    SlotContractMismatch {
        slot: String,
        dir: &'static str,
        frame: String,
    },

    /// A slot's `initial occupant="…"` names an occupant outside the slot's
    /// `allow` set. Without this error such a typo would boot the slot empty
    /// with no diagnostic. The runtime load path validates against the same
    /// set.
    #[error(
        "`slot \"{slot}\"` initial occupant `{occupant}` is not in the allowed set \
         (allowed: {allowed})"
    )]
    UnknownInitialOccupant {
        slot: String,
        occupant: String,
        /// The comma-joined allowed occupant names, for the message and label.
        allowed: String,
    },

    /// Two allowed occupants of one slot have incompatible descriptors. A
    /// slot derives its single contract from the shared shape of its allowed
    /// set, so every occupant's ports must agree with the first one's.
    #[error("`slot \"{slot}\"` occupant `{occupant}` is incompatible with the slot contract")]
    SlotOccupantMismatch { slot: String, occupant: String },
}
