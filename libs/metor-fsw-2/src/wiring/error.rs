//! Errors raised while loading and resolving a wiring document.

use metor_fsw_2_core::params::{Anchor, ParamError, ParamErrorKind};
use miette::{Diagnostic, LabeledSpan, SourceCode, SourceSpan};
use thiserror::Error;

use crate::coordinator::WireError;
use crate::dl::DlError;

/// A reason a wiring failed to validate or resolve into a runnable system
/// graph, from missing params through graph-level [`WireError`]s and
/// shared-library loading failures ([`DlError`]).
///
/// The [`kind`](LoadError::kind) carries the failure's payload and display
/// string; the [`anchor`](LoadError::anchor) — when it has one — the source
/// snippet and offending span, so rendering the error as a [`miette`] report
/// points at the target line responsible. A few kinds are spanless (version
/// skew, front-end metadata bugs) and carry no anchor.
///
/// [`SourceRef`]: super::SourceRef
#[derive(Debug)]
pub struct LoadError {
    pub kind: LoadErrorKind,
    pub anchor: Option<Anchor>,
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

impl Diagnostic for LoadError {
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        Some(Box::new(self.kind.code()))
    }

    fn help<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        self.kind
            .help()
            .map(|h| Box::new(h) as Box<dyn std::fmt::Display>)
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        self.anchor.as_ref().map(|a| &a.src as &dyn SourceCode)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        let anchor = self.anchor.as_ref()?;
        let label = self.kind.label()?;
        Some(Box::new(std::iter::once(LabeledSpan::new_with_span(
            Some(label),
            anchor.span,
        ))))
    }
}

impl From<ParamError> for LoadError {
    fn from(e: ParamError) -> Self {
        LoadError {
            kind: LoadErrorKind::Params(e.kind),
            anchor: e.anchor,
        }
    }
}

impl LoadErrorKind {
    /// Anchor this kind to a source snippet and the span of the offending node.
    pub fn at(self, src: impl Into<String>, span: SourceSpan) -> LoadError {
        LoadError {
            kind: self,
            anchor: Some(Anchor {
                src: src.into(),
                span,
            }),
        }
    }

    /// Anchor this kind to a snippet whose whole extent is the label span, the
    /// common case for resolve-time snippets that carry no interior spans.
    pub fn whole(self, src: impl Into<String>) -> LoadError {
        let src = src.into();
        let span = (0, src.len()).into();
        self.at(src, span)
    }

    /// A spanless [`LoadError`]: version skew or a front-end metadata bug, with
    /// no document text to point at.
    pub fn bare(self) -> LoadError {
        LoadError {
            kind: self,
            anchor: None,
        }
    }

