//! The KDL → [`Wiring`] parse stage: the top-level node walk and the per-node
//! parsers, split out of `wiring/mod.rs` (C6). Parse only — no [`Registry`]
//! lookups, no `dlopen`, no graph validation (those are `resolve`'s job).

use std::collections::HashSet;

use kdl::{KdlDocument, KdlNode};

use super::model::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec,
    InitialOccupantSpec, SlotInitState, SlotSpec, SystemSpec, TelemetryModeSpec, TelemetrySpec,
    UplinkSpec, Wiring,
};
use super::{ALLOW_RESERVED, LoadError, ParamSource, SYSTEM_RESERVED, de};

/// Deserialize a KDL wiring document into the [`Wiring`] data model — one of the two
/// front-ends onto `Wiring` (the other is [`WiringBuilder`]).
///
/// This is **parse only**: it touches no [`Registry`], `dlopen`s nothing, and does not
/// validate the graph — those are [`resolve`]'s job. It carries the
/// `coordinator`/`system`/`connect`/`telemetry` surface plus the `artifact` node +
/// per-`system` `artifact=` reference for dl systems.
///
/// **Params** (for either kind) are carried as the KDL `system` node's source text in
/// [`ParamSource::Kdl`] when the node has config properties/children; a config-less
/// system carries [`ParamSource::None`]. The decoder is chosen at [`resolve`] time by
/// [`SystemSpec::artifact`]: a **static** system re-deserializes the text into its
/// typed `Params` (`serde`); a **dl** system schema-encodes it against the `.so`'s
/// exported `Params` schema, so KDL ≡ the Rust builder on the wire.
pub fn parse(kdl: &str) -> Result<Wiring, LoadError> {
    let doc = kdl.parse::<KdlDocument>().map_err(|source| LoadError::Parse {
        source,
        src: kdl.to_string(),
        span: (0, kdl.len()).into(),
    })?;

    // One pass over the top-level nodes; each list keeps document order, and an
    // unknown node name is a spanned error rather than a silent skip.
    let mut coordinator: Option<CoordinatorSpec> = None;
    let mut artifacts: Vec<Artifact> = Vec::new();
    let mut systems: Vec<SystemSpec> = Vec::new();
    let mut slots: Vec<SlotSpec> = Vec::new();
    let mut edges: Vec<EdgeSpec> = Vec::new();
    let mut telemetry: Option<TelemetrySpec> = None;
    let mut uplink: Option<UplinkSpec> = None;
    let mut seen: HashSet<String> = HashSet::new();

    for node in doc.nodes() {
        match node.name().value() {
            "coordinator" => {
                if coordinator.is_some() {
                    return Err(LoadError::MultipleCoordinators {
                        src: kdl.to_string(),
                        span: node.span(),
                    });
                }
                coordinator = Some(parse_coordinator(node, kdl)?);
            }
            "artifact" => artifacts.push(parse_artifact(node, kdl)?),
            "system" => systems.push(parse_system(node, kdl, &mut seen)?),
            // sequences-slots.md §5: a slot is an instance like a system.
            "slot" => slots.push(parse_slot(node, kdl, &mut seen)?),
            "connect" => edges.push(parse_edge(node, kdl)?),
            "telemetry" => {
                let (addr, mode) = parse_telemetry(node, kdl)?;
                telemetry = Some(TelemetrySpec { addr, mode });
            }
            // The command uplink (`docs/messages.md` §4.4): registers the instance
            // name "uplink" so command edges can name it.
            "uplink" => {
                let addr = parse_transport_addr(node, kdl, "uplink")?;
                uplink = Some(UplinkSpec { addr });
            }
            other => {
                return Err(LoadError::UnknownTopLevelNode {
                    node: other.to_string(),
                    src: kdl.to_string(),
                    span: node.span(),
                });
            }
        }
    }

    let coordinator = coordinator.ok_or(LoadError::MissingCoordinator)?;

    Ok(Wiring {
        coordinator,
        artifacts,
        systems,
        slots,
        edges,
        telemetry,
        uplink,
    })
}

