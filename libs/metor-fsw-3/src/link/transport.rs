//! Where a link's bytes flow: a socket it accepts on, or one it dials.

use core::cell::{Cell, RefCell};
use core::time::Duration;
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Rc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use stellarator::JoinHandleDropGuard;
use stellarator::net::{TcpListener, TcpStream};
use stellarator::sync::WaitQueue;

use crate::async_system::Stop;
use crate::coordinator::ParamError;

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
                let listener = TcpListener::bind(addr.as_str())
                    .map_err(|e| ParamError::Decode(format!("binding `{addr}`: {e}")))?;
                Ok(Endpoint::Listen {
                    listener,
                    slots: (*max_connections).max(1),
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

    /// Whether this endpoint accepts peers, rather than dialing one.
    pub(crate) fn listens(&self) -> bool {
        matches!(self, Self::Listen { .. })
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

/// The connections a link is handed, one at a time, from a task of its own.
///
/// The source runs apart from the link's loop so a batch to send never cancels
/// an accept or a half-established dial.
pub(crate) struct Incoming {
    stream: RefCell<Option<TcpStream>>,
    ready: WaitQueue,
    wanted: WaitQueue,
    want: Cell<bool>,
}

impl Incoming {
    /// Waits for the next connection; never resolves once the source is gone.
    pub(crate) async fn next(&self) -> TcpStream {
        loop {
            if let Some(stream) = self.stream.borrow_mut().take() {
                return stream;
            }
            let _ = self.ready.wait_for(|| self.stream.borrow().is_some()).await;
        }
    }

    /// Asks the source for another connection.
    pub(crate) fn want(&self) {
        if !self.want.replace(true) {
            self.wanted.wake_all();
        }
    }
}

/// Starts `endpoint`'s source task, which runs until `stop` or the guard drops.
pub(crate) fn incoming(endpoint: Endpoint, stop: Stop) -> (Rc<Incoming>, JoinHandleDropGuard<()>) {
    let shared = Rc::new(Incoming {
        stream: RefCell::new(None),
        ready: WaitQueue::new(),
        wanted: WaitQueue::new(),
        want: Cell::new(false),
    });
    let task = stellarator::spawn(source(shared.clone(), endpoint, stop)).drop_guard();
    (shared, task)
}

/// Accepts or dials whenever the link asks for a connection and has nowhere to
/// put the last one.
async fn source(incoming: Rc<Incoming>, mut endpoint: Endpoint, stop: Stop) {
    loop {
        let _ = incoming
            .wanted
            .wait_for(|| incoming.want.get() && incoming.stream.borrow().is_none())
            .await;
        if stop.is_set() {
            return;
        }
        let stream = match &mut endpoint {
            Endpoint::Listen { listener, .. } => {
                futures_lite::future::or(async { listener.accept().await.ok() }, async {
                    stop.wait().await;
                    None
                })
                .await
            }
            Endpoint::Connect { dialer } => dialer.connect(&stop).await,
        };
        if stop.is_set() {
            return;
        }
        let Some(stream) = stream else { continue };
        incoming.want.set(false);
        *incoming.stream.borrow_mut() = Some(stream);
        incoming.ready.wake_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::async_system::stop_pair;

    fn listen(addr: &str) -> Transport {
        Transport::Listen {
            addr: addr.to_string(),
            max_connections: MAX_CONNECTIONS,
        }
    }

    #[test]
    fn a_listen_transport_reads_as_snake_case_with_a_default_slot_count() {
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
    fn a_connect_transport_reads_its_address() {
        let parsed: Transport =
            serde_json::from_str(r#"{"connect":{"addr":"127.0.0.1:2240"}}"#).expect("decodes");
        assert!(matches!(parsed, Transport::Connect { addr } if addr == "127.0.0.1:2240"));
    }

    #[test]
    fn binding_port_zero_reports_the_port_it_took() {
        let endpoint = listen("127.0.0.1:0").bind().expect("a free port");
        let addr = endpoint.local_addr().expect("a bound listener");
        assert_ne!(addr.port(), 0);
        assert_eq!(endpoint.slots(), MAX_CONNECTIONS);
    }

    #[test]
    fn a_taken_address_names_itself_in_the_error() {
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
    fn an_unresolvable_dial_address_is_an_error() {
        let transport = Transport::Connect {
            addr: "not a host".into(),
        };
        assert!(matches!(transport.bind(), Err(ParamError::Decode(_))));
    }

    #[stellarator::test]
    async fn a_dialer_gives_up_once_stop_is_set() {
        let (handle, stop) = stop_pair();
        // A port nothing listens on, so the dial is refused at once.
        let mut dialer = Dialer::new("127.0.0.1:1".parse().expect("an address"));
        handle.stop();
        assert!(dialer.connect(&stop).await.is_none());
    }

    #[stellarator::test]
    async fn a_dialer_reaches_a_listener_and_keeps_its_first_delay() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let addr = listener.local_addr().expect("bound");
        let (_handle, stop) = stop_pair();
        let mut dialer = Dialer::new(addr);
        assert!(dialer.connect(&stop).await.is_some());
        assert_eq!(dialer.delay, BACKOFF_INITIAL);
    }
}
