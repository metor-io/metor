//! What `adcs.omega_b` means, answered from the component tree.
//!
//! `metor-expr` refuses to know what a name is; it asks a host, once, at
//! compile time. This is the panel's answer — a snapshot of the db's visible
//! components taken before compiling, not a live view of it.
//!
//! Snapshotting rather than borrowing is the point. Compilation runs on the
//! [`DynamicWorker`](crate::dynamic::worker) thread while the UI thread
//! keeps touching the db, and a resolver that held the state lock would put
//! the two in each other's way for as long as a parse takes. A snapshot also
//! makes the resolution *reproducible*: every name in one compile resolves
//! against the same component tree, so a component appearing mid-parse cannot
//! make two halves of the same expression disagree.
//!
//! Only `f64`-shaped components are offered. The language's tensors are `f64`
//! today, and a component the compiler cannot type is better absent — a "no
//! component `x`" diagnostic naming what is missing beats a type error about
//! a name the operator did not know was the wrong shape.

use std::collections::BTreeMap;

use metor_db::DB;
use metor_expr::{CompSchema, Dtype, FrameSchema, Resolver, Ty};
use metor_proto::types::{ComponentId, PrimType};

/// The component tree as it stood when a compile began.
pub struct DbResolver {
    components: BTreeMap<String, (ComponentId, Ty)>,
}

impl DbResolver {
    pub fn snapshot(db: &DB) -> Self {
        let components = db.with_state(|state| {
            state
                .component_metadata_iter()
                .filter(|(_, meta)| !meta.is_hidden())
                .filter_map(|(id, meta)| {
                    let schema = &state.get_component(*id)?.schema;
                    Some((
                        meta.name.clone(),
                        (*id, ty_of(schema.prim_type, &schema.dim)?),
                    ))
                })
                .collect()
        });
        DbResolver { components }
    }

    /// The id of a component this resolver resolved.
    ///
    /// The id is carried, never re-derived. A component's id belongs to
    /// whoever created it — a producer names its own channels, and
    /// `ComponentId::new` masks a bit that `persist`'s hash does not — so
    /// hashing a name a second time agrees with the real id for only about
    /// half of all names. That is a bug that hides: it looks like the
    /// component has not published.
    pub fn id_of(&self, path: &str) -> Option<ComponentId> {
        self.components.get(path).map(|(id, _)| *id)
    }
}

impl Resolver for DbResolver {
    fn component(&self, path: &str) -> Option<CompSchema> {
        self.components
            .get(path)
            .map(|(_, ty)| CompSchema { ty: ty.clone() })
    }

    fn suffix(&self, name: &str) -> Vec<String> {
        let tail = format!(".{name}");
        self.components
            .keys()
            .filter(|path| path.ends_with(&tail) || *path == name)
            .cloned()
            .collect()
    }

    /// The panel declares no frames of its own — a component tree is flat, and
    /// the frames a program uses are the ones it declares. FSW is where a
    /// `bind=` target can name a frame the host already defines.
    fn frame(&self, _name: &str) -> Option<FrameSchema> {
        None
    }

    /// Every component the language can address, for completion to offer.
    fn paths(&self) -> Vec<String> {
        self.components.keys().cloned().collect()
    }
}

/// A component's type in the language.
///
/// Everything numeric reads as `f64`, whatever its element type on the wire.
/// That is not a simplification imposed here — it is what the panel already
/// does (`dynamic/tensor.rs` computes in `f64` and casts at write time) and
/// what the runtime does when it fills a frame, so offering an `i32` counter
/// as `i64` would be fidelity in name only. `bool` stays itself, because a
/// flag read as a number is worse to write conditions against.
///
/// One rule, and it lets a single expression span a float sensor and an
/// integer counter without saying so.
fn ty_of(prim: PrimType, dim: &[usize]) -> Option<Ty> {
    match (prim, dim.is_empty()) {
        (PrimType::Bool, true) => Some(Ty::Bool),
        (PrimType::Bool, false) => None,
        (_, true) => Some(Ty::F64),
        (_, false) => Some(Ty::Tensor {
            dtype: Dtype::F64,
            shape: dim.to_vec(),
        }),
    }
}
