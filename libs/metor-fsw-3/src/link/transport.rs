//! How a link gets its connections: by listening for peers, or by dialing one.

use core::time::Duration;
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Rc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use stellarator::net::{TcpListener, TcpStream};

use crate::async_system::Stop;
use crate::coordinator::ParamError;

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
    /// Returns how many connections this endpoint serves at once.
    pub(crate) fn slots(&self) -> usize {
        match self {
            Self::Listen { slots, .. } => *slots,
            Self::Connect { .. } => 1,
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

    /// Dials until a connection succeeds, or returns `None` once `stop` is set.
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

/// Opens and serves connections until `stop` is set.
///
/// A listener accepts every peer, and closes a peer right away when every slot
/// is taken. A dialer keeps one connection open and redials when it closes.
/// This runs as its own task, separate from the link's loop, so sending a
/// batch never cancels an accept or a dial that is halfway through.
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

/// Accepts the next peer.
///
/// Returns `None` once `stop` is set, or after an accept fails.
///
/// An accept failure comes from the host (for example, running out of file
/// descriptors), not from the peer, so this pauses before returning instead
/// of letting the caller retry at once.
pub(crate) async fn accept(listener: &TcpListener, stop: &Stop) -> Option<TcpStream> {
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

/// Logs an accept failure and pauses. `stop` cuts the pause short.
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

    /// An accept only fails with `EMFILE` or `ENFILE`, which a test cannot
    /// force. Lowering the process fd limit by hand shows the busy loop the
    /// backoff prevents. This test covers only the stop path.
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
