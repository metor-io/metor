use metor_proto::types::{ComponentId, EntityId};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
