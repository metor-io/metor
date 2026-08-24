use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Context, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render, SharedString, Task,
    Window, div, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::ComponentId;
use metor_expr::state::Snapshot;

use crate::dynamic::{BuildError, DynamicNode, NodeId};
use crate::dynamic::ops::program::{self, Compiled, DEFAULT_FUEL, Health};
use crate::dynamic::ops::{self, db_source, persist};
use crate::dynamic::resolver::DbResolver;
use crate::inspector::rows::TextField;
use crate::node_editor::projected_view::{self, Placements};
use crate::node_editor::projection::{self, Position, Projection};
use crate::node_editor::worker::DynamicWorker;
use crate::theme::theme;
use crate::tiles::PaneItem;

/// The pane's whole persisted state.
///
/// The source is the artifact — everything the canvas shows is derived from it
/// on load. Layout is the one thing that is not, because the source has no
/// business knowing where a card sits, so positions ride alongside as a
/// sidecar keyed by system name.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ProgramPaneConfig {
    pub source: String,
    pub graph: bool,
    pub placements: Vec<Placement>,
}

/// Where one system's card sits, remembered across a reload.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Placement {
    pub system: String,
    pub x: f32,
    pub y: f32,
}

/// One system as the pane knows it: what runs it, what it publishes, and
/// whether it is still running.
struct Running {
    name: String,
    /// The system node plus one node per output field. Held for their drop:
    /// the last `Arc` going away is what cancels a node's task, so this is
    /// the whole of a removed system's teardown.
    _nodes: Vec<Arc<dyn DynamicNode>>,
    publishes: Vec<String>,
    health: Health,
    state: program::StateCell,
    /// What identified this system when it was built, so the next compile can
    /// tell whether the edit touched it.
    hash: NodeId,
}

pub struct ProgramPane {
    db: Arc<DB>,
    focus_handle: FocusHandle,
    editor: TextField,
    running: Vec<Running>,
    /// Spans the last compile complained about, paired with their messages.
    diagnostics: Vec<(std::ops::Range<usize>, String)>,
    rebuild: Option<Task<()>>,
    /// The program as a graph, re-derived on every successful compile. Read
    /// only: proving the projection is this phase's job, editing through it is
    /// the next one's.
    projection: Projection,
    placements: Placements,
    /// Whether the pane is showing the canvas or the text it comes from.
    graph: bool,
}

