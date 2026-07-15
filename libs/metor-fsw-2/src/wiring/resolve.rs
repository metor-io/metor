//! Walk a [`Wiring`] IR into a built [`Coordinator`].
//!
//! [`resolve`]/[`resolve_with`] are the whole front-end's single exit: validate
//! the IR ([`validate`](super::validate)), broadcast it as a [`WiringManifest`](metor_proto_wkt::WiringManifest),
//! then run the systems, slots, deferred receive-all, and edges passes onto an
//! [`InitGraph`](crate::coordinator::init::InitGraph) before handing it to
//! [`InitGraph::build`](crate::coordinator::init::InitGraph::build). Both
//! front-ends (Python eval and the Rust [`WiringBuilder`](super::WiringBuilder))
//! land here, so every graph check runs identically for either.
//!
//! A static system is instantiated through the [`Registry`] factory; a dl
//! system is opened from its built [`Artifact`] and a `process=#true` system is
//! described by a short-lived worker (the host never dlopens it). The three
//! paths converge on [`resolve_occupant`], the one open→select→encode step.
//! Host-environment policy (worker executable, session root, process timing)
//! rides in via [`ResolveOptions`] rather than the portable IR.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use miette::SourceSpan;

use metor_proto::types::ComponentId;

use crate::coordinator::init::{InitGraph, Node, SystemBind};
use crate::coordinator::slot::{SlotReg, plan_slot};
use crate::coordinator::{
    AllowedOccupant, ClockMode, Coordinator, CoordinatorConfig, InitialOccupant, OccupantBacking,
    PortRef, SlotConfigError, SystemHandle,
};
use crate::descriptor::{PortId, SystemDescriptor};
use crate::dl::DlSystem;

use super::error::{LoadError, LoadErrorKind};
use super::model::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec, ParamSource,
    SlotInitState, SlotSpec, SourceRef, SystemSpec, Wiring,
};
use super::registry::{LoadCtx, Registry, StaticParams};
use super::{encode_value_params, stubgen, validate};

/// One resolved instance after the systems pass.
struct Instance {
    handle: SystemHandle,
    desc: SystemDescriptor,
}

/// Which port list an endpoint resolves against.
#[derive(Clone, Copy)]
enum Dir {
    Out,
    In,
}

/// Resolve-time overrides that a [`Wiring`] does not itself carry.
///
/// A `Wiring` is a portable mission description, so host-environment policy —
/// where a process worker's executable lives, where the shared-memory session
/// dir is rooted, how a worker's steps are timed and its crashes recovered — is
/// supplied here at [`resolve_with`] time rather than baked into the IR. These
/// are deployment decisions, not mission topology, so they stay off the
/// serialized IR (`wiring.json` is unaffected); each override falls back to the
/// matching [`CoordinatorConfig`] default when `None`. The defaults (re-exec
/// the host binary as the worker, `/dev/shm` or the OS temp dir for sessions,
/// the config's step-timeout and restart policy) are what [`resolve`] uses.
#[derive(Default, Clone, Debug)]
pub struct ResolveOptions {
    /// The process-worker executable, instead of re-executing the host binary.
    /// Tests point this at a fixture worker; a host whose own binary cannot
    /// serve as a worker sets it to a leaner one.
    pub worker_exe: Option<PathBuf>,
    /// The shared-memory session parent dir, instead of the default
    /// (`/dev/shm` when present, else the OS temp dir).
    pub shm_dir: Option<PathBuf>,
    /// Override of [`CoordinatorConfig::proc_step_timeout`]: how long a process
    /// system's step waits for the worker's ack before the cycle moves on.
    pub proc_step_timeout: Option<Duration>,
    /// Override of [`CoordinatorConfig::proc_max_restarts`]: how many times a
    /// dead or panicked worker is respawned over the slot's life (`0` disables
    /// restart).
    pub proc_max_restarts: Option<u32>,
    /// Override of [`CoordinatorConfig::proc_restart_backoff`]: how long a dead
    /// worker's slot waits before respawning.
    pub proc_restart_backoff: Option<Duration>,
}

/// Walk a [`Wiring`] and produce a built [`Coordinator`], the default-option
/// twin of [`resolve_with`].
pub fn resolve(wiring: &Wiring, registry: &Registry) -> Result<Coordinator, LoadError> {
    resolve_with(wiring, registry, ResolveOptions::default())
}

