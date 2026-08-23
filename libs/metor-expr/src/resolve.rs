//! The boundary where names become addresses.
//!
//! The compiler cannot know what `adcs.omega_b` is. Only a host can — the
//! panel from its component vtables, the vehicle from its frame registry — so
//! rather than depend on either, the compiler asks. [`Resolver`] is the whole
//! of that dependency: three questions, answered once, at compile time.
//!
//! "At compile time" is the load-bearing half. A bare name resolves by unique
//! suffix, which is a fact about the component tree *now*; a component added
//! tomorrow would make the same name ambiguous. So the suffix rule never
//! outlives authoring: what the manifest records is the resolved full path,
//! and a saved expression is read back through that path rather than through
//! the search that found it.

use crate::Ty;

/// A component's shape and element type, as the host knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompSchema {
    pub ty: Ty,
}

/// A frame the host already defines: what a `bind=` target must match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameSchema {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
}

/// What a host must answer for a program to be compiled against it.
pub trait Resolver: Sync {
    /// The component at a full dotted path, if one exists.
    fn component(&self, path: &str) -> Option<CompSchema>;

    /// Every full path ending in `.name`, for the one-liner tier's bare
    /// names. More than one is an ambiguity the caller reports; the compiler
    /// never picks.
    fn suffix(&self, name: &str) -> Vec<String>;

    /// A frame the host defines, for checking a `bind=` target field by
    /// field.
    fn frame(&self, name: &str) -> Option<FrameSchema>;
}

/// A host that knows nothing, for compiling a module of plain `def`s.
pub struct Unresolved;

impl Resolver for Unresolved {
    fn component(&self, _path: &str) -> Option<CompSchema> {
        None
    }

    fn suffix(&self, _name: &str) -> Vec<String> {
        Vec::new()
    }

    fn frame(&self, _name: &str) -> Option<FrameSchema> {
        None
    }
}
