//! The "add view" menu, shared by panes and dashboards.
//!
//! Every entry ends the same way: a [`WidgetKind`] and the JSON config its
//! wizard produced. The host decides where that lands through a
//! [`ViewSink`], so a pane's "New Tab" and a dashboard's "Add Widget" are one
//! list of wizards rather than two copies that drift apart.
use std::sync::Arc;

use gpui::{AnyEntity, App, Window};
use metor_db::DB;
use serde::Serialize;

use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow, NavRow, RowAction};
use crate::inspector::{InspectorMode, InspectorRequest, OpenInspectorCallback};
use crate::views::dashboard::{ImageWidgetConfig, WidgetKind};
use crate::views::instrument::{ScaleSeed, scale_seeds_for_traces};
use crate::views::{
    AttitudeConfig, GaugeConfig, ListPlotPanelConfig, ListTraceConfig, MapConfig, MeterConfig,
    MonitorConfig, PlotPanelConfig, SamplesTableConfig, SequenceControlConfig,
    SpectrogramPanelConfig, SpectrogramTraceConfig, StateChipConfig, TraceConfig,
    TrafficLightConfig, XyPlotPanelConfig, XyTraceConfig,
};

/// Where a configured view lands. Returns the entity the inspector should
/// open on, when the host made one.
pub(crate) type ViewSink = Arc<dyn Fn(&WidgetKind, String, &mut App) -> Option<AnyEntity>>;

/// The surface a menu is built for. A few entries belong to only one: a
/// dashboard can't nest a dashboard, and images have no pane form.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Host {
    Pane,
    Dashboard,
}

/// One host's "add view" menu.
#[derive(Clone)]
pub(crate) struct AddMenu {
    pub db: Arc<DB>,
    pub host: Host,
    pub sink: ViewSink,
    /// Opened on what a trace wizard creates, so the new plot's traces can
    /// be tuned straight away.
    pub on_open_inspector: Option<OpenInspectorCallback>,
}

