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
//! The per-item helpers are `pub(crate)` so the builder can call the same
//! logic at insert time and panic on violation: a builder-produced `Wiring`
//! always passes [`validate`], so the panic can never fire on a wiring that
//! reached resolve through the builder.

use miette::SourceSpan;

use super::model::IR_VERSION;
use super::model::{Artifact, ParamSource, SlotSpec, StateSpec, SystemSpec, Wiring};
use super::resolve::{slot_config_error, slot_src, src_anchor, state_src, system_src};
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
    for spec in &wiring.systems {
        check_system(spec, wiring)?;
    }
    for slot in &wiring.slots {
        check_slot(slot, wiring)?;
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
/// coordinator — must be unique. This is the whole-wiring twin of
/// [`check_instance_available`], which the builder runs per insert.
fn check_instance_names(wiring: &Wiring) -> Result<(), LoadError> {
    let mut seen: Vec<&str> = vec![RESERVED_INSTANCE];
    for spec in &wiring.systems {
        if seen.contains(&spec.name.as_str()) {
            return Err(LoadErrorKind::DuplicateInstance {
                name: spec.name.clone(),
            }
            .whole(system_src(spec)));
        }
        seen.push(&spec.name);
    }
    for slot in &wiring.slots {
        if seen.contains(&slot.name.as_str()) {
            return Err(LoadErrorKind::DuplicateInstance {
                name: slot.name.clone(),
            }
            .whole(slot_src(slot)));
        }
        seen.push(&slot.name);
    }
    Ok(())
}

/// Artifact ids must be unique: a system's `artifact=` and a slot's `allow`
/// address a pack by id, so a duplicate would silently shadow.
fn check_artifact_ids(wiring: &Wiring) -> Result<(), LoadError> {
    let mut seen: Vec<&str> = Vec::new();
    for artifact in &wiring.artifacts {
        if seen.contains(&artifact.id.as_str()) {
            return Err(LoadErrorKind::DuplicateArtifact {
                id: artifact.id.clone(),
            }
            .whole(artifact_src(artifact)));
        }
        seen.push(&artifact.id);
    }
    Ok(())
}

/// State names and types are each unique: a state type has exactly one
/// instance (the pack declared one cell), so a second spec of either kind
/// could only shadow or double-construct.
fn check_state_names(wiring: &Wiring) -> Result<(), LoadError> {
    let mut names: Vec<&str> = Vec::new();
    let mut types: Vec<&str> = Vec::new();
    for state in &wiring.states {
        if names.contains(&state.name.as_str()) || types.contains(&state.ty.as_str()) {
            return Err(LoadErrorKind::DuplicateState {
                name: state.name.clone(),
            }
            .whole(state_src(state)));
        }
        names.push(&state.name);
        types.push(&state.ty);
    }
    Ok(())
}

/// One state spec's structural rules: states construct on the static value
/// path only, so typed postcard params cannot reach one.
pub(crate) fn check_state(state: &StateSpec) -> Result<(), LoadError> {
    if matches!(state.params, ParamSource::Postcard(_)) {
        return Err(LoadErrorKind::StateInit {
            name: state.name.clone(),
            ty: state.ty.clone(),
            message: "typed postcard params cannot construct a state (states decode value trees)"
                .into(),
        }
        .whole(state_src(state)));
    }
    Ok(())
}

/// One system spec's structural rules: a named artifact must exist, a
/// `process` system must name one, and a static system must carry a `type` and
/// no [`ParamSource::Postcard`] (the static path has no postcard decoder).
pub(crate) fn check_system(spec: &SystemSpec, wiring: &Wiring) -> Result<(), LoadError> {
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
            .whole(system_src(spec)));
        }
        if spec.artifact.is_some() {
            return Err(LoadErrorKind::AttachOnNonSharedSystem {
                system: spec.name.clone(),
                attach: attach.clone(),
            }
            .whole(system_src(spec)));
        }
    }
    match (&spec.artifact, spec.process) {
        (Some(artifact), _) => {
            if !artifact_exists(wiring, artifact) {
                return Err(LoadErrorKind::UnknownArtifact {
                    system: spec.name.clone(),
                    artifact: artifact.clone(),
                }
                .whole(system_src(spec)));
            }
        }
        (None, true) => {
            return Err(LoadErrorKind::ProcessNeedsArtifact {
                name: spec.name.clone(),
            }
            .whole(system_src(spec)));
        }
        (None, false) => {
            let Some(ty) = spec.ty.as_deref() else {
                return Err(LoadErrorKind::MissingType {
                    name: spec.name.clone(),
                }
                .whole(system_src(spec)));
            };
            if matches!(spec.params, ParamSource::Postcard(_)) {
                return Err(LoadErrorKind::StaticPostcardParams {
                    system: spec.name.clone(),
                    ty: ty.to_string(),
                }
                .whole(system_src(spec)));
            }
        }
    }
    Ok(())
}

