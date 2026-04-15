use metor_db::DB;
use metor_proto::types::ComponentId;

/// List all components from the DB, sorted by name.
pub(crate) fn list_components(db: &DB) -> Vec<(ComponentId, String)> {
    let mut components: Vec<_> = db.with_state(|state| {
        state
            .component_metadata_iter()
            .map(|(id, meta)| (*id, meta.name.clone()))
            .collect()
    });
    components.sort_by(|a, b| a.1.cmp(&b.1));
    components
}

/// Return element names for a component from the DB, or an empty vec if not found.
pub fn element_names_for_component(db: &DB, component_id: ComponentId) -> Vec<String> {
    db.with_state(|state| {
        state
            .get_component(component_id)
            .map(|c| element_names(c.schema.dim.as_slice()))
            .unwrap_or_default()
    })
}

/// Generate default element names from a shape (e.g. [3] -> ["x", "y", "z"]).
pub(crate) fn element_names(shape: &[usize]) -> Vec<String> {
    fn walk(shape: &[usize], prefix: &str, out: &mut Vec<String>) {
        if shape.is_empty() {
            out.push(prefix.to_string());
            return;
        }
        const NAMES: [char; 8] = ['x', 'y', 'z', 'w', 'u', 'v', 's', 't'];
        for x in 0..shape[0] {
            let mut elem = prefix.to_string();
            if let Some(c) = NAMES.get(x) {
                elem.push(*c);
            } else {
                elem.push_str(&x.to_string());
            }
            walk(&shape[1..], &elem, out);
        }
    }
    let mut out = Vec::new();
    walk(shape, "", &mut out);
    out
}
