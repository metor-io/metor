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
use std::sync::Arc;
use std::time::Duration;

use metor_proto::types::ComponentId;

use crate::coordinator::init::{InitGraph, Node, SystemBind, WasmReg};
use crate::coordinator::slot::{SlotReg, plan_slot};
use crate::coordinator::{
    AllowedOccupant, ClockMode, Coordinator, CoordinatorConfig, InitialOccupant, OccupantBacking,
    PortRef, SlotConfigError, SystemHandle,
};
use crate::dl::DlSystem;
use metor_fsw_2_core::{PortId, SystemDescriptor};

use super::error::{LoadError, LoadErrorKind};
use super::model::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec, ParamSource,
    SlotInitState, SlotSpec, StateSpec, SystemSpec, Wiring,
};
use super::registry::{LoadCtx, Registry, StaticParams};
use super::{encode_value_params, pack_module, validate};
use crate::ir::ArtifactKind;

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
/// A `Wiring` is a portable target description, so host-environment policy —
/// where a process worker's executable lives, where the shared-memory session
/// dir is rooted, how a worker's steps are timed and its crashes recovered — is
/// supplied here at [`resolve_with`] time rather than baked into the IR. These
/// are deployment decisions, not target topology, so they stay off the
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
/// from its built [`Artifact::path`] and registered as a loaded cyclic node.
///
/// Wiring faults are structured [`LoadError`] variants. Parameter decoding has
/// its own field-aware error variants.
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
    // The target namespace rides the registry/announce seam (`InitGraph::qualify`)
    // and is threaded into each static system's `configure` via `LoadCtx`.
    graph.namespace = wiring.coordinator.namespace.clone();

    // Serialized path-stripped so the telemetered topology matches the
    // bundle's `wiring.json` byte-for-byte regardless of the build tree.
    let ir_json = serde_json::to_string(&wiring.path_stripped())
        .expect("a resolvable Wiring serializes to JSON");
    graph.set_wiring_manifest(metor_proto_wkt::WiringManifest {
        ir_version: wiring.ir_version,
        ir_json,
    });

    // --- States pass: pack-shared states construct before any system, so
    //     an attached entry's create finds its instance in place. Each
    //     construction yields the token a by-name attach downcasts, keyed by
    //     the state's declaration name for the systems pass.
    let mut state_tokens: HashMap<&str, metor_fsw_2_core::AttachTarget> = HashMap::new();
    for spec in &wiring.states {
        let target = resolve_state(spec, registry)?;
        state_tokens.insert(spec.name.as_str(), target);
    }

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
    let mut wasm = WasmCache::default();
    let mut pending: Vec<PendingSynth> = Vec::new();
    for spec in &wiring.systems {
        // The artifact's kind picks the loader; the dl and proc arms must
        // never see a wasm module (dlopen on one would fail obscurely).
        let wasm_backed = spec.artifact.as_deref().is_some_and(|id| {
            wiring
                .artifacts
                .iter()
                .any(|a| a.id == id && a.kind == ArtifactKind::Wasm)
        });
        let (handle, desc) = match (&spec.artifact, spec.process) {
            (Some(artifact_id), false) if wasm_backed => {
                resolve_wasm(spec, artifact_id, wiring, &mut wasm, &mut pending, &mut graph)?
            }
            (Some(_), true) if wasm_backed => {
                return Err(LoadErrorKind::WasmSystem(
                    format!(
                        "system `{}`: `process=#true` is redundant for a wasm system \
                         (the interpreter already isolates it)",
                        spec.name
                    )
                    .into_boxed_str(),
                )
                .bare());
            }
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
                resolve_static(spec, registry, &state_tokens, &mut graph)?
            }
        };
        // `validate` guaranteed instance-name uniqueness, so the insert is new.
        instances.insert(spec.name.clone(), Instance { handle, desc });
    }

    // --- Slots pass: a slot `connect`s by name like a system, so it joins
    //     `instances` before the edges pass.
    for slot in &wiring.slots {
        let (handle, desc) = resolve_slot(slot, wiring, &mut packs, &mut wasm, &mut graph)?;
        instances.insert(slot.name.clone(), Instance { handle, desc });
    }

    // --- Deferred receive-all systems: the last cyclic registrations, still
    //     ahead of the edges pass, which only needs the finished instance map.
    for spec in deferred {
        let (handle, desc) = resolve_static(spec, registry, &state_tokens, &mut graph)?;
        instances.insert(spec.name.clone(), Instance { handle, desc });
    }

    // --- Synthesized edges: the bindings a compiled Python entry bakes are
    //     wired only now, over the finished instance map, so a spec's list
    //     position never decides whether its producer is visible. Staleness
    //     stays uniform with explicit edges: a compiled system listed ahead
    //     of a producer it reads fails at build like any native pair.
    let added: HashMap<(&str, &str), &str> = pending
        .iter()
        .map(|p| ((p.artifact.as_str(), p.entry.as_str()), p.instance.as_str()))
        .collect();
    for p in &pending {
        let module = &wasm.modules[&p.artifact];
        let manifest = module.expr.as_ref().expect("pending implies a compiled module");
        let system = manifest.system(&p.entry).expect("pending keys a manifest entry");
        synth_edges(system, manifest, &instances, &added, p, &mut graph)?;
    }

    // Every declared state must have gained an attachment by now (attach
    // counts at entry create): a state serving nobody is a config defect —
    // a link server with no downlink serves silence — so it fails like any
    // other wiring mistake.
    for spec in &wiring.states {
        let entry = registry
            .states
            .get(spec.ty.as_str())
            .expect("the states pass resolved this type");
        if entry.borrow().cell.attached() == 0 {
            return Err(LoadErrorKind::StateUnused {
                name: spec.name.clone(),
                ty: spec.ty.clone(),
            }
            .bare());
        }
    }

    // --- Edges pass ------------------------------------------------------
    for edge in &wiring.edges {
        let (producer, consumer) = match edge.kind {
            EdgeKind::Frame => (
                resolve_endpoint(&instances, &edge.from, &edge.out, Dir::Out)?,
                resolve_endpoint(&instances, &edge.to, &edge.in_, Dir::In)?,
            ),
            EdgeKind::Msg => resolve_msg_edge(&instances, edge)?,
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

    graph
        .build()
        .map_err(|source| LoadErrorKind::Wire { source }.bare())
}

/// Construct one pack-shared state through its registered
/// [`StateEntry`](crate::StateEntry): decode the spec's params off the value
/// surface and run the state's init fn. Construction failure (a listener
/// bind) is a load error, not a runtime one. Returns the
/// [`AttachTarget`](metor_fsw_2_core::AttachTarget) a by-name attach downcasts.
fn resolve_state(
    spec: &StateSpec,
    registry: &Registry,
) -> Result<metor_fsw_2_core::AttachTarget, LoadError> {
    let Some(entry) = registry.states.get(spec.ty.as_str()) else {
        let mut available: Vec<&str> = registry.states.keys().copied().collect();
        available.sort_unstable();
        return Err(LoadErrorKind::UnknownStateType {
            name: spec.name.clone(),
            ty: spec.ty.clone(),
            available: available.join(", "),
        }
        .bare());
    };
    // A paramless spec conforms an empty object against the state's schema,
    // the same all-defaults decode a paramless pack entry gets.
    let empty = serde_json::Value::Object(serde_json::Map::new());
    let value = match &spec.params {
        ParamSource::Postcard(_) => {
            unreachable!("validate() rejects postcard params on a state")
        }
        ParamSource::Value(value) => value,
        ParamSource::None => &empty,
    };
    let params = metor_fsw_2_core::EntryParams::Value {
        value,
        src: "",
        name: &spec.name,
        msgs: &registry.msgs,
        attach: None,
    };
    entry.borrow_mut().create(params).map_err(|e| match e {
        metor_fsw_2_core::MakeError::Params(e) => (*e).into(),
        other => LoadErrorKind::StateInit {
            name: spec.name.clone(),
            ty: spec.ty.clone(),
            message: other.to_string(),
        }
        .bare(),
    })?;
    let entry = entry.borrow();
    Ok(metor_fsw_2_core::AttachTarget {
        ty: entry.name(),
        token: entry.token.clone(),
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
    state_tokens: &HashMap<&str, metor_fsw_2_core::AttachTarget>,
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
        .ok_or_else(|| LoadErrorKind::UnknownType { ty: ty.to_string() }.bare())?;

    // Attach/shared consistency: a shared-state entry needs an `attach`, and a
    // plain entry must not carry one. `validate` already proved a present
    // `attach` names a declared state, so the map lookup here always hits.
    let attach = match (factory.shared, spec.attach.as_deref()) {
        (true, Some(name)) => Some(
            state_tokens
                .get(name)
                .expect("validate() proved this attach names a declared state"),
        ),
        (true, None) => {
            return Err(LoadErrorKind::MissingAttach {
                system: spec.name.clone(),
            }
            .bare());
        }
        (false, Some(attach)) => {
            return Err(LoadErrorKind::AttachOnNonSharedSystem {
                system: spec.name.clone(),
                attach: attach.to_string(),
            }
            .bare());
        }
        (false, None) => None,
    };

    let params = match &spec.params {
        ParamSource::Postcard(_) => {
            unreachable!("validate() rejects postcard params on a static system")
        }
        ParamSource::Value(value) => StaticParams::Value(value),
        ParamSource::None => StaticParams::None,
    };
    let node = (factory.factory)(&mut LoadCtx {
        params,
        name: &spec.name,
        msgs: &registry.msgs,
        namespace: graph.namespace.as_deref(),
        attach,
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
    ) -> Result<&crate::dl::DlPack, LoadError> {
        if !self.packs.contains_key(artifact_id) {
            let artifact = find_built_artifact(wiring, artifact_id, owner)?;
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
                .bare()
            })?;
            self.packs.insert(artifact_id.to_string(), pack);
        }
        Ok(&self.packs[artifact_id])
    }
}

/// The per-resolve cache of described wasm artifacts, the [`PackCache`] twin:
/// bytes read once, the pack manifest decoded through a short-lived describe
/// instance, and — for a compiled Python pack — the expr manifest read
/// alongside, since edge synthesis and `@rng` seeding both key off it.
#[derive(Default)]
struct WasmCache {
    modules: HashMap<String, WasmModule>,
}

/// One described wasm artifact.
struct WasmModule {
    bytes: Arc<Vec<u8>>,
    entries: Vec<metor_fsw_2_core::abi::PackEntryDesc>,
    /// The expr manifest a compiled Python pack also bakes; `None` for an
    /// ordinary Rust-authored pack.
    expr: Option<metor_expr::Manifest>,
}

impl WasmCache {
    /// The described module for `artifact_id`, reading and describing it on
    /// first use.
    fn open(
        &mut self,
        wiring: &Wiring,
        artifact_id: &str,
        owner: &str,
        max_memory: usize,
    ) -> Result<&WasmModule, LoadError> {
        if !self.modules.contains_key(artifact_id) {
            let bad = |detail: String| {
                LoadErrorKind::WasmSystem(
                    format!("`{owner}`: wasm artifact `{artifact_id}`: {detail}").into_boxed_str(),
                )
                .bare()
            };
            let artifact = find_built_artifact(wiring, artifact_id, owner)?;
            let path = artifact
                .path
                .as_ref()
                .expect("checked by find_built_artifact");
            let bytes =
                std::fs::read(path).map_err(|e| bad(format!("reading {}: {e}", path.display())))?;
            let mut pack = crate::wasm::WasmPack::open_with_memory_limit(
                &bytes,
                crate::coordinator::slot::WASM_SETUP_FUEL,
                max_memory,
            )
            .map_err(|e| bad(e.to_string()))?;
            let entries = pack.manifest().systems.clone();
            let expr = match pack.expr_manifest_bytes().map_err(|e| bad(e.to_string()))? {
                Some(manifest) => Some(
                    metor_expr::describe(&manifest)
                        .map_err(|e| bad(format!("expr manifest: {e}")))?,
                ),
                None => None,
            };
            self.modules.insert(
                artifact_id.to_string(),
                WasmModule {
                    bytes: Arc::new(bytes),
                    entries,
                    expr,
                },
            );
        }
        Ok(&self.modules[artifact_id])
    }
}

/// Select a wasm entry by name (or the sole export when the spec named
/// none), the wasm shape of [`resolve_occupant`]'s select step.
fn wasm_entry<'e>(
    entries: &'e [metor_fsw_2_core::abi::PackEntryDesc],
    ty: Option<&str>,
    bad: &dyn Fn(String) -> LoadError,
) -> Result<(u32, &'e metor_fsw_2_core::abi::PackEntryDesc), LoadError> {
    let available = || {
        entries
            .iter()
            .map(|e| e.descriptor.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match ty {
        Some(name) => entries
            .iter()
            .enumerate()
            .find(|(_, e)| e.descriptor.name == name)
            .map(|(i, e)| (i as u32, e))
            .ok_or_else(|| {
                bad(format!(
                    "module exports no `{name}` entry (available: {})",
                    available()
                ))
            }),
        None => match entries {
            [only] => Ok((0, only)),
            _ => Err(bad(format!(
                "`type=` is required (module exports: {})",
                available()
            ))),
        },
    }
}

/// One compiled Python instance awaiting edge synthesis, recorded during the
/// systems pass and wired after every instance is registered.
struct PendingSynth {
    artifact: String,
    /// The pack entry (declaration) name — the manifest's key.
    entry: String,
    /// The instance name of this registration, the spec's own (possibly
    /// scope-prefixed, and free to differ from the entry name).
    instance: String,
    handle: SystemHandle,
}

/// Resolve a wired wasm system (plan D6): describe the artifact once per
/// resolve, select the entry, encode its params — a compiled Python entry
/// with an `@rng` slot takes a host-entropy seed on the params channel
/// instead (plan D9) — and register it. The edges its compiled bindings
/// imply (plan D7) are queued on `pending` and synthesized after the systems
/// pass.
fn resolve_wasm(
    spec: &SystemSpec,
    artifact_id: &str,
    wiring: &Wiring,
    wasm: &mut WasmCache,
    pending: &mut Vec<PendingSynth>,
    graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    let max_memory = graph.config.wasm_memory_limit_bytes;
    let module = wasm.open(wiring, artifact_id, &spec.name, max_memory)?;
    let bad = |detail: String| {
        LoadErrorKind::WasmSystem(
            format!(
                "system `{}` (artifact `{artifact_id}`): {detail}",
                spec.name
            )
            .into_boxed_str(),
        )
        .bare()
    };
    let (index, entry) = wasm_entry(&module.entries, spec.ty.as_deref(), &bad)?;
    let compiled = module
        .expr
        .as_ref()
        .and_then(|m| m.system(&entry.descriptor.name));
    let params = match compiled {
        Some(system)
            if system
                .state
                .iter()
                .any(|s| s.name == metor_expr::state::RNG_FIELD) =>
        {
            // Fresh per boot and distinct per instance; the guest's `create`
            // stores it into the `@rng` slot before the seed guard runs.
            let entropy =
                metor_proto::types::Timestamp::now().0 as u64 ^ ComponentId::new(&spec.name).0;
            entropy.to_le_bytes().to_vec()
        }
        _ => encode_occupant_params(
            &spec.params,
            &entry.params_schema,
            entry.params_default.as_deref(),
            &spec.name,
        )?,
    };
    let desc = entry.descriptor.clone();
    let handle = graph.add_wasm_cyclic(
        &spec.name,
        desc.clone(),
        WasmReg {
            bytes: module.bytes.clone(),
            index,
            params,
        },
    );
    if compiled.is_some() {
        pending.push(PendingSynth {
            artifact: artifact_id.to_string(),
            entry: desc.name.to_string(),
            instance: spec.name.clone(),
            handle,
        });
    }
    Ok((handle, desc))
}

/// Wire the edges a compiled entry's bindings imply (plan D7): one edge per
/// distinct producing port, in the same first-appearance order the compiler
/// grouped the descriptor's inputs by. The two walks share their key — the
/// binding list — so a mismatch is artifact/wiring drift and fails loudly.
/// A `Produced` binding names a *declaration*; `added` maps it to the
/// instance the declaration was registered under.
fn synth_edges(
    system: &metor_expr::System,
    manifest: &metor_expr::Manifest,
    instances: &HashMap<String, Instance>,
    added: &HashMap<(&str, &str), &str>,
    p: &PendingSynth,
    graph: &mut InitGraph,
) -> Result<(), LoadError> {
    let owner = p.instance.as_str();
    let consumer = p.handle;
    let desc = &instances[&p.instance].desc;
    let drift = |detail: String| {
        LoadErrorKind::WasmSystem(format!("Python system `{owner}`: {detail}").into_boxed_str())
            .bare()
    };
    let mut seen: Vec<(String, String)> = Vec::new();
    for port in &system.inputs {
        for binding in &port.bindings {
            let key = match binding {
                metor_expr::Binding::Component(path) => locate_producer(instances, path, owner)?,
                metor_expr::Binding::Produced { system: s, .. } => {
                    let producer = &manifest.systems[*s];
                    let instance = added
                        .get(&(p.artifact.as_str(), producer.name.as_str()))
                        .ok_or_else(|| {
                            drift(format!(
                                "bound declaration `{}` is not registered as a system",
                                producer.name
                            ))
                        })?;
                    (instance.to_string(), producer.output.name.clone())
                }
                metor_expr::Binding::Resampled { .. } => {
                    return Err(drift(
                        "artifact carries a resample stage the build gate rejects".into(),
                    ));
                }
            };
            if seen.contains(&key) {
                continue;
            }
            let Some(input) = desc.inputs.get(seen.len()) else {
                return Err(drift(format!(
                    "bindings imply more input ports than the descriptor's {}",
                    desc.inputs.len()
                )));
            };
            if input.name != key.1 {
                return Err(drift(format!(
                    "descriptor input `{}` does not match bound producer port `{}.{}`",
                    input.name, key.0, key.1
                )));
            }
            let producer = instances.get(&key.0).ok_or_else(|| {
                LoadErrorKind::UnknownInstance {
                    name: key.0.clone(),
                }
                .bare()
            })?;
            let out = producer
                .desc
                .outputs
                .iter()
                .find(|p| p.name == key.1)
                .ok_or_else(|| LoadErrorKind::UnknownFrame {
                    instance: key.0.clone(),
                    frame: key.1.clone(),
                })
                .map_err(LoadErrorKind::bare)?;
            graph.connect(
                PortRef {
                    system: producer.handle,
                    port: out.id(),
                },
                PortRef {
                    system: consumer,
                    port: input.id(),
                },
            );
            seen.push(key);
        }
    }
    if seen.len() != desc.inputs.len() {
        return Err(drift(format!(
            "bindings imply {} input ports, the descriptor declares {}",
            seen.len(),
            desc.inputs.len()
        )));
    }
    Ok(())
}

/// The producing `(instance, port)` behind a bound component path: the
/// longest registered instance name prefixing the path, then the output port
/// whose name heads the remainder.
fn locate_producer(
    instances: &HashMap<String, Instance>,
    path: &str,
    owner: &str,
) -> Result<(String, String), LoadError> {
    let bad = |detail: String| {
        LoadErrorKind::WasmSystem(
            format!("Python system `{owner}`: bound component `{path}` {detail}").into_boxed_str(),
        )
        .bare()
    };
    let mut best: Option<&str> = None;
    for name in instances.keys() {
        if path.starts_with(name.as_str())
            && path.as_bytes().get(name.len()) == Some(&b'.')
            && best.is_none_or(|b| name.len() > b.len())
        {
            best = Some(name);
        }
    }
    let instance = best.ok_or_else(|| bad("names no registered instance".into()))?;
    let rest = &path[instance.len() + 1..];
    let port = instances[instance]
        .desc
        .outputs
        .iter()
        .find(|p| {
            rest == p.name
                || rest
                    .strip_prefix(p.name.as_str())
                    .is_some_and(|r| r.starts_with('.'))
        })
        .ok_or_else(|| bad(format!("names no output port of `{instance}`")))?;
    Ok((instance.to_string(), port.name.clone()))
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
        entries: &'a [metor_fsw_2_core::abi::PackEntryDesc],
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
) -> Result<(OccupantEntry, Vec<u8>), LoadError> {
    let name = match entry {
        Some(name) => name,
        None => source.sole_entry().ok_or_else(|| {
            LoadErrorKind::PackTypeRequired {
                system: owner.to_string(),
                artifact: source.artifact().to_string(),
                available: source.entry_names().join(", "),
            }
            .bare()
        })?,
    };
    let pack_system = |source: crate::dl::DlError| {
        LoadErrorKind::PackSystem {
            system: owner.to_string(),
            source: Box::new(source),
        }
        .bare()
    };
    let not_reloadable = || {
        LoadErrorKind::OccupantNotReloadable {
            slot: owner.to_string(),
            occupant: name.to_string(),
        }
        .bare()
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
    let pack = packs.open(wiring, artifact_id, &spec.name)?;
    let source = EntrySource::Opened {
        pack,
        artifact: artifact_id,
    };
    let (entry, params) =
        resolve_occupant(&source, spec.ty.as_deref(), &spec.params, &spec.name, false)?;
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
            .bare()
        })?;
    if artifact.path.is_none() {
        return Err(LoadErrorKind::ArtifactNotBuilt {
            artifact: artifact_id.to_string(),
        }
        .bare());
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
    let artifact = find_built_artifact(wiring, artifact_id, &spec.name)?;
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
        .bare()
    };
    let bytes = crate::proc::host::describe_via_worker(None, path)
        .map_err(|e| proc_describe(e.to_string()))?;
    let entries: Vec<metor_fsw_2_core::abi::PackEntryDesc> =
        crate::dl::decode_pack_manifest(&bytes).map_err(|e| proc_describe(e.to_string()))?;
    let source = EntrySource::Described {
        entries: &entries,
        artifact: artifact_id,
    };
    let (entry, params) =
        resolve_occupant(&source, spec.ty.as_deref(), &spec.params, &spec.name, false)?;
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
    .bare())
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
        // Prefer the build driver's sidecar (no dlopen); otherwise describe in
        // process. A describe failure is not a staleness verdict, so let the
        // later dlopen path surface it.
        let Some(bytes) =
            crate::dl::manifest_sidecar_bytes(path).or_else(|| crate::dl::describe_raw(path).ok())
        else {
            continue;
        };
        if pack_module::manifest_hash(&bytes) != recorded {
            return Err(LoadErrorKind::StaleStubs {
                artifact: artifact.id.clone(),
            }
            .bare());
        }
    }
    Ok(())
}