/// One slot spec's structural rules: a non-empty allow set with the `initial`
/// name inside it ([`validate_slot_spec`]), and every `allow` that names an
/// artifact references a declared one.
pub(crate) fn check_slot(slot: &SlotSpec, wiring: &Wiring) -> Result<(), LoadError> {
    check_slot_spec(slot)?;
    let src = slot_src(slot);
    let span: SourceSpan = (0, src.len()).into();
    for occ in &slot.allow {
        if let Some(artifact) = &occ.artifact
            && !artifact_exists(wiring, artifact)
        {
            return Err(LoadErrorKind::UnknownArtifact {
                system: slot.name.clone(),
                artifact: artifact.clone(),
            }
            .at(src.clone(), span));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-item helpers the builder calls at insert time.
// ---------------------------------------------------------------------------

/// A new instance `name` must not collide with an existing system, slot, or
/// the reserved coordinator. Spanless: the builder formats it into a panic.
pub(crate) fn check_instance_available(wiring: &Wiring, name: &str) -> Result<(), LoadError> {
    if name == RESERVED_INSTANCE
        || wiring.systems.iter().any(|s| s.name == name)
        || wiring.slots.iter().any(|s| s.name == name)
    {
        return Err(LoadErrorKind::DuplicateInstance {
            name: name.to_string(),
        }
        .bare());
    }
    Ok(())
}

/// A new artifact `id` must not duplicate a declared one.
pub(crate) fn check_artifact_id_available(wiring: &Wiring, id: &str) -> Result<(), LoadError> {
    if artifact_exists(wiring, id) {
        return Err(LoadErrorKind::DuplicateArtifact { id: id.to_string() }.bare());
    }
    Ok(())
}

/// An `artifact=`/`allow_from` reference must name a declared artifact. `owner`
/// is the referencing system or slot, for the message.
pub(crate) fn check_artifact_declared(
    wiring: &Wiring,
    owner: &str,
    id: &str,
) -> Result<(), LoadError> {
    if !artifact_exists(wiring, id) {
        return Err(LoadErrorKind::UnknownArtifact {
            system: owner.to_string(),
            artifact: id.to_string(),
        }
        .bare());
    }
    Ok(())
}

/// The pure-spec half of slot validation ([`validate_slot_spec`]): a non-empty
/// allow set with the `initial` name inside it. Shared by [`check_slot`] and
/// the builder's `slot(..).end()`.
pub(crate) fn check_slot_spec(slot: &SlotSpec) -> Result<(), LoadError> {
    let src = slot_src(slot);
    let span: SourceSpan = (0, src.len()).into();
    let names: Vec<&str> = slot.allow.iter().map(|a| a.occupant.as_str()).collect();
    validate_slot_spec(&names, slot.initial.as_ref().map(|i| i.occupant.as_str()))
        .map_err(|e| slot_config_error(e, slot, &src, span))
}

/// Whether `id` names a declared artifact.
fn artifact_exists(wiring: &Wiring, id: &str) -> bool {
    wiring.artifacts.iter().any(|a| a.id == id)
}

/// A best-effort source snippet for an artifact's structural errors.
fn artifact_src(artifact: &Artifact) -> String {
    format!(
        "artifact \"{}\"{}",
        artifact.id,
        src_anchor(artifact.src.as_ref())
    )
}
