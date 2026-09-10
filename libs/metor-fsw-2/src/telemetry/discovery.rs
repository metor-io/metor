//! mDNS/DNS-SD advertisement for the link server.
//!
//! [`advertise`] publishes a [`FSW_SERVICE_TYPE`] service instance so ground
//! tools discover this fsw by name on the local link, without a central
//! registry. Discovery is best-effort: any failure logs and leaves the link
//! reachable by direct address, since a link that can't advertise still
//! serves. [`browse_peer`] is the other direction: one bounded round of
//! browsing for a peer member's link, the subscriber's last candidate. A
//! loopback bind is skipped: loopback isn't a multicast link, so
//! advertising `127.0.0.1` would be noise nothing on the network can reach.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use metor_proto_wkt::{
    FSW_SERVICE_TYPE, LINK_PROTOCOL_VERSION, TXT_LINK, TXT_NAMESPACE, TXT_PROTOCOL_VERSION,
    TXT_ROLE,
};

/// Advertise this link over mDNS under `name`, returning the running daemon;
/// drop it (or [`ServiceDaemon::shutdown`]) to unregister and send a goodbye.
/// `namespace` and `link` ride the TXT records as the machine identity a
/// subscriber matches on; `name` is the human instance name. `role` names
/// what serves this link when it is more than a plain flight computer (a
/// gateway member's embedded db); `None` advertises `"fsw"`.
/// `None` when the bind is loopback or the daemon can't start; the link stays
/// reachable by direct address either way.
pub(crate) fn advertise(
    name: &str,
    addr: SocketAddr,
    namespace: Option<&str>,
    link: &str,
    role: Option<&str>,
) -> Option<ServiceDaemon> {
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
    // The daemon is its own mDNS responder, so the host it claims must not be
    // the OS hostname: the system responder (mDNSResponder, Avahi) treats a
    // second answerer for its name as a conflict and renames the machine.
    let host = format!("{name}.metor.local.");
    let protocol_version = LINK_PROTOCOL_VERSION.to_string();
    let mut props: Vec<(&str, &str)> = vec![
        (TXT_PROTOCOL_VERSION, protocol_version.as_str()),
        (TXT_ROLE, role.unwrap_or("fsw")),
        (TXT_LINK, link),
    ];
    if let Some(namespace) = namespace {
        props.push((TXT_NAMESPACE, namespace));
    }
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
            tracing::info!(
                %name,
                port = addr.port(),
                namespace,
                link,
                "advertising fsw link over mDNS"
            );
            Some(daemon)
        }
        Err(err) => {
            tracing::warn!(%err, "mDNS registration failed; link not discoverable");
            None
        }
    }
}

/// Addresses of every [`FSW_SERVICE_TYPE`] instance advertising
/// `ns=<namespace>` and `link=<link>`, within `timeout`.
///
/// Blocking: the daemon hands its events over a std channel, so a subscriber
/// runs this off its task. An empty answer is the ordinary case on a host
/// with no multicast link, and the caller still has its direct candidates.
pub(crate) fn browse_peer(namespace: &str, link: &str, timeout: Duration) -> Vec<SocketAddr> {
    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(err) => {
            tracing::warn!(%err, "mDNS daemon failed to start; peer discovery skipped");
            return Vec::new();
        }
    };
    let events = match daemon.browse(FSW_SERVICE_TYPE) {
        Ok(events) => events,
        Err(err) => {
            tracing::warn!(%err, "mDNS browse failed; peer discovery skipped");
            return Vec::new();
        }
    };
    let deadline = Instant::now() + timeout;
    let mut found = Vec::new();
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let Ok(event) = events.recv_timeout(deadline - now) else {
            break;
        };
        if let ServiceEvent::ServiceResolved(info) = event
            && advertises(&info, namespace, link)
            && let Some(addr) = pick_addr(&info)
            && !found.contains(&addr)
        {
            tracing::debug!(
                instance = %instance_name(info.get_fullname()),
                %addr,
                "peer candidate discovered over mDNS"
            );
            found.push(addr);
        }
    }
    let _ = daemon.stop_browse(FSW_SERVICE_TYPE);
    let _ = daemon.shutdown();
    found
}

/// Whether an instance's TXT records carry the machine identity a subscriber
/// matches on: the peer's namespace and the serving link's name.
fn advertises(info: &ServiceInfo, namespace: &str, link: &str) -> bool {
    info.get_property_val_str(TXT_NAMESPACE) == Some(namespace)
        && info.get_property_val_str(TXT_LINK) == Some(link)
}

/// Pick one address for an instance: the first non-loopback IPv4 (stable and
/// widely routable), else any non-loopback. `None` when the instance
/// advertised only loopback, which a link server never does.
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
/// (`"plant._metor-fsw._tcp.local."` -> `"plant"`), unescaping the `\.` /
/// `\\` DNS-SD escapes.
fn instance_name(fullname: &str) -> String {
    let suffix = format!(".{FSW_SERVICE_TYPE}");
    let base = fullname.strip_suffix(&suffix).unwrap_or(fullname);
    base.replace("\\.", ".").replace("\\\\", "\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(props: &[(&str, &str)], addrs: &str) -> ServiceInfo {
        ServiceInfo::new(
            FSW_SERVICE_TYPE,
            "plant",
            "plant.metor.local.",
            addrs,
            2242,
            props,
        )
        .expect("service info")
    }

    /// Both TXT keys are the identity; either one wrong is a different peer.
    #[test]
    fn the_txt_filter_takes_one_namespace_and_link() {
        let props = [
            (TXT_ROLE, "fsw"),
            (TXT_NAMESPACE, "plant"),
            (TXT_LINK, "peer"),
        ];
        assert!(advertises(&info(&props, "10.0.0.5"), "plant", "peer"));
        assert!(!advertises(&info(&props, "10.0.0.5"), "fsw", "peer"));
        assert!(!advertises(&info(&props, "10.0.0.5"), "plant", "ground"));
        assert!(!advertises(
            &info(&[(TXT_LINK, "peer")], "10.0.0.5"),
            "plant",
            "peer"
        ));
    }

    /// A routable IPv4 wins over both loopback and IPv6, and an instance with
    /// nothing but loopback yields no candidate.
    #[test]
    fn pick_addr_prefers_a_routable_ipv4() {
        let props = [(TXT_NAMESPACE, "plant"), (TXT_LINK, "peer")];
        let addr = pick_addr(&info(&props, "fd00::5,10.0.0.5,127.0.0.1")).expect("an address");
        assert_eq!(addr, SocketAddr::from(([10, 0, 0, 5], 2242)));
        let addr = pick_addr(&info(&props, "fd00::5")).expect("an address");
        assert_eq!(addr.port(), 2242);
        assert!(!addr.ip().is_ipv4());
        assert!(pick_addr(&info(&props, "127.0.0.1")).is_none());
    }

    /// The DNS-SD escapes come back off a fullname.
    #[test]
    fn instance_name_unescapes() {
        assert_eq!(instance_name("plant._metor-fsw._tcp.local."), "plant");
        assert_eq!(instance_name("a\\.b._metor-fsw._tcp.local."), "a.b");
    }
}
