//! Resolve [`Wiring`] into a built [`Coordinator`].
//!
//! States are created before the systems that attach to them. Systems and slots
//! are registered in IR order, with receive-all systems deferred until last.
//! Synthesized and declared edges are connected after all instances exist.
//!
//! Native and process packs share entry selection and parameter encoding;
//! WebAssembly packs are described through the interpreter. [`ResolveOptions`]
//! supplies host-specific worker and timing settings outside the portable IR.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use artifacts::{
    EntrySource, PackCache, WasmCache, check_manifest_hashes, encode_occupant_params,
    find_built_artifact, resolve_occupant,
};
use endpoints::{resolve_endpoint, resolve_msg_edge, synth_edges};
use metor_fsw_2_core::SystemDescriptor;
use metor_proto::types::ComponentId;
use slots::resolve_slot;

use super::error::{LoadError, LoadErrorKind};
use super::model::{
    ClockSpec, CoordinatorSpec, EdgeKind, ParamSource, StateSpec, SystemSpec, Wiring,
};
use super::registry::{LoadCtx, Registry, StaticParams};
use super::validate;
use crate::coordinator::init::{InitGraph, WasmReg};
use crate::coordinator::{ClockMode, Coordinator, CoordinatorConfig, SystemHandle};
use crate::ir::ArtifactKind;

mod artifacts;
mod endpoints;
mod slots;

