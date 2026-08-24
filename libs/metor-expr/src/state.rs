//! Carrying state across a rebuild.
//!
//! Editing must not reset the world. A system is hashed on its own source, so
//! an edit rebuilds one instance and leaves the rest running — but the one
//! that *is* rebuilt would come up at its defaults, and a filter that forgets
//! itself every keystroke is not a filter. So the host takes a snapshot before
//! the swap and seeds the new instance from it.
//!
//! The rule is the design doc's: state is keyed `(system, field, type)`, and a
//! triple that no longer matches is simply not restored. Renaming a field or
//! changing its shape resets that field and nothing else — which is the
//! behaviour an operator can predict without knowing anything about how the
//! rebuild works.
//!
//! This module does the keying and the matching. It does not touch a wasm
//! instance, because both hosts already own one and their ways of reading
//! memory differ: what it hands back is *where to copy*, as slot indices to
//! pass to `<system>_state_ptr(i)` and byte counts. The last slot of every
//! stateful system is the seed guard, which is why a restore that copies every
//! slot it is given also stops the first evaluation from overwriting what it
//! just wrote.

use crate::{Manifest, Ty};

/// The state field `random()` advances.
///
/// A declared field is a Python identifier, so none can collide with this one
/// — which matters because it is real state: it is snapshotted and restored
/// like any other field, so an edit does not restart the sequence. The host
/// writes it at instantiation, since its declared default is zero and a shared
/// seed would make two generators draw the same numbers.
pub const RNG_FIELD: &str = "@rng";

/// What makes two state fields the same field across a rebuild.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateKey {
    pub system: String,
    pub field: String,
    pub ty: Ty,
}

/// One block of state to copy, and where to find it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub key: StateKey,
    /// Index into [`Manifest::systems`], for naming `<system>_state_ptr`.
    pub system: usize,
    /// The argument to `<system>_state_ptr`.
    pub index: usize,
    pub bytes: u32,
}

/// Every state field a program holds, in the order a host should read them.
///
/// The seed guard is deliberately absent: it is not state, it is a fact about
/// one instance, and snapshotting it would be meaningless.
pub fn slots(manifest: &Manifest) -> Vec<Slot> {
    let mut out = Vec::new();
    for (system, desc) in manifest.systems.iter().enumerate() {
        for (index, field) in desc.state.iter().enumerate() {
            out.push(Slot {
                key: StateKey {
                    system: desc.name.clone(),
                    field: field.name.clone(),
                    ty: field.ty.clone(),
                },
                system,
                index,
                bytes: bytes_of(&field.ty),
            });
        }
    }
    out
}

/// The seed guard of each stateful system: the slot index a restore writes
/// `1` into so the rebuilt instance does not seed over what it was given.
pub fn guards(manifest: &Manifest) -> Vec<(usize, usize)> {
    manifest
        .systems
        .iter()
        .enumerate()
        .filter(|(_, desc)| !desc.state.is_empty())
        .map(|(system, desc)| (system, desc.state.len()))
        .collect()
}

/// One instance's state, read out before it is dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub entries: Vec<(StateKey, Vec<u8>)>,
}

impl Snapshot {
    /// Where each carried-over field belongs in a rebuilt program.
    ///
    /// A field whose triple no longer matches is left out, and so is a field
    /// the new program does not have — this is the whole of the compatibility
    /// rule, and there is not a second one anywhere.
    pub fn restore<'a>(&'a self, manifest: &Manifest) -> Vec<(Slot, &'a [u8])> {
        slots(manifest)
            .into_iter()
            .filter_map(|slot| {
                let (_, bytes) = self.entries.iter().find(|(key, _)| *key == slot.key)?;
                (bytes.len() as u32 == slot.bytes).then_some((slot, bytes.as_slice()))
            })
            .collect()
    }
}

fn bytes_of(ty: &Ty) -> u32 {
    match ty {
        Ty::Tensor { shape, .. } => shape.iter().product::<usize>() as u32 * 8,
        _ => 8,
    }
}
