use gpui::App;
use metor_db::DB;

use super::binding::{ElementRef, component_meta};
use super::time_series::Trace;
use super::{GaugeConfig, MeterConfig};

/// Component identity, display label, and an alarm-informed initial scale for
/// one scalar instrument selected through the trace picker.
pub(crate) struct ScaleSeed {
    pub component: String,
    pub element: usize,
    pub label: String,
    pub min: f64,
    pub max: f64,
}

pub(crate) fn scale_seeds_for_traces(db: &DB, traces: &[Trace], cx: &App) -> Vec<ScaleSeed> {
    traces
        .iter()
        .map(|trace| {
            let at = ElementRef::new(trace.component_id, trace.element_index);
            let (min, max) = super::meter::suggested_scale(at, cx);
            ScaleSeed {
                component: crate::dynamic::expressions::binding_text(db, trace.component_id)
                    .unwrap_or_else(|| component_meta(db, trace.component_id).name.to_string()),
                element: trace.element_index,
                label: trace.label.to_string(),
                min,
                max,
            }
        })
        .collect()
}

impl From<ScaleSeed> for MeterConfig {
    fn from(seed: ScaleSeed) -> Self {
        Self {
            component: seed.component,
            element: seed.element,
            label: Some(seed.label),
            min: seed.min,
            max: seed.max,
            ..Default::default()
        }
    }
}

impl From<ScaleSeed> for GaugeConfig {
    fn from(seed: ScaleSeed) -> Self {
        Self {
            component: seed.component,
            element: seed.element,
            label: Some(seed.label),
            min: seed.min,
            max: seed.max,
            ..Default::default()
        }
    }
}
