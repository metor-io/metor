//! How a link gets its connections: by listening for peers, or by dialing one.

use core::time::Duration;
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Rc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use stellarator::net::{TcpListener, TcpStream};

use crate::async_system::Stop;
use crate::coordinator::{ParamError, PeerLink};

use super::conn::Connections;

/// The delay after the first refused dial. Each later refusal doubles it, up
/// to [`BACKOFF_MAX`].
const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// How a link's socket is established.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// Listen on `addr` and accept up to `max_connections` peers at once.
    Listen {
        addr: String,
        #[serde(default = "max_connections")]
        max_connections: usize,
    },
    /// Dial `addr` and keep a single connection open.
    Connect { addr: String },
    /// Dial another link in the same deployment. `metor build` replaces this
    /// with a `Connect` to that link's address.
    Peer(PeerLink),
}

/// The default number of connections a listening link serves at once.
fn max_connections() -> usize {
    8
}

/// A [`Transport`] that is ready to connect.
///
/// Links bind at build time, so a taken address fails the build instead of
/// the run.
pub(crate) enum Endpoint {
    Listen { listener: TcpListener, slots: usize },
    Connect { dialer: Dialer },
}

impl Transport {
    /// Binds the listener for `Listen`, or resolves the address for `Connect`.
    /// A `Peer` that `metor build` has not resolved is an error.
    pub(crate) fn bind(&self) -> Result<Endpoint, ParamError> {
        match self {
            Self::Listen {
                addr,
                max_connections,
            } => {
                if *max_connections == 0 {
                    return Err(ParamError::Decode("`max_connections` is at least 1".into()));
                }
                let listener = TcpListener::bind(addr.as_str())
                    .map_err(|e| ParamError::Decode(format!("binding `{addr}`: {e}")))?;
                Ok(Endpoint::Listen {
                    listener,
                    slots: *max_connections,
                })
            }
            Self::Connect { addr } => Ok(Endpoint::Connect {
                dialer: Dialer::new(resolve(addr)?),
            }),
            Self::Peer(peer) => Err(ParamError::Decode(format!(
                "peer link `{}` on target `{}` is not resolved; run `metor build`",
                peer.link, peer.target
            ))),
        }
    }
}

impl Endpoint {
    /// Returns how many connections this endpoint serves at once.
    pub(crate) fn slots(&self) -> usize {
        match self {
            Self::Listen { slots, .. } => *slots,
            Self::Connect { .. } => 1,
        }
    }

    /// Returns the bound listener, for a system that runs its own accept loop.
    #[cfg(feature = "db")]
    pub(crate) fn listener(&self) -> Option<&TcpListener> {
        match self {
            Self::Listen { listener, .. } => Some(listener),
            Self::Connect { .. } => None,
        }
    }

    /// Returns the address the listener bound. This is how a caller that asked
    /// for port zero learns the real port.
    pub(crate) fn local_addr(&self) -> Option<SocketAddr> {
        match self {
            Self::Listen { listener, .. } => listener.local_addr().ok(),
            Self::Connect { .. } => None,
        }
    }
}

fn resolve(addr: &str) -> Result<SocketAddr, ParamError> {
    addr.to_socket_addrs()
        .map_err(|e| ParamError::Decode(format!("resolving `{addr}`: {e}")))?
        .next()
        .ok_or_else(|| ParamError::Decode(format!("`{addr}` resolves to no address")))
}

/// Dials one address, waiting longer after each refused attempt.
pub(crate) struct Dialer {
    addr: SocketAddr,
    delay: Duration,
}

impl Dialer {
    fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            delay: BACKOFF_INITIAL,
        }
    }

    /// Dials until a connection succeeds.
    pub(crate) async fn connect(&mut self) -> TcpStream {
        loop {
            if let Ok(stream) = TcpStream::connect(self.addr).await {
                self.delay = BACKOFF_INITIAL;
                return stream;
            }
            let delay = self.delay;
            self.delay = (delay * 2).min(BACKOFF_MAX);
            stellarator::sleep(delay).await;
        }
    }
}

impl Endpoint {
    /// Opens and serves connections until `stop` is set.
    ///
    /// A listener accepts every peer, and closes a peer right away when every
    /// slot is taken. A dialer keeps one connection open and redials when it
    /// closes. This runs as its own task, separate from the link's loop, so
    /// sending a batch never cancels an accept or a dial that is halfway
    /// through.
    pub(crate) async fn run(self, conns: Rc<Connections>, stop: Stop) {
        let serve = async {
            match self {
                Self::Listen { listener, .. } => listen(listener, &conns, &stop).await,
                Self::Connect { dialer } => dial(dialer, &conns, &stop).await,
            }
        };
        futures_lite::future::or(serve, stop.wait()).await;
    }
}

