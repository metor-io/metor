//! Where a link's bytes flow: a socket it accepts on, or one it dials.

use core::time::Duration;
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Rc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use stellarator::net::{TcpListener, TcpStream};

use crate::async_system::Stop;
use crate::coordinator::ParamError;

use super::conn::Connections;

/// The delay between the first two refused dials, and the longest one.
const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// Connections a listening link serves at once, unless its params say otherwise.
const MAX_CONNECTIONS: usize = 8;

/// How a link's socket is established.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// Accept up to `max_connections` peers on `addr`.
    Listen {
        addr: String,
        #[serde(default = "max_connections")]
        max_connections: usize,
    },
    /// Dial `addr`, holding one connection.
    Connect { addr: String },
}

fn max_connections() -> usize {
    MAX_CONNECTIONS
}

/// A [`Transport`] whose listener is bound, so a taken address fails the build.
pub(crate) enum Endpoint {
    Listen { listener: TcpListener, slots: usize },
    Connect { dialer: Dialer },
}

impl Transport {
    /// Binds a listening transport, or resolves a dialed one.
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
        }
    }
}

impl Endpoint {
    /// How many connections this endpoint serves at once.
    pub(crate) fn slots(&self) -> usize {
        match self {
            Self::Listen { slots, .. } => *slots,
            Self::Connect { .. } => 1,
        }
    }

    /// The address a listener bound, for a caller that asked for port zero.
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

/// Dials one address, backing off between refused attempts.
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

    /// The next connection, or `None` once `stop` resolves.
    pub(crate) async fn connect(&mut self, stop: &Stop) -> Option<TcpStream> {
        loop {
            if stop.is_set() {
                return None;
            }
            let dial = futures_lite::future::or(
                async { TcpStream::connect(self.addr).await.ok() },
                async {
                    stop.wait().await;
                    None
                },
            );
            if let Some(stream) = dial.await {
                self.delay = BACKOFF_INITIAL;
                return Some(stream);
            }
            if stop.is_set() {
                return None;
            }
            let delay = self.delay;
            self.delay = (delay * 2).min(BACKOFF_MAX);
            futures_lite::future::or(stellarator::sleep(delay), stop.wait()).await;
        }
    }
}

/// Fills `conns` from `endpoint` until `stop`.
///
/// A listener accepts every peer and closes at once the ones it has no slot
/// for; a dialer holds one connection and redials when it ends. The task runs
/// apart from the link's loop so a batch never cancels an accept or a
/// half-established dial.
pub(crate) async fn source(endpoint: Endpoint, conns: Rc<Connections>, stop: Stop) {
    match endpoint {
        Endpoint::Listen { listener, .. } => listen(listener, conns, stop).await,
        Endpoint::Connect { dialer } => dial(dialer, conns, stop).await,
    }
}

async fn listen(listener: TcpListener, conns: Rc<Connections>, stop: Stop) {
    while !stop.is_set() {
        let Some(stream) = accept(&listener, &stop).await else {
            continue;
        };
        let Some(slot) = conns.claim() else {
            continue;
        };
        let (conns, stop) = (conns.clone(), stop.clone());
        drop(stellarator::spawn(async move {
            conns.serve(slot, stream, &stop).await;
        }));
    }
}

async fn dial(mut dialer: Dialer, conns: Rc<Connections>, stop: Stop) {
    while let Some(stream) = dialer.connect(&stop).await {
        if let Some(slot) = conns.claim() {
            conns.serve(slot, stream, &stop).await;
        }
    }
}

/// The next peer, or `None` once `stop` is set or an accept failed.
///
/// A failure is the host's, not the peer's, so it is waited out rather than
/// retried at once.
async fn accept(listener: &TcpListener, stop: &Stop) -> Option<TcpStream> {
    let accepted = futures_lite::future::or(async { Some(listener.accept().await) }, async {
        stop.wait().await;
        None
    })
    .await;
    match accepted {
        Some(Ok(stream)) => Some(stream),
        Some(Err(error)) => {
            accept_failed(&error, stop).await;
            None
        }
        None => None,
    }
}

/// One accept failure: a line and a pause, cut short by `stop`.
async fn accept_failed(error: &stellarator::Error, stop: &Stop) {
    tracing::warn!(%error, "accept failed, backing off");
    futures_lite::future::or(stellarator::sleep(BACKOFF_INITIAL), stop.wait()).await;
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::async_system::stop_pair;

    fn listen(addr: &str) -> Transport {
        Transport::Listen {
            addr: addr.to_string(),
            max_connections: MAX_CONNECTIONS,
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
        assert_eq!((addr.as_str(), max_connections), ("127.0.0.1:0", 8));
    }

    #[test]
    fn test_decode_connect_address() {
        let parsed: Transport =
            serde_json::from_str(r#"{"connect":{"addr":"127.0.0.1:2240"}}"#).expect("decodes");
        assert!(matches!(parsed, Transport::Connect { addr } if addr == "127.0.0.1:2240"));
    }

    #[test]
    fn test_bind_reports_assigned_port() {
        let endpoint = listen("127.0.0.1:0").bind().expect("a free port");
        let addr = endpoint.local_addr().expect("a bound listener");
        assert_ne!(addr.port(), 0);
        assert_eq!(endpoint.slots(), MAX_CONNECTIONS);
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

    /// An accept that fails takes `EMFILE` or `ENFILE`, which a test cannot
    /// force; lowering the process fd limit by hand shows the spin this
    /// backoff replaces. The stop path is what stays covered here.
    #[stellarator::test]
    async fn test_stop_skips_accept_backoff() {
        let (handle, stop) = stop_pair();
        handle.stop();
        let error = std::io::Error::from(std::io::ErrorKind::Other).into();
        let started = Instant::now();
        accept_failed(&error, &stop).await;
        assert!(
            started.elapsed() < BACKOFF_INITIAL / 2,
            "{:?}",
            started.elapsed()
        );
    }

    #[stellarator::test]
    async fn test_stop_cancels_dial() {
        let (handle, stop) = stop_pair();
        // A port nothing listens on, so the dial is refused at once.
        let mut dialer = Dialer::new("127.0.0.1:1".parse().expect("an address"));
        handle.stop();
        assert!(dialer.connect(&stop).await.is_none());
    }

    #[stellarator::test]
    async fn test_dial_connects_without_backoff() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let addr = listener.local_addr().expect("bound");
        let (_handle, stop) = stop_pair();
        let mut dialer = Dialer::new(addr);
        assert!(dialer.connect(&stop).await.is_some());
        assert_eq!(dialer.delay, BACKOFF_INITIAL);
    }
}
