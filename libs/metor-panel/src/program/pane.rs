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
use crate::dynamic::ops::{db_source, persist};
use crate::dynamic::resolver::DbResolver;
use crate::inspector::rows::TextField;
use crate::node_editor::worker::DynamicWorker;
use crate::theme::theme;
use crate::tiles::PaneItem;

/// The pane's whole persisted state: the source is the artifact, and
/// everything else is derived from it on load.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProgramPaneConfig {
    #[serde(default)]
    pub source: String,
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
        for index in 0..compiled.manifest.systems.len() {
            match self.reconcile(&compiled, index, &worker) {
                Ok(system) => kept.push(system),
                Err(why) => {
                    let span = compiled.manifest.systems[index].source;
                    self.diagnostics
                        .push((span.start as usize..span.end as usize, why));
                }
            }
        }
        // Dropping what is left cancels the tasks of every system the edit
        // removed, and only those.
        self.running = kept;
        self.paint_diagnostics(cx);
        cx.notify();
    }

    /// Keep a system whose identity survived the edit; build the rest.
    fn reconcile(
        &mut self,
        compiled: &Arc<Compiled>,
        index: usize,
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
            sources.push(self.source_of(port, compiled)?);
        }
        let port_ids: Vec<NodeId> = sources
            .iter()
            .map(|(id, _)| db_source::from_db_id(*id))
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
                for (id, _) in &sources {
                    ports.push(db_source::from_db(&db, *id)?);
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

    /// The component one input port reads, and what to call it if it is
    /// missing.
    ///
    /// A `Produced` binding names another system in this module; it resolves
    /// to the component that system publishes, which exists because systems
    /// are built in declaration order and a binding may only name an earlier
    /// one.
    fn source_of(
        &self,
        port: &metor_expr::Port,
        compiled: &Arc<Compiled>,
    ) -> Result<(ComponentId, String), String> {
        let name = match &port.bindings[0] {
            metor_expr::Binding::Component(path) => path.clone(),
            metor_expr::Binding::Produced { system, field } => {
                compiled.manifest.systems[*system].publishes[*field].clone()
            }
        };
        let id = ComponentId(persist::component_id_for_name(&name));
        if self.db.with_state(|s| s.get_component(id).is_none()) {
            return Err(format!("`{name}` has not published yet"));
        }
        Ok((id, name))
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
            .key_context("ProgramPane")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _w, cx| this.on_key(event, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_primary)
            .child(
                div()
                    .flex_1()
                    .p_2()
                    .overflow_hidden()
                    .text_size(px(12.0))
                    .child(self.editor.lines_element()),
            )
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
        }
    }
}