impl AddMenu {
    /// The menu in display order, without the entries `host` can't hold.
    pub(crate) fn rows(self) -> Vec<Box<dyn InspectorRow>> {
        use Host::{Dashboard, Pane};
        let entries: Vec<(Option<Host>, Box<dyn InspectorRow>)> = vec![
            (None, self.time_series()),
            (
                Some(Pane),
                self.wizard(
                    "XY Plot",
                    WidgetKind::new("xy_plot"),
                    |db, commit| {
                        crate::views::xy_plot::trace_picker::select_xy_trace_wizard_rows(
                            db,
                            Arc::new(|_| 0),
                            commit,
                        )
                    },
                    |trace| XyPlotPanelConfig {
                        traces: vec![XyTraceConfig::from(&trace)],
                        ..Default::default()
                    },
                ),
            ),
            (
                Some(Pane),
                self.wizard(
                    "List Plot",
                    WidgetKind::new("list_plot"),
                    |db, commit| {
                        crate::views::list_plot::trace_picker::select_list_trace_wizard_rows(
                            db,
                            Arc::new(|_| 0),
                            commit,
                        )
                    },
                    |trace| ListPlotPanelConfig {
                        traces: vec![ListTraceConfig::from(&trace)],
                        ..Default::default()
                    },
                ),
            ),
            (
                Some(Pane),
                self.wizard(
                    "Spectrogram",
                    WidgetKind::new("spectrogram"),
                    |db, commit| {
                        crate::views::spectrogram::trace_picker::select_spectrogram_trace_wizard_rows(
                            db, commit,
                        )
                    },
                    |trace| SpectrogramPanelConfig {
                        traces: vec![SpectrogramTraceConfig::from(&trace)],
                        ..Default::default()
                    },
                ),
            ),
            (
                None,
                self.component("Monitor", WidgetKind::monitor(), |component| {
                    MonitorConfig {
                        component,
                        ..Default::default()
                    }
                }),
            ),
            (
                None,
                self.component("Traffic Light", WidgetKind::traffic_light(), |component| {
                    TrafficLightConfig {
                        component,
                        color: None,
                    }
                }),
            ),
            (None, self.annunciator()),
            (
                None,
                self.instrument("Meter", WidgetKind::meter(), MeterConfig::from),
            ),
            (
                None,
                self.instrument("Gauge", WidgetKind::gauge(), GaugeConfig::from),
            ),
            (
                None,
                self.instrument("State Chip", WidgetKind::state_chip(), |seed| {
                    StateChipConfig {
                        component: seed.component,
                        element: seed.element,
                        label: Some(seed.label),
                        ..Default::default()
                    }
                }),
            ),
            (
                None,
                self.instrument("Toggle Switch", WidgetKind::state_chip(), |seed| {
                    StateChipConfig {
                        component: seed.component,
                        element: seed.element,
                        label: Some(seed.label),
                        states: crate::views::state_chip::default_toggle_states(),
                        style: crate::views::StateChipStyle::Toggle,
                        ..Default::default()
                    }
                }),
            ),
            (
                None,
                self.instrument("State Image", WidgetKind::state_image(), |seed| {
                    crate::views::StateImageConfig {
                        component: seed.component,
                        element: seed.element,
                        label: Some(seed.label),
                        ..Default::default()
                    }
                }),
            ),
            (
                None,
                self.component("Attitude", WidgetKind::attitude(), |component| {
                    AttitudeConfig {
                        component,
                        ..Default::default()
                    }
                }),
            ),
            (
                None,
                self.component("Map", WidgetKind::map(), |component| MapConfig {
                    component,
                    ..Default::default()
                }),
            ),
            (
                None,
                self.component("Samples", WidgetKind::samples_table(), |component| {
                    SamplesTableConfig {
                        component,
                        ..Default::default()
                    }
                }),
            ),
            (None, self.sequence_control()),
            (None, self.command("Outline", WidgetKind::table())),
            (
                Some(Pane),
                self.command("Component Browser", WidgetKind::new("component_browser")),
            ),
            (None, self.label()),
            (Some(Dashboard), self.image()),
            (None, self.image_view()),
            (None, self.command("3D Viewer", WidgetKind::viewer3d())),
            (Some(Pane), self.command("Dashboard", WidgetKind::new("dashboard"))),
            (Some(Pane), self.command("Alarms", WidgetKind::new("alarm"))),
            (Some(Pane), self.command("Inspector", WidgetKind::new("inspector"))),
            (Some(Pane), self.command("Logs", WidgetKind::new("logs"))),
            (Some(Pane), self.command("Sequences", WidgetKind::new("sequence"))),
            (
                Some(Pane),
                self.command("Sequence Grid", WidgetKind::new("sequence_grid")),
            ),
            (Some(Pane), self.command("Timeline", WidgetKind::new("timeline"))),
            (
                Some(Pane),
                self.command("Execution Timeline", WidgetKind::new("exec_timeline")),
            ),
        ];
        entries
            .into_iter()
            .filter(|(only, _)| only.is_none_or(|host| host == self.host))
            .map(|(_, row)| row)
            .collect()
    }

    fn add(&self, kind: &WidgetKind, config: &impl Serialize, cx: &mut App) -> Option<AnyEntity> {
        let config = serde_json::to_string(config).expect("view configs serialize");
        (self.sink)(kind, config, cx)
    }

    /// Open the entry labelled `label` in the centred palette, at the page
    /// choosing it from the menu would show: how a starter button begins a
    /// wizard without the menu in front of it.
    pub(crate) fn open_entry(self, label: &str, window: &mut Window, cx: &mut App) {
        let Some(open) = crate::inspector::open_inspector(cx) else {
            return;
        };
        let Some(mut row) = self.rows().into_iter().find(|row| row.label() == label) else {
            return;
        };
        let rows = match row.activate(window, cx) {
            RowAction::Cascade(rows) | RowAction::CascadeWith { rows, .. } => rows,
            // A command row has already done its work.
            _ => return,
        };
        open(
            InspectorRequest {
                rows,
                mode: InspectorMode::Centered,
            },
            window,
            cx,
        );
    }

