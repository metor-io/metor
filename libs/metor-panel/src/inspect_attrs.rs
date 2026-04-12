/// Custom Facet attributes for the property inspector.
///
/// These attributes control how fields appear in the UI inspector and
/// command palette. Applied via `#[facet(inspect::...)]` syntax.
facet::define_attr_grammar! {
    ns "inspect";
    crate_path ::metor_panel::inspect_attrs;

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
