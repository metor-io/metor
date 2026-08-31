//! Which lanes the timeline shows, in the order the coordinator steps them.
//!
//! The row list is derived from the live wiring IR rather than from the
//! database's component tree, because the IR is the only thing that knows
//! *step order* — and step order is what makes the prefix-sum bar layout mean
//! anything. Two systems' run records carry a duration each but no start; the
//! bars only line up because the coordinator runs them serially in this order.
//!
//! Everything here is pure so the ordering rules can be tested against a
//! synthetic `Wiring` without a database or a window.

use gpui::SharedString;
use metor_fsw_2::ir::{DOWNLINK_TYPE, EdgeKind, SlotSpec, SystemSpec, Wiring};
use metor_proto::types::ComponentId;

/// The instance name the coordinator publishes its own run record under.
pub(crate) const COORDINATOR: &str = "coordinator";

/// What a lane stands for. Presentational only — the scan treats every row
/// identically; the kind decides the gutter's styling and whether the
/// `show_slots` / `show_coordinator_row` filters apply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowKind {
    System,
    Slot,
    Coordinator,
}

/// One lane: an instance and the two `system_status` leaves its bars are built
/// from. Both ids are resolved once here so the data path never re-derives a
/// name.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExecRow {
    /// The instance name, which is also the gutter label and the id the
    /// inspector looks the node up by.
    pub name: SharedString,
    pub kind: RowKind,
    /// `<prefix>.last_execute_us` — how long the last step took.
    pub duration_id: ComponentId,
    /// `<prefix>.state`, as [`SlotState::code`](metor_fsw_2::SlotState::code).
    pub state_id: ComponentId,
}

impl ExecRow {
    fn new(namespace: Option<&str>, name: &str, over: Option<&str>, kind: RowKind) -> Self {
        let prefix = status_prefix(namespace, name, over);
        Self {
            name: SharedString::from(name.to_string()),
            kind,
            duration_id: ComponentId::new(&format!("{prefix}.last_execute_us")),
            state_id: ComponentId::new(&format!("{prefix}.state")),
        }
    }
}

/// Where an instance's run record lives, as a dotted component-name prefix.
///
/// A spec's `status` override wins verbatim — namespace included — because the
/// point of the field is to aim a node at a foreign timing source. Otherwise
/// the framework convention applies: the coordinator's namespace, the instance
/// name, and the host-appended `system_status` frame.
pub(crate) fn status_prefix(namespace: Option<&str>, name: &str, over: Option<&str>) -> String {
    if let Some(over) = over {
        return over.to_string();
    }
    match namespace {
        Some(ns) => format!("{ns}.{name}.system_status"),
        None => format!("{name}.system_status"),
    }
}

/// The lanes of one target: the encompassing record first, then the order
/// `wiring::resolve` registers everything else — ordinary systems, then slots,
/// then the receive-all systems it defers behind both.
///
/// The envelope leads because that is what it is: the whole cycle, with every
/// other lane nested inside it. Reading top-to-bottom then goes from the cycle
/// to its parts. Everything downstream takes it as row 0 — the authoritative
/// cycle set and the context band both. It is the system marked
/// [`SystemSpec::encompassing`], or, when nothing is marked, the framework's
/// own `<ns.>coordinator.system_status`, which is not a `SystemSpec` at all and
/// so can only be synthesized.
///
/// Receive-all-ness is not in the IR, so only the built-in downlink is
/// recognized. A custom receive-all system therefore lands one cycle-position
/// too early: its bars can shuffle within a cycle, though every duration stays
/// correct.
pub(crate) fn derive_rows(wiring: &Wiring) -> Vec<ExecRow> {
    let namespace = wiring.coordinator.namespace.as_deref();
    let deferred = |spec: &SystemSpec| spec.ty.as_deref() == Some(DOWNLINK_TYPE);
    let row_of =
        |spec: &SystemSpec, kind| ExecRow::new(namespace, &spec.name, spec.status.as_deref(), kind);
    let slot_row = |spec: &SlotSpec| {
        ExecRow::new(namespace, &spec.name, spec.status.as_deref(), RowKind::Slot)
    };

    // Only the first marked system is honored; a second one is a config
    // mistake, and demoting it to an ordinary lane loses no information.
    let encompassing = wiring.systems.iter().position(|s| s.encompassing);
    let ordinary = |(i, spec): &(usize, &SystemSpec)| Some(*i) != encompassing && !deferred(spec);

    let indexed: Vec<(usize, &SystemSpec)> = wiring.systems.iter().enumerate().collect();
    let mut rows: Vec<ExecRow> = vec![match encompassing.map(|i| &wiring.systems[i]) {
        Some(spec) => row_of(spec, RowKind::Coordinator),
        None => ExecRow::new(namespace, COORDINATOR, None, RowKind::Coordinator),
    }];
    rows.extend(
        indexed
            .iter()
            .filter(|entry| ordinary(entry))
            .map(|(_, spec)| row_of(spec, RowKind::System)),
    );
    rows.extend(wiring.slots.iter().map(slot_row));
    rows.extend(
        indexed
            .iter()
            .filter(|(i, spec)| Some(*i) != encompassing && deferred(spec))
            .map(|(_, spec)| row_of(spec, RowKind::System)),
    );
    rows
}

/// One drawn data-flow connector: a producer lane, a consumer lane, and every
/// port pair between them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FlowEdge {
    /// Index into the row list of the producing lane.
    pub from: usize,
    /// Index into the row list of the consuming lane.
    pub to: usize,
    /// True when this connector carries the one-cycle-delayed feedback ports.
    /// Delay is part of the grouping key, not a summary of it: a delayed line
    /// reaches back to the previous cycle's producer bar, so it cannot share
    /// geometry with a same-cycle one between the same two instances.
    pub delayed: bool,
    /// `"<out> → <in>"` per grouped edge, in declaration order, for the hover
    /// readout.
    pub ports: Vec<SharedString>,
}

/// Group the wiring's frame edges into one connector per
/// `(producer, consumer, delayed)` triple.
///
/// Several ports commonly run between the same two systems; drawing a line
/// each would turn a busy graph into hatching, so they collapse into one line
/// that names them all on hover. Delay joins the key because the two kinds land
/// in different places — a forward port is consumed in the cycle that produced
/// it, a delayed one in the next — so a pair wired both ways gets two lines.
///
/// Message edges are left out deliberately: they are many-to-many pub/sub the
/// FSW itself excludes from cycle ordering, so they carry no "produced here,
/// consumed there, this cycle" meaning. Edges naming an instance with no lane
/// are dropped, as are self-edges, which have nothing to connect.
pub(crate) fn derive_edges(wiring: &Wiring, rows: &[ExecRow]) -> Vec<FlowEdge> {
    let index_of = |name: &str| rows.iter().position(|r| r.name.as_ref() == name);
    let mut out: Vec<FlowEdge> = Vec::new();
    for edge in &wiring.edges {
        if edge.kind != EdgeKind::Frame {
            continue;
        }
        let (Some(from), Some(to)) = (index_of(&edge.from), index_of(&edge.to)) else {
            continue;
        };
        if from == to {
            continue;
        }
        let port = SharedString::from(format!("{} → {}", edge.out, edge.in_));
        match out
            .iter_mut()
            .find(|e| e.from == from && e.to == to && e.delayed == edge.delayed)
        {
            Some(existing) => existing.ports.push(port),
            None => out.push(FlowEdge {
                from,
                to,
                delayed: edge.delayed,
                ports: vec![port],
            }),
        }
    }
    out
}