/// Decorate a library **stem** into the platform's produced `cdylib` file name:
/// `lib{stem}.dylib` (macOS), `lib{stem}.so` (Linux/other), `{stem}.dll` (Windows).
///
/// The wiring's `artifact` `lib=` and the Rust builder both carry the bare stem
/// (`adcs_plant`), so a single mission document is portable across a dev mac and a
/// Linux flight target; the framework computes the concrete file name here at parse
/// time. [`Artifact::cdylib`](crate::Artifact) then holds that produced name — the
/// build driver, the bundle, and `resolve` all consume it unchanged.
pub fn cdylib_file_name(stem: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

/// Parse one `artifact "id" crate="..." lib="foo" type="Foo"` node into an
/// [`Artifact`]. `lib=` is the library **stem** (`foo`); the produced file name is
/// computed per-platform via [`cdylib_file_name`] so a static mission document stays
/// portable across host OSes.
fn parse_artifact(node: &KdlNode, src: &str) -> Result<Artifact, LoadError> {
    let missing = |property: &'static str| LoadError::MissingArtifactField {
        property,
        src: src.to_string(),
        span: node.span(),
    };
    let id = first_arg_string(node).ok_or_else(|| missing("id"))?;
    let crate_name = prop_string(node, "crate").ok_or_else(|| missing("crate"))?;
    let stem = prop_string(node, "lib").ok_or_else(|| missing("lib"))?;
    let system_type = prop_string(node, "type").ok_or_else(|| missing("type"))?;
    Ok(Artifact {
        id: id.to_string(),
        crate_name: crate_name.to_string(),
        cdylib: cdylib_file_name(stem),
        system_type: system_type.to_string(),
        path: None,
    })
}

/// Parse one `system` node into a [`SystemSpec`]. An `artifact=` ⇒ a dl system
/// referencing that [`Artifact`] (its `type=` is then optional — the artifact's
/// `system_type` is authoritative, and a given `type=` is validated at resolve);
/// otherwise a static system, whose `type=` is required. See [`parse`] for the
/// static-params / dl-params-deferred handling.
fn parse_system(
    node: &KdlNode,
    src: &str,
    seen: &mut HashSet<String>,
) -> Result<SystemSpec, LoadError> {
    let name = first_arg_string(node).ok_or_else(|| LoadError::MissingInstanceName {
        src: src.to_string(),
        span: node.span(),
    })?;
    // The pre-rename `lib=` spelling gets a guidance error pointing at the entry.
    if let Some(entry) = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.value()) == Some("lib"))
    {
        return Err(LoadError::SystemLibRenamed {
            src: src.to_string(),
            span: entry.span(),
        });
    }
    let artifact = prop_string(node, "artifact").map(str::to_string);
    let ty = prop_string(node, "type").map(str::to_string);
    if ty.is_none() && artifact.is_none() {
        return Err(LoadError::MissingType {
            name: name.to_string(),
            src: src.to_string(),
            span: node.span(),
        });
    }
    if !seen.insert(name.to_string()) {
        return Err(LoadError::DuplicateInstance {
            name: name.to_string(),
            src: src.to_string(),
            span: node.span(),
        });
    }
    // Any property other than the reserved `type=`/`artifact=`, or any child node,
    // is params config.
    let has_config = node
        .entries()
        .iter()
        .any(|e| matches!(e.name().map(|n| n.value()), Some(k) if !SYSTEM_RESERVED.contains(&k)))
        || node.children().is_some_and(|c| !c.nodes().is_empty());
    // Both static and dl systems carry the node's source text when configured; the
    // decoder is chosen at `resolve` by `artifact` (static ⇒ the typed serde
    // deserialize, dl ⇒ schema-guided postcard encode).
    let params = if has_config {
        ParamSource::Kdl(node.to_string())
    } else {
        ParamSource::None
    };
    Ok(SystemSpec {
        name: name.to_string(),
        ty,
        artifact,
        params,
    })
}

