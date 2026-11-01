//! Sending commands to the connected system.
//!
//! The connected remote describes the commands it takes, and the panel's db
//! learns them on every connect ([`DB::commands`]). [`CommandCatalog`] turns
//! each description into a [`Command`] the palette can build a form for, and
//! keeps a draft per command so the values an operator typed survive closing
//! the palette. Sending pushes the encoded draft onto the command's log in
//! the local db; the link forwards that log from its live edge, so the
//! catalog refuses to send while no link is up rather than record a command
//! nothing would carry.

mod encode;
mod form;
mod frame;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task};
use metor_db::DB;
use metor_proto::types::{PacketId, Timestamp};
use metor_proto_wkt::{CommandInfo, CommandKind, ComponentMetadata};
use serde_json::Value;

use crate::connections::ConnectionStatus;
use crate::inspector::palette::{Category, InspectionItem, ItemProvider, ItemRegistry};

#[cfg(test)]
use encode::EncodeError;
pub(crate) use encode::Form;

/// One command the connected system takes, as the palette offers it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Command {
    pub id: PacketId,
    /// A frame's label leads with the path of the port it is laid out for,
    /// so two vehicles taking the same frame read apart.
    pub label: SharedString,
    pub summary: SharedString,
    pub form: Form,
}

impl Command {
    fn new(
        info: &CommandInfo,
        metadata: &dyn Fn(metor_proto::types::ComponentId) -> Option<ComponentMetadata>,
    ) -> Self {
        let form = Form::of(info, metadata);
        let (label, summary): (SharedString, SharedString) = match (&info.kind, &form) {
            (_, Form::Unsupported(reason)) => (command_name(info), reason.clone()),
            (CommandKind::Frame { name, .. }, Form::Frame(layout)) => match layout.prefix() {
                Some(prefix) => (format!("{prefix}.{name}").into(), "Frame".into()),
                None => (name.clone().into(), "Frame".into()),
            },
            (_, Form::Json) => (command_name(info), "JSON".into()),
            _ => (command_name(info), SharedString::default()),
        };
        Self {
            id: info.id,
            label,
            summary,
            form,
        }
    }
}

fn command_name(info: &CommandInfo) -> SharedString {
    match &info.kind {
        CommandKind::Msg(meta) => meta.name.clone().into(),
        CommandKind::Frame { name, .. } => name.clone().into(),
    }
}

/// What became of the last attempt to send a command, shown beside its
/// Send row.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Outcome {
    Sent(Timestamp),
    Refused(SharedString),
}

/// Reads where the panel's connections stand. The catalog asks on every
/// send, since a link can drop between opening the palette and sending.
pub(crate) type LinkProbe = Arc<dyn Fn(&App) -> Vec<ConnectionStatus>>;

/// Why nothing would carry a command now, or `None` when some link is up.
pub(crate) fn link_refusal(statuses: &[ConnectionStatus]) -> Option<SharedString> {
    if statuses.contains(&ConnectionStatus::Connected) {
        return None;
    }
    let failed = statuses.iter().find_map(|status| match status {
        ConnectionStatus::Failed(reason) => Some(reason.clone()),
        _ => None,
    });
    Some(match failed {
        Some(reason) => format!("Not sent: link failed ({reason})").into(),
        None if statuses.contains(&ConnectionStatus::Reconnecting) => {
            "Not sent: link down, reconnecting".into()
        }
        None if statuses.contains(&ConnectionStatus::Connecting) => {
            "Not sent: still connecting".into()
        }
        None => "Not sent: not connected".into(),
    })
}

/// The commands the connected system takes, each with the operator's draft.
///
/// Rebuilt whenever the db's metadata moves, since that is when a connect
/// describes new commands and when a frame's field names arrive. A draft
/// survives a rebuild unless its command's form changed.
pub(crate) struct CommandCatalog {
    db: Arc<DB>,
    link: LinkProbe,
    commands: Vec<Command>,
    drafts: HashMap<PacketId, Value>,
    outcomes: HashMap<PacketId, Outcome>,
    _watch: Task<()>,
}

struct GlobalCatalog(Entity<CommandCatalog>);
impl Global for GlobalCatalog {}

impl CommandCatalog {
    /// Install the catalog, gated on the panel's connections, and offer its
    /// commands in the palette.
    pub(crate) fn init(db: Arc<DB>, cx: &mut App) {
        let link: LinkProbe = Arc::new(|cx| {
            crate::connections::try_global(cx)
                .map(|store| store.read(cx).active().iter().map(|c| c.status()).collect())
                .unwrap_or_default()
        });
        Self::install(db, link, cx);
        Self::register_provider(cx);
    }