/// Walk a [`Wiring`] and produce a built [`Coordinator`], applying `opts`.
///
/// Both front-ends land here, so static and dl systems go through identical
/// validation, sizing, and telemetry passes. A static system (no `artifact`)
/// is instantiated through the [`Registry`] factory; a dl system is opened
/// from its built [`Artifact::path`] and registered with
/// [`InitGraph::add_dl_cyclic`](crate::coordinator::init::InitGraph::add_dl_cyclic).
///
/// A `Wiring` carries no source text (a builder-origin one never had any), so
/// resolve-time [`LoadError`]s hold a best-effort snippet rather than original
/// document spans. The error variants are the same either way.
pub fn resolve_with(
    wiring: &Wiring,
    registry: &Registry,
    opts: ResolveOptions,
) -> Result<Coordinator, LoadError> {
    // The one structural gate: version, scope indices, name/id uniqueness, and
    // each spec's well-formedness (`validate` module). Everything past here is
    // registry- or filesystem-dependent.
    validate::validate(wiring)?;
    check_manifest_hashes(wiring)?;
    let mut config = coordinator_config(&wiring.coordinator)?;
    // Host-environment policy overrides (see `ResolveOptions`): applied onto the
    // config the IR derived, never onto the IR itself.
    if let Some(timeout) = opts.proc_step_timeout {
        config.proc_step_timeout = timeout;
    }
    if let Some(max) = opts.proc_max_restarts {
        config.proc_max_restarts = max;
    }
    if let Some(backoff) = opts.proc_restart_backoff {
        config.proc_restart_backoff = backoff;
    }
    let mut graph = InitGraph::new(config);
    graph.worker_exe = opts.worker_exe;
    graph.shm_dir = opts.shm_dir;

    // Serialized path-stripped so the telemetered topology matches the
    // bundle's `wiring.json` byte-for-byte regardless of the build tree.
    let ir_json = serde_json::to_string(&wiring.path_stripped())
        .expect("a resolvable Wiring serializes to JSON");
    graph.set_wiring_manifest(metor_proto_wkt::WiringManifest {
        ir_version: wiring.ir_version,
        ir_json,
    });

    // --- Systems pass: static via the Registry, dl via the loader --------
    let mut instances: HashMap<String, Instance> = HashMap::new();
    // The coordinator joins the instance namespace up front so command edges
    // can name it; `validate` already rejected any user spec of that name.
    let coord_handle = graph.coordinator_handle();
    instances.insert(
        "coordinator".to_string(),
        Instance {
            handle: coord_handle,
            desc: graph.descriptor_of(coord_handle).clone(),
        },
    );
    // A static `ReceiveAll` system must be the last cyclic registration or
    // `build()` rejects the graph with `ReceiveAllNotLast`, so it is deferred
    // behind every other system and slot. Deferral also gives it the right
    // step position, after every producer and before telemetry. Only the
    // static branch defers; dl systems never carry capabilities.
    let mut deferred: Vec<&SystemSpec> = Vec::new();
    let mut packs = PackCache::default();
    for spec in &wiring.systems {
        let (handle, desc) = match (&spec.artifact, spec.process) {
            (Some(artifact_id), true) => resolve_proc(spec, artifact_id, wiring, &mut graph)?,
            (Some(artifact_id), false) => {
                resolve_dl(spec, artifact_id, wiring, &mut packs, &mut graph)?
            }
            (None, true) => {
                unreachable!("validate() rejects a process system without an artifact")
            }
            (None, false) => {
                if registry.is_receive_all(spec.ty.as_deref()) {
                    deferred.push(spec);
                    continue;
                }
                resolve_static(spec, registry, &mut graph)?
            }
        };
        // `validate` guaranteed instance-name uniqueness, so the insert is new.
        instances.insert(spec.name.clone(), Instance { handle, desc });
    }

    // --- Slots pass: a slot `connect`s by name like a system, so it joins
    //     `instances` before the edges pass.
    for slot in &wiring.slots {
        let (handle, desc) = resolve_slot(slot, wiring, &mut packs, &mut graph)?;
        instances.insert(slot.name.clone(), Instance { handle, desc });
    }

    // --- Deferred receive-all systems: the last cyclic registrations, still
    //     ahead of the edges pass, which only needs the finished instance map.
    for spec in deferred {
        let (handle, desc) = resolve_static(spec, registry, &mut graph)?;
        instances.insert(spec.name.clone(), Instance { handle, desc });
    }

    // --- Edges pass ------------------------------------------------------
    for edge in &wiring.edges {
        let arrow = if edge.delayed { "~>" } else { "->" };
        let src = format!(
            "connect \"{}\" {arrow} \"{}\" out=\"{}\" in=\"{}\"{}",
            edge.from,
            edge.to,
            edge.out,
            edge.in_,
            src_anchor(edge.src.as_ref())
        );
        let span: SourceSpan = (0, src.len()).into();
        let (producer, consumer) = match edge.kind {
            EdgeKind::Frame => (
                resolve_endpoint(&instances, &src, &edge.from, &edge.out, Dir::Out, span)?,
                resolve_endpoint(&instances, &src, &edge.to, &edge.in_, Dir::In, span)?,
            ),
            EdgeKind::Msg => resolve_msg_edge(&instances, &src, edge, span)?,
        };
        // One `connect` entry point for every edge; its behavior is inferred
        // from the connected ports' descriptors, and `EdgeKind` only picked
        // the name-lookup space above. The edge is validated at build, where a
        // defect (`delayed=#true` into a Log input, an incompatible pair) is a
        // `WireError`.
        if edge.delayed {
            graph.connect_delayed(producer, consumer);
        } else {
            graph.connect(producer, consumer);
        }
    }

    // A `Wiring` is format-independent, so a build-time `WireError` uses its own
    // rendered message as the diagnostic snippet, not original document spans.
    graph.build().map_err(|err| {
        let src = err.to_string();
        LoadErrorKind::Wire { source: err }.whole(src)
    })
}

