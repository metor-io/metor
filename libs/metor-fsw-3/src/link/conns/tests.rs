use std::collections::{HashMap, VecDeque};

use zerocopy::IntoBytes;

use crate::io::test::{TestIo, TestPort, cap_with_ops};
use crate::io::{Op, RawHandle};

use super::*;

const SEED: &[u8] = b"seed";
const CAP: usize = 64;
const PEER: &str = "10.0.0.2:2240";

fn listen(slots: usize) -> ConnsConfig {
    ConnsConfig {
        slots,
        conn_cap: CAP,
        dialing: false,
        write_timeout: Duration::from_millis(50),
    }
}

fn dial() -> ConnsConfig {
    ConnsConfig {
        dialing: true,
        ..listen(1)
    }
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut bytes = (body.len() as u32).to_le_bytes().to_vec();
    bytes.extend_from_slice(body);
    bytes
}

/// A link's connections, and the far end that answers their ops.
struct Rig {
    conns: Conns,
    io: TestIo,
    net: Net,
    bodies: Vec<Vec<u8>>,
    events: Vec<Event>,
    turn: usize,
}

/// The far end: the streams the endpoint opens next, and each stream's
/// peer.
#[derive(Default)]
struct Net {
    streams: VecDeque<Result<RawHandle, IoError>>,
    peers: HashMap<RawHandle, Peer>,
    closed: Vec<RawHandle>,
    /// The turns a stream op was answered on.
    asked: Vec<usize>,
}

#[derive(Default)]
struct Peer {
    /// What the peer sends, as recvs ask for it.
    outbound: Vec<u8>,
    received: Vec<u8>,
    /// The most bytes one send takes.
    max_send: Option<usize>,
    /// Sends never finish.
    hang: bool,
    eof: bool,
    fail_recv: bool,
    fail_send: bool,
}

/// Allocates `config`'s buffers and binds them to `ep`.
fn make(
    io: &TestIo,
    ep: &Handle<Endpoint>,
    config: ConnsConfig,
    seed: &[u8],
) -> Result<Conns, MakeError> {
    let mem = io.make(|init| ConnsMem::make(init, config));
    Conns::new(mem, ep, seed, 0)
}

fn rig(config: ConnsConfig) -> Rig {
    let (ep, io) = cap_with_ops::<Endpoint>(ops_for(config.slot_count()) as u16);
    let conns = make(&io, &ep, config, SEED).expect("fits");
    Rig {
        conns,
        io,
        net: Net::default(),
        bodies: Vec::new(),
        events: Vec::new(),
        turn: 0,
    }
}

impl Rig {
    /// Queues a stream for the endpoint to open, and returns its handle.
    fn connect(&mut self) -> RawHandle {
        let handle = RawHandle::new(10 + self.net.peers.len() as u16, 1);
        self.net.streams.push_back(Ok(handle));
        self.net.peers.insert(handle, Peer::default());
        handle
    }

    /// Connects a peer and runs until its seed is sent.
    fn accept(&mut self) -> RawHandle {
        let handle = self.connect();
        self.turns(4);
        handle
    }

    fn peer(&mut self, handle: RawHandle) -> &mut Peer {
        self.net.peers.get_mut(&handle).expect("a connected peer")
    }

    /// The far end answers what it can, without the connections polling.
    fn serve(&mut self) {
        self.turn += 1;
        let port = self.io.turn();
        self.net.serve(port, self.turn);
    }

    /// One turn. The ops in flight never pass the budget.
    fn turn(&mut self) {
        self.serve();
        let bodies = &mut self.bodies;
        let turn = self.conns.poll(|_, body| bodies.push(body.to_vec()));
        self.events.extend(turn.events());
        let budget = ops_for(self.conns.slots.len());
        assert!(self.io.in_flight() <= budget, "{} ops", self.io.in_flight());
    }

    fn turns(&mut self, n: usize) {
        (0..n).for_each(|_| self.turn());
    }

    fn events(&mut self) -> Vec<Event> {
        core::mem::take(&mut self.events)
    }

