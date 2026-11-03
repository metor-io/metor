//! The persistence thread: one per [`DB`](crate::DB), running everything that
//! writes the database's directory in the background.
//!
//! Draining WALs, sealing rolled nodes and downsampling for LoD all do disk
//! I/O and checksums that would stall ingest or serving if they shared an
//! executor. The thread owns no list of work: it finds the components and
//! message logs in the database and starts one task per new one, so whoever
//! adds a component, on whatever thread, needs no handle to it.

use std::{collections::HashSet, sync::Arc};

use crate::{DbCore, lod};

pub(crate) async fn run(db: Arc<DbCore>) {
    let mut components = HashSet::new();
    let mut msg_logs = HashSet::new();
    let mut lod_running = false;
    loop {
        // Read before the scan, so an addition during it wakes the wait below.
        let generation = db.storage_gen.latest();
        let (new_components, new_logs) = db.with_state(|state| {
            let new_components: Vec<_> = state
                .components
                .iter()
                .filter(|(id, _)| !components.contains(*id))
                .map(|(id, component)| (*id, component.clone()))
                .collect();
            let new_logs: Vec<_> = state
                .msg_logs
                .iter()
                .filter(|(id, _)| !msg_logs.contains(*id))
                .map(|(id, log)| (*id, log.clone()))
                .collect();
            (new_components, new_logs)
        });
        for (id, component) in new_components {
            components.insert(id);
            stellarator::spawn(component.persist());
            stellarator::spawn(component.time_series.clone().lifecycle());
        }
        for (id, log) in new_logs {
            msg_logs.insert(id);
            stellarator::spawn(log.persist());
        }
        if !lod_running && db.lod_enabled() {
            lod_running = true;
            stellarator::spawn(lod::run(db.clone()));
        }
        db.storage_gen.wait_for(|g| g != generation).await;
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc, time::Duration};

    use metor_proto::types::{ComponentId, LenPacket, PrimType, Timestamp};
    use metor_proto::vtable::builder::{
        component, raw_field, raw_table, schema, timestamp, vtable,
    };
    use metor_proto_stellar::Client;
    use metor_proto_wkt::VTableMsg;
    use stellarator::net::TcpListener;

    use crate::{DB, Server};

    const ID: [u8; 2] = [0x71, 3];

    fn serve(db: Arc<DB>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Server { listener, db };
        stellarator::struc_con::stellar(move || server.run());
        addr
    }

    fn table(ts: i64) -> LenPacket {
        let mut pkt = LenPacket::table(ID, 16);
        pkt.extend_aligned(&[ts]);
        pkt.extend_aligned(&[ts as f64]);
        pkt
    }

    fn latest(db: &DB) -> Option<Timestamp> {
        db.with_state(|s| {
            let component = s.get_component(ComponentId::new("x"))?;
            Some(component.time_series.latest()?.timestamp())
        })
    }

    async fn wait_latest(db: &DB, ts: i64) -> bool {
        for _ in 0..200 {
            if latest(db) == Some(Timestamp(ts)) {
                return true;
            }
            stellarator::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// A component outlives the connection that announced it, so its samples
    /// keep reaching disk after that connection closes.
    #[stellarator::test]
    async fn persistence_outlives_the_announcing_connection() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(dir.path().join("db")).unwrap());
        let addr = serve(db.clone());

        let mut first = Client::connect(addr).await.unwrap();
        let vtable = vtable([raw_field(
            8,
            8,
            timestamp(raw_table(0, 8), schema(PrimType::F64, &[], component("x"))),
        )]);
        first.send(&VTableMsg { id: ID, vtable }).await.0.unwrap();
        first.send(table(1)).await.0.unwrap();
        assert!(wait_latest(&db, 1).await, "sample 1 never persisted");
        drop(first);
        // Let the server notice the close and tear the connection down.
        stellarator::sleep(Duration::from_millis(200)).await;

        let mut second = Client::connect(addr).await.unwrap();
        second.send(table(2)).await.0.unwrap();
        assert!(
            wait_latest(&db, 2).await,
            "sample 2 never persisted: latest {:?}",
            latest(&db)
        );
    }
}