/// Instantiate a static system through the [`Registry`] factory.
///
/// A value-tree spec passes its params straight through as
/// [`StaticParams::Value`]; a config-less one decodes the entry's defaults via
/// [`StaticParams::None`].
fn resolve_static(
    spec: &SystemSpec,
    registry: &Registry,
    graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    // `type=` and non-postcard params are guaranteed for a static system by
    // `validate`; only the registry lookup (`UnknownType`) is left here.
    let ty = spec
        .ty
        .as_deref()
        .expect("validate() requires a static system's type");
    let factory = registry
        .factories
        .get(ty)
        .ok_or_else(|| LoadErrorKind::UnknownType { ty: ty.to_string() }.whole(system_src(spec)))?;
    let snippet;
    let params = match &spec.params {
        ParamSource::Postcard(_) => {
            unreachable!("validate() rejects postcard params on a static system")
        }
        ParamSource::Value(value) => {
            snippet = system_src(spec);
            StaticParams::Value {
                value,
                src: &snippet,
            }
        }
        ParamSource::None => StaticParams::None,
    };
    let node = (factory.factory)(&mut LoadCtx {
        params,
        name: &spec.name,
        msgs: &registry.msgs,
    })?;
    let desc = node.desc.clone();
    let handle = graph.push_node(node);
    Ok((handle, desc))
}

/// The per-resolve cache of opened packs, keyed by artifact id, so an
/// artifact serving several systems or occupants is opened (and its pack
/// constructed) exactly once.
#[derive(Default)]
struct PackCache {
    packs: HashMap<String, crate::dl::DlPack>,
}

impl PackCache {
    /// The opened pack for `artifact_id`, opening it on first use.
    fn open(
        &mut self,
        wiring: &Wiring,
        artifact_id: &str,
        owner: &str,
        src: &str,
        span: SourceSpan,
    ) -> Result<&crate::dl::DlPack, LoadError> {
        if !self.packs.contains_key(artifact_id) {
            let artifact = find_built_artifact(wiring, artifact_id, owner, src, span)?;
            let path = artifact
                .path
                .as_ref()
                .expect("checked by find_built_artifact");
            let pack = crate::dl::DlPack::open(path).map_err(|source| {
                LoadErrorKind::DlOpen {
                    system: owner.to_string(),
                    artifact: artifact_id.to_string(),
                    source: Box::new(source),
                }
                .at(src, span)
            })?;
            self.packs.insert(artifact_id.to_string(), pack);
        }
        Ok(&self.packs[artifact_id])
    }
}

/// The two ways an artifact self-describes at resolve: a pack opened in
/// process, or the manifest a describe worker reported for a `process=#true`
/// system or slot (whose artifacts the host never dlopens).
pub(super) enum EntrySource<'a> {
    Opened {
        pack: &'a crate::dl::DlPack,
        artifact: &'a str,
    },
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Described {
        entries: &'a [crate::abi::PackEntryDesc],
        artifact: &'a str,
    },
}

impl EntrySource<'_> {
    /// The exported entry names, in manifest order.
    fn entry_names(&self) -> Vec<String> {
        match self {
            EntrySource::Opened { pack, .. } => pack.system_names().map(str::to_string).collect(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            EntrySource::Described { entries, .. } => entries
                .iter()
                .map(|e| e.descriptor.name.to_string())
                .collect(),
        }
    }

    /// The sole exported entry name, in which case a spec may omit `type=`.
    fn sole_entry(&self) -> Option<&str> {
        match self {
            EntrySource::Opened { pack, .. } => pack.sole_system(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            EntrySource::Described { entries, .. } => match entries {
                [only] => Some(only.descriptor.name.as_str()),
                _ => None,
            },
        }
    }

    /// The artifact id, for diagnostics.
    fn artifact(&self) -> &str {
        match self {
            EntrySource::Opened { artifact, .. } => artifact,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            EntrySource::Described { artifact, .. } => artifact,
        }
    }
}

/// What [`resolve_occupant`] selected for registration: the loaded handle on
/// the dl path (the builder takes it by value), or the descriptor alone on
/// the process path (a worker owns the code; the host keeps the shape).
pub(super) enum OccupantEntry {
    Opened(DlSystem),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Described(SystemDescriptor),
}

impl OccupantEntry {
    /// The loaded handle behind an [`EntrySource::Opened`] resolve.
    fn opened(self) -> DlSystem {
        match self {
            OccupantEntry::Opened(loaded) => loaded,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            OccupantEntry::Described(_) => unreachable!("selected from an opened pack"),
        }
    }

    /// The descriptor behind an [`EntrySource::Described`] resolve.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn described(self) -> SystemDescriptor {
        match self {
            OccupantEntry::Described(desc) => desc,
            OccupantEntry::Opened(_) => unreachable!("selected from a describe worker"),
        }
    }
}

