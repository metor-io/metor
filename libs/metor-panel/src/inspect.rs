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

        /// Select a custom widget for this field.
        ///
        /// Usage: `#[facet(inspect::widget = "color_picker")]`
        #[target(field)]
        Widget(&'static str),

        /// Mark a field as visible but not editable.
        ///
        /// Usage: `#[facet(inspect::read_only)]`
        #[target(field)]
        ReadOnly,

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