    fn time_series(&self) -> Box<dyn InspectorRow> {
        let menu = self.clone();
        Box::new(NavRow::new(
            "Time Series Plot",
            "",
            Box::new(move |_| menu.time_series_rows()),
        ))
    }

    /// The time series wizard's first page, for entry points that open
    /// straight into it (an empty pane's Time Series button).
    pub(crate) fn time_series_rows(&self) -> Vec<Box<dyn InspectorRow>> {
        self.wizard_rows(
            WidgetKind::plot(),
            &|db, commit: crate::inspector::trace_picker::OnTracesSelected| {
                crate::inspector::trace_picker::select_traces_wizard_rows(
                    db,
                    Arc::new(|_| 0),
                    commit,
                )
            },
            |traces| PlotPanelConfig {
                traces: traces.iter().map(TraceConfig::from).collect(),
                ..Default::default()
            },
        )
    }

    /// A multi-page picker whose result configures one view, which the
    /// inspector then opens on.
    fn wizard<T: 'static, C: Serialize + 'static>(
        &self,
        label: &'static str,
        kind: WidgetKind,
        pick: impl Fn(Arc<DB>, Arc<dyn Fn(T, &mut Window, &mut App)>) -> Vec<Box<dyn InspectorRow>>
        + 'static,
        configure: fn(T) -> C,
    ) -> Box<dyn InspectorRow> {
        let menu = self.clone();
        let pick = Arc::new(pick);
        Box::new(NavRow::new(
            label,
            "",
            Box::new(move |_| menu.wizard_rows(kind.clone(), &*pick, configure)),
        ))
    }

    fn wizard_rows<T: 'static, C: Serialize + 'static>(
        &self,
        kind: WidgetKind,
        pick: &dyn Fn(Arc<DB>, Arc<dyn Fn(T, &mut Window, &mut App)>) -> Vec<Box<dyn InspectorRow>>,
        configure: fn(T) -> C,
    ) -> Vec<Box<dyn InspectorRow>> {
        let menu = self.clone();
        pick(
            self.db.clone(),
            Arc::new(move |selected, window, cx| {
                if let Some(entity) = menu.add(&kind, &configure(selected), cx) {
                    menu.inspect_created(entity, window, cx);
                }
            }),
        )
    }

