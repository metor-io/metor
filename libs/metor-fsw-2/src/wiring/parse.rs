//! The KDL parse stage, turning a wiring document into the [`Wiring`] data
//! model.
//!
//! Parsing is purely syntactic. It walks the top-level nodes of a document and
//! builds specs; registry lookups, library loading, and graph validation all
//! happen later, in `resolve`. Every error carries the document source and a
//! span, so it renders as a pointed diagnostic.
//!
//! Params are not decoded here. When a `system` node (or a slot's `allow`
//! child) carries any non-reserved property or any child node, its KDL source
//! text is captured as [`ParamSource::Kdl`] and decoded at resolve time, where
//! the target type is known. A static system deserializes the text into its
//! typed `Params`; a dynamically loaded system encodes it against the schema
//! exported by its artifact. A node with no config carries
//! [`ParamSource::None`].

use std::collections::HashSet;

use kdl::{KdlDocument, KdlNode};

use super::model::{
    AllowedOccupantSpec, Artifact, ClockSpec, CoordinatorSpec, EdgeKind, EdgeSpec,
    InitialOccupantSpec, SlotInitState, SlotSpec, SystemSpec, TCP_DOWNLINK_TYPE, TCP_UPLINK_TYPE,
    Wiring,
};
use super::{ALLOW_RESERVED, LoadError, ParamSource, SYSTEM_RESERVED, de};

/// Parse a KDL wiring document into a [`Wiring`].
///
/// The document is a flat list of `coordinator`, `artifact`, `system`, `slot`,
/// and `connect` nodes. Exactly one `coordinator` is required, and an unknown
/// node name is a spanned error rather than a silent skip.
pub fn parse(kdl: &str) -> Result<Wiring, LoadError> {
    let doc = kdl
        .parse::<KdlDocument>()
        .map_err(|source| LoadError::Parse {
            source,
            src: kdl.to_string(),
            span: (0, kdl.len()).into(),
        })?;

    // Each list keeps document order.
    let mut coordinator: Option<CoordinatorSpec> = None;
    let mut artifacts: Vec<Artifact> = Vec::new();
    let mut systems: Vec<SystemSpec> = Vec::new();
    let mut slots: Vec<SlotSpec> = Vec::new();
    let mut edges: Vec<EdgeSpec> = Vec::new();
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
            "slot" => slots.push(parse_slot(node, kdl, &mut seen)?),
            "connect" => edges.push(parse_edge(node, kdl)?),
            // `telemetry` and `uplink` were dedicated nodes before the links
            // became ordinary registry systems. A document that still uses
            // them gets a guidance error naming the `system` spelling that
            // replaced each.
            "telemetry" => {
                return Err(legacy_link_node(
                    node,
                    kdl,
                    "telemetry",
                    TCP_DOWNLINK_TYPE,
                    "2240",
                ));
            }
            "uplink" => {
                return Err(legacy_link_node(
                    node,
                    kdl,
                    "uplink",
                    TCP_UPLINK_TYPE,
                    "2241",
                ));
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
    })
}

/// Build the guidance error for a removed dedicated link node, spelling out
/// the equivalent `system` declaration.
fn legacy_link_node(
    node: &KdlNode,
    src: &str,
    name: &str,
    ty: &str,
    example_port: &str,
) -> LoadError {
    LoadError::LegacyLinkNode {
        node: name.to_string(),
        example: format!("`system \"{name}\" type=\"{ty}\" addr=\"127.0.0.1:{example_port}\"`"),
        src: src.to_string(),
        span: node.span(),
    }
}