pub(super) use slots::slot_config_error;

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
/// Where a process worker's executable lives, where the shared-memory session
/// dir is rooted, and how a worker's steps are timed and crashes recovered
/// are supplied here at [`resolve_with`] time rather than baked into the
/// portable IR, so they never touch the serialized `wiring.json`. Each
/// override falls back to the matching [`CoordinatorConfig`] default when
/// `None`; those defaults (re-exec the host binary as the worker, `/dev/shm`
/// or the OS temp dir for sessions, the config's step-timeout and restart
/// policy) are what [`resolve`] uses.
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
/// from its built [`Artifact::path`](crate::ir::Artifact::path) and registered as a loaded cyclic node.
///
/// Wiring faults are structured [`LoadError`] variants. Parameter decoding has
/// its own field-aware error variants.
pub fn resolve_with(
    wiring: &Wiring,
    registry: &Registry,
    opts: ResolveOptions,
) -> Result<Coordinator, LoadError> {
    // The one structural gate: version, scope indices, name/id uniqueness, and
    // each spec's well-formedness. Everything past here is registry- or
    // filesystem-dependent.
    validate::validate(wiring)?;
    check_manifest_hashes(wiring)?;
    let mut config = coordinator_config(&wiring.coordinator)?;
    // ResolveOptions apply onto the derived config, never onto the IR itself.
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
    // Rides the registry/announce seam and is threaded into each static
    // system's `configure` via `LoadCtx`.
    graph.namespace = wiring.coordinator.namespace.clone();

    // Path-stripped so the telemetered topology matches the bundle's
    // `wiring.json` byte-for-byte regardless of the build tree.
    let ir_json = serde_json::to_string(&wiring.path_stripped())
        .expect("a resolvable Wiring serializes to JSON");
    graph.set_wiring_manifest(metor_proto_wkt::WiringManifest {
        ir_version: wiring.ir_version,
        ir_json,
    });

    // States pass
    let mut state_tokens: HashMap<&str, metor_fsw_2_core::AttachTarget> = HashMap::new();
    for spec in &wiring.states {
        let target = resolve_state(spec, registry)?;
        state_tokens.insert(spec.name.as_str(), target);
    }

    // Systems pass
    let mut instances: HashMap<String, Instance> = HashMap::new();
    // Joins the instance namespace up front so command edges can name it;
    // `validate` already rejected any user spec of that name.
    let coord_handle = graph.coordinator_handle();
    instances.insert(
        "coordinator".to_string(),
        Instance {
            handle: coord_handle,
            desc: graph.descriptor_of(coord_handle).clone(),
        },
    );
    // A static `ReceiveAll` system must be the last cyclic registration or
    // `build()` rejects the graph with `ReceiveAllNotLast`. Only the static
    // branch defers; dl systems never carry capabilities.
    let mut deferred: Vec<&SystemSpec> = Vec::new();
    let mut packs = PackCache::default();
    let mut wasm = WasmCache::default();
    let mut pending: Vec<PendingSynth> = Vec::new();
    for spec in &wiring.systems {
        // The dl and proc arms must never see a wasm module: dlopen on one
        // would fail obscurely.
        let wasm_backed = spec.artifact.as_deref().is_some_and(|id| {
            wiring
                .artifacts
                .iter()
                .any(|a| a.id == id && a.kind == ArtifactKind::Wasm)
        });
        let (handle, desc) = match (&spec.artifact, spec.process) {
            (Some(artifact_id), false) if wasm_backed => resolve_wasm(
                spec,
                artifact_id,
                wiring,
                &mut wasm,
                &mut pending,
                &mut graph,
            )?,
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

    // Slots pass: a slot connects by name like a system
    for slot in &wiring.slots {
        let (handle, desc) = resolve_slot(slot, wiring, &mut packs, &mut wasm, &mut graph)?;
        instances.insert(slot.name.clone(), Instance { handle, desc });
    }

    // Deferred receive-all systems
    for spec in deferred {
        let (handle, desc) = resolve_static(spec, registry, &state_tokens, &mut graph)?;
        instances.insert(spec.name.clone(), Instance { handle, desc });
    }

    // --- Synthesized edges: a compiled system listed ahead of a producer it
    //     reads fails at build like any native pair.
    let added: HashMap<(&str, &str), &str> = pending
        .iter()
        .map(|p| ((p.artifact.as_str(), p.entry.as_str()), p.instance.as_str()))
        .collect();
    for p in &pending {
        let module = &wasm.modules[&p.artifact];
        let manifest = module
            .expr
            .as_ref()
            .expect("pending implies a compiled module");
        let system = manifest
            .system(&p.entry)
            .expect("pending keys a manifest entry");
        synth_edges(system, manifest, &instances, &added, p, &mut graph)?;
    }

    // Every declared state must have gained an attachment by now (attach
    // counts at entry create): a state serving nobody, a link server with no
    // downlink, is a config defect and fails like any other wiring mistake.
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

    // Edges pass
    for edge in &wiring.edges {
        let (producer, consumer) = match edge.kind {
            EdgeKind::Frame => (
                resolve_endpoint(&instances, &edge.from, &edge.out, Dir::Out)?,
                resolve_endpoint(&instances, &edge.to, &edge.in_, Dir::In)?,
            ),
            EdgeKind::Msg => resolve_msg_edge(&instances, edge)?,
        };
        // `EdgeKind` only picked the name-lookup space above; a `connect`'s
        // actual behavior is inferred from the connected ports' descriptors.
        // Build validates the edge, where a defect (`delayed=#true` into a
        // Log input, an incompatible pair) becomes a `WireError`.
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
    /// The pack entry (declaration) name, the manifest's key.
    entry: String,
    /// The instance name of this registration, the spec's own (possibly
    /// scope-prefixed, and free to differ from the entry name).
    instance: String,
    handle: SystemHandle,
}

/// Resolve a wired wasm system: describe the artifact once per resolve,
/// select the entry, encode its params (a compiled Python entry with an
/// `@rng` slot takes a host-entropy seed on the params channel instead), and
/// register it. The edges its compiled bindings imply are queued on
/// `pending` and synthesized after the systems pass.
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

/// Resolve a `process=#true` system: run a **describe-mode worker** over the
/// built artifact, since the host never dlopens it, decode the descriptor and
/// `Params` schema from the worker's bytes, encode the spec's params against
/// that schema, and register through
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