    fn install(db: Arc<DB>, link: LinkProbe, cx: &mut App) -> Entity<Self> {
        let catalog = cx.new(|cx| {
            let seen = db.metadata_gen.latest();
            let watched = db.clone();
            let mut catalog = Self {
                db,
                link,
                commands: Vec::new(),
                drafts: HashMap::new(),
                outcomes: HashMap::new(),
                _watch: cx.spawn(async move |this, cx| {
                    let mut seen = seen;
                    loop {
                        watched.metadata_gen.wait_for(|g| g != seen).await;
                        seen = watched.metadata_gen.latest();
                        if this
                            .update(cx, |catalog: &mut Self, cx| catalog.rebuild(cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                }),
            };
            catalog.rebuild(cx);
            catalog
        });
        cx.set_global(GlobalCatalog(catalog.clone()));
        catalog
    }

    pub(crate) fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalCatalog>()
            .map(|global| global.0.clone())
    }

    fn register_provider(cx: &mut App) {
        let provider: ItemProvider = Arc::new(|cx: &App| {
            let Some(catalog) = Self::global(cx) else {
                return Vec::new();
            };
            catalog
                .read(cx)
                .commands
                .iter()
                .map(|command| {
                    let catalog = catalog.clone();
                    let id = command.id;
                    InspectionItem::SubMenu {
                        label: command.label.clone(),
                        summary: command.summary.clone(),
                        build: Arc::new(move |cx| form::command_rows(&catalog, id, cx)),
                    }
                })
                .collect()
        });
        ItemRegistry::register(cx, Category::Custom("Send".into()), provider);
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let infos = self.db.commands();
        let commands: Vec<Command> = self.db.with_state(|state| {
            let metadata = |id| state.get_component_metadata(id).cloned();
            infos
                .iter()
                .map(|info| Command::new(info, &metadata))
                .collect()
        });
        if commands == self.commands {
            return;
        }
        let mut drafts = HashMap::new();
        for command in &commands {
            let unchanged = self
                .commands
                .iter()
                .any(|old| old.id == command.id && old.form == command.form);
            let draft = match unchanged {
                true => self.drafts.remove(&command.id),
                false => None,
            };
            drafts.insert(
                command.id,
                draft.unwrap_or_else(|| command.form.default_draft()),
            );
        }
        self.outcomes
            .retain(|id, _| commands.iter().any(|command| command.id == *id));
        self.drafts = drafts;
        self.commands = commands;
        cx.notify();
    }

    pub(crate) fn command(&self, id: PacketId) -> Option<&Command> {
        self.commands.iter().find(|command| command.id == id)
    }

    /// The draft value at JSON `pointer` in command `id`'s draft.
    pub(crate) fn field(&self, id: PacketId, pointer: &str) -> Option<&Value> {
        self.drafts.get(&id)?.pointer(pointer)
    }

    /// Replace the draft value at `pointer`. An edit clears the last
    /// outcome, which described the draft before it.
    pub(crate) fn set_field(
        &mut self,
        id: PacketId,
        pointer: &str,
        value: Value,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self
            .drafts
            .get_mut(&id)
            .and_then(|draft| draft.pointer_mut(pointer))
        else {
            return;
        };
        *slot = value;
        self.outcomes.remove(&id);
        cx.notify();
    }

    /// Record why an edit to command `id` was not taken, where the Send
    /// row reports.
    pub(crate) fn refuse(&mut self, id: PacketId, reason: SharedString, cx: &mut Context<Self>) {
        self.outcomes.insert(id, Outcome::Refused(reason));
        cx.notify();
    }

    pub(crate) fn outcome(&self, id: PacketId) -> Option<&Outcome> {
        self.outcomes.get(&id)
    }

    /// Encode command `id`'s draft and record it for the link to carry,
    /// or refuse with the reason the operator sees.
    pub(crate) fn send(
        &mut self,
        id: PacketId,
        cx: &mut Context<Self>,
    ) -> Result<Timestamp, SharedString> {
        let result = self.try_send(id, cx);
        let outcome = match &result {
            Ok(at) => Outcome::Sent(*at),
            Err(reason) => Outcome::Refused(reason.clone()),
        };
        match &outcome {
            Outcome::Sent(_) => tracing::info!(id = ?id, "command sent"),
            Outcome::Refused(reason) => tracing::warn!(id = ?id, %reason, "command refused"),
        }
        self.outcomes.insert(id, outcome);
        cx.notify();
        result
    }

    fn try_send(&self, id: PacketId, cx: &App) -> Result<Timestamp, SharedString> {
        let command = self
            .command(id)
            .ok_or_else(|| SharedString::from("Not sent: the command is no longer offered"))?;
        if let Some(reason) = link_refusal(&(self.link)(cx)) {
            return Err(reason);
        }
        let draft = self.drafts.get(&id).unwrap_or(&Value::Null);
        let now = Timestamp::now();
        let bytes = command
            .form
            .encode(draft, now)
            .map_err(|err| SharedString::from(format!("Not sent: {err}")))?;
        self.db
            .push_msg(now, id, &bytes)
            .map_err(|err| SharedString::from(format!("Not sent: {err}")))?;
        Ok(now)
    }
}