/// Parse one `slot "name" { input/output/allow/initial }` node into a [`SlotSpec`]
/// (sequences-slots.md §5). The slot name is the node's first string arg (like a
/// `system`/`artifact` name) and shares the instance namespace (`seen`), so a name
/// colliding with a system is a [`DuplicateInstance`](LoadError::DuplicateInstance).
///
/// Command wiring is ordinary message wiring (A2): a slot receives commands only from
/// producers explicitly edged into it (`connect "<producer>" -> "<slot>"
/// msg="SequenceCommand"`). A slot with **zero** command edges is legal and silently
/// command-less (an autonomy-free, wiring-frozen slot) — deliberate: `resolve` has no
/// non-fatal diagnostics surface, and warning on every such mission would be noise;
/// the absence is visible in the wiring document itself.
///
/// Children: `input frame="…"`/`output frame="…"` declare the user-port contract;
/// `allow occupant="…" …` is one allowed occupant — everything on it other than the
/// reserved `occupant=` is its default params, as line properties
/// (`allow occupant="x" gain=0.8`) and/or children (`allow occupant="x" { gain 0.8 }`),
/// carried as [`ParamSource::Kdl`] (else [`ParamSource::None`] — mirroring
/// [`parse_system`]); `initial occupant="…" state="…"` is the startup occupant.
fn parse_slot(
    node: &KdlNode,
    src: &str,
    seen: &mut HashSet<String>,
) -> Result<SlotSpec, LoadError> {
    let name = first_arg_string(node).ok_or_else(|| LoadError::MissingInstanceName {
        src: src.to_string(),
        span: node.span(),
    })?;
    if !seen.insert(name.to_string()) {
        return Err(LoadError::DuplicateInstance {
            name: name.to_string(),
            src: src.to_string(),
            span: node.span(),
        });
    }

    let mut inputs: Vec<String> = Vec::new();
    let mut outputs: Vec<String> = Vec::new();
    let mut allow: Vec<AllowedOccupantSpec> = Vec::new();
    let mut initial: Option<InitialOccupantSpec> = None;

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().value() {
                "input" => {
                    let frame = prop_string(child, "frame").ok_or_else(|| {
                        LoadError::MissingParam {
                            property: "frame".to_string(),
                            system: "slot".to_string(),
                            src: src.to_string(),
                            span: child.span(),
                        }
                    })?;
                    inputs.push(frame.to_string());
                }
                "output" => {
                    let frame = prop_string(child, "frame").ok_or_else(|| {
                        LoadError::MissingParam {
                            property: "frame".to_string(),
                            system: "slot".to_string(),
                            src: src.to_string(),
                            span: child.span(),
                        }
                    })?;
                    outputs.push(frame.to_string());
                }
                "allow" => {
                    let occupant = prop_string(child, "occupant").ok_or_else(|| {
                        LoadError::MissingParam {
                            property: "occupant".to_string(),
                            system: "slot".to_string(),
                            src: src.to_string(),
                            span: child.span(),
                        }
                    })?;
                    // Any non-reserved line property OR any child ⇒ params: carry the
                    // `allow` node's source text for the resolve-time schema-guided
                    // encoder (mirrors `parse_system`'s `Kdl` decision); else paramless.
                    let has_config = child.entries().iter().any(|e| {
                        matches!(e.name().map(|n| n.value()), Some(k) if !ALLOW_RESERVED.contains(&k))
                    }) || child.children().is_some_and(|c| !c.nodes().is_empty());
                    let params = if has_config {
                        ParamSource::Kdl(child.to_string())
                    } else {
                        ParamSource::None
                    };
                    allow.push(AllowedOccupantSpec {
                        occupant: occupant.to_string(),
                        params,
                    });
                }
                "initial" => {
                    let occupant = prop_string(child, "occupant").ok_or_else(|| {
                        LoadError::MissingParam {
                            property: "occupant".to_string(),
                            system: "slot".to_string(),
                            src: src.to_string(),
                            span: child.span(),
                        }
                    })?;
                    // `state` is optional; an `initial` with no `state` defaults to
                    // `loaded` (built but not auto-started — the conservative default).
                    let state = match prop_string(child, "state") {
                        None | Some("loaded") => SlotInitState::Loaded,
                        Some("running") => SlotInitState::Running,
                        Some("empty") => SlotInitState::Empty,
                        Some(other) => {
                            return Err(LoadError::BadSlotState {
                                slot: name.to_string(),
                                state: other.to_string(),
                                src: src.to_string(),
                                span: child.span(),
                            });
                        }
                    };
                    initial = Some(InitialOccupantSpec {
                        occupant: occupant.to_string(),
                        state,
                    });
                }
                other => {
                    return Err(LoadError::UnknownSlotChild {
                        child: other.to_string(),
                        src: src.to_string(),
                        span: child.span(),
                    });
                }
            }
        }
    }

    Ok(SlotSpec {
        name: name.to_string(),
        inputs,
        outputs,
        allow,
        initial,
    })
}