/// Select `entry` from an artifact's self-description (or its sole entry
/// when the spec named none) and encode `params` against the entry's
/// exported `Params` schema — the one open→select→encode path behind
/// [`resolve_dl`], [`resolve_proc`], and both of [`resolve_slot`]'s occupant
/// loops.
///
/// `require_reloadable` is set only for slot occupants, which are
/// instantiated on every load; a wired system is instantiated once, so a
/// non-reloadable `.state(...)` entry is legal there. `owner` names the
/// `system` or `slot` instance in diagnostics.
pub(super) fn resolve_occupant(
    source: &EntrySource<'_>,
    entry: Option<&str>,
    params: &ParamSource,
    owner: &str,
    require_reloadable: bool,
    src: &str,
    span: SourceSpan,
) -> Result<(OccupantEntry, Vec<u8>), LoadError> {
    let name = match entry {
        Some(name) => name,
        None => source.sole_entry().ok_or_else(|| {
            LoadErrorKind::PackTypeRequired {
                system: owner.to_string(),
                artifact: source.artifact().to_string(),
                available: source.entry_names().join(", "),
            }
            .at(src, span)
        })?,
    };
    let pack_system = |source: crate::dl::DlError| {
        LoadErrorKind::PackSystem {
            system: owner.to_string(),
            source: Box::new(source),
        }
        .at(src, span)
    };
    let not_reloadable = || {
        LoadErrorKind::OccupantNotReloadable {
            slot: owner.to_string(),
            occupant: name.to_string(),
        }
        .at(src, span)
    };
    match source {
        EntrySource::Opened { pack, .. } => {
            let loaded = pack.system(name).map_err(pack_system)?;
            if require_reloadable && !loaded.reloadable() {
                return Err(not_reloadable());
            }
            let params = encode_occupant_params(
                params,
                loaded.params_schema(),
                loaded.params_default(),
                owner,
            )?;
            Ok((OccupantEntry::Opened(loaded), params))
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        EntrySource::Described { entries, .. } => {
            let meta = entries
                .iter()
                .find(|e| e.descriptor.name == name)
                .ok_or_else(|| {
                    pack_system(crate::dl::DlError::UnknownPackSystem {
                        name: name.to_string(),
                        available: source.entry_names(),
                    })
                })?;
            if require_reloadable && !meta.reloadable {
                return Err(not_reloadable());
            }
            let params = encode_occupant_params(
                params,
                &meta.params_schema,
                meta.params_default.as_deref(),
                owner,
            )?;
            Ok((OccupantEntry::Described(meta.descriptor.clone()), params))
        }
    }
}

/// Resolve a [`ParamSource`] to canonical postcard bytes against an entry's
/// exported `Params` schema and declared defaults. A value tree is
/// schema-encoded (the host never links the type), producing the same bytes
/// a typed builder's `Postcard` source carries; an absent config takes the
/// defaults verbatim.
fn encode_occupant_params(
    params: &ParamSource,
    schema: &postcard_schema::schema::owned::OwnedNamedType,
    defaults: Option<&[u8]>,
    owner: &str,
) -> Result<Vec<u8>, LoadError> {
    Ok(match params {
        ParamSource::None => defaults.unwrap_or_default().to_vec(),
        ParamSource::Postcard(bytes) => bytes.clone(),
        ParamSource::Value(value) => encode_value_params(value, schema, owner, defaults)?,
    })
}

/// Load a pack entry and register it via
/// [`InitGraph::add_dl_cyclic`](crate::coordinator::init::InitGraph::add_dl_cyclic). The artifact is opened once per
/// resolve (the cache) and the reconstructed descriptor is returned for edge
/// validation.
fn resolve_dl(
    spec: &SystemSpec,
    artifact_id: &str,
    wiring: &Wiring,
    packs: &mut PackCache,
    graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    let src = system_src(spec);
    let span: SourceSpan = (0, src.len()).into();
    let pack = packs.open(wiring, artifact_id, &spec.name, &src, span)?;
    let source = EntrySource::Opened {
        pack,
        artifact: artifact_id,
    };
    let (entry, params) = resolve_occupant(
        &source,
        spec.ty.as_deref(),
        &spec.params,
        &spec.name,
        false,
        &src,
        span,
    )?;
    let loaded = entry.opened();
    let desc = loaded.descriptor().clone();
    let handle = graph.add_dl_cyclic(&spec.name, loaded, params);
    Ok((handle, desc))
}

/// Find an [`Artifact`] by id and require its built `path`, the shared front
/// of the dl and process resolve paths.
fn find_built_artifact<'w>(
    wiring: &'w Wiring,
    artifact_id: &str,
    owner: &str,
    src: &str,
    span: SourceSpan,
) -> Result<&'w Artifact, LoadError> {
    let artifact = wiring
        .artifacts
        .iter()
        .find(|a| a.id == artifact_id)
        .ok_or_else(|| {
            LoadErrorKind::UnknownArtifact {
                system: owner.to_string(),
                artifact: artifact_id.to_string(),
            }
            .at(src, span)
        })?;
    if artifact.path.is_none() {
        return Err(LoadErrorKind::ArtifactNotBuilt {
            artifact: artifact_id.to_string(),
        }
        .at(src, span));
    }
    Ok(artifact)
}