/// Decorate a library stem into the platform's cdylib file name, so `adcs`
/// becomes `libadcs.dylib` on macOS, `libadcs.so` on Linux, or `adcs.dll` on
/// Windows.
///
/// Wiring documents and the builder both carry the bare stem, which keeps a
/// single document portable across host platforms; the concrete file name is
/// computed here at parse time and consumed unchanged from then on.
pub fn cdylib_file_name(stem: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

/// Parse an `artifact "id" crate="..." lib="..." type="..."` node into an
/// [`Artifact`]. `lib=` is the bare library stem; the platform file name comes
/// from [`cdylib_file_name`].
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

/// Parse a `system` node into a [`SystemSpec`].
///
/// A node with `artifact=` declares a dynamically loaded system and may omit
/// `type=`, since the artifact's type is authoritative (a `type=` given anyway
/// is checked against it at resolve). Without `artifact=`, the system is
/// static and `type=` is required.
fn parse_system(
    node: &KdlNode,
    src: &str,
    seen: &mut HashSet<String>,
) -> Result<SystemSpec, LoadError> {
    let name = first_arg_string(node).ok_or_else(|| LoadError::MissingInstanceName {
        src: src.to_string(),
        span: node.span(),
    })?;
    // `lib=` was renamed to `artifact=`; the error points at the entry.
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
    // Any property beyond the reserved set, or any child node, is params
    // config; see the module docs for how the captured text is decoded.
    let has_config =
        node.entries().iter().any(
            |e| matches!(e.name().map(|n| n.value()), Some(k) if !SYSTEM_RESERVED.contains(&k)),
        ) || node.children().is_some_and(|c| !c.nodes().is_empty());
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

/// Parse a `slot "name" { input/output/allow/initial }` node into a
/// [`SlotSpec`].
///
/// Slots share the instance namespace with systems, so a slot named like an
/// existing system is a [`DuplicateInstance`](LoadError::DuplicateInstance)
/// error. The children declare the slot's contract. `input frame="..."` and
/// `output frame="..."` name the user-port frames. Each `allow occupant="..."`
/// names one permitted occupant, and anything on it beyond the reserved
/// `occupant=` property, whether line properties or children, is that
/// occupant's default params, captured as with a `system` node. An
/// `initial occupant="..." state="..."` child picks the startup occupant.
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
                    let frame =
                        prop_string(child, "frame").ok_or_else(|| LoadError::MissingParam {
                            property: "frame".to_string(),
                            system: "slot".to_string(),
                            src: src.to_string(),
                            span: child.span(),
                        })?;
                    inputs.push(frame.to_string());
                }
                "output" => {
                    let frame =
                        prop_string(child, "frame").ok_or_else(|| LoadError::MissingParam {
                            property: "frame".to_string(),
                            system: "slot".to_string(),
                            src: src.to_string(),
                            span: child.span(),
                        })?;
                    outputs.push(frame.to_string());
                }
                "allow" => {
                    let occupant =
                        prop_string(child, "occupant").ok_or_else(|| LoadError::MissingParam {
                            property: "occupant".to_string(),
                            system: "slot".to_string(),
                            src: src.to_string(),
                            span: child.span(),
                        })?;
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
                    let occupant =
                        prop_string(child, "occupant").ok_or_else(|| LoadError::MissingParam {
                            property: "occupant".to_string(),
                            system: "slot".to_string(),
                            src: src.to_string(),
                            span: child.span(),
                        })?;
                    // With no `state`, the occupant defaults to `loaded`,
                    // built but not auto-started.
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

/// Parse the `coordinator` node into a [`CoordinatorSpec`].
///
/// A `sim_dt` property (seconds) selects a free-running simulated clock;
/// without it the coordinator runs on the wall clock. Properties go through
/// the shared KDL params deserializer, so an unknown property or a bad value
/// is an entry-precise spanned error.
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

/// Parse a `connect` node into an [`EdgeSpec`].
///
/// Two forms are accepted. The shorthand `"a" -> "b"` names the port with
/// exactly one of `frame=` (a component frame, same name on both ends) or
/// `msg=` (a message type). The explicit `from=.. out=.. to=.. in=..` form is
/// for frame edges whose port names differ.
fn parse_edge(node: &KdlNode, src: &str) -> Result<EdgeSpec, LoadError> {
    let span = node.span();
    let missing = |property: &str| LoadError::MissingEdgeField {
        property: property.to_string(),
        src: src.to_string(),
        span,
    };
    // `delayed=#true` marks a feedback back-edge read one cycle late.
    let delayed = node
        .get("delayed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if let Some(from) = prop_string(node, "from") {
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

    // Shorthand: the nameless arguments are `"from"`, an optional `->`, and
    // `"to"`.
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

/// The first nameless string argument of a node, such as an instance name.
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