    /// The component list, preceded by the expression row so typing `=`
    /// binds the view to a computed channel instead of a named one. Either
    /// way the config stores a string, and a leading `=` is what tells the
    /// builder to compile it rather than look it up.
    fn component<C: Serialize + 'static>(
        &self,
        label: &'static str,
        kind: WidgetKind,
        configure: fn(String) -> C,
    ) -> Box<dyn InspectorRow> {
        let menu = self.clone();
        Box::new(NavRow::new(
            label,
            "",
            Box::new(move |_| {
                let menu = menu.clone();
                let kind = kind.clone();
                crate::inspector::trace_picker::binding_picker_rows(
                    menu.db.clone(),
                    move |binding, cx| {
                        menu.add(&kind, &configure(binding.text(&menu.db)), cx);
                    },
                )
            }),
        ))
    }

    /// The trace wizard for the scalar instruments: every picked element
    /// becomes its own view, seeded with an alarm-informed scale.
    fn instrument<C: Serialize + 'static>(
        &self,
        label: &'static str,
        kind: WidgetKind,
        configure: fn(ScaleSeed) -> C,
    ) -> Box<dyn InspectorRow> {
        let menu = self.clone();
        Box::new(NavRow::new(
            label,
            "",
            Box::new(move |_| {
                let menu = menu.clone();
                let kind = kind.clone();
                crate::inspector::trace_picker::select_traces_wizard_rows(
                    menu.db.clone(),
                    Arc::new(|_| 0),
                    Arc::new(move |traces, _window, cx| {
                        for seed in scale_seeds_for_traces(&menu.db, &traces, cx) {
                            menu.add(&kind, &configure(seed), cx);
                        }
                    }),
                )
            }),
        ))
    }

    /// A view that needs no setup; it opens on its config's defaults.
    fn command(&self, label: &'static str, kind: WidgetKind) -> Box<dyn InspectorRow> {
        let sink = self.sink.clone();
        Box::new(CommandRow::new(
            label,
            Arc::new(move |_, cx| {
                sink(&kind, "{}".to_string(), cx);
            }),
        ))
    }

    fn annunciator(&self) -> Box<dyn InspectorRow> {
        let menu = self.clone();
        Box::new(NavRow::new(
            "Annunciator",
            "",
            Box::new(move |_| {
                let menu = menu.clone();
                vec![crate::views::annunciator::glob_prompt_row(Arc::new(
                    move |pattern, _window, cx| {
                        let cfg = crate::views::annunciator::seeded_config(&pattern);
                        menu.add(&WidgetKind::annunciator(), &cfg, cx);
                    },
                ))]
            }),
        ))
    }

    fn sequence_control(&self) -> Box<dyn InspectorRow> {
        let menu = self.clone();
        Box::new(NavRow::new(
            "Sequence Control",
            "",
            Box::new(move |cx| {
                let menu = menu.clone();
                crate::views::sequence_control::channel_picker_rows(cx, move |channel, cx| {
                    let cfg = SequenceControlConfig {
                        channel,
                        ..Default::default()
                    };
                    menu.add(&WidgetKind::sequence_control(), &cfg, cx);
                })
            }),
        ))
    }

    fn image(&self) -> Box<dyn InspectorRow> {
        let menu = self.clone();
        Box::new(NavRow::new(
            "Image",
            "",
            Box::new(move |_| {
                let menu = menu.clone();
                vec![Box::new(DefaultActionRow::new(
                    "Image file path...",
                    Arc::new(move |input, _window, cx| {
                        if !input.is_empty() {
                            let cfg = ImageWidgetConfig {
                                path: input,
                                data: String::new(),
                            };
                            menu.add(&WidgetKind::image(), &cfg, cx);
                        }
                    }),
                ))]
            }),
        ))
    }

    /// The frame-buffer records to view, as images.
    fn image_view(&self) -> Box<dyn InspectorRow> {
        let menu = self.clone();
        Box::new(NavRow::new(
            "Image View",
            "",
            Box::new(move |_| {
                let menu = menu.clone();
                crate::views::image_view::image_picker_rows(
                    menu.db.clone(),
                    move |component, cx| {
                        let config = crate::views::image_view::image_view_config(component);
                        (menu.sink)(&WidgetKind::image_view(), config, cx);
                    },
                )
            }),
        ))
    }

    /// Asks for the text, then opens the inspector on the new label so its
    /// colour, size and alignment can be set straight away.
    fn label(&self) -> Box<dyn InspectorRow> {
        let menu = self.clone();
        Box::new(NavRow::new(
            "Label",
            "",
            Box::new(move |_| {
                let menu = menu.clone();
                vec![Box::new(DefaultActionRow::new(
                    "Label text...",
                    Arc::new(move |input, window, cx| {
                        let mut cfg = crate::views::LabelConfig::default();
                        if !input.is_empty() {
                            cfg.text = input;
                        }
                        if let Some(entity) = menu.add(&WidgetKind::label(), &cfg, cx) {
                            menu.inspect_created(entity, window, cx);
                        }
                    }),
                ))]
            }),
        ))
    }

    fn inspect_created(&self, entity: AnyEntity, window: &mut Window, cx: &mut App) {
        let Some(on_open) = &self.on_open_inspector else {
            return;
        };
        let Some(rows) = crate::inspector::reflect::rows_for_any_entity(&entity, &self.db, cx)
        else {
            return;
        };
        on_open(
            InspectorRequest {
                rows,
                mode: InspectorMode::Centered,
            },
            window,
            cx,
        );
    }
}