    fn connections(&self) -> u32 {
        self.conns.stats().connections
    }
}

impl Net {
    fn serve(&mut self, port: &mut TestPort, turn: usize) {
        let mut requests = port.requests();
        while let Some(mut r) = requests.next() {
            let handle = r.handle();
            match r.op() {
                Op::Stream => {
                    let Some(next) = self.streams.pop_front() else {
                        continue;
                    };
                    self.asked.push(turn);
                    let peer = RawAddr::from(PEER.parse::<SocketAddr>().expect("an address"));
                    r.buf()[..RawAddr::LEN].copy_from_slice(peer.as_bytes());
                    r.done(next.map(|h| h.get() as usize));
                }
                Op::Recv => {
                    let Some(peer) = self.peers.get_mut(&handle) else {
                        continue;
                    };
                    let n = peer.outbound.len().min(r.buf().len());
                    if peer.fail_recv {
                        r.done(Err(IoError::Refused));
                    } else if n > 0 {
                        r.buf()[..n].copy_from_slice(&peer.outbound[..n]);
                        peer.outbound.drain(..n);
                        r.done(Ok(n));
                    } else if peer.eof {
                        r.done(Ok(0));
                    }
                }
                Op::Send => {
                    let Some(peer) = self.peers.get_mut(&handle).filter(|p| !p.hang) else {
                        continue;
                    };
                    if peer.fail_send {
                        r.done(Err(IoError::Refused));
                        continue;
                    }
                    let n = r.buf().len().min(peer.max_send.unwrap_or(usize::MAX));
                    peer.received.extend_from_slice(&r.buf()[..n]);
                    r.done(Ok(n));
                }
                Op::Close => {
                    self.closed.push(handle);
                    r.done(Ok(0));
                }
                _ => {}
            }
        }
    }
}

fn peer_addr() -> Option<SocketAddr> {
    PEER.parse().ok()
}

#[test]
fn test_accept_sends_the_seed_then_reads_and_broadcasts() {
    let mut r = rig(listen(2));
    let peer = r.accept();
    assert_eq!(r.events(), [Event::Opened(peer_addr())]);
    assert_eq!(r.peer(peer).received, SEED);
    r.peer(peer).outbound = [frame(b"ping"), frame(b"pong")].concat();
    r.turns(2);
    assert_eq!(r.bodies, [b"ping".to_vec(), b"pong".to_vec()]);
    r.conns.broadcast(b"one");
    r.turn();
    r.conns.broadcast(b"two");
    r.turn();
    assert_eq!(r.peer(peer).received, b"seedonetwo");
    let stats = Stats {
        connections: 1,
        bytes_out: 10,
        batches_dropped: 0,
    };
    assert_eq!(r.conns.stats(), stats);
}

#[test]
fn test_a_body_split_over_reads_arrives_whole() {
    let mut r = rig(listen(1));
    let peer = r.accept();
    let bytes = frame(b"split body");
    r.peer(peer).outbound = bytes[..5].to_vec();
    r.turns(2);
    assert!(r.bodies.is_empty());
    r.peer(peer).outbound = bytes[5..].to_vec();
    r.turns(2);
    assert_eq!(r.bodies, [b"split body".to_vec()]);
}

#[test]
fn test_each_connection_gets_the_seed() {
    let mut r = rig(listen(2));
    let (first, second) = (r.connect(), r.connect());
    r.turns(6);
    assert_eq!(r.connections(), 2);
    assert_eq!(r.peer(first).received, SEED);
    assert_eq!(r.peer(second).received, SEED);
}

#[test]
fn test_a_full_table_closes_the_extra_stream() {
    let mut r = rig(listen(1));
    let (_, extra) = (r.connect(), r.connect());
    r.turns(5);
    assert_eq!(r.events(), [Event::Opened(peer_addr()), Event::Refused]);
    assert_eq!(r.net.closed, [extra]);
    assert_eq!(r.connections(), 1);
}