impl ProgramPane {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::from_config(ProgramPaneConfig::default(), db, cx)
    }

    pub fn from_config(cfg: ProgramPaneConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let mut editor = TextField::new("adcs.omega_b * 100.0", cx).multiline();
        editor.set_text(cfg.source);
        let mut this = Self {
            db,
            focus_handle: cx.focus_handle(),
            editor,
            running: Vec::new(),
            diagnostics: Vec::new(),
            rebuild: None,
            projection: Projection::default(),
            placements: cfg
                .placements
                .into_iter()
                .map(|p| (p.system, Position { x: p.x, y: p.y }))
                .collect(),
            graph: cfg.graph,
        };
        this.schedule_rebuild(cx);
        this
    }

    /// Cancel any pending compile and queue a fresh one after the debounce
    /// window — the same 200 ms the node editor rebuilds on, and far more
    /// than a compile takes.
    fn schedule_rebuild(&mut self, cx: &mut Context<Self>) {
        self.rebuild.take();
        self.rebuild = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let _ = this.update(cx, |this, cx| this.rebuild(cx));
        }));
    }

    /// Compile the source and reconcile what is running against it.
    ///
    /// A compile that fails leaves the previous systems alone. Half-typed
    /// source is the normal case in an editor, and tearing live systems down
    /// on every keystroke would make the pane unusable.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let resolver = DbResolver::snapshot(&self.db);
        let compiled = match Compiled::module(&self.editor.text, &resolver) {
            Ok(compiled) => Arc::new(compiled),
            Err(diags) => {
                self.diagnostics = diags
                    .iter()
                    .map(|d| {
                        (
                            d.span.start as usize..d.span.end as usize,
                            d.message.clone(),
                        )
                    })
                    .collect();
                self.paint_diagnostics(cx);
                cx.notify();
                return;
            }
        };
        self.diagnostics.clear();

        let worker = cx.global::<DynamicWorker>().handle().clone();
        let mut kept: Vec<Running> = Vec::new();
        // Declaration order, because a declaration may read an earlier one and
        // what it reads has to exist by then.
        for decl in compiled.manifest.declarations() {
            let (span, built) = match decl {
                metor_expr::Decl::System(index) => (
                    compiled.manifest.systems[index].source,
                    self.reconcile(&compiled, index, &resolver, &worker),
                ),
                metor_expr::Decl::Stage(index) => (
                    compiled.manifest.stages[index].source_span,
                    self.reconcile_stage(&compiled, index, &resolver, &worker),
                ),
            };
            match built {
                Ok(running) => kept.push(running),
                Err(why) => self
                    .diagnostics
                    .push((span.start as usize..span.end as usize, why)),
            }
        }
        // Dropping what is left cancels the tasks of every system the edit
        // removed, and only those.
        self.running = kept;
        self.projection = projection::project(&compiled.manifest, &self.placements);
        self.paint_diagnostics(cx);
        cx.notify();
    }

    /// Keep a system whose identity survived the edit; build the rest.
    fn reconcile(
        &mut self,
        compiled: &Arc<Compiled>,
        index: usize,
        resolver: &DbResolver,
        worker: &crate::node_editor::worker::WorkerHandle,
    ) -> Result<Running, String> {
        let desc = &compiled.manifest.systems[index];

        // Which component each port reads is decided here; the node that
        // reads it is built on the worker thread, because building one spawns
        // a task and tasks belong to that thread. The two can be separated
        // because a source node's identity follows from its component id
        // alone, so the hash below needs no node to exist yet.
        let mut sources = Vec::with_capacity(desc.inputs.len());
        for port in &desc.inputs {
            sources.push(self.source_of(port, compiled, resolver)?);
        }
        let port_ids: Vec<NodeId> = sources
            .iter()
            .map(|id| db_source::from_db_id(*id))
            .collect();
        let hash = compiled.system_hash(index, &port_ids);

        if let Some(at) = self.running.iter().position(|r| r.hash == hash) {
            return Ok(self.running.remove(at));
        }

        // Same name, different hash: this system was edited, so its state
        // comes across. `metor_expr::state` decides field by field what of it
        // still means the same thing.
        let seed: Option<Snapshot> = self
            .running
            .iter()
            .find(|r| r.name == desc.name)
            .map(|r| r.state.snapshot());

        // A system is several nodes plus the cells that watch it, which is
        // why the handle returns what the closure returns rather than a node.
        let built = {
            let compiled = compiled.clone();
            let db = self.db.clone();
            let names: Vec<String> = desc.publishes.clone();
            worker.call(move || -> Result<Built, BuildError> {
                let mut ports = Vec::with_capacity(sources.len());
                for id in &sources {
                    ports.push(program::PortSource {
                        node: db_source::from_db(&db, *id)?,
                        seed: program::latest_sample(&db, *id),
                    });
                }
                let system = program::system(&compiled, index, ports, DEFAULT_FUEL, seed.as_ref())?;
                let mut nodes = vec![system.node.clone()];
                for (field, name) in names.iter().enumerate() {
                    let node = program::field(&compiled, index, field, system.node.clone())?;
                    nodes.push(persist::persist(&db, name.clone(), node)?);
                }
                Ok(Built {
                    nodes,
                    health: system.health,
                    state: system.state,
                })
            })
        };
        let built = built
            .ok_or_else(|| "the node worker is gone".to_string())?
            .map_err(|e| e.to_string())?;

        Ok(Running {
            name: desc.name.clone(),
            _nodes: built.nodes,
            publishes: desc.publishes.clone(),
            health: built.health,
            state: built.state,
            hash,
        })
    }

    /// Keep or build one resample stage.
    ///
    /// A stage is not compiled and has no state to carry, so reconciling one
    /// is only the question of whether anything about it changed: what it
    /// reads, how it fills a tick, and how fast it ticks.
    fn reconcile_stage(
        &mut self,
        compiled: &Arc<Compiled>,
        index: usize,
        resolver: &DbResolver,
        worker: &crate::node_editor::worker::WorkerHandle,
    ) -> Result<Running, String> {
        let stage = &compiled.manifest.stages[index];
        let source = self.component_of(&stage.source, compiled, resolver)?;
        let hash = crate::dynamic::hash_id(
            crate::dynamic::op_tag::EXPR_STAGE,
            &[db_source::from_db_id(source)],
            |h| {
                use std::hash::Hash;
                stage.name.hash(h);
                (stage.kind == metor_expr::Resample::Linear).hash(h);
                stage.rate.to_bits().hash(h);
            },
        );
        if let Some(at) = self.running.iter().position(|r| r.hash == hash) {
            return Ok(self.running.remove(at));
        }

        let mode = match stage.kind {
            metor_expr::Resample::Zoh => ops::resample::ResampleMode::Zoh,
            metor_expr::Resample::Linear => ops::resample::ResampleMode::Linear,
        };
        let (name, rate) = (stage.name.clone(), stage.rate);
        let nodes = {
            let db = self.db.clone();
            worker.call(move || -> Result<Vec<Arc<dyn DynamicNode>>, BuildError> {
                let input = db_source::from_db(&db, source)?;
                let clock = ops::clock::fixed_rate(rate)?;
                let resampled = ops::resample::resample(input, clock.clone(), mode)?;
                Ok(vec![clock, persist::persist(&db, name, resampled)?])
            })
        };
        let nodes = nodes
            .ok_or_else(|| "the node worker is gone".to_string())?
            .map_err(|e| e.to_string())?;

        Ok(Running {
            name: stage.name.clone(),
            _nodes: nodes,
            publishes: vec![stage.name.clone()],
            health: Health::default(),
            state: program::StateCell::default(),
            hash,
        })
    }

    /// The component one input port reads.
    ///
    /// An external component's id is *carried* from the resolver that found
    /// it, never re-derived: a producer names its own channels, so hashing
    /// the name again agrees with the real id for only about half of all
    /// names — and when it disagrees the component looks absent rather than
    /// misaddressed.
    ///
    /// A `Produced` binding is the one case where deriving is right, because
    /// `persist` created that component from the same name moments earlier.
    /// Systems are built in declaration order and a binding may only name an
    /// earlier one, so it exists by the time this runs.
    fn source_of(
        &self,
        port: &metor_expr::Port,
        compiled: &Arc<Compiled>,
        resolver: &DbResolver,
    ) -> Result<ComponentId, String> {
        self.component_of(&port.bindings[0], compiled, resolver)
    }

    /// The component behind one binding, whatever produced it.
    fn component_of(
        &self,
        binding: &metor_expr::Binding,
        compiled: &Arc<Compiled>,
        resolver: &DbResolver,
    ) -> Result<ComponentId, String> {
        let produced = match binding {
            metor_expr::Binding::Component(path) => {
                return resolver
                    .id_of(path)
                    .ok_or_else(|| format!("`{path}` is not a known component"));
            }
            metor_expr::Binding::Produced { system, field } => {
                &compiled.manifest.systems[*system].publishes[*field]
            }
            metor_expr::Binding::Resampled { stage } => &compiled.manifest.stages[*stage].name,
        };
        let id = ComponentId::new(produced);
        match self.db.with_state(|s| s.get_component(id).is_some()) {
            true => Ok(id),
            false => Err(format!("`{produced}` has not published yet")),
        }
    }

    /// Underline every span the last compile complained about.
    fn paint_diagnostics(&mut self, cx: &App) {
        let color = theme(cx).error_accent;
        self.editor.marks = self
            .diagnostics
            .iter()
            .map(|(span, _)| (span.clone(), color))
            .collect();
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        // The canvas is a view of the text, so one key turns it over rather
        // than the two being separate places to be.
        let mods = &event.keystroke.modifiers;
        let primary = if cfg!(target_os = "macos") {
            mods.platform
        } else {
            mods.control
        };
        if primary && event.keystroke.key.as_str() == "g" {
            self.graph = !self.graph;
            cx.notify();
            return;
        }
        if self.graph {
            return;
        }
        if self.editor.handle_key_down(event, cx) {
            self.editor.follow_cursor();
            self.schedule_rebuild(cx);
            cx.notify();
        }
    }
}