/// Parse a `telemetry` node into a `(addr, mode)` pair (telemetry.md §8):
///
/// ```kdl
/// telemetry {
///     transport "tcp" addr="127.0.0.1:2240"
///     mode "all"                       // or: mode "subset"
///     // subset only:
///     // tap instance="imu_left"
///     // tap frame="control"
/// }
/// ```
/// Parse the shared `transport "tcp" addr="…"` child of a `telemetry`/`uplink` node
/// into its socket address (v1 supports only `"tcp"`); `label` names the node in
/// diagnostics.
fn parse_transport_addr(
    node: &KdlNode,
    src: &str,
    label: &str,
) -> Result<std::net::SocketAddr, LoadError> {
    let invalid = |property: &str, expected: &str| LoadError::InvalidParam {
        property: property.to_string(),
        system: label.to_string(),
        expected: expected.to_string(),
        src: src.to_string(),
        span: node.span(),
    };
    let missing = |property: &str| LoadError::MissingParam {
        property: property.to_string(),
        system: label.to_string(),
        src: src.to_string(),
        span: node.span(),
    };

    let children = node.children().ok_or_else(|| missing("transport"))?;
    let transport_node = children
        .nodes()
        .iter()
        .find(|n| n.name().value() == "transport")
        .ok_or_else(|| missing("transport"))?;
    match first_arg_string(transport_node) {
        Some("tcp") => {}
        _ => return Err(invalid("transport", "\"tcp\" (the only v1 transport)")),
    }
    let addr_str = prop_string(transport_node, "addr").ok_or_else(|| missing("addr"))?;
    addr_str
        .parse::<std::net::SocketAddr>()
        .map_err(|_| invalid("addr", "a socket address like 127.0.0.1:2240"))
}

fn parse_telemetry(
    node: &KdlNode,
    src: &str,
) -> Result<(std::net::SocketAddr, TelemetryModeSpec), LoadError> {
    let invalid = |property: &str, expected: &str| LoadError::InvalidParam {
        property: property.to_string(),
        system: "telemetry".to_string(),
        expected: expected.to_string(),
        src: src.to_string(),
        span: node.span(),
    };

    let addr = parse_transport_addr(node, src, "telemetry")?;
    let children = node.children().expect("parse_transport_addr requires children");

    // mode "all" | "subset"  — subset taps the `tap instance=.. / frame=..` children.
    let mode_str = children
        .nodes()
        .iter()
        .find(|n| n.name().value() == "mode")
        .and_then(first_arg_string)
        .unwrap_or("all");
    let mode = match mode_str {
        "all" => TelemetryModeSpec::All,
        "subset" => {
            let mut instances = Vec::new();
            let mut frames = Vec::new();
            for tap in children.nodes().iter().filter(|n| n.name().value() == "tap") {
                if let Some(i) = prop_string(tap, "instance") {
                    instances.push(i.to_string());
                }
                if let Some(f) = prop_string(tap, "frame") {
                    frames.push(f.to_string());
                }
            }
            TelemetryModeSpec::Subset { instances, frames }
        }
        _ => return Err(invalid("mode", "\"all\" or \"subset\"")),
    };
    Ok((addr, mode))
}

