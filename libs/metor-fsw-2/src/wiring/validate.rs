//! The one structural gate every ingested [`Wiring`] passes before resolve.
//!
//! [`resolve_with`](super::resolve_with) runs [`validate`] first, so both
//! front-ends, the serde-ingested Python IR and the Rust
//! [`WiringBuilder`](super::WiringBuilder), clear the same checks. The checks
//! here are the ones that need only the IR itself: version skew, scope-table
//! indices, name and id uniqueness, and each spec's own well-formedness.
//! Anything needing the [`Registry`](super::Registry) or the filesystem
//! (unknown types, manifest freshness, occupant/entry resolution, a built
//! artifact path) stays in resolve, next to what it needs.
//!
use std::collections::HashSet;

use super::LoadError;
use super::model::IR_VERSION;
use super::model::{
    ArtifactKind, DOWNLINK_TYPE, Deployment, ParamSource, SlotSpec, StateSpec, SystemSpec,
    TCP_SERVER_TYPE, Wiring,
};
use super::resolve::slot_config_error;
use crate::coordinator::validate_slot_spec;

/// The instance name the coordinator itself occupies. A user spec of this name
/// collides with it, surfacing as a [`DuplicateInstance`](LoadError::DuplicateInstance).
const RESERVED_INSTANCE: &str = "coordinator";

/// Reject a structurally invalid [`Deployment`] envelope before selection.
///
/// The envelope's own rules only: version, non-empty, and the member identity
/// rules a deployment of several needs. Each member's own checks stay in
/// [`validate`], which [`resolve`](super::resolve) runs on the selected one.
pub fn validate_deployment(deployment: &Deployment) -> Result<(), LoadError> {
    if deployment.ir_version != IR_VERSION {
        return Err(LoadError::IrVersionMismatch {
            found: deployment.ir_version,
            expected: IR_VERSION,
        });
    }
    if deployment.targets.is_empty() {
        return Err(LoadError::EmptyDeployment);
    }
    if deployment.targets.len() == 1 {
        // A member runs alone as a deployment of one — a packaged bundle, or
        // `--target` — and its mirrors then name members no envelope carries.
        return Ok(());
    }
    check_hosts_and_peers(deployment)?;

    let mut seen: Vec<&str> = Vec::new();
    for (index, member) in deployment.targets.iter().enumerate() {
        let namespace = member
            .coordinator
            .namespace
            .as_deref()
            .ok_or(LoadError::NamespaceRequired { index })?;
        for other in &seen {
            if *other == namespace {
                return Err(LoadError::DuplicateNamespace {
                    namespace: namespace.to_string(),
                });
            }
            if let Some((outer, inner)) = nested(other, namespace) {
                return Err(LoadError::NamespaceOverlap {
                    outer: outer.to_string(),
                    inner: inner.to_string(),
                });
            }
        }
        seen.push(namespace);
    }
    Ok(())
}

/// The cross-member rules of a deployment of several: a `hosts` key and a
/// mirror's `peer.namespace` both name a member. A mirror naming its own
/// member is [`validate`]'s to reject, since only it knows the member's own
/// namespace.
fn check_hosts_and_peers(deployment: &Deployment) -> Result<(), LoadError> {
    let members: HashSet<&str> = deployment
        .targets
        .iter()
        .filter_map(|w| w.coordinator.namespace.as_deref())
        .collect();
    for namespace in deployment.hosts.keys() {
        if !members.contains(namespace.as_str()) {
            return Err(LoadError::UnknownHost {
                namespace: namespace.clone(),
            });
        }
    }
    for spec in deployment.targets.iter().flat_map(|w| &w.systems) {
        if let Some(peer) = &spec.peer
            && !members.contains(peer.namespace.as_str())
        {
            return Err(LoadError::UnknownPeer {
                system: spec.name.clone(),
                namespace: peer.namespace.clone(),
            });
        }
    }
    Ok(())
}

/// The two namespaces ordered outer-first when one is a dotted prefix of the
/// other, `None` when they are disjoint.
fn nested<'a>(a: &'a str, b: &'a str) -> Option<(&'a str, &'a str)> {
    if b.strip_prefix(a).is_some_and(|rest| rest.starts_with('.')) {
        Some((a, b))
    } else if a.strip_prefix(b).is_some_and(|rest| rest.starts_with('.')) {
        Some((b, a))
    } else {
        None
    }
}

