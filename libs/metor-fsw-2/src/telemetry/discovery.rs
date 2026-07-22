//! mDNS/DNS-SD advertisement for the link server.
//!
//! [`advertise`] publishes a [`FSW_SERVICE_TYPE`] service instance so ground
//! tools (metor-panel) discover this fsw by name on the local link, without a
//! central registry. Discovery is best-effort: any failure logs and leaves the
//! link reachable by direct address — a link that can't advertise still
//! serves. A loopback bind is skipped: loopback isn't a multicast link, so
//! advertising `127.0.0.1` would be noise nothing on the network can reach.

use std::net::SocketAddr;

use mdns_sd::{ServiceDaemon, ServiceInfo};
use metor_proto_wkt::{
    FSW_SERVICE_TYPE, LINK_PROTOCOL_VERSION, TXT_PROTOCOL_VERSION, TXT_ROLE,
};

/// Advertise this link over mDNS under `name`, returning the running daemon —
/// drop it (or [`ServiceDaemon::shutdown`]) to unregister and send a goodbye.
/// `None` when the bind is loopback or the daemon can't start; the link stays
/// reachable by direct address either way.
pub(crate) fn advertise(name: &str, addr: SocketAddr) -> Option<ServiceDaemon> {
    let ip = addr.ip();
    if ip.is_loopback() {
        tracing::debug!(%addr, "link bound to loopback; skipping mDNS advertisement");
        return None;
    }
    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(err) => {
            tracing::warn!(%err, "mDNS daemon failed to start; link not discoverable");
            return None;
        }
    };
    let host = format!("{}.local.", gethostname::gethostname().to_string_lossy());
    let protocol_version = LINK_PROTOCOL_VERSION.to_string();
    let props: [(&str, &str); 2] = [
        (TXT_PROTOCOL_VERSION, protocol_version.as_str()),
        (TXT_ROLE, "fsw"),
    ];
    // A wildcard bind (0.0.0.0/::) has no single advertisable address, so let
    // the daemon enumerate the host's interfaces; a specific bind advertises
    // exactly that address.
    let info = if ip.is_unspecified() {
        ServiceInfo::new(FSW_SERVICE_TYPE, name, &host, "", addr.port(), &props[..])
            .map(|info| info.enable_addr_auto())
    } else {
        ServiceInfo::new(FSW_SERVICE_TYPE, name, &host, ip, addr.port(), &props[..])
    };
    let info = match info {
        Ok(info) => info,
        Err(err) => {
            tracing::warn!(%err, "mDNS service info rejected; link not discoverable");
            return None;
        }
    };
    match daemon.register(info) {
        Ok(()) => {
            tracing::info!(%name, port = addr.port(), "advertising fsw link over mDNS");
            Some(daemon)
        }
        Err(err) => {
            tracing::warn!(%err, "mDNS registration failed; link not discoverable");
            None
        }
    }
}