/// One system as it comes back from the worker thread: its nodes, and the two
/// cells that report on it.
struct Built {
    nodes: Vec<Arc<dyn DynamicNode>>,
    health: Health,
    state: program::StateCell,
}

impl Focusable for ProgramPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ProgramPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            // `TextInput` is what keeps single-key shortcuts — the leader
            // above all — from being stolen out of the editor. Declared only
            // while the editor is showing: on the canvas there is nothing
            // typing into, so the shortcuts should work as they do anywhere.
            .key_context(match self.graph {
                true => "ProgramPane",
                false => "ProgramPane TextInput",
            })
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _w, cx| this.on_key(event, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_primary)
            .child(match self.graph {
                true => div()
                    .flex_1()
                    .overflow_hidden()
                    .child(projected_view::render(&self.projection, (0.0, 0.0), cx)),
                false => div()
                    .flex_1()
                    .p_2()
                    .overflow_hidden()
                    .text_size(px(12.0))
                    .child(self.editor.lines_element()),
            })
            .child(status(self, cx))
    }
}

/// The strip under the editor: what each system is doing, or why the module
/// did not compile.
fn status(pane: &ProgramPane, cx: &App) -> impl IntoElement {
    let theme = theme(cx);
    let mut strip = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px_2()
        .py_1()
        .border_t_1()
        .border_color(theme.border_primary)
        .bg(theme.bg_secondary)
        .text_size(px(11.0));

    for (span, message) in &pane.diagnostics {
        let line = pane.editor.text[..span.start.min(pane.editor.text.len())]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
            + 1;
        strip = strip.child(
            div()
                .text_color(theme.error_accent)
                .child(SharedString::from(format!("{line}: {message}"))),
        );
    }

    for system in &pane.running {
        let (color, detail) = match system.health.fault() {
            Some(why) => (theme.error_accent, why),
            None => (theme.text_secondary, system.publishes.join(", ")),
        };
        strip = strip.child(
            div()
                .flex()
                .gap_2()
                .child(
                    div()
                        .text_color(theme.text_primary)
                        .child(SharedString::from(system.name.clone())),
                )
                .child(div().text_color(color).child(SharedString::from(detail))),
        );
    }

    strip
}

impl PaneItem for ProgramPane {
    type Config = ProgramPaneConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        "Program".into()
    }

    fn serialization_key() -> &'static str {
        "program"
    }

    fn to_config(&self, _cx: &App) -> ProgramPaneConfig {
        ProgramPaneConfig {
            source: self.editor.text.clone(),
            graph: self.graph,
            placements: self
                .placements
                .iter()
                .map(|(system, at)| Placement {
                    system: system.clone(),
                    x: at.x,
                    y: at.y,
                })
                .collect(),
        }
    }
}