/// Resolve a `process=#true` system (`docs/process-systems.md`): run a
/// **describe-mode worker** over the built artifact — the host never dlopens
/// it — decode the descriptor and `Params` schema from the worker's bytes,
/// encode the spec's params against that schema, and register through
/// [`InitGraph::add_proc_cyclic`](crate::coordinator::init::InitGraph::add_proc_cyclic). The run worker is spawned later,
/// at `build()`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn resolve_proc(
    spec: &SystemSpec,
    artifact_id: &str,
    wiring: &Wiring,
    graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    let src = system_src(spec);
    let span: SourceSpan = (0, src.len()).into();
    let artifact = find_built_artifact(wiring, artifact_id, &spec.name, &src, span)?;
    let path = artifact
        .path
        .as_ref()
        .expect("checked by find_built_artifact");
    let proc_describe = |detail: String| {
        LoadErrorKind::ProcDescribe {
            system: spec.name.clone(),
            artifact: artifact_id.to_string(),
            detail,
        }
        .at(src.clone(), span)
    };
    let bytes = crate::proc::host::describe_via_worker(None, path)
        .map_err(|e| proc_describe(e.to_string()))?;
    let entries: Vec<crate::abi::PackEntryDesc> =
        crate::dl::decode_pack_manifest(&bytes).map_err(|e| proc_describe(e.to_string()))?;
    let source = EntrySource::Described {
        entries: &entries,
        artifact: artifact_id,
    };
    let (entry, params) = resolve_occupant(
        &source,
        spec.ty.as_deref(),
        &spec.params,
        &spec.name,
        false,
        &src,
        span,
    )?;
    let desc = entry.described();
    let entry_name = desc.name.to_string();
    let handle = graph.add_proc_cyclic(&spec.name, desc.clone(), path.clone(), entry_name, params);
    Ok((handle, desc))
}

/// Without a cross-process futex there is no worker to describe or run.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn resolve_proc(
    spec: &SystemSpec,
    _artifact_id: &str,
    _wiring: &Wiring,
    _graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    Err(LoadErrorKind::ProcessUnsupported {
        name: spec.name.clone(),
    }
    .whole(system_src(spec)))
}

/// Enforce generated-stub freshness: for each artifact whose stub module
/// recorded a `manifest_hash`, compare it against the live pack manifest and
/// fail with [`LoadErrorKind::StaleStubs`] on a mismatch. Artifacts with no
/// recorded hash (builder-authored, hand-written `pack()` handles) or no built
/// path yet are skipped; the dlopen path still opens them.
fn check_manifest_hashes(wiring: &Wiring) -> Result<(), LoadError> {
    for artifact in &wiring.artifacts {
        let Some(recorded) = artifact.manifest_hash.as_deref() else {
            continue;
        };
        let Some(path) = artifact.path.as_deref() else {
            continue;
        };
        // The live manifest is the sidecar the build driver wrote (no dlopen);
        // absent that, describe the built object in process — resolve dlopens
        // it momentarily anyway.
        let bytes = match crate::dl::manifest_sidecar_bytes(path) {
            Some(bytes) => bytes,
            None => match crate::dl::describe_raw(path) {
                Ok(bytes) => bytes,
                // A describe failure is not a staleness verdict; let the
                // dlopen path surface the real load error.
                Err(_) => continue,
            },
        };
        if stubgen::manifest_hash(&bytes) != recorded {
            return Err(LoadErrorKind::StaleStubs {
                artifact: artifact.id.clone(),
            }
            .bare());
        }
    }
    Ok(())
}

/// The ` @ file:line:col` suffix locating a spec in its source document, or
/// nothing for a spec with no recorded [`SourceRef`] (the Rust builder).
pub(super) fn src_anchor(src: Option<&SourceRef>) -> String {
    match src {
        Some(SourceRef {
            file: Some(file),
            line,
            col,
        }) => format!("  @ {file}:{line}:{col}"),
        Some(SourceRef {
            file: None,
            line,
            col,
        }) => format!("  @ {line}:{col}"),
        None => String::new(),
    }
}

/// A best-effort source snippet for a system's resolve-time errors (a
/// [`Wiring`] carries no original document text, only [`SourceRef`] anchors).
pub(super) fn system_src(spec: &SystemSpec) -> String {
    let anchor = src_anchor(spec.src.as_ref());
    match &spec.ty {
        Some(ty) => format!("system \"{}\" type=\"{}\"{anchor}", spec.name, ty),
        None => format!("system \"{}\"{anchor}", spec.name),
    }
}

