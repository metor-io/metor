//! The one structural gate every ingested [`Wiring`] passes before resolve.
//!
//! [`resolve_with`](super::resolve_with) runs [`validate`] first, so both
//! front-ends — the serde-ingested Python IR and the Rust
//! [`WiringBuilder`](super::WiringBuilder) — clear the same checks. The checks
//! here are the ones that need only the IR itself: version skew, scope-table
//! indices, name and id uniqueness, and each spec's own well-formedness.
//! Anything needing the [`Registry`](super::Registry) or the filesystem
//! (unknown types, manifest freshness, occupant/entry resolution, a built
//! artifact path) stays in resolve, next to what it needs.
//!
use std::collections::HashSet;

use super::model::{EXPR_TYPE, IR_VERSION};
use super::model::{ParamSource, SlotSpec, StateSpec, SystemSpec, Wiring};
use super::resolve::slot_config_error;
use super::{LoadError, LoadErrorKind};
use crate::coordinator::validate_slot_spec;

/// The instance name the coordinator itself occupies. A user spec of this name
/// collides with it, surfacing as a [`DuplicateInstance`](LoadErrorKind::DuplicateInstance).
const RESERVED_INSTANCE: &str = "coordinator";

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
    check_program(wiring)?;
    for spec in &wiring.systems {
        check_system(spec, wiring)?;
    }
    for slot in &wiring.slots {
        check_slot(slot, wiring)?;
    }
    Ok(())
}

/// Whether a spec is a compiled Python system: the reserved [`EXPR_TYPE`]
/// with no artifact (a pack may still export an entry named `expr`).
pub(crate) fn is_expr(spec: &SystemSpec) -> bool {
    spec.artifact.is_none() && spec.ty.as_deref() == Some(EXPR_TYPE)
}

/// The captured program's structural rules: declaration names are unique
/// (an `expr` system addresses its declaration by name), and every `expr`
/// system references one, carries no params, and attaches to nothing.
fn check_program(wiring: &Wiring) -> Result<(), LoadError> {
    if let Some(program) = &wiring.program {
        let mut seen = HashSet::new();
        for decl in &program.decls {
            if !seen.insert(&decl.name) {
                return Err(LoadErrorKind::DuplicateProgramDecl {
                    name: decl.name.clone(),
                }
                .bare());
            }
        }
    }
    for spec in wiring.systems.iter().filter(|s| is_expr(s)) {
        let declared = wiring
            .program
            .as_ref()
            .is_some_and(|p| p.decls.iter().any(|d| d.name == spec.name));
        if !declared {
            return Err(LoadErrorKind::ExprUnknownDecl {
                name: spec.name.clone(),
            }
            .bare());
        }
        if !matches!(spec.params, ParamSource::None) {
            return Err(LoadErrorKind::ExprParams {
                name: spec.name.clone(),
            }
            .bare());
        }
        if let Some(attach) = &spec.attach {
            return Err(LoadErrorKind::AttachOnNonSharedSystem {
                system: spec.name.clone(),
                attach: attach.clone(),
            }
            .bare());
        }
    }
    Ok(())
}

/// The [`Wiring`] must be stamped with this build's [`IR_VERSION`]. Spanless:
/// version skew is producer/host drift, not a document mistake.
fn check_ir_version(wiring: &Wiring) -> Result<(), LoadError> {
    if wiring.ir_version != IR_VERSION {
        return Err(LoadErrorKind::IrVersionMismatch {
            found: wiring.ir_version,
            expected: IR_VERSION,
        }
        .bare());
    }
    Ok(())
}

