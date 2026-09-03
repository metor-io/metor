use darling::FromField;

/// A single struct field as parsed by the component derives, pairing its
/// ident and type with the `#[fsw(...)]`/`#[metor_fsw(...)]` attributes they
/// all share.
#[derive(Debug, FromField)]
#[darling(attributes(fsw, metor_fsw))]
pub struct Field {
    pub ident: Option<syn::Ident>,
    pub ty: syn::Type,
    pub component_id: Option<String>,
    #[darling(default)]
    pub timestamp: bool,
    /// Descend into a sub-frame/struct through the component traits instead
    /// of treating the field as a leaf scalar.
    #[darling(default)]
    pub nest: bool,
    /// Max cardinality for a `FrameList`/`FrameMap` field. The const-generic
    /// on the type is the source of truth; this is accepted for
    /// forward-compat but unused by the derives.
    #[darling(default)]
    #[allow(dead_code)]
    pub max: Option<usize>,
    /// `#[fsw(skip)]` force-hides a field from telemetry; `#[fsw(skip =
    /// false)]` opts a `_`-prefixed field back in. Absent, a field is skipped
    /// iff its name starts with `_` — the convention for `#[repr(C)]`
    /// padding.
    #[darling(default)]
    pub skip: Option<bool>,
}

impl Field {
    /// The field's component id, defaulting to the field name.
    pub fn component_name(&self) -> String {
        match &self.component_id {
            Some(c) => c.clone(),
            None => {
                let ident = self.ident.as_ref().expect("field must have ident");
                ident.to_string()
            }
        }
    }

    /// The component id under an optional dotted prefix.
    pub fn qualified_component_name(&self, parent: Option<&str>) -> String {
        let name = self.component_name();
        match parent {
            Some(parent) => format!("{parent}.{name}"),
            None => name,
        }
    }

    /// Whether the field is omitted from telemetry: never becomes a component
    /// and never round-trips through encode/decode. `_`-prefixed fields skip
    /// by default (padding), overridable in either direction with
    /// `#[fsw(skip)]`.
    pub fn skipped(&self) -> bool {
        self.skip.unwrap_or_else(|| {
            self.ident
                .as_ref()
                .is_some_and(|i| i.to_string().starts_with('_'))
        })
    }

    /// Whether the field recurses through the component traits rather than
    /// emitting a scalar leaf, either explicitly via `#[fsw(nest)]` or
    /// implicitly because it is dynamic.
    pub fn is_nested(&self) -> bool {
        self.nest || self.is_dynamic()
    }

    /// Whether the field type's outermost path segment is `FrameList` or
    /// `FrameMap`. Such fields carry no in-struct value, so the scalar
    /// encode/decode paths skip them and `MAX_SIZE` sizes their trailer
    /// instead.
    pub fn is_dynamic(&self) -> bool {
        if let syn::Type::Path(p) = &self.ty
            && let Some(seg) = p.path.segments.last()
        {
            return seg.ident == "FrameList" || seg.ident == "FrameMap";
        }
        false
    }
}