/// Read one `coordinator` node into a [`CoordinatorSpec`] (wiring.md §1.1).
/// `sim_dt` (seconds) present ⇒ a free-running [`Simulated`](ClockSpec::Simulated)
/// clock; absent ⇒ [`Wall`](ClockSpec::Wall). Deserialized through the shared KDL
/// params deserializer, so an unknown/misspelled coordinator property is a spanned
/// [`UnknownParam`](LoadError::UnknownParam) and a bad value is entry-precise.
fn parse_coordinator(node: &KdlNode, src: &str) -> Result<CoordinatorSpec, LoadError> {
    #[derive(serde::Deserialize)]
    struct CoordinatorProps {
        cycle_rate: f64,
        default_depth: Option<usize>,
        sim_dt: Option<f64>,
    }
    let props: CoordinatorProps = de::from_kdl_node(node, src, "coordinator", &[], 0)?;
    let clock = match props.sim_dt {
        Some(dt_secs) => ClockSpec::Simulated { dt_secs },
        None => ClockSpec::Wall,
    };
    Ok(CoordinatorSpec {
        cycle_rate: props.cycle_rate,
        default_depth: props.default_depth,
        clock,
    })
}

/// Parse a `connect` edge in either the shorthand (`"a" -> "b" frame="f"`) or the
/// explicit (`from=.. out=.. to=.. in=..`) form (wiring.md §1.3), directly into the
/// data model's [`EdgeSpec`] (the former parse-local `Edge` twin is gone — C5).
fn parse_edge(node: &KdlNode, src: &str) -> Result<EdgeSpec, LoadError> {
    let span = node.span();
    let missing = |property: &str| LoadError::MissingEdgeField {
        property: property.to_string(),
        src: src.to_string(),
        span,
    };
    // `delayed=#true` marks a one-cycle-delayed feedback back-edge (default false).
    let delayed = node.get("delayed").and_then(|v| v.as_bool()).unwrap_or(false);

    if let Some(from) = prop_string(node, "from") {
        // Explicit long form — frame edges only (a message edge uses the `msg=` shorthand).
        let to = prop_string(node, "to").ok_or_else(|| missing("to"))?;
        let out = prop_string(node, "out").ok_or_else(|| missing("out"))?;
        let in_ = prop_string(node, "in").ok_or_else(|| missing("in"))?;
        return Ok(EdgeSpec {
            from: from.to_string(),
            to: to.to_string(),
            out: out.to_string(),
            in_: in_.to_string(),
            delayed,
            kind: EdgeKind::Frame,
        });
    }

    // Shorthand: the nameless arguments are `"from"`, (optional `->`), `"to"`; the port is
    // named by exactly one of `frame=` (a component frame) or `msg=` (a message type).
    let args: Vec<&str> = node
        .entries()
        .iter()
        .filter(|e| e.name().is_none())
        .filter_map(|e| e.value().as_string())
        .collect();
    let (from, to) = match args.as_slice() {
        [from, "->", to] => (*from, *to),
        [from, to] => (*from, *to),
        _ => return Err(missing("from/to")),
    };
    let (port_name, kind) = match (prop_string(node, "frame"), prop_string(node, "msg")) {
        (Some(frame), None) => (frame, EdgeKind::Frame),
        (None, Some(msg)) => (msg, EdgeKind::Msg),
        (Some(_), Some(_)) => return Err(missing("frame/msg (name exactly one)")),
        (None, None) => return Err(missing("frame")),
    };
    Ok(EdgeSpec {
        from: from.to_string(),
        to: to.to_string(),
        out: port_name.to_string(),
        in_: port_name.to_string(),
        delayed,
        kind,
    })
}

/// The first nameless string argument of a node (e.g. a `system` instance name).
fn first_arg_string(node: &KdlNode) -> Option<&str> {
    node.entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
}

/// A node's string-valued property `key`.
fn prop_string<'a>(node: &'a KdlNode, key: &str) -> Option<&'a str> {
    node.get(key).and_then(|v| v.as_string())
}