/// A best-effort source snippet for a slot's resolve-time errors.
pub(super) fn slot_src(slot: &SlotSpec) -> String {
    format!("slot \"{}\"{}", slot.name, src_anchor(slot.src.as_ref()))
}

/// The artifact whose pack exports `occ.occupant`: the `artifact=` the allow
/// line named, or (absent one) the unique artifact exporting an entry of
/// that name. No match or more than one is a clean error either way.
fn occupant_artifact(
    wiring: &Wiring,
    packs: &mut PackCache,
    occ: &AllowedOccupantSpec,
    slot: &str,
    src: &str,
    span: SourceSpan,
) -> Result<String, LoadError> {
    if let Some(artifact) = &occ.artifact {
        return Ok(artifact.clone());
    }
    let mut matches: Vec<String> = Vec::new();
    for artifact in &wiring.artifacts {
        let pack = packs.open(wiring, &artifact.id, slot, src, span)?;
        if pack.system_names().any(|n| n == occ.occupant) {
            matches.push(artifact.id.clone());
        }
    }
    match matches.as_slice() {
        [only] => Ok(only.clone()),
        _ => Err(LoadErrorKind::OccupantAmbiguous {
            slot: slot.to_string(),
            occupant: occ.occupant.clone(),
            matches,
        }
        .at(src, span)),
    }
}

/// Map a [`SlotConfigError`] — from the shared pure-spec validation or from
/// [`plan_slot`](crate::coordinator::slot::plan_slot) — onto this front-end's spanned slot
/// diagnostics.
pub(super) fn slot_config_error(
    err: SlotConfigError,
    slot: &SlotSpec,
    src: &str,
    span: SourceSpan,
) -> LoadError {
    let name = slot.name.clone();
    let src = src.to_string();
    match err {
        SlotConfigError::Empty => LoadErrorKind::EmptySlot { slot: name }.at(src, span),
        SlotConfigError::UnknownInitial { occupant, allowed } => {
            LoadErrorKind::UnknownInitialOccupant {
                slot: name,
                occupant,
                allowed,
            }
            .at(src, span)
        }
        // A mount-reserved port is an occupant-contract defect too: the
        // artifact predates the pack ABI and must be rebuilt.
        SlotConfigError::OccupantMismatch { occupant, .. }
        | SlotConfigError::ReservedPort { occupant, .. } => LoadErrorKind::SlotOccupantMismatch {
            slot: name,
            occupant,
        }
        .at(src, span),
        SlotConfigError::MixedBacking => {
            unreachable!("resolve_slot sources every occupant of a slot from one backing arm")
        }
    }
}

/// Resolve a [`SlotSpec`] into a registered slot: each allowed occupant goes
/// through [`resolve_occupant`] (or, for a `process=#true` slot,
/// [`describe_occupants`]), and the descriptor returned for the edges pass is
/// the one [`plan_slot`](crate::coordinator::slot::plan_slot) derives, not a
/// raw occupant descriptor.
fn resolve_slot(
    slot: &SlotSpec,
    wiring: &Wiring,
    packs: &mut PackCache,
    graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    let src = slot_src(slot);
    let span: SourceSpan = (0, src.len()).into();

    // The pure-spec checks (non-empty allow set, `initial` inside it) ran in
    // `validate` before any artifact was opened. Open and param-encode each
    // allowed occupant; a process slot takes the describe-worker path instead,
    // and the backing decides the slot's mode in `plan_slot`.
    let allowed: Vec<AllowedOccupant> = if slot.process {
        describe_occupants(slot, wiring, &src, span)?
    } else {
        let mut allowed = Vec::with_capacity(slot.allow.len());
        for occ in &slot.allow {
            let artifact_id = occupant_artifact(wiring, packs, occ, &slot.name, &src, span)?;
            let pack = packs.open(wiring, &artifact_id, &slot.name, &src, span)?;
            let source = EntrySource::Opened {
                pack,
                artifact: &artifact_id,
            };
            let (entry, params) = resolve_occupant(
                &source,
                Some(&occ.occupant),
                &occ.params,
                &slot.name,
                true,
                &src,
                span,
            )?;
            allowed.push(AllowedOccupant::dl(
                occ.occupant.clone(),
                entry.opened(),
                params,
            ));
        }
        allowed
    };

    let initial = match &slot.initial {
        None => None,
        Some(i) => match i.state {
            SlotInitState::Empty => None,
            SlotInitState::Loaded => Some(InitialOccupant {
                occupant: i.occupant.clone(),
                start: false,
            }),
            SlotInitState::Running => Some(InitialOccupant {
                occupant: i.occupant.clone(),
                start: true,
            }),
        },
    };

    // `plan_slot` is the one place the registered contract is derived and the
    // descriptor-level checks run, so this front-end cannot drift from it.
    let (registered, ports, process) =
        plan_slot(&slot.name, &allowed).map_err(|e| slot_config_error(e, slot, &src, span))?;
    let desc = registered.clone();
    let handle = graph.push_node(Node {
        name: slot.name.clone(),
        desc: registered,
        bind: SystemBind::Slot(SlotReg {
            allowed,
            initial,
            ports,
            process,
        }),
    });

    // Every declared `input`/`output` frame must name an edge-connected
    // registered port. The runner-held tail (slot control, slot status, the
    // sequence channels) is not part of the user contract; the edge outputs
    // do include the implicit status and log tail, which a declaration may
    // name but need not.
    use crate::descriptor::PortConn;
    for frame in &slot.inputs {
        if !desc
            .inputs
            .iter()
            .any(|p| p.conn == PortConn::Edge && &p.name == frame)
        {
            return Err(LoadErrorKind::SlotContractMismatch {
                slot: slot.name.clone(),
                dir: "input",
                frame: frame.clone(),
            }
            .at(src.clone(), span));
        }
    }
    for frame in &slot.outputs {
        if !desc
            .outputs
            .iter()
            .any(|p| p.conn == PortConn::Edge && &p.name == frame)
        {
            return Err(LoadErrorKind::SlotContractMismatch {
                slot: slot.name.clone(),
                dir: "output",
                frame: frame.clone(),
            }
            .at(src.clone(), span));
        }
    }

    Ok((handle, desc))
}