/// Reject a structurally invalid [`Wiring`] before any system is built.
pub(crate) fn validate(wiring: &Wiring) -> Result<(), LoadError> {
    check_ir_version(wiring)?;
    check_scope_refs(wiring)?;
    check_instance_names(wiring)?;
    check_artifact_ids(wiring)?;
    check_state_names(wiring)?;
    check_link_names(wiring)?;
    check_downlinks(wiring)?;
    for state in &wiring.states {
        check_state(state)?;
    }
    check_artifact_fields(wiring)?;
    check_program(wiring)?;
    for spec in &wiring.systems {
        check_system(spec, wiring)?;
    }
    for slot in &wiring.slots {
        check_slot(slot, wiring)?;
    }
    Ok(())
}

/// Per-artifact field rules the kind implies: a cdylib is built (or located)
/// through cargo, so it must name a crate and a lib stem; a program-built
/// wasm artifact requires the captured program it compiles from.
fn check_artifact_fields(wiring: &Wiring) -> Result<(), LoadError> {
    for artifact in &wiring.artifacts {
        if artifact.kind == ArtifactKind::Cdylib
            && (artifact.crate_name.is_empty() || artifact.lib.is_empty())
        {
            return Err(LoadError::ArtifactMissingCrate {
                id: artifact.id.clone(),
            });
        }
        if artifact.is_program() && wiring.program.is_none() {
            return Err(LoadError::ProgramArtifactWithoutProgram {
                id: artifact.id.clone(),
            });
        }
    }
    Ok(())
}

/// The captured program's structural rules: declaration names are unique
/// (a program-built entry addresses its declaration by name), and every
/// system loading from the program artifact references one.
fn check_program(wiring: &Wiring) -> Result<(), LoadError> {
    if let Some(program) = &wiring.program {
        let mut seen = HashSet::new();
        for decl in &program.decls {
            if !seen.insert(&decl.name) {
                return Err(LoadError::DuplicateProgramDecl {
                    name: decl.name.clone(),
                });
            }
        }
    }
    let program_ids: HashSet<&str> = wiring
        .artifacts
        .iter()
        .filter(|a| a.is_program())
        .map(|a| a.id.as_str())
        .collect();
    for spec in &wiring.systems {
        let Some(artifact) = spec.artifact.as_deref() else {
            continue;
        };
        if !program_ids.contains(artifact) {
            continue;
        }
        let entry = spec.ty.as_deref().unwrap_or(spec.name.as_str());
        let declared = wiring
            .program
            .as_ref()
            .is_some_and(|p| p.decls.iter().any(|d| d.name == entry));
        if !declared {
            return Err(LoadError::ProgramUnknownDecl {
                name: entry.to_string(),
            });
        }
    }
    Ok(())
}

/// The [`Wiring`] must be stamped with this build's [`IR_VERSION`]. Spanless:
/// version skew is producer/host drift, not a document mistake.
fn check_ir_version(wiring: &Wiring) -> Result<(), LoadError> {
    if wiring.ir_version != IR_VERSION {
        return Err(LoadError::IrVersionMismatch {
            found: wiring.ir_version,
            expected: IR_VERSION,
        });
    }
    Ok(())
}

/// Range-check every scope index in the wiring: the specs' `scope` fields and
/// the table's own `parent` links. The table is front-end metadata, so a bad
/// index is a front-end bug, caught before any system is built.
fn check_scope_refs(wiring: &Wiring) -> Result<(), LoadError> {
    let len = wiring.scopes.len();
    let check = |owner: String, index: Option<usize>| match index {
        Some(index) if index >= len => Err(LoadError::BadScopeRef { owner, index, len }),
        _ => Ok(()),
    };
    for scope in &wiring.scopes {
        check(format!("scope `{}`", scope.path), scope.parent)?;
    }
    for spec in &wiring.systems {
        check(format!("system `{}`", spec.name), spec.scope)?;
    }
    for slot in &wiring.slots {
        check(format!("slot `{}`", slot.name), slot.scope)?;
    }
    Ok(())
}

/// Instance names, systems and slots in one flat namespace plus the reserved
/// coordinator, must be unique.
fn check_instance_names(wiring: &Wiring) -> Result<(), LoadError> {
    let mut seen = HashSet::from([RESERVED_INSTANCE]);
    for name in wiring
        .systems
        .iter()
        .map(|spec| &spec.name)
        .chain(wiring.slots.iter().map(|slot| &slot.name))
    {
        if !seen.insert(name) {
            return Err(LoadError::DuplicateInstance { name: name.clone() });
        }
    }
    Ok(())
}

