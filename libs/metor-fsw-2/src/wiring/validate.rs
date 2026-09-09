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
    ArtifactKind, Deployment, ParamSource, SlotSpec, StateSpec, SystemSpec, Wiring,
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
        return Ok(());
    }

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

/// State names and types are each unique: a state type has exactly one
/// instance (the pack declared one cell), so a second spec of either kind
/// could only shadow or double-construct.
fn check_state_names(wiring: &Wiring) -> Result<(), LoadError> {
    let mut names = HashSet::new();
    let mut types = HashSet::new();
    for state in &wiring.states {
        if !names.insert(&state.name) || !types.insert(&state.ty) {
            return Err(LoadError::DuplicateState {
                name: state.name.clone(),
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
        }
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