/// Resolve a process slot's allowed set (`docs/process-slots.md` §8): the
/// [`resolve_proc`] recipe once per allowed occupant, so the host never
/// dlopens any occupant artifact. The resulting [`AllowedOccupant`]s carry
/// [`OccupantBacking::Artifact`], which is what makes [`plan_slot`] register
/// the slot process-mode.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn describe_occupants(
    slot: &SlotSpec,
    wiring: &Wiring,
    src: &str,
    span: SourceSpan,
) -> Result<Vec<AllowedOccupant>, LoadError> {
    // Manifests by artifact id, so a slot allowing several entries of one
    // pack runs one describe worker for it, not one per occupant.
    let mut manifests: HashMap<String, Vec<crate::abi::PackEntryDesc>> = HashMap::new();
    fn describe(
        manifests: &mut HashMap<String, Vec<crate::abi::PackEntryDesc>>,
        wiring: &Wiring,
        slot: &SlotSpec,
        artifact_id: &str,
        src: &str,
        span: SourceSpan,
    ) -> Result<(), LoadError> {
        if manifests.contains_key(artifact_id) {
            return Ok(());
        }
        let artifact = find_built_artifact(wiring, artifact_id, &slot.name, src, span)?;
        let path = artifact
            .path
            .as_ref()
            .expect("checked by find_built_artifact");
        let proc_describe = |detail: String| {
            LoadErrorKind::ProcDescribe {
                system: slot.name.clone(),
                artifact: artifact_id.to_string(),
                detail,
            }
            .at(src, span)
        };
        let bytes = crate::proc::host::describe_via_worker(None, path)
            .map_err(|e| proc_describe(e.to_string()))?;
        let entries =
            crate::dl::decode_pack_manifest(&bytes).map_err(|e| proc_describe(e.to_string()))?;
        manifests.insert(artifact_id.to_string(), entries);
        Ok(())
    }

    let mut allowed = Vec::with_capacity(slot.allow.len());
    for occ in &slot.allow {
        // The occupant's artifact: named, or the unique artifact whose
        // manifest exports the entry (each described through a worker; the
        // host never dlopens a process slot's artifacts).
        let artifact_id = match &occ.artifact {
            Some(id) => id.clone(),
            None => {
                for artifact in &wiring.artifacts {
                    describe(&mut manifests, wiring, slot, &artifact.id, src, span)?;
                }
                let matches: Vec<String> = manifests
                    .iter()
                    .filter(|(_, entries)| {
                        entries.iter().any(|e| e.descriptor.name == occ.occupant)
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                match matches.as_slice() {
                    [only] => only.clone(),
                    _ => {
                        return Err(LoadErrorKind::OccupantAmbiguous {
                            slot: slot.name.clone(),
                            occupant: occ.occupant.clone(),
                            matches,
                        }
                        .at(src, span));
                    }
                }
            }
        };
        describe(&mut manifests, wiring, slot, &artifact_id, src, span)?;
        let source = EntrySource::Described {
            entries: &manifests[&artifact_id],
            artifact: &artifact_id,
        };
        let (entry, params) = resolve_occupant(
            &source,
            Some(&occ.occupant),
            &occ.params,
            &slot.name,
            true,
            src,
            span,
        )?;
        let artifact = find_built_artifact(wiring, &artifact_id, &slot.name, src, span)?;
        let path = artifact
            .path
            .as_ref()
            .expect("checked by find_built_artifact");
        allowed.push(AllowedOccupant {
            name: occ.occupant.clone(),
            params,
            descriptor: entry.described(),
            backing: OccupantBacking::Artifact(path.clone()),
        });
    }
    Ok(allowed)
}

/// Without a cross-process futex there is no worker to describe or spawn, so
/// a process slot is rejected like a process system.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn describe_occupants(
    slot: &SlotSpec,
    _wiring: &Wiring,
    src: &str,
    span: SourceSpan,
) -> Result<Vec<AllowedOccupant>, LoadError> {
    Err(LoadErrorKind::ProcessUnsupported {
        name: slot.name.clone(),
    }
    .at(src, span))
}

/// Convert the serializable [`CoordinatorSpec`] into the runtime
/// [`CoordinatorConfig`].
fn coordinator_config(spec: &CoordinatorSpec) -> Result<CoordinatorConfig, LoadError> {
    let mut config = CoordinatorConfig {
        cycle_rate: spec.cycle_rate,
        ..CoordinatorConfig::default()
    };
    if let Some(depth) = spec.default_depth {
        config.default_depth = depth;
    }
    config.clock = match spec.clock {
        ClockSpec::Wall => ClockMode::Wall,
        ClockSpec::Simulated { dt_secs } => {
            let dt = Duration::try_from_secs_f64(dt_secs)
                .map_err(|_| LoadErrorKind::InvalidSimulatedStep { dt_secs }.bare())?;
            if dt.is_zero() {
                return Err(LoadErrorKind::InvalidSimulatedStep { dt_secs }.bare());
            }
            ClockMode::Simulated { dt }
        }
    };
    Ok(config)
}

/// Resolve a `msg=` edge's two endpoints jointly.
///
/// The token names the message type and is matched against each endpoint's
/// packet-port display names. An endpoint whose port carries an overridden
/// display name (a coordinator-minted channel such as `"commands"`) is then
/// matched by the packet id the token resolved to on the other endpoint. Only
/// when neither endpoint matches the token is the edge an
/// [`UnknownMsg`](LoadErrorKind::UnknownMsg).
fn resolve_msg_edge(
    instances: &HashMap<String, Instance>,
    src: &str,
    edge: &EdgeSpec,
    span: SourceSpan,
) -> Result<(PortRef, PortRef), LoadError> {
    let inst = |name: &str| {
        instances.get(name).ok_or_else(|| {
            LoadErrorKind::UnknownInstance {
                name: name.to_string(),
            }
            .at(src, span)
        })
    };
    let prod = inst(&edge.from)?;
    let cons = inst(&edge.to)?;

    let by_name = |ports: &[crate::descriptor::PortDesc], token: &str| {
        ports
            .iter()
            .find(|p| matches!(p.id(), PortId::Packet(_)) && p.name == token)
            .map(|p| p.id())
    };
    let by_id = |ports: &[crate::descriptor::PortDesc], id: PortId| {
        ports.iter().find(|p| p.id() == id).map(|p| p.id())
    };
    let unknown = |instance: &str, msg: &str| {
        LoadErrorKind::UnknownMsg {
            instance: instance.to_string(),
            msg: msg.to_string(),
        }
        .at(src, span)
    };

    let p_named = by_name(&prod.desc.outputs, &edge.out);
    let c_named = by_name(&cons.desc.inputs, &edge.in_);
    let (p_port, c_port) = match (p_named, c_named) {
        (Some(p), Some(c)) => (p, c),
        (Some(p), None) => (
            p,
            by_id(&cons.desc.inputs, p).ok_or_else(|| unknown(&edge.to, &edge.in_))?,
        ),
        (None, Some(c)) => (
            by_id(&prod.desc.outputs, c).ok_or_else(|| unknown(&edge.from, &edge.out))?,
            c,
        ),
        (None, None) => return Err(unknown(&edge.from, &edge.out)),
    };
    Ok((
        PortRef {
            system: prod.handle,
            port: p_port,
        },
        PortRef {
            system: cons.handle,
            port: c_port,
        },
    ))
}

/// Resolve one `(instance, port)` endpoint to a [`PortRef`], validating the
/// name against the instance descriptor's port list so a typo is a load error.
fn resolve_endpoint(
    instances: &HashMap<String, Instance>,
    src: &str,
    name: &str,
    port_name: &str,
    dir: Dir,
    span: SourceSpan,
) -> Result<PortRef, LoadError> {
    let inst = instances.get(name).ok_or_else(|| {
        LoadErrorKind::UnknownInstance {
            name: name.to_string(),
        }
        .at(src, span)
    })?;
    let ports = match dir {
        Dir::Out => &inst.desc.outputs,
        Dir::In => &inst.desc.inputs,
    };
    let id = PortId::Component(ComponentId::new(port_name));
    if !ports.iter().any(|p| p.id() == id) {
        return Err(LoadErrorKind::UnknownFrame {
            instance: name.to_string(),
            frame: port_name.to_string(),
        }
        .at(src, span));
    }
    Ok(PortRef {
        system: inst.handle,
        port: id,
    })
}