/// Artifact ids must be unique: a system's `artifact=` and a slot's `allow`
/// address a pack by id, so a duplicate would silently shadow.
fn check_artifact_ids(wiring: &Wiring) -> Result<(), LoadError> {
    let mut seen = HashSet::new();
    for artifact in &wiring.artifacts {
        if !seen.insert(&artifact.id) {
            return Err(LoadError::DuplicateArtifact {
                id: artifact.id.clone(),
            });
        }
    }
    Ok(())
}

/// State names are unique: `attach` addresses a state by name, so a second
/// spec of one name could only shadow. Several states of one type are fine —
/// a target serving both a ground link and a peer link declares two
/// `TcpServer`s.
fn check_state_names(wiring: &Wiring) -> Result<(), LoadError> {
    let mut names = HashSet::new();
    for state in &wiring.states {
        if !names.insert(&state.name) {
            return Err(LoadError::DuplicateState {
                name: state.name.clone(),
            });
        }
    }
    Ok(())
}

/// Every `TcpServer` advertises a distinct mDNS instance name. An unnamed
/// server takes the target namespace, else the host name, so a second unnamed
/// server on one target collides exactly as two alike `name=` would.
fn check_link_names(wiring: &Wiring) -> Result<(), LoadError> {
    let default = wiring
        .coordinator
        .namespace
        .as_deref()
        .unwrap_or("<host name>");
    let mut names = HashSet::new();
    for state in &wiring.states {
        if state.ty != TCP_SERVER_TYPE {
            continue;
        }
        let name = match &state.params {
            ParamSource::Value(value) => value.get("name").and_then(|n| n.as_str()),
            _ => None,
        }
        .unwrap_or(default);
        if !names.insert(name) {
            return Err(LoadError::DuplicateLinkName {
                state: state.name.clone(),
                name: name.to_string(),
            });
        }
    }
    Ok(())
}

/// One `Downlink` per server. A second attached to the same state replays a
/// second announce set over the same clients, which the wire cannot carry.
fn check_downlinks(wiring: &Wiring) -> Result<(), LoadError> {
    let mut served = HashSet::new();
    for spec in &wiring.systems {
        if spec.ty.as_deref() != Some(DOWNLINK_TYPE) {
            continue;
        }
        if let Some(attach) = &spec.attach
            && !served.insert(attach)
        {
            return Err(LoadError::DuplicateDownlink {
                state: attach.clone(),
            });
        }
    }
    Ok(())
}

/// One state spec's structural rules: states construct on the static value
/// path only, so typed postcard params cannot reach one.
fn check_state(state: &StateSpec) -> Result<(), LoadError> {
    if matches!(state.params, ParamSource::Postcard(_)) {
        return Err(LoadError::StateInit {
            name: state.name.clone(),
            ty: state.ty.clone(),
            message: "typed postcard params cannot construct a state (states decode value trees)"
                .into(),
        });
    }
    Ok(())
}

