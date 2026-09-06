//! Per-field params documentation for generated pack modules (ABI v6).
//!
//! `#[derive(ParamsDocs)]` on a system's `Params` struct submits its field
//! `#[doc]` comments into a pack-crate-local [`inventory`] collection. Because
//! a pack's manifest is built *inside* its own `.so`, [`run_pack_describe`]
//! there can look each entry's params docs up by schema name and ship them in
//! the manifest ([`PackEntryDesc::params_docs`]), with no doc field crossing
//! the ABI as anything but plain strings, and no trait bound forcing every
//! params type to opt in. Pack module generation renders them as the units
//! and prose that make params usable.
//!
//! [`run_pack_describe`]: crate::abi::run_pack_describe
//! [`PackEntryDesc::params_docs`]: crate::abi::PackEntryDesc

/// One params struct's documented fields, submitted by [`ParamsDocs`] and
/// keyed by the struct's postcard-schema name.
///
/// [`ParamsDocs`]: macro@crate::ParamsDocs
pub struct ParamsDocEntry {
    /// The `Params` type's schema name (`<P as Schema>::SCHEMA.name`), which is
    /// the struct identifier the derive spells.
    pub schema: &'static str,
    /// `(field name, doc text)` for every field that carries a doc comment.
    pub fields: &'static [(&'static str, &'static str)],
}

inventory::collect!(ParamsDocEntry);

/// The `(field, doc)` pairs a params type of schema name `schema` declared, or
/// empty when it derived no [`ParamsDocs`](macro@crate::ParamsDocs) (docs are
/// optional). Called from the manifest describe path inside a pack `.so`.
pub(crate) fn params_docs_for(schema: &str) -> Vec<(String, String)> {
    inventory::iter::<ParamsDocEntry>
        .into_iter()
        .find(|e| e.schema == schema)
        .map(|e| {
            e.fields
                .iter()
                .map(|(f, d)| ((*f).to_string(), (*d).to_string()))
                .collect()
        })
        .unwrap_or_default()
}
