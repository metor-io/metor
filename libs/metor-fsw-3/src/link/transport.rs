//! How a link gets its connections: by listening for peers, or by dialing one.

use core::num::NonZeroUsize;
use core::time::Duration;
use std::net::{SocketAddr, ToSocketAddrs};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use stellarator::net::{TcpListener, TcpStream};

use crate::async_system::Stop;
use crate::coordinator::{MakeError, ParamError};

use super::conn::Serving;

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
        max_connections: NonZeroUsize,
    },
    /// Dial `addr` and keep a single connection open.
    Connect { addr: String },
}

/// The default number of connections a listening link serves at once.
fn max_connections() -> NonZeroUsize {
    // PANIC Safety: evaluated at compile time.
    const { NonZeroUsize::new(8).expect("8 is non-zero") }
}

/// A [`Transport`] that is ready to connect.
///
/// Links bind at build time, so a taken address fails the build instead of
/// the run.
pub(crate) enum Endpoint {
    Listen {
        listener: TcpListener,
        slots: NonZeroUsize,
    },
    Connect {
        dialer: Dialer,
    },
}

impl Transport {
    /// Binds the listener for `Listen`, or resolves the address for `Connect`.
    pub(crate) fn bind(&self) -> Result<Endpoint, MakeError> {
        match self {
            Self::Listen {
                addr,
                max_connections,
            } => {
                let listener = TcpListener::bind(addr.as_str()).map_err(|e| MakeError::Bind {
                    addr: addr.clone(),
                    message: e.to_string(),
                })?;
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
            Self::Listen { slots, .. } => slots.get(),
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

    /// Opens and serves connections until `stop` is set.
    ///
    /// A listener accepts every peer, and closes a peer right away when every
    /// slot is taken. A dialer keeps one connection open and redials when it
    /// closes. This runs as its own task, separate from the link's loop, so
    /// sending a batch never cancels an accept or a dial that is halfway
    /// through.
    pub(crate) async fn run(self, mut serving: Serving, stop: Stop) {
        let serve = async {
            match self {
                Self::Listen { listener, .. } => listen(listener, &mut serving, &stop).await,
                Self::Connect { dialer } => dial(dialer, &serving, &stop).await,
            }
        };
        futures_lite::future::or(serve, stop.wait()).await;
    }
}

fn resolve(addr: &str) -> Result<SocketAddr, MakeError> {
    addr.to_socket_addrs()
        .map_err(|e| ParamError::Decode(format!("resolving `{addr}`: {e}")))?
        .next()
        .ok_or_else(|| ParamError::Decode(format!("`{addr}` resolves to no address")).into())
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

async fn listen(listener: TcpListener, serving: &mut Serving, stop: &Stop) {
    loop {
        if let Some(stream) = accept(&listener).await {
            serving.spawn(stream, stop);
        }
    }
}

/// Serves one connection at a time in slot 0.
async fn dial(mut dialer: Dialer, serving: &Serving, stop: &Stop) {
    loop {
        let stream = dialer.connect().await;
        serving.serve(0, stream, stop).await;
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
    fn test_peer_transport_does_not_parse() {
        let peer = r#"{"peer":{"target":"a","link":"pub"}}"#;
        assert!(serde_json::from_str::<Transport>(peer).is_err());
    }

    #[test]
    fn test_bind_reports_assigned_port() {
        let endpoint = listen("127.0.0.1:0").bind().expect("a free port");
        let addr = endpoint.local_addr().expect("a bound listener");
        assert_ne!(addr.port(), 0);
        assert_eq!(endpoint.slots(), max_connections().get());
    }

    #[test]
    fn test_bind_error_reports_address() {
        let held = listen("127.0.0.1:0").bind().expect("a free port");
        let addr = held.local_addr().expect("a bound listener");
        // SO_REUSEADDR lets a second bind share the port, so name an address
        // no host owns instead.
        let Err(MakeError::Bind { addr: refused, .. }) = listen("203.0.113.1:9").bind() else {
            panic!("an unassignable address is an error")
        };
        assert_eq!(refused, "203.0.113.1:9");
        assert_ne!(addr.port(), 0);
    }

    #[test]
    fn test_reject_zero_connections() {
        let text = r#"{"listen":{"addr":"127.0.0.1:0","max_connections":0}}"#;
        assert!(serde_json::from_str::<Transport>(text).is_err());
    }

    #[test]
    fn test_reject_invalid_connect_address() {
        let transport = Transport::Connect {
            addr: "not a host".into(),
        };
        assert!(matches!(
            transport.bind(),
            Err(MakeError::Params(ParamError::Decode(_)))
        ));
    }

    #[stellarator::test]
    async fn test_stop_ends_a_dialing_endpoint() {
        let (handle, stop) = Stop::pair();
        // A port nothing listens on, so every dial is refused.
        let dialer = Dialer::new("127.0.0.1:1".parse().expect("an address"));
        let inbox = std::rc::Rc::new(super::super::inbox::Inbox::blackhole());
        let cap = super::super::ConnCap::try_from(64).expect("a small ring");
        let (_conns, serving) = super::super::conn::Connections::new(1, cap, Vec::new(), inbox);
        handle.stop();
        Endpoint::Connect { dialer }.run(serving, stop).await;
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