/// One system spec's structural rules: a named artifact must exist, a
/// `process` system must name one, and a static system must carry a `type` and
/// no [`ParamSource::Postcard`] (the static path has no postcard decoder).
fn check_system(spec: &SystemSpec, wiring: &Wiring) -> Result<(), LoadError> {
    if let Some(peer) = &spec.peer {
        let reason = if !matches!(spec.params, ParamSource::None) {
            Some("a mirror runs the built-in subscriber, so it takes no params".to_string())
        } else if spec.process {
            Some("a mirror is an async client, not a worker process".to_string())
        } else if spec.attach.is_some() {
            Some("a mirror attaches to no state; it dials its peer".to_string())
        } else if peer.port == 0 {
            Some(format!(
                "peer `{}.{}` publishes on port 0, which is a listener choice, not an address",
                peer.namespace, peer.link
            ))
        } else if peer
            .host
            .as_ref()
            .is_some_and(|host| host.trim().is_empty())
        {
            Some(format!(
                "peer `{}.{}` has an empty host override; `--peer <ns>=<host>` names one",
                peer.namespace, peer.link
            ))
        } else if Some(peer.namespace.as_str()) == wiring.coordinator.namespace.as_deref() {
            Some(format!(
                "peer namespace `{}` is this target's own; a mirror names another member",
                peer.namespace
            ))
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(LoadError::PeerSpec {
                system: spec.name.clone(),
                reason,
            });
        }
    }
    // An `attach` must name a declared state, and only a static system can hold
    // one: a loaded/process pack cannot own shared state (the pack ABI forbids
    // it). The static shared-vs-plain check needs the registry and lives in
    // `resolve` instead.
    if let Some(attach) = &spec.attach {
        if !wiring.states.iter().any(|s| &s.name == attach) {
            return Err(LoadError::AttachUnknownState {
                system: spec.name.clone(),
                attach: attach.clone(),
            });
        }
        if spec.artifact.is_some() {
            return Err(LoadError::AttachOnNonSharedSystem {
                system: spec.name.clone(),
                attach: attach.clone(),
            });
        }
    }
    match (&spec.artifact, spec.process) {
        (Some(artifact), _) => {
            if !artifact_exists(wiring, artifact) {
                return Err(LoadError::UnknownArtifact {
                    system: spec.name.clone(),
                    artifact: artifact.clone(),
                });
            }
        }
        (None, true) => {
            return Err(LoadError::ProcessNeedsArtifact {
                name: spec.name.clone(),
            });
        }
        (None, false) => {
            let Some(ty) = spec.ty.as_deref() else {
                return Err(LoadError::MissingType {
                    name: spec.name.clone(),
                });
            };
            if matches!(spec.params, ParamSource::Postcard(_)) {
                return Err(LoadError::StaticPostcardParams {
                    system: spec.name.clone(),
                    ty: ty.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// One slot spec's structural rules: a non-empty allow set with the `initial`
/// name inside it ([`validate_slot_spec`]), and every `allow` that names an
/// artifact references a declared one.
fn check_slot(slot: &SlotSpec, wiring: &Wiring) -> Result<(), LoadError> {
    let names: Vec<&str> = slot.allow.iter().map(|a| a.occupant.as_str()).collect();
    validate_slot_spec(&names, slot.initial.as_ref().map(|i| i.occupant.as_str()))
        .map_err(|e| slot_config_error(e, slot))?;
    for occ in &slot.allow {
        if let Some(artifact) = &occ.artifact
            && !artifact_exists(wiring, artifact)
        {
            return Err(LoadError::UnknownArtifact {
                system: slot.name.clone(),
                artifact: artifact.clone(),
            });
        }
    }
    Ok(())
}

/// Whether `id` names a declared artifact.
fn artifact_exists(wiring: &Wiring, id: &str) -> bool {
    wiring.artifacts.iter().any(|a| a.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Artifact, ProgramDecl, ProgramSpec};
    use crate::wiring::WiringBuilder;

    /// A one-member envelope, its member namespaced when `namespace` is set.
    fn deployment(namespaces: &[Option<&str>]) -> Deployment {
        Deployment {
            ir_version: IR_VERSION,
            targets: namespaces
                .iter()
                .map(|ns| {
                    let mut w = WiringBuilder::new().build();
                    w.coordinator.namespace = ns.map(str::to_string);
                    w
                })
                .collect(),
            hosts: Default::default(),
        }
    }

    /// A target with two `TcpServer` states, advertising `names`.
    fn two_servers(namespace: Option<&str>, names: [Option<&str>; 2]) -> Wiring {
        let mut w = WiringBuilder::new().build();
        w.coordinator.namespace = namespace.map(str::to_string);
        w.states = ["a", "b"]
            .iter()
            .zip(names)
            .map(|(state, name)| {
                StateSpec::tcp_server_named(state, "0.0.0.0:0".parse().unwrap(), name)
            })
            .collect();
        w
    }

    #[test]
    fn servers_advertise_distinct_names() {
        assert!(validate(&two_servers(Some("sat"), [None, Some("b")])).is_ok());

        assert!(matches!(
            validate(&two_servers(Some("sat"), [None, None])).unwrap_err(),
            LoadError::DuplicateLinkName { state, name } if state == "b" && name == "sat"
        ));
        assert!(matches!(
            validate(&two_servers(None, [Some("one"), Some("one")])).unwrap_err(),
            LoadError::DuplicateLinkName { state, name } if state == "b" && name == "one"
        ));
    }

    /// A well-formed mirror on member `b` of the plant `a`.
    fn peer_spec() -> crate::ir::PeerSpec {
        crate::ir::PeerSpec {
            namespace: "a".into(),
            link: "peer".into(),
            port: 2242,
            instance: "plant".into(),
            telemetered: true,
            host: None,
        }
    }

    /// A member `b` whose one system mirrors `a.plant`, after `edit`.
    fn mirror(edit: impl FnOnce(&mut SystemSpec)) -> Wiring {
        let mut w = WiringBuilder::new()
            .subscribe("plant", "Plant", None, peer_spec())
            .build();
        w.coordinator.namespace = Some("b".into());
        edit(&mut w.systems[0]);
        w
    }

    #[test]
    fn a_mirror_takes_nothing_but_its_peer() {
        assert!(validate(&mirror(|_| {})).is_ok());

        let reason = |w: Wiring| match validate(&w).unwrap_err() {
            LoadError::PeerSpec { reason, .. } => reason,
            other => panic!("expected a peer-spec fault, got {other}"),
        };
        assert!(
            reason(mirror(
                |s| s.params = ParamSource::Value(serde_json::json!({}))
            ))
            .contains("params")
        );
        assert!(reason(mirror(|s| s.process = true)).contains("worker process"));
        assert!(
            reason(mirror(|s| s.attach = Some("link".into()))).contains("attaches to no state")
        );
        assert!(reason(mirror(|s| s.peer.as_mut().unwrap().port = 0)).contains("port 0"));
        assert!(
            reason(mirror(|s| s.peer.as_mut().unwrap().host = Some("  ".into())))
                .contains("empty host")
        );
        assert!(
            validate(&mirror(
                |s| s.peer.as_mut().unwrap().host = Some("10.0.0.5".into())
            ))
            .is_ok()
        );
        assert!(
            reason(mirror(|s| s.peer.as_mut().unwrap().namespace = "b".into()))
                .contains("this target's own")
        );
    }

    #[test]
    fn one_downlink_per_server() {
        let mut w = WiringBuilder::new()
            .serve("127.0.0.1:2240".parse().unwrap())
            .build();
        assert!(validate(&w).is_ok());

        w.systems.push(SystemSpec::downlink("second"));
        assert!(matches!(
            validate(&w).unwrap_err(),
            LoadError::DuplicateDownlink { state } if state == "link"
        ));
    }

    #[test]
    fn hosts_and_peers_name_members() {
        let members = |mirror: Option<Wiring>| {
            let mut a = WiringBuilder::new().build();
            a.coordinator.namespace = Some("a".into());
            Deployment {
                ir_version: IR_VERSION,
                targets: vec![a, mirror.unwrap_or_else(mirror_member)],
                hosts: Default::default(),
            }
        };
        fn mirror_member() -> Wiring {
            let mut w = WiringBuilder::new()
                .subscribe("plant", "Plant", None, peer_spec())
                .build();
            w.coordinator.namespace = Some("b".into());
            w
        }
        assert!(validate_deployment(&members(None)).is_ok());

        let mut unknown_host = members(None);
        unknown_host
            .hosts
            .insert("ground".into(), "10.0.0.9".into());
        assert!(matches!(
            validate_deployment(&unknown_host).unwrap_err(),
            LoadError::UnknownHost { namespace } if namespace == "ground"
        ));

        let mut elsewhere = mirror_member();
        elsewhere.systems[0].peer.as_mut().unwrap().namespace = "ground".into();
        assert!(matches!(
            validate_deployment(&members(Some(elsewhere))).unwrap_err(),
            LoadError::UnknownPeer { system, namespace } if system == "plant" && namespace == "ground"
        ));
    }

    /// The two builder spellings of the peer shapes render the specs the
    /// Python front end records.
    #[test]
    fn publish_and_subscribe_render_their_specs() {
        let w = WiringBuilder::new()
            .state("peer", "TcpServer")
            .publish("peer", ["plant"])
            .subscribe("mirror", "Plant", Some("adcs"), peer_spec())
            .build();

        let publish = &w.systems[0];
        assert_eq!(publish.name, "peer_publish");
        assert_eq!(publish.ty.as_deref(), Some(DOWNLINK_TYPE));
        assert_eq!(publish.attach.as_deref(), Some("peer"));
        assert_eq!(
            publish.params,
            ParamSource::Value(serde_json::json!({ "instances": ["plant"] }))
        );

        let subscribe = &w.systems[1];
        assert_eq!(subscribe.ty.as_deref(), Some("Plant"));
        assert_eq!(subscribe.artifact.as_deref(), Some("adcs"));
        assert_eq!(subscribe.params, ParamSource::None);
        assert_eq!(subscribe.peer, Some(peer_spec()));
    }

    #[test]
    fn envelope_version_and_emptiness_are_checked() {
        let mut d = deployment(&[None]);
        assert!(
            validate_deployment(&d).is_ok(),
            "one member needs no namespace"
        );

        d.ir_version = IR_VERSION - 1;
        assert!(matches!(
            validate_deployment(&d).unwrap_err(),
            LoadError::IrVersionMismatch { .. }
        ));

        let empty = deployment(&[]);
        assert!(matches!(
            validate_deployment(&empty).unwrap_err(),
            LoadError::EmptyDeployment
        ));
    }

    #[test]
    fn several_members_need_distinct_disjoint_namespaces() {
        assert!(
            validate_deployment(&deployment(&[Some("fleet.sat1"), Some("fleet.sat2")])).is_ok()
        );

        assert!(matches!(
            validate_deployment(&deployment(&[Some("fsw"), None])).unwrap_err(),
            LoadError::NamespaceRequired { index: 1 }
        ));
        assert!(matches!(
            validate_deployment(&deployment(&[Some("fsw"), Some("fsw")])).unwrap_err(),
            LoadError::DuplicateNamespace { namespace } if namespace == "fsw"
        ));
        assert!(matches!(
            validate_deployment(&deployment(&[Some("sat"), Some("sat.plant")])).unwrap_err(),
            LoadError::NamespaceOverlap { outer, inner } if outer == "sat" && inner == "sat.plant"
        ));
        assert!(matches!(
            validate_deployment(&deployment(&[Some("sat.plant"), Some("sat")])).unwrap_err(),
            LoadError::NamespaceOverlap { outer, inner } if outer == "sat" && inner == "sat.plant"
        ));
    }

    #[test]
    fn selection_covers_every_arm() {
        let one = deployment(&[None]);
        assert!(one.target(None).is_ok(), "the only member needs no request");
        assert!(matches!(
            one.target(Some("sat")).unwrap_err(),
            LoadError::UnknownTarget { requested, available }
                if requested == "sat" && available.is_empty()
        ));

        let named = deployment(&[Some("fsw")]);
        assert!(named.target(Some("fsw")).is_ok());

        let two = deployment(&[Some("plant"), Some("fsw")]);
        assert_eq!(
            two.target(Some("fsw"))
                .unwrap()
                .coordinator
                .namespace
                .as_deref(),
            Some("fsw")
        );
        assert!(matches!(
            two.target(None).unwrap_err(),
            LoadError::TargetRequired { available } if available == ["plant", "fsw"]
        ));
        assert!(matches!(
            two.target(Some("fws")).unwrap_err(),
            LoadError::UnknownTarget { requested, .. } if requested == "fws"
        ));

        assert!(matches!(
            deployment(&[]).target(None).unwrap_err(),
            LoadError::EmptyDeployment
        ));
    }

    fn program_wiring() -> Wiring {
        let mut wiring = WiringBuilder::new()
            .system("f")
            .ty("f")
            .from_artifact("program")
            .end()
            .build();
        wiring.artifacts.push(Artifact {
            id: "program".into(),
            kind: ArtifactKind::Wasm,
            crate_name: String::new(),
            lib: String::new(),
            path: None,
            prebuilt_dir: None,
            dist: None,
            manifest_hash: None,
            src: None,
        });
        wiring.program = Some(ProgramSpec {
            source: "def f() -> f64:\n    return 1.0\n".into(),
            decls: vec![ProgramDecl {
                name: "f".into(),
                src: None,
                offset: 0,
            }],
        });
        wiring
    }

    #[test]
    fn program_system_must_reference_a_program_decl() {
        assert!(validate(&program_wiring()).is_ok());

        let mut wiring = program_wiring();
        wiring.program.as_mut().unwrap().decls.clear();
        assert!(matches!(
            validate(&wiring).unwrap_err(),
            LoadError::ProgramUnknownDecl { name } if name == "f"
        ));
    }

    #[test]
    fn program_artifact_and_decl_shape_are_checked() {
        let mut wiring = program_wiring();
        wiring.program = None;
        assert!(matches!(
            validate(&wiring).unwrap_err(),
            LoadError::ProgramArtifactWithoutProgram { .. }
        ));

        let mut wiring = program_wiring();
        let decl = wiring.program.as_ref().unwrap().decls[0].clone();
        wiring.program.as_mut().unwrap().decls.push(decl);
        assert!(matches!(
            validate(&wiring).unwrap_err(),
            LoadError::DuplicateProgramDecl { .. }
        ));

        let mut wiring = program_wiring();
        wiring.artifacts[0].kind = ArtifactKind::Cdylib;
        assert!(matches!(
            validate(&wiring).unwrap_err(),
            LoadError::ArtifactMissingCrate { .. }
        ));
    }
}