async fn listen(listener: TcpListener, conns: &Rc<Connections>, stop: &Stop) {
    loop {
        let Some(stream) = accept(&listener).await else {
            continue;
        };
        if let Some(served) = conns.serve(stream, stop) {
            drop(stellarator::spawn(served));
        }
    }
}

async fn dial(mut dialer: Dialer, conns: &Rc<Connections>, stop: &Stop) {
    loop {
        let stream = dialer.connect().await;
        if let Some(served) = conns.serve(stream, stop) {
            served.await;
        }
    }
}

/// Accepts the next peer, or returns `None` after an accept fails.
///
/// An accept failure comes from the host (for example, running out of file
/// descriptors), not from the peer, so this pauses before returning instead
/// of letting the caller retry at once.
pub(crate) async fn accept(listener: &TcpListener) -> Option<TcpStream> {
    match listener.accept().await {
        Ok(stream) => Some(stream),
        Err(error) => {
            tracing::warn!(%error, "accept failed, backing off");
            stellarator::sleep(BACKOFF_INITIAL).await;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::async_system::stop_pair;

    fn listen(addr: &str) -> Transport {
        Transport::Listen {
            addr: addr.to_string(),
            max_connections: max_connections(),
        }
    }

    #[test]
    fn test_decode_listen_defaults() {
        let parsed: Transport =
            serde_json::from_str(r#"{"listen":{"addr":"127.0.0.1:0"}}"#).expect("decodes");
        let Transport::Listen {
            addr,
            max_connections,
        } = parsed
        else {
            panic!("a listening transport")
        };
        assert_eq!(addr, "127.0.0.1:0");
        assert_eq!(max_connections, super::max_connections());
    }

    #[test]
    fn test_peer_transport_is_unresolved() {
        let parsed: Transport =
            serde_json::from_str(r#"{"peer":{"target":"a","link":"pub"}}"#).expect("peer form");
        let Err(ParamError::Decode(message)) = parsed.bind() else {
            panic!("a peer transport does not bind")
        };
        assert!(
            message.contains("`pub`") && message.contains("metor build"),
            "{message}"
        );
    }

    #[test]
    fn test_bind_reports_assigned_port() {
        let endpoint = listen("127.0.0.1:0").bind().expect("a free port");
        let addr = endpoint.local_addr().expect("a bound listener");
        assert_ne!(addr.port(), 0);
        assert_eq!(endpoint.slots(), max_connections());
    }

    #[test]
    fn test_bind_error_reports_address() {
        let held = listen("127.0.0.1:0").bind().expect("a free port");
        let addr = held.local_addr().expect("a bound listener");
        // SO_REUSEADDR lets a second bind share the port, so name an address
        // no host owns instead.
        let Err(ParamError::Decode(message)) = listen("203.0.113.1:9").bind() else {
            panic!("an unassignable address is an error")
        };
        assert!(message.contains("203.0.113.1:9"), "{message}");
        assert_ne!(addr.port(), 0);
    }

    #[test]
    fn test_reject_zero_connections() {
        let transport = Transport::Listen {
            addr: "127.0.0.1:0".into(),
            max_connections: 0,
        };
        let Err(ParamError::Decode(message)) = transport.bind() else {
            panic!("a listener with no slots is an error")
        };
        assert!(message.contains("max_connections"), "{message}");
    }

    #[test]
    fn test_reject_invalid_connect_address() {
        let transport = Transport::Connect {
            addr: "not a host".into(),
        };
        assert!(matches!(transport.bind(), Err(ParamError::Decode(_))));
    }

    #[stellarator::test]
    async fn test_stop_ends_a_dialing_endpoint() {
        let (handle, stop) = stop_pair();
        // A port nothing listens on, so every dial is refused.
        let dialer = Dialer::new("127.0.0.1:1".parse().expect("an address"));
        let inbox = Rc::new(super::super::inbox::Inbox::blackhole());
        let conns = Connections::new(1, 64, Vec::new(), inbox).expect("a small ring");
        handle.stop();
        Endpoint::Connect { dialer }.run(Rc::new(conns), stop).await;
    }

    #[stellarator::test]
    async fn test_dial_connects_without_backoff() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let addr = listener.local_addr().expect("bound");
        let mut dialer = Dialer::new(addr);
        dialer.connect().await;
        assert_eq!(dialer.delay, BACKOFF_INITIAL);
    }
}
