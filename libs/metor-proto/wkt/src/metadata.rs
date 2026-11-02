use metor_proto::types::{ComponentId, EntityId};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `"true"` or `"false"`: whether the db builds LoD levels for a component.
pub const LOD_KEY: &str = "lod";
/// The frame type a component is a field of.
pub const FRAME_KEY: &str = "frame";
/// The path a frame's fields sit under; one frame instance per root.
pub const FRAME_ROOT_KEY: &str = "frame.root";
/// The member templates of the frame maps a field sits in, comma
/// separated: each map's path with no keys, as `{root}.ifaces`. A keyed
/// member inherits it with the rest of its template's metadata, which is how
/// the ground finds the key segments in the member's own path.
pub const FRAME_MAPS_KEY: &str = "frame.maps";

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Schema)]
#[cfg_attr(feature = "bevy", derive(bevy::prelude::Component))]
pub struct ComponentMetadata {
    pub component_id: ComponentId,
    pub name: String,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl ComponentMetadata {
    pub fn element_names(&self) -> &str {
        self.metadata
            .get("element_names")
            .map(|v| v.as_str())
            .unwrap_or_default()
    }

    pub fn enum_variants(&self) -> Option<impl Iterator<Item = &str>> {
        self.metadata.get("enum_variants").map(|s| s.split(","))
    }

    pub fn with_prefix(self, prefix: &str) -> Self {
        let name = format!("{}.{}", prefix, self.name);
        Self {
            component_id: ComponentId::new(&name),
            name,
            metadata: HashMap::new(),
        }
    }

    pub fn with_element_names<'a>(
        mut self,
        element_names: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let element_names = element_names.into_iter();
        self.metadata.insert(
            "element_names".to_string(),
            element_names.collect::<Vec<_>>().join(","),
        );
        self
    }

    pub fn with_enum<'s>(mut self, variants: impl IntoIterator<Item = &'s str>) -> Self {
        self.metadata.insert(
            "enum_variants".to_string(),
            variants.into_iter().collect::<Vec<_>>().join(","),
        );
        self
    }

    pub fn is_string(&self) -> bool {
        self.metadata
            .get("is_string")
            .map(|v| v == "true")
            .unwrap_or_default()
    }

    /// Marks a shaped `U8` component as UTF-8 text, so the panel renders the
    /// buffer up to its first NUL as a string instead of a numeric array.
    pub fn with_string(mut self) -> Self {
        self.metadata
            .insert("is_string".to_string(), "true".to_string());
        self
    }

    /// Marks this component as a field of a frame: `name` is the frame's
    /// type and `root` the path its fields sit under, so a ground tool can
    /// gather every instance of one frame type without guessing from names.
    pub fn with_frame(mut self, name: &str, root: &str) -> Self {
        self.metadata
            .insert(FRAME_KEY.to_string(), name.to_string());
        self.metadata
            .insert(FRAME_ROOT_KEY.to_string(), root.to_string());
        self
    }

    /// The frame type and root this component is a field of, when its
    /// producer said.
    pub fn frame(&self) -> Option<(&str, &str)> {
        Some((
            self.metadata.get(FRAME_KEY)?.as_str(),
            self.metadata.get(FRAME_ROOT_KEY)?.as_str(),
        ))
    }

    /// Marks this component as a member of the frame map at `path`, the
    /// map's template path. Called innermost map first.
    pub fn with_frame_map(mut self, path: &str) -> Self {
        let maps = self.metadata.entry(FRAME_MAPS_KEY.to_string()).or_default();
        *maps = if maps.is_empty() {
            path.to_string()
        } else {
            format!("{path},{maps}")
        };
        self
    }

    /// Every frame instance this component is a field of: the frame's own
    /// root, then one per enclosing frame map, whose type is the frame's name
    /// and the map's path within it (`nics.ifaces`) and whose root is the
    /// keyed entry (`{root}.ifaces.eth0`). A map of plain values has no
    /// fields under its entries, so it names no instances.
    pub fn frame_instances(&self) -> Vec<(String, String)> {
        let Some((frame, root)) = self.frame() else {
            return Vec::new();
        };
        let mut out = vec![(frame.to_string(), root.to_string())];
        let Some(maps) = self.metadata.get(FRAME_MAPS_KEY) else {
            return out;
        };
        let segments: Vec<&str> = self.name.split('.').collect();
        let mut maps: Vec<&str> = maps.split(',').filter(|m| !m.is_empty()).collect();
        maps.sort_by_key(|m| m.split('.').count());
        let mut keys = Vec::new();
        for map in maps {
            let template: Vec<&str> = map.split('.').collect();
            let key = template.len() + keys.len();
            if segments.len() <= key + 1 {
                break;
            }
            // The member's path with the outer keys taken out must lead
            // with the map's template, or the tag doesn't describe it.
            let unkeyed = segments[..key]
                .iter()
                .enumerate()
                .filter(|(ix, _)| !keys.contains(ix))
                .map(|(_, s)| *s);
            if !unkeyed.eq(template.iter().copied()) {
                break;
            }
            keys.push(key);
            let Some(within) = map.strip_prefix(root).and_then(|m| m.strip_prefix('.')) else {
                break;
            };
            out.push((format!("{frame}.{within}"), segments[..=key].join(".")));
        }
        out
    }

    pub fn group_name(&self) -> Option<&str> {
        self.metadata.get("group_name").map(|v| v.as_str())
    }

    /// Hidden components are DB-internal (e.g. derived LoD series): excluded
    /// from UI listings and live streams, but still queryable by id.
    pub fn is_hidden(&self) -> bool {
        self.metadata
            .get("hidden")
            .map(|v| v == "true")
            .unwrap_or_default()
    }

    pub fn with_hidden(mut self) -> Self {
        self.metadata
            .insert("hidden".to_string(), "true".to_string());
        self
    }

    pub fn with_lod(mut self, lod: bool) -> Self {
        self.metadata.insert(LOD_KEY.to_string(), lod.to_string());
        self
    }

    /// `None` unless `lod` is `"true"` or `"false"`.
    pub fn lod(&self) -> Option<bool> {
        match self.metadata.get(LOD_KEY)?.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }
}

