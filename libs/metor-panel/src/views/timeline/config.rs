use super::model::Navigation;
use serde::{Deserialize, Serialize};

/// Persisted local presentation; global time remains in the temporal controller.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TimelineConfig {
    pub label: String,
    pub navigation: Navigation,
    pub sources: Vec<String>,
    pub snap: bool,
    pub collapsed: Vec<String>,
}
impl Default for TimelineConfig {
    fn default() -> Self {
        Self {
            label: "Timeline".into(),
            navigation: Navigation::Fit,
            sources: vec!["logs".into(), "alarms".into(), "sequences".into()],
            snap: true,
            collapsed: Vec::new(),
        }
    }
}
