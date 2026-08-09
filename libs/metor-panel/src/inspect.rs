//! Facet attribute grammar driving inspector rendering.
//!
//! Types opt into richer inspector behavior by annotating fields with
//! `#[facet(inspect::...)]`. The registry dispatch in `inspector::registry`
//! reads these attributes when choosing a row builder.
//!
//! The grammar lives in its own module (not the derive crate) because the
//! `define_attr_grammar!` macro needs to resolve `Attr` at the call site.
facet::define_attr_grammar! {
    ns "inspect";
    crate_path ::metor_panel::inspect;

    /// Inspector attributes for metor-panel fields and types.
    pub enum Attr {
        /// Override the display label shown in the inspector.
        ///
        /// Usage: `#[facet(inspect::label = "Display Name")]`
        #[target(field)]
        Label(&'static str),

        /// Restrict the enum variants shown by the inspector.
        ///
        /// Names are comma-separated and may contain surrounding whitespace.
        /// Usage: `#[facet(inspect::variants = "Line, Scatter")]`
        #[target(field)]
        Variants(&'static str),

        /// Slider range for numeric fields. Values are parsed as f64 at runtime.
        ///
        /// Usage: `#[facet(inspect::range(min = "0.0", max = "10.0"))]`
        #[target(field)]
        Range(Range),
    }

    /// Slider bounds for numeric inspector fields.
    pub struct Range {
        /// Minimum value (parsed as f64 at runtime).
        pub min: &'static str,
        /// Maximum value (parsed as f64 at runtime).
        pub max: &'static str,
    }
}

#[doc(hidden)]
pub use __attr;

pub(crate) fn field_label(field: &facet::Field) -> Option<&'static str> {
    field
        .get_attr(Some("inspect"), "label")
        .and_then(|attr| attr.get_as::<&'static str>())
        .copied()
}

pub(crate) fn field_range(field: &facet::Field) -> Option<(f64, f64)> {
    let attr = field.get_attr(Some("inspect"), "range")?;
    let Attr::Range(range) = attr.get_as::<Attr>()? else {
        return None;
    };
    Some((range.min.parse().ok()?, range.max.parse().ok()?))
}

pub(crate) fn field_variants(field: &facet::Field) -> Option<&'static str> {
    field
        .get_attr(Some("inspect"), "variants")
        .and_then(|attr| attr.get_as::<&'static str>())
        .copied()
}

#[cfg(test)]
mod tests {
    use facet::Facet;

    use crate::inspect;

    #[derive(Facet)]
    struct Attributes {
        #[facet(inspect::label = "Gain")]
        #[facet(inspect::range(min = "0.5", max = "10.0"))]
        gain: f32,
        #[facet(inspect::variants = "Line, Scatter")]
        style: Style,
    }

    #[derive(Facet)]
    #[repr(u8)]
    enum Style {
        Line,
        Scatter,
        Bar,
    }

    #[test]
    fn decodes_supported_field_attributes() {
        let facet::Type::User(facet::UserType::Struct(struct_type)) = Attributes::SHAPE.ty else {
            panic!("Attributes must reflect as a struct")
        };
        let fields = struct_type.fields;

        assert_eq!(super::field_label(&fields[0]), Some("Gain"));
        assert_eq!(super::field_range(&fields[0]), Some((0.5, 10.0)));
        assert_eq!(super::field_variants(&fields[1]), Some("Line, Scatter"));
    }
}