impl From<&str> for ComponentMetadata {
    fn from(id: &str) -> Self {
        Self {
            component_id: ComponentId::new(id),
            name: id.to_string(),
            metadata: HashMap::new(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Schema)]
#[cfg_attr(feature = "bevy", derive(bevy::prelude::Component))]
pub struct EntityMetadata {
    pub entity_id: EntityId,
    pub name: String,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

pub trait MetadataExt {
    fn metadata_mut(&mut self) -> &mut HashMap<String, String>;
    fn metadata(&self) -> &HashMap<String, String>;
    fn priority(&self) -> i64 {
        self.metadata()
            .get("priority")
            .and_then(|v| v.parse().ok())
            .unwrap_or(10)
    }
    fn set_priority(&mut self, priority: i64) {
        self.set("priority", &priority.to_string());
    }
    fn set(&mut self, key: &str, value: &str) {
        self.metadata_mut()
            .insert(key.to_string(), value.to_string());
    }
    fn get(&self, key: &str) -> Option<&str> {
        self.metadata().get(key).map(|v| v.as_str())
    }
}

impl MetadataExt for ComponentMetadata {
    fn metadata_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.metadata
    }
    fn metadata(&self) -> &HashMap<String, String> {
        &self.metadata
    }
}

impl MetadataExt for EntityMetadata {
    fn metadata_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.metadata
    }
    fn metadata(&self) -> &HashMap<String, String> {
        &self.metadata
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A field of an `ifaces` entry, inheriting the template's tags as the
    /// db hands them down.
    fn member(name: &str, maps: &[&str]) -> ComponentMetadata {
        let mut meta = ComponentMetadata {
            component_id: ComponentId::new(name),
            name: name.to_string(),
            metadata: Default::default(),
        }
        .with_frame("nics", "sys.nics");
        // Innermost first, as the producer's metadata nests.
        for map in maps.iter().rev() {
            meta = meta.with_frame_map(map);
        }
        meta
    }

    #[test]
    fn a_map_entry_is_an_instance_of_its_own_type() {
        let meta = member("sys.nics.ifaces.eth0.rx_bytes", &["sys.nics.ifaces"]);
        assert_eq!(
            meta.frame_instances(),
            [
                ("nics".into(), "sys.nics".into()),
                ("nics.ifaces".into(), "sys.nics.ifaces.eth0".into()),
            ]
        );
    }

    #[test]
    fn nested_maps_find_each_key_and_plain_values_name_nothing() {
        let maps = ["sys.nics.ifaces", "sys.nics.ifaces.ethtool"];
        let meta = member("sys.nics.ifaces.eth0.ethtool.rx_crc", &maps);
        assert_eq!(
            meta.frame_instances(),
            [
                ("nics".into(), "sys.nics".into()),
                ("nics.ifaces".into(), "sys.nics.ifaces.eth0".into()),
            ]
        );
        assert_eq!(
            meta.metadata[FRAME_MAPS_KEY],
            "sys.nics.ifaces,sys.nics.ifaces.ethtool"
        );
    }

    #[test]
    fn a_tag_that_does_not_fit_the_path_names_no_entry() {
        let meta = member("sys.nics.other.eth0.rx_bytes", &["sys.nics.ifaces"]);
        assert_eq!(meta.frame_instances().len(), 1);
    }
}
