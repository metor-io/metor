//! mDNS/DNS-SD browsing: discover fsw links on the local network by name.
//!
//! [`mdns_source`] is a discovery source for
//! [`PanelApp::connection_source`](crate::PanelApp::connection_source). At
//! startup it browses [`FSW_SERVICE_TYPE`] on a background thread and upserts a
//! [`ConnectionTarget`] per resolved instance through the [`RegistryHandle`],
//! so discovered fsw links appear in the picker by name with no central
//! registry. A goodbye (or TTL expiry) removes the target again. Discovery is
//! a hint only: the target still dials and authenticates like a typed one.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use metor_proto_wkt::FSW_SERVICE_TYPE;

use super::{ConnectionTarget, RegistryHandle, TargetId};

/// A discovery source that browses the local link for fsw services. Pass it to
/// [`PanelApp::connection_source`](crate::PanelApp::connection_source); it
/// spawns the browse loop when handed the registry at startup.
pub fn mdns_source() -> impl FnOnce(RegistryHandle) {
    move |handle| {
        if let Err(err) = std::thread::Builder::new()
            .name("mdns-discovery".into())
            .spawn(move || browse(handle))
        {
            tracing::warn!(%err, "failed to spawn mDNS discovery thread");
        }
    }
}

/// Browse forever, translating service events into registry ops. Returns when
/// the daemon's channel closes (app shutdown).
fn browse(handle: RegistryHandle) {
    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(err) => {
            tracing::warn!(%err, "mDNS daemon failed to start; discovery disabled");
            return;
        }
    };
    let events = match daemon.browse(FSW_SERVICE_TYPE) {
        Ok(events) => events,
        Err(err) => {
            tracing::warn!(%err, "mDNS browse failed; discovery disabled");
            return;
        }
    };
    // Fullname -> the target id we upserted, so a goodbye maps back to the
    // right registry removal without reparsing the address.
    let mut seen: HashMap<String, TargetId> = HashMap::new();
    while let Ok(event) = events.recv() {
        match event {
            ServiceEvent::ServiceResolved(info) => {
                let Some(addr) = pick_addr(&info) else {
                    continue;
                };
                let target = ConnectionTarget::tcp(instance_name(info.get_fullname()), addr);
                seen.insert(info.get_fullname().to_string(), target.id.clone());
                handle.upsert(target);
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                if let Some(id) = seen.remove(&fullname) {
                    handle.remove(id);
                }
            }
            _ => {}
        }
    }
    drop(daemon);
}

/// Pick one address for the target id: the first non-loopback IPv4 (stable and
/// widely routable), else any non-loopback address. `None` when the instance
/// advertised only loopback. Deterministic, so a multi-homed instance keeps a
/// stable `tcp:<addr>` id across re-resolutions.
fn pick_addr(info: &ServiceInfo) -> Option<SocketAddr> {
    let port = info.get_port();
    let mut ips: Vec<IpAddr> = info
        .get_addresses()
        .iter()
        .copied()
        .filter(|ip| !ip.is_loopback())
        .collect();
    ips.sort();
    let ip = ips.iter().find(|ip| ip.is_ipv4()).or_else(|| ips.first())?;
    Some(SocketAddr::new(*ip, port))
}

/// The human instance name from a DNS-SD fullname
/// (`"Orbital Sim 3._metor-fsw._tcp.local."` -> `"Orbital Sim 3"`), unescaping
/// the `\.` / `\\` DNS-SD escapes.
fn instance_name(fullname: &str) -> String {
    let suffix = format!(".{FSW_SERVICE_TYPE}");
    let base = fullname.strip_suffix(&suffix).unwrap_or(fullname);
    base.replace("\\.", ".").replace("\\\\", "\\")
}
