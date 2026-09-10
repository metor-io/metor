//! The gateway's own telemetry, written straight into its embedded db.
//!
//! `Record` taps the same rings a [`Downlink`](crate::TelemetrySystem) taps
//! and writes their records into [`DbState`]'s `DB` without framing a packet:
//! the ring bytes are exactly what `ingest_table` and `push_msg` read. It
//! carries `ReceiveAll`, so resolve defers it to the tail beside a downlink;
//! its own `system_status` and `log` are in the tap set from the next cycle,
//! as a downlink's are.

use metor_db::DB;
use metor_fsw_2_core::log::LogLevel;
use metor_fsw_2_core::{AllOutputs, BuildSystem, CyclicSystem, Out, Shared, System, split_record};
use metor_proto::types::Timestamp;
use metor_proto_wkt::VTableMsg;

use super::DbState;
use crate::telemetry::taps::{Announce, Tap, TelemetryMode, Wire, collect_taps, drain_taps};

/// The whole graph, tapped; a recorder mints no port of its own beyond the
/// implicit log.
#[derive(crate::SystemOutput)]
pub struct RecordPorts {
    all: AllOutputs,
}

/// Stores this target's telemetered outputs into the embedded db.
#[derive(Default)]
pub struct RecordSystem {
    /// The shared db this recorder writes into. `None` on a detached instance
    /// ([`BuildSystem::new`]); the gateway pack's ctor attaches it.
    db: Option<Shared<DbState>>,
    taps: Vec<Tap>,
}

impl RecordSystem {
    /// A detached recorder; attach the db via the gateway pack.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the shared db this recorder writes into.
    pub fn attach(mut self, db: Shared<DbState>) -> Self {
        self.db = Some(db);
        self
    }
}

impl BuildSystem for RecordSystem {
    type Params = ();

    fn new(_params: ()) -> Self {
        Self::default()
    }
}

impl System for RecordSystem {
    type Input = ();
    type Output = Out<RecordPorts>;
    const NAME: &'static str = "record";

    /// Claim one view per tapped ring and register every announced schema in
    /// the db, so the first record of each already has its vtable.
    fn init(&mut self, output: &mut Self::Output) {
        let taps = collect_taps(&output.all, &TelemetryMode::All);

        for (refused, kept) in &taps.collisions {
            output.log().fault(
                LogLevel::Error,
                "telemetry_table_id_collision",
                "two tables hash to one packet id; the later is not recorded",
                &[("refused", refused), ("kept", kept)],
            );
        }
        for key in &taps.exhausted {
            output.log().fault(
                LogLevel::Warn,
                "telemetry_reader_slot",
                &format!("no reader slot left on `{key}` — raise CoordinatorConfig::reader_slack"),
                &[],
            );
        }

        let db = self
            .db
            .as_ref()
            .expect("record attached to a Db state (the gateway pack's ctor)")
            .get()
            .db()
            .clone();
        for announce in &taps.announces {
            if let Err(err) = announce_into(&db, announce) {
                let name = match announce {
                    Announce::Table { packet_id, .. } => format!("table {packet_id:?}"),
                    Announce::Msg(m) => m.metadata.name.clone(),
                };
                output.log().fault(
                    LogLevel::Error,
                    "record_table_conflict",
                    "the db refused an announced schema; its records are not recorded",
                    &[("tap", &name), ("error", &err)],
                );
            }
        }
        self.taps = taps.taps;
    }
}

impl CyclicSystem for RecordSystem {
    fn execute(&mut self, now: Timestamp, _input: &mut (), output: &mut Self::Output) {
        let db = self
            .db
            .as_ref()
            .expect("record attached to a Db state (the gateway pack's ctor)")
            .get()
            .db()
            .clone();
        let mut rejected = 0u64;
        let corrupt = drain_taps(&mut self.taps, |wire, _, rec| {
            let stored = match wire {
                Wire::Table { packet_id } => db.ingest_table(*packet_id, rec),
                Wire::Msg => match split_record(rec) {
                    Some((id, payload)) => db.push_msg(now, id, payload),
                    None => return,
                },
            };
            if stored.is_err() {
                rejected += 1;
            }
        });
        if rejected > 0 {
            output.log().fault(
                LogLevel::Warn,
                "record_rejected",
                "the db refused records this cycle",
                &[("rejected", &rejected)],
            );
        }
        if corrupt > 0 {
            output.log().fault(
                LogLevel::Warn,
                "record_corrupt",
                "unreadable records this cycle",
                &[("corrupt", &corrupt)],
            );
        }
    }
}

/// Register one announced schema: a table's vtable and its components'
/// metadata, or a message log's payload schema.
fn announce_into(db: &DB, announce: &Announce) -> Result<(), metor_db::Error> {
    match announce {
        Announce::Table {
            packet_id,
            vtable,
            metadata,
        } => {
            db.insert_vtable(VTableMsg {
                id: *packet_id,
                vtable: vtable.clone(),
            })?;
            db.with_state_mut(|state| {
                for entry in metadata {
                    state.set_component_metadata(entry.clone(), &db.path)?;
                }
                Ok(())
            })
        }
        Announce::Msg(m) => {
            db.with_state_mut(|state| state.set_msg_metadata(m.id, m.metadata.clone(), &db.path))
        }
    }
}