    /// This kind's stable diagnostic code.
    fn code(&self) -> &'static str {
        use LoadErrorKind::*;
        match self {
            IrVersionMismatch { .. } => "fsw_wiring::ir_version_mismatch",
            InvalidSimulatedStep { .. } => "fsw_wiring::invalid_simulated_step",
            BadScopeRef { .. } => "fsw_wiring::bad_scope_ref",
            MissingType { .. } => "fsw_wiring::missing_type",
            DuplicateArtifact { .. } => "fsw_wiring::duplicate_artifact",
            UnknownType { .. } => "fsw_wiring::unknown_type",
            ProcessNeedsArtifact { .. } => "fsw_wiring::process_needs_artifact",
            ProcessUnsupported { .. } => "fsw_wiring::process_unsupported",
            ProcDescribe { .. } => "fsw_wiring::proc_describe",
            DuplicateInstance { .. } => "fsw_wiring::duplicate_instance",
            Params(kind) => kind.code(),
            UnknownInstance { .. } => "fsw_wiring::unknown_instance",
            UnknownFrame { .. } => "fsw_wiring::unknown_frame",
            UnknownMsg { .. } => "fsw_wiring::unknown_msg",
            UnknownMsgName { .. } => "fsw_wiring::unknown_msg_name",
            Wire { .. } => "fsw_wiring::wire",
            UnknownArtifact { .. } => "fsw_wiring::unknown_artifact",
            StaleStubs { .. } => "fsw_wiring::stale_stubs",
            ArtifactNotBuilt { .. } => "fsw_wiring::artifact_not_built",
            DlOpen { .. } => "fsw_wiring::dl_open",
            StaticPostcardParams { .. } => "fsw_wiring::static_postcard_params",
            PackCreate { .. } => "fsw_wiring::pack_create",
            UnknownStateType { .. } => "fsw_wiring::unknown_state_type",
            DuplicateState { .. } => "fsw_wiring::duplicate_state",
            StateInit { .. } => "fsw_wiring::state_init",
            StateUnused { .. } => "fsw_wiring::state_unused",
            AttachUnknownState { .. } => "fsw_wiring::attach_unknown_state",
            AttachOnNonSharedSystem { .. } => "fsw_wiring::attach_on_non_shared_system",
            MissingAttach { .. } => "fsw_wiring::missing_attach",
            AttachTypeMismatch { .. } => "fsw_wiring::attach_type_mismatch",
            PackSystem { .. } => "fsw_wiring::pack_system",
            PackTypeRequired { .. } => "fsw_wiring::pack_type_required",
            OccupantAmbiguous { .. } => "fsw_wiring::occupant_ambiguous",
            OccupantNotReloadable { .. } => "fsw_wiring::occupant_not_reloadable",
            EmptySlot { .. } => "fsw_wiring::empty_slot",
            SlotContractMismatch { .. } => "fsw_wiring::slot_contract_mismatch",
            UnknownInitialOccupant { .. } => "fsw_wiring::unknown_initial_occupant",
            SlotOccupantMismatch { .. } => "fsw_wiring::slot_occupant_mismatch",
        }
    }

    /// This kind's `help` line, for the kinds that carry one.
    fn help(&self) -> Option<&'static str> {
        use LoadErrorKind::*;
        Some(match self {
            ProcessNeedsArtifact { .. } => {
                "a process system runs in a worker that loads its cdylib, so only \
                 artifact-backed systems can cross; a static-registry type cannot"
            }
            ProcessUnsupported { .. } => {
                "process systems need a cross-process futex: Linux or macOS 14.4+"
            }
            ProcDescribe { .. } => {
                "the worker dlopens the artifact and reports its descriptor; its captured \
                 stderr is in the message above"
            }
            UnknownMsgName { .. } => {
                "register the message type via `Registry::register_msg::<M>()`"
            }
            _ => return None,
        })
    }

    /// The label pointing at the offending span, for the spanned kinds.
    fn label(&self) -> Option<String> {
        use LoadErrorKind::*;
        Some(match self {
            MissingType { .. } => "set a registered `type`".into(),
            UnknownType { .. } => "register this type before loading".into(),
            ProcessNeedsArtifact { .. } => "set an `artifact` or drop `process`".into(),
            ProcessUnsupported { .. } => "this instance cannot run cross-process here".into(),
            ProcDescribe { .. } => "describing this artifact failed".into(),
            DuplicateInstance { .. } => "instance names must be unique".into(),
            DuplicateArtifact { .. } => "artifact ids must be unique".into(),
            Params(kind) => kind.label(),
            UnknownInstance { .. } => "no `system` declares this instance".into(),
            UnknownFrame { .. } => "misspelled or wrong-direction frame".into(),
            UnknownMsg { .. } => "misspelled or wrong-direction message type".into(),
            UnknownMsgName { .. } => "not a registered NamedMsg token".into(),
            Wire { .. } => "introduced here".into(),
            UnknownArtifact { artifact, .. } => {
                format!("declare an `artifact \"{artifact}\" ...` node, or fix the `artifact=` ref")
            }
            ArtifactNotBuilt { .. } => {
                "`provision_artifacts` must set this artifact's `path` before `resolve`".into()
            }
            DlOpen { .. } => "this dl system failed to load".into(),
            StaticPostcardParams { .. } => {
                "typed `params(...)` cannot reach a static system".into()
            }
            PackCreate { .. } => "this system".into(),
            UnknownStateType { .. } => "no registered pack declares this state type".into(),
            DuplicateState { .. } => "state names must be unique".into(),
            StateInit { .. } => "this state's construction failed".into(),
            StateUnused { .. } => "declared here, attached nowhere".into(),
            AttachUnknownState { .. } => "no `state` declares this name".into(),
            AttachOnNonSharedSystem { .. } => "this system type declares no shared state".into(),
            MissingAttach { .. } => "a shared-state system needs an `attach` state".into(),
            AttachTypeMismatch { .. } => "this state's type is not the system's shared type".into(),
            PackSystem { .. } => "this instance".into(),
            PackTypeRequired { .. } => "add `type=\"…\"`".into(),
            OccupantAmbiguous { .. } => "this slot".into(),
            OccupantNotReloadable { .. } => "this slot".into(),
            EmptySlot { .. } => "add an `allow occupant=\"...\"` child".into(),
            SlotContractMismatch { dir, .. } => format!("no occupant {dir} port is named this"),
            UnknownInitialOccupant { allowed, .. } => {
                format!("no `allow occupant=` names this (allowed: {allowed})")
            }
            SlotOccupantMismatch { .. } => {
                "this occupant's ports differ from the first allowed occupant's".into()
            }
            IrVersionMismatch { .. }
            | InvalidSimulatedStep { .. }
            | BadScopeRef { .. }
            | StaleStubs { .. } => return None,
        })
    }
}

/// The payload and display string behind a [`LoadError`], one variant per
/// failure. The rendered snippet and span live on the [`LoadError`]'s
/// [`anchor`](LoadError::anchor), not here.
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

    #[error("system `{name}` sets `process` without an `artifact`")]
    ProcessNeedsArtifact { name: String },

    /// A `process=#true` system or slot on a target without process-worker
    /// support. `name` is the instance name of either.
    #[error("`{name}` sets `process=#true`, unsupported on this target")]
    ProcessUnsupported { name: String },

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
    /// different pack manifest than the one now built — its params, ports, or
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
    /// failure surfaces as its own spanned error instead.
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

    /// A shared state's own init fn failed — resource acquisition, like a
    /// listener bind — or its params did not decode.
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
