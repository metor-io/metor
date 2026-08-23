//! What `adcs.omega_b` means, answered from the component tree.
//!
//! `metor-expr` refuses to know what a name is; it asks a host, once, at
//! compile time. This is the panel's answer — a snapshot of the db's visible
//! components taken before compiling, not a live view of it.
//!
//! Snapshotting rather than borrowing is the point. Compilation runs on the
//! [`DynamicWorker`](crate::node_editor::worker) thread while the UI thread
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
use metor_proto::types::PrimType;

/// The component tree as it stood when a compile began.
pub struct DbResolver {
    components: BTreeMap<String, Ty>,
}

impl DbResolver {
    pub fn snapshot(db: &DB) -> Self {
        let components = db.with_state(|state| {
            state
                .component_metadata_iter()
                .filter(|(_, meta)| !meta.is_hidden())
                .filter_map(|(id, meta)| {
                    let schema = &state.get_component(*id)?.schema;
                    Some((meta.name.clone(), ty_of(schema.prim_type, &schema.dim)?))
                })
                .collect()
        });
        DbResolver { components }
    }

    /// Every component the language can address, for the picker's expression
    /// mode to offer as completions.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.components.keys().map(String::as_str)
    }
}

impl Resolver for DbResolver {
    fn component(&self, path: &str) -> Option<CompSchema> {
        self.components
            .get(path)
            .map(|ty| CompSchema { ty: ty.clone() })
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
}

/// A component's type in the language.
///
/// Narrower element types widen: an `f32` channel reads as `f64` and an `i32`
/// as `i64`, because the language has no narrower numbers and a frame field is
/// eight bytes per element either way. The widening is the runtime's to
/// perform when it fills the frame.
fn ty_of(prim: PrimType, dim: &[usize]) -> Option<Ty> {
    match (prim, dim.is_empty()) {
        (PrimType::F64 | PrimType::F32, true) => Some(Ty::F64),
        (PrimType::I64 | PrimType::I32 | PrimType::I16 | PrimType::I8, true) => Some(Ty::I64),
        (PrimType::U64 | PrimType::U32 | PrimType::U16 | PrimType::U8, true) => Some(Ty::I64),
        (PrimType::Bool, true) => Some(Ty::Bool),
        (PrimType::F64 | PrimType::F32, false) => Some(Ty::Tensor {
            dtype: Dtype::F64,
            shape: dim.to_vec(),
        }),
        _ => None,
    }
}