#[test]
fn test_a_partial_send_resubmits_the_rest() {
    let mut r = rig(listen(1));
    let peer = r.connect();
    r.peer(peer).max_send = Some(3);
    r.turns(5);
    assert_eq!(r.peer(peer).received, SEED);
    r.conns.broadcast(b"abcdefg");
    r.turns(3);
    assert_eq!(r.peer(peer).received, b"seedabcdefg");
    assert_eq!(r.conns.stats().bytes_out, 11);
}

#[test]
fn test_a_batch_past_the_room_left_is_dropped() {
    let mut r = rig(listen(1));
    let peer = r.accept();
    r.peer(peer).hang = true;
    r.conns.broadcast(&[1; CAP]);
    r.conns.broadcast(&[2; CAP]);
    r.conns.broadcast(&[3; 1]);
    assert_eq!(r.conns.stats().batches_dropped, 1, "one sends, one fills");
}

#[test]
fn test_a_failed_recv_frees_the_slot_once_its_ops_return() {
    let mut r = rig(listen(1));
    let peer = r.accept();
    r.peer(peer).hang = true;
    r.conns.broadcast(b"stuck");
    r.peer(peer).fail_recv = true;
    r.turn();
    assert_eq!(r.events(), [Event::Opened(peer_addr()), Event::Closed]);
    assert!(!r.conns.slots[0].is_free(), "the send is still out");
    let next = r.connect();
    r.turns(3);
    assert_eq!(r.net.closed, [peer]);
    assert_eq!(r.events(), [Event::Opened(peer_addr())]);
    assert_eq!(r.peer(next).received, SEED, "the slot starts clean");
}

#[test]
fn test_a_failed_send_frees_the_slot_once_the_recv_returns() {
    let mut r = rig(listen(1));
    let peer = r.accept();
    r.peer(peer).fail_send = true;
    r.conns.broadcast(b"lost");
    r.turn();
    assert_eq!(r.connections(), 0);
    assert!(!r.conns.slots[0].is_free(), "the recv is still out");
    r.turns(2);
    assert!(r.conns.slots[0].is_free());
    assert_eq!(r.net.closed, [peer]);
}

#[test]
fn test_a_send_of_nothing_closes() {
    let mut r = rig(listen(1));
    let peer = r.accept();
    r.peer(peer).max_send = Some(0);
    r.conns.broadcast(b"x");
    r.turn();
    assert_eq!(r.connections(), 0);
}

#[test]
fn test_churn_reuses_every_slot() {
    let mut r = rig(listen(2));
    for _ in 0..20 {
        let (a, b) = (r.connect(), r.connect());
        r.turns(6);
        assert_eq!(r.connections(), 2);
        r.peer(a).eof = true;
        r.peer(b).fail_recv = true;
        r.turns(3);
        assert_eq!(r.connections(), 0);
    }
    assert_eq!(r.net.closed.len(), 40);
    assert!(r.conns.slots.iter().all(Conn::is_free));
}

#[test]
fn test_hang_up_closes_and_listens_on() {
    let mut r = rig(listen(1));
    let peer = r.accept();
    r.peer(peer).eof = true;
    r.turns(2);
    assert_eq!(r.connections(), 0);
    r.connect();
    r.turns(3);
    assert_eq!(r.connections(), 1, "no backoff on a listener");
}

#[test]
fn test_oversized_body_closes() {
    let mut r = rig(listen(1));
    let peer = r.accept();
    r.peer(peer).outbound = (CAP as u32).to_le_bytes().to_vec();
    r.turns(2);
    assert_eq!(r.connections(), 0);
    assert!(r.bodies.is_empty());
}

#[test]
fn test_a_stuck_send_times_out() {
    let mut r = rig(listen(1));
    let peer = r.accept();
    r.peer(peer).hang = true;
    r.conns.broadcast(b"x");
    r.turns(4);
    assert_eq!(r.connections(), 1);
    r.turns(3);
    assert_eq!(r.connections(), 0, "past the 50 ms write timeout");
    assert_eq!(r.net.closed, [peer]);
    assert!(r.events().ends_with(&[Event::Closed]));
    r.turn();
    assert!(r.conns.slots[0].is_free(), "the send was canceled");
}

