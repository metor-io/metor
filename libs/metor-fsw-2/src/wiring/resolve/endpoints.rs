//! Resolve declared and compiled component connections.

use std::collections::HashMap;

use metor_fsw_2_core::PortId;
use metor_proto::types::ComponentId;

use super::{Dir, Instance, PendingSynth};
use crate::coordinator::PortRef;
use crate::coordinator::init::InitGraph;
use crate::wiring::{EdgeSpec, LoadError, LoadErrorKind};

/// Wire the edges a compiled entry's bindings imply: one edge per distinct
/// producing port, in the same first-appearance order the compiler grouped
/// the descriptor's inputs by. The two walks share their key, the binding
/// list, so a mismatch is artifact/wiring drift and fails loudly. A
/// `Produced` binding names a *declaration*; `added` maps it to the instance
/// the declaration was registered under.
pub(super) fn synth_edges(
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
                // The stamp rides the record the frame's fields came from.
                metor_expr::Binding::Timestamp => continue,
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
pub(super) fn locate_producer(
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
    let instance = instances
        .keys()
        .map(String::as_str)
        .filter(|name| {
            path.strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('.'))
        })
        .max_by_key(|name| name.len())
        .ok_or_else(|| bad("names no registered instance".into()))?;
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

/// Resolve a `msg=` edge's two endpoints jointly.
///
/// The token names the message type and is matched against each endpoint's
/// packet-port display names. An endpoint whose port carries an overridden
/// display name (a coordinator-minted channel such as `"commands"`) is then
/// matched by the packet id the token resolved to on the other endpoint. Only
/// when neither endpoint matches the token is the edge an
/// [`UnknownMsg`](LoadErrorKind::UnknownMsg).
pub(super) fn resolve_msg_edge(
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
        ports.iter().any(|port| port.id() == id).then_some(id)
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
pub(super) fn resolve_endpoint(
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