/// Range-check every scope index in the wiring — the specs' `scope` fields and
/// the table's own `parent` links. The table is front-end metadata, so a bad
/// index is a front-end bug, caught before any system is built.
fn check_scope_refs(wiring: &Wiring) -> Result<(), LoadError> {
    let len = wiring.scopes.len();
    let check = |owner: String, index: Option<usize>| match index {
        Some(index) if index >= len => Err(LoadErrorKind::BadScopeRef { owner, index, len }.bare()),
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

/// Instance names — systems and slots in one flat namespace, plus the reserved
/// coordinator — must be unique.
fn check_instance_names(wiring: &Wiring) -> Result<(), LoadError> {
    let mut seen = HashSet::from([RESERVED_INSTANCE]);
    for name in wiring
        .systems
        .iter()
        .map(|spec| &spec.name)
        .chain(wiring.slots.iter().map(|slot| &slot.name))
    {
        if !seen.insert(name) {
            return Err(LoadErrorKind::DuplicateInstance { name: name.clone() }.bare());
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
            return Err(LoadErrorKind::DuplicateArtifact {
                id: artifact.id.clone(),
            }
            .bare());
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
            return Err(LoadErrorKind::DuplicateState {
                name: state.name.clone(),
            }
            .bare());
        }
    }
    Ok(())
}

/// One state spec's structural rules: states construct on the static value
/// path only, so typed postcard params cannot reach one.
fn check_state(state: &StateSpec) -> Result<(), LoadError> {
    if matches!(state.params, ParamSource::Postcard(_)) {
        return Err(LoadErrorKind::StateInit {
            name: state.name.clone(),
            ty: state.ty.clone(),
            message: "typed postcard params cannot construct a state (states decode value trees)"
                .into(),
        }
        .bare());
    }
    Ok(())
}

/// One system spec's structural rules: a named artifact must exist, a
/// `process` system must name one, and a static system must carry a `type` and
/// no [`ParamSource::Postcard`] (the static path has no postcard decoder).
fn check_system(spec: &SystemSpec, wiring: &Wiring) -> Result<(), LoadError> {
    // An `attach` must name a declared state, and only a static system can hold
    // one — a loaded/process pack cannot own shared state (the pack ABI forbids
    // it). The static shared-vs-plain check needs the registry and lives in
    // `resolve` instead.
    if let Some(attach) = &spec.attach {
        if !wiring.states.iter().any(|s| &s.name == attach) {
            return Err(LoadErrorKind::AttachUnknownState {
                system: spec.name.clone(),
                attach: attach.clone(),
            }
            .bare());
        }
        if spec.artifact.is_some() {
            return Err(LoadErrorKind::AttachOnNonSharedSystem {
                system: spec.name.clone(),
                attach: attach.clone(),
            }
            .bare());
        }
    }
    match (&spec.artifact, spec.process) {
        (Some(artifact), _) => {
            if !artifact_exists(wiring, artifact) {
                return Err(LoadErrorKind::UnknownArtifact {
                    system: spec.name.clone(),
                    artifact: artifact.clone(),
                }
                .bare());
            }
        }
        (None, true) => {
            return Err(LoadErrorKind::ProcessNeedsArtifact {
                name: spec.name.clone(),
            }
            .bare());
        }
        (None, false) => {
            let Some(ty) = spec.ty.as_deref() else {
                return Err(LoadErrorKind::MissingType {
                    name: spec.name.clone(),
                }
                .bare());
            };
            if matches!(spec.params, ParamSource::Postcard(_)) {
                return Err(LoadErrorKind::StaticPostcardParams {
                    system: spec.name.clone(),
                    ty: ty.to_string(),
                }
                .bare());
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
            return Err(LoadErrorKind::UnknownArtifact {
                system: slot.name.clone(),
                artifact: artifact.clone(),
            }
            .bare());
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
    use crate::ir::{ProgramDecl, ProgramSpec};
    use crate::wiring::WiringBuilder;

    fn expr_wiring() -> Wiring {
        let mut wiring = WiringBuilder::new().system("f").ty(EXPR_TYPE).end().build();
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
    fn expr_system_must_reference_a_program_decl() {
        assert!(validate(&expr_wiring()).is_ok());

        let mut wiring = expr_wiring();
        wiring.program = None;
        assert!(matches!(
            validate(&wiring).unwrap_err().kind,
            LoadErrorKind::ExprUnknownDecl { name } if name == "f"
        ));
    }

    #[test]
    fn expr_system_params_and_duplicate_decls_are_rejected() {
        let mut wiring = expr_wiring();
        wiring.systems[0].params = ParamSource::Value(serde_json::json!({ "a": 1 }));
        assert!(matches!(
            validate(&wiring).unwrap_err().kind,
            LoadErrorKind::ExprParams { .. }
        ));

        let mut wiring = expr_wiring();
        let decl = wiring.program.as_ref().unwrap().decls[0].clone();
        wiring.program.as_mut().unwrap().decls.push(decl);
        assert!(matches!(
            validate(&wiring).unwrap_err().kind,
            LoadErrorKind::DuplicateProgramDecl { .. }
        ));
    }
}