#[test]
fn test_a_slow_send_that_progresses_never_times_out() {
    let mut r = rig(listen(1));
    let peer = r.connect();
    r.peer(peer).max_send = Some(1);
    r.turns(6);
    r.conns.broadcast(&[7; 20]);
    r.turns(30);
    assert_eq!(r.connections(), 1);
    assert_eq!(r.peer(peer).received.len(), SEED.len() + 20);
}

#[test]
fn test_dial_redials_after_backoff_when_the_connection_is_lost() {
    let mut r = rig(dial());
    let first = r.accept();
    assert_eq!(r.events(), [Event::Opened(peer_addr())]);
    r.peer(first).eof = true;
    r.turns(2);
    assert_eq!(r.events(), [Event::Closed]);
    r.connect();
    r.turns(45);
    assert_eq!(r.connections(), 0, "waiting out 500 ms");
    r.turns(10);
    assert_eq!(r.connections(), 1);
}

#[test]
fn test_a_dialer_holds_one_connection() {
    let mut r = rig(ConnsConfig { slots: 4, ..dial() });
    r.connect();
    r.connect();
    r.turns(10);
    assert_eq!(r.conns.slots.len(), 1);
    assert_eq!(r.net.streams.len(), 1, "no dial while the slot is taken");
}

#[test]
fn test_repeated_dial_failures_back_off_then_reset() {
    let mut r = rig(dial());
    for _ in 0..3 {
        r.net.streams.push_back(Err(IoError::Refused));
    }
    let peer = r.connect();
    r.turns(400);
    let gaps: Vec<usize> = r.net.asked.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(gaps, [51, 101, 201], "500 ms, 1 s, 2 s, a turn each to ask");
    assert_eq!(r.connections(), 1);
    r.peer(peer).eof = true;
    r.connect();
    r.turns(60);
    let last = r.net.asked.len() - 1;
    let gap = r.net.asked[last] - r.net.asked[last - 1];
    assert!(gap < 100, "reset to 500 ms, not 4 s: {gap}");
}

#[test]
fn test_a_dialers_backoff_doubles_to_ten_seconds() {
    let (ep, _io) = cap_with_ops::<Endpoint>(16);
    let mut backoff = Backoff::new(true);
    let mut delays = Vec::new();
    for _ in 0..7 {
        delays.push(backoff.delay.as_millis());
        backoff.start();
        assert!(backoff.waiting(&ep));
        backoff.wait = None;
    }
    assert_eq!(delays, [500, 1_000, 2_000, 4_000, 8_000, 10_000, 10_000]);
}

#[test]
fn test_a_listener_backs_off_a_fixed_500_ms() {
    let mut r = rig(listen(1));
    for _ in 0..3 {
        r.net.streams.push_back(Err(IoError::Refused));
    }
    r.connect();
    r.turns(200);
    let gaps: Vec<usize> = r.net.asked.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(gaps, [51, 51, 51]);
    let failed = Event::Failed(IoError::Refused);
    assert_eq!(
        r.events(),
        [failed, failed, failed, Event::Opened(peer_addr())]
    );
}

#[test]
fn test_reject_a_budget_short_of_the_slots() {
    let (ep, io) = cap_with_ops::<Endpoint>(ops_for(2) as u16 - 1);
    let made = make(&io, &ep, listen(2), SEED);
    let (needed, budget) = (ops_for(2), ops_for(2) - 1);
    assert!(
        matches!(made, Err(MakeError::IoOps { needed: n, budget: b }) if n == needed && b == budget)
    );
}

#[test]
fn test_reject_a_seed_past_conn_cap() {
    let (ep, io) = cap_with_ops::<Endpoint>(ops_for(1) as u16);
    let made = make(&io, &ep, listen(1), &[0; CAP + 1]);
    assert!(matches!(
        made,
        Err(MakeError::SeedTooLarge {
            seed: 65,
            conn_cap: CAP
        })
    ));
}