/// The artifact whose pack exports `occ.occupant`: the `artifact=` the allow
/// line named, or (absent one) the unique artifact exporting an entry of
/// that name. No match or more than one is a clean error either way.
fn occupant_artifact(
    wiring: &Wiring,
    packs: &mut PackCache,
    wasm: &mut WasmCache,
    occ: &AllowedOccupantSpec,
    slot: &str,
    max_memory: usize,
) -> Result<String, LoadError> {
    if let Some(artifact) = &occ.artifact {
        return Ok(artifact.clone());
    }
    let mut matches: Vec<String> = Vec::new();
    for artifact in &wiring.artifacts {
        // A wasm artifact is described through the interpreter, never dlopened.
        let exports = if artifact.kind == ArtifactKind::Wasm {
            wasm.open(wiring, &artifact.id, slot, max_memory)?
                .entries
                .iter()
                .any(|e| e.descriptor.name == occ.occupant)
        } else {
            packs
                .open(wiring, &artifact.id, slot)?
                .system_names()
                .any(|n| n == occ.occupant)
        };
        if exports {
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
        .bare()),
    }
}

/// Map a [`SlotConfigError`] — from the shared pure-spec validation or from
/// [`plan_slot`](crate::coordinator::slot::plan_slot) — onto the resolver's
/// public error variants.
pub(super) fn slot_config_error(err: SlotConfigError, slot: &SlotSpec) -> LoadError {
    let name = slot.name.clone();
    match err {
        SlotConfigError::Empty => LoadErrorKind::EmptySlot { slot: name }.bare(),
        SlotConfigError::UnknownInitial { occupant, allowed } => {
            LoadErrorKind::UnknownInitialOccupant {
                slot: name,
                occupant,
                allowed,
            }
            .bare()
        }
        // A mount-reserved port and a declared capability are
        // occupant-contract defects too: the occupant cannot honor the
        // slot's contract as declared.
        SlotConfigError::OccupantMismatch { occupant, .. }
        | SlotConfigError::ReservedPort { occupant, .. }
        | SlotConfigError::CapabilityOccupant { occupant } => LoadErrorKind::SlotOccupantMismatch {
            slot: name,
            occupant,
        }
        .bare(),
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
/// Describe one wasm occupant: open the module under the interpreter, find the
/// named entry in its manifest, and encode its params.
///
/// The module is opened only to be *described* — the returned backing carries
/// its path, and the slot loads a fresh instance per `Load`.
fn resolve_wasm_occupant(
    art: &crate::ir::Artifact,
    module: &WasmModule,
    occ: &crate::ir::AllowedOccupantSpec,
    slot: &str,
) -> Result<AllowedOccupant, LoadError> {
    let bad = |detail: String| {
        LoadErrorKind::WasmOccupant(
            format!(
                "slot `{slot}`: wasm occupant `{}` from artifact `{}`: {detail}",
                occ.occupant, art.id
            )
            .into_boxed_str(),
        )
        .bare()
    };
    let path = art
        .path
        .as_ref()
        .ok_or_else(|| bad("wasm artifact has no path".into()))?;
    let (_, e) = wasm_entry(&module.entries, Some(&occ.occupant), &bad)?;
    let params = match &occ.params {
        ParamSource::None => e.params_default.clone().unwrap_or_default(),
        ParamSource::Postcard(bytes) => bytes.clone(),
        ParamSource::Value(value) => metor_fsw_2_core::params::encode_value_params(
            value,
            &e.params_schema,
            &occ.occupant,
            e.params_default.as_deref(),
        )
        .map_err(|err| bad(err.to_string()))?,
    };
    Ok(AllowedOccupant {
        name: occ.occupant.clone(),
        params,
        descriptor: e.descriptor.clone(),
        backing: OccupantBacking::Wasm {
            path: path.clone(),
            entry_identity: crate::wasm::entry_identity(e),
        },
    })
}

fn resolve_slot(
    slot: &SlotSpec,
    wiring: &Wiring,
    packs: &mut PackCache,
    wasm: &mut WasmCache,
    graph: &mut InitGraph,
) -> Result<(SystemHandle, SystemDescriptor), LoadError> {
    // The pure-spec checks (non-empty allow set, `initial` inside it) ran in
    // `validate` before any artifact was opened. Open and param-encode each
    // allowed occupant; a process slot takes the describe-worker path instead,
    // and the backing decides the slot's mode in `plan_slot`.
    let allowed: Vec<AllowedOccupant> = if slot.process {
        describe_occupants(slot, wiring)?
    } else {
        let max_memory = graph.config.wasm_memory_limit_bytes;
        let mut allowed = Vec::with_capacity(slot.allow.len());
        for occ in &slot.allow {
            let artifact_id = occupant_artifact(wiring, packs, wasm, occ, &slot.name, max_memory)?;
            // A wasm artifact is read as bytes and described through the
            // interpreter, not `dlopen`ed: its manifest comes from the module
            // itself, so nothing about it enters this process's address space.
            if let Some(art) = wiring.artifacts.iter().find(|a| a.id == artifact_id)
                && art.kind == crate::ir::ArtifactKind::Wasm
            {
                let module = wasm.open(wiring, &artifact_id, &slot.name, max_memory)?;
                allowed.push(resolve_wasm_occupant(art, module, occ, &slot.name)?);
                continue;
            }
            let pack = packs.open(wiring, &artifact_id, &slot.name)?;
            let source = EntrySource::Opened {
                pack,
                artifact: &artifact_id,
            };
            let (entry, params) =
                resolve_occupant(&source, Some(&occ.occupant), &occ.params, &slot.name, true)?;
            allowed.push(AllowedOccupant::dl(
                occ.occupant.clone(),
                entry.opened(),
                params,
            ));
        }
        allowed
    };

    let initial = slot.initial.as_ref().map(|i| InitialOccupant {
        occupant: i.occupant.clone(),
        start: i.state == SlotInitState::Running,
    });

    // `plan_slot` is the one place the registered contract is derived and the
    // descriptor-level checks run, so this front-end cannot drift from it.
    let (registered, ports, process) =
        plan_slot(&slot.name, &allowed).map_err(|e| slot_config_error(e, slot))?;
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
    use metor_fsw_2_core::PortConn;
    for (dir, frames, ports) in [
        ("input", &slot.inputs, &desc.inputs),
        ("output", &slot.outputs, &desc.outputs),
    ] {
        for frame in frames {
            if !ports
                .iter()
                .any(|p| p.conn == PortConn::Edge && &p.name == frame)
            {
                return Err(LoadErrorKind::SlotContractMismatch {
                    slot: slot.name.clone(),
                    dir,
                    frame: frame.clone(),
                }
                .bare());
            }
        }
    }

    Ok((handle, desc))
}

/// Resolve a process slot's allowed set (`docs/process-systems.md`): the
/// [`resolve_proc`] recipe once per allowed occupant, so the host never
/// dlopens any occupant artifact. The resulting [`AllowedOccupant`]s carry
/// [`OccupantBacking::Artifact`], which is what makes [`plan_slot`] register
/// the slot process-mode.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn describe_occupants(slot: &SlotSpec, wiring: &Wiring) -> Result<Vec<AllowedOccupant>, LoadError> {
    // Manifests by artifact id, so a slot allowing several entries of one
    // pack runs one describe worker for it, not one per occupant.
    let mut manifests: HashMap<String, Vec<metor_fsw_2_core::abi::PackEntryDesc>> = HashMap::new();
    fn describe(
        manifests: &mut HashMap<String, Vec<metor_fsw_2_core::abi::PackEntryDesc>>,
        wiring: &Wiring,
        slot: &SlotSpec,
        artifact_id: &str,
    ) -> Result<(), LoadError> {
        if manifests.contains_key(artifact_id) {
            return Ok(());
        }
        let artifact = find_built_artifact(wiring, artifact_id, &slot.name)?;
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
            .bare()
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
                    describe(&mut manifests, wiring, slot, &artifact.id)?;
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
                        .bare());
                    }
                }
            }
        };
        describe(&mut manifests, wiring, slot, &artifact_id)?;
        let source = EntrySource::Described {
            entries: &manifests[&artifact_id],
            artifact: &artifact_id,
        };
        let (entry, params) =
            resolve_occupant(&source, Some(&occ.occupant), &occ.params, &slot.name, true)?;
        let artifact = find_built_artifact(wiring, &artifact_id, &slot.name)?;
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
) -> Result<Vec<AllowedOccupant>, LoadError> {
    Err(LoadErrorKind::ProcessUnsupported {
        name: slot.name.clone(),
    }
    .bare())
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
    if let Some(fuel) = spec.wasm_fuel_per_poll {
        if fuel == 0 {
            return Err(LoadErrorKind::InvalidWasmFuel.bare());
        }
        config.wasm_fuel_per_poll = fuel;
    }
    if let Some(bytes) = spec.wasm_memory_limit_bytes {
        let bytes = usize::try_from(bytes)
            .map_err(|_| LoadErrorKind::InvalidWasmMemory { bytes }.bare())?;
        if bytes == 0 {
            return Err(LoadErrorKind::InvalidWasmMemory { bytes: 0 }.bare());
        }
        config.wasm_memory_limit_bytes = bytes;
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
    edge: &EdgeSpec,
) -> Result<(PortRef, PortRef), LoadError> {
    let inst = |name: &str| {
        instances.get(name).ok_or_else(|| {
            LoadErrorKind::UnknownInstance {
                name: name.to_string(),
            }
            .bare()
        })
    };
    let prod = inst(&edge.from)?;
    let cons = inst(&edge.to)?;

    let by_name = |ports: &[metor_fsw_2_core::PortDesc], token: &str| {
        ports
            .iter()
            .find(|p| matches!(p.id(), PortId::Packet(_)) && p.name == token)
            .map(|p| p.id())
    };
    let by_id = |ports: &[metor_fsw_2_core::PortDesc], id: PortId| {
        ports.iter().find(|p| p.id() == id).map(|p| p.id())
    };
    let unknown = |instance: &str, msg: &str| {
        LoadErrorKind::UnknownMsg {
            instance: instance.to_string(),
            msg: msg.to_string(),
        }
        .bare()
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
    name: &str,
    port_name: &str,
    dir: Dir,
) -> Result<PortRef, LoadError> {
    let inst = instances.get(name).ok_or_else(|| {
        LoadErrorKind::UnknownInstance {
            name: name.to_string(),
        }
        .bare()
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
        .bare());
    }
    Ok(PortRef {
        system: inst.handle,
        port: id,
    })
}
