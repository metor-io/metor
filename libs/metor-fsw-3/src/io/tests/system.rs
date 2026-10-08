//! `io_submit` and `io_complete` over a scripted `Os`.

use core::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;

use crate::coordinator::{InputConfig, OutputConfig, SystemConfig, SystemTable};
use crate::io::system::{complete_entry, submit_entry_with};
use crate::io::*;

/// How the test `Os` answers one op.
pub(crate) type Answer = Box<dyn Fn(&Sqe) -> Option<Cqe>>;

/// What the test `Os` saw, and how it answers.
#[derive(Default)]
pub(crate) struct Script {
    pub(crate) pushed: Vec<Sqe>,
    pub(crate) registered: Vec<FdClass>,
    pub(crate) dials: Vec<RawAddr>,
    pub(crate) tables: Option<Tables>,
    /// Answers each pushed op on the next reap, or never with `None`.
    pub(crate) answer: Option<Answer>,
    landed: Vec<Cqe>,
}

thread_local! {
    static SCRIPT: RefCell<Rc<RefCell<Script>>> = RefCell::default();
}

/// The script the next test `Os` built on this thread uses.
pub(crate) fn script() -> Rc<RefCell<Script>> {
    let fresh = Rc::new(RefCell::new(Script::default()));
    SCRIPT.with(|s| *s.borrow_mut() = fresh.clone());
    fresh
}

struct TestOs(Rc<RefCell<Script>>);

fn test_os(_limits: &Limits, tables: Tables, dials: &[RawAddr]) -> Result<Box<dyn Os>, OsError> {
    let script = SCRIPT.with(|s| s.borrow().clone());
    script.borrow_mut().dials = dials.to_vec();
    script.borrow_mut().tables = Some(tables);
    Ok(Box::new(TestOs(script)))
}

impl Os for TestOs {
    fn push(&mut self, sqe: Sqe) {
        let mut s = self.0.borrow_mut();
        s.pushed.push(sqe);
        if let Some(cqe) = s.answer.as_ref().and_then(|answer| answer(&sqe)) {
            s.landed.push(cqe);
        }
    }

    fn submit(&mut self) {}

    fn reap(&mut self, done: &mut Vec<Cqe>) {
        done.append(&mut self.0.borrow_mut().landed);
    }

    fn register(&mut self, _fd: OwnedFd, class: FdClass) -> RawHandle {
        let mut s = self.0.borrow_mut();
        s.registered.push(class);
        RawHandle::new((s.dials.len() + s.registered.len() - 1) as u32, 0)
    }
}

/// Adds the io steps to `table`, over the test `Os`.
pub(crate) fn io_steps(table: &mut SystemTable) {
    table
        .insert(IO_SUBMIT_TYPE, submit_entry_with(test_os))
        .unwrap();
    table.insert(IO_COMPLETE_TYPE, complete_entry()).unwrap();
}

/// `systems` with the io steps appended, wired to every system with grants.
pub(crate) fn with_io(systems: Vec<SystemConfig>) -> Vec<SystemConfig> {
    let granted: Vec<String> = systems
        .iter()
        .filter(|s| !s.grants.is_empty())
        .map(|s| s.id.clone())
        .collect();
    with_io_for(systems, &granted)
}

/// `systems` with the io steps appended, wired to the drivers `granted`.
pub(crate) fn with_io_for(mut systems: Vec<SystemConfig>, granted: &[String]) -> Vec<SystemConfig> {
    for system in systems.iter_mut().filter(|s| granted.contains(&s.id)) {
        let cq = InputConfig::new(IO_CQ, "io_complete", system.id.as_str());
        system.inputs.push(cq);
    }
    systems.push(SystemConfig {
        inputs: granted
            .iter()
            .map(|id| InputConfig::new(id.as_str(), id.as_str(), IO_SQ))
            .collect(),
        ..SystemConfig::new("io_submit", IO_SUBMIT_TYPE)
    });
    systems.push(SystemConfig {
        outputs: granted
            .iter()
            .map(|id| OutputConfig::record(id.as_str(), "io_cqe"))
            .collect(),
        ..SystemConfig::new("io_complete", IO_COMPLETE_TYPE)
    });
    systems
}

mod cases {
    use core::cell::RefCell;
    use std::rc::Rc;

    use metor_proto::types::Timestamp;
    use metor_proto_wkt::{SequenceCommand, SequenceCommandKind};

    use super::{Script, io_steps, script, with_io};
    use crate::coordinator::{
        Adapter, BuildError, ChannelConfig, CoordinatorConfig, InputConfig, RunState, SystemConfig,
        SystemTable,
    };
    use crate::io::status::Counters;
    use crate::io::system::due;
    use crate::io::*;
    use crate::port::{Input, Output};
    use crate::{Coordinator, system};

    type Results = Rc<RefCell<Vec<(Timestamp, Result<usize, IoError>)>>>;

    /// Sends `abc` whenever its buffer is idle, recording each result. With
    /// `quit`, it closes its cap first.
    struct Sender {
        tx: Slot<Send>,
        results: Results,
        quit: bool,
        closing: Option<Pending<Close>>,
    }

    #[system]
    fn sender(#[state] s: &mut Sender, now: Timestamp, udp: &Cap<Udp>) {
        if core::mem::take(&mut s.quit) {
            s.closing = Some(udp.close());
        }
        if let Some(result) = s.tx.poll() {
            s.results.borrow_mut().push((now, result));
        }
        if let Some(buf) = s.tx.take_idle() {
            s.tx = Slot::Busy(udp.send(buf));
        }
    }

    /// Sends once and receives once, on the first cycle.
    struct Echo {
        tx: Slot<Send>,
        rx: Slot<Recv>,
        started: bool,
    }

    #[system]
    fn echo(#[state] s: &mut Echo, udp: &Cap<Udp>) {
        let _ = (s.tx.poll(), s.rx.poll());
        if !s.started {
            s.started = true;
            let (tx, rx) = (s.tx.take_idle(), s.rx.take_idle());
            if let (Some(tx), Some(rx)) = (tx, rx) {
                s.tx = Slot::Busy(udp.send(tx));
                s.rx = Slot::Busy(udp.recv(rx));
            }
        }
    }

    type Accepted = Rc<RefCell<Vec<Result<RawHandle, IoError>>>>;

    /// Accepts whenever no accept is pending; closes each stream at once if
    /// `close` is set.
    struct Acceptor {
        pending: Option<Pending<Accept>>,
        streams: Vec<Handle<Stream>>,
        closing: Vec<Pending<Close>>,
        close: bool,
        seen: Accepted,
    }

    #[system]
    fn acceptor(#[state] s: &mut Acceptor, listen: &Cap<Listen>) {
        if let Some(result) = s.pending.as_mut().and_then(Pending::poll) {
            s.pending = None;
            s.seen
                .borrow_mut()
                .push(result.as_ref().map(Handle::raw).map_err(|e| *e));
            if let Ok(stream) = result {
                match s.close {
                    true => s.closing.push(stream.close()),
                    false => s.streams.push(stream),
                }
            }
        }
        if s.pending.is_none() {
            s.pending = Some(listen.accept());
        }
    }

    /// Two caps; sends on `b` only.
    struct Pair {
        tx: Slot<Send>,
    }

    #[system]
    fn pair(#[state] s: &mut Pair, a: &Cap<Udp>, b: &Cap<Udp>) {
        let _ = (a, s.tx.poll());
        if let Some(buf) = s.tx.take_idle() {
            s.tx = Slot::Busy(b.send(buf));
        }
    }

    #[system]
    fn status_probe(
        #[state] seen: &mut Rc<RefCell<Vec<ResourceStatus>>>,
        st: &mut Input<ResourceStatus>,
    ) {
        seen.borrow_mut().extend(st.drain().flatten().copied());
    }

    #[system]
    fn channel_probe(
        #[state] seen: &mut Rc<RefCell<Vec<u32>>>,
        st: &mut Input<crate::channel::ChannelStatus>,
    ) {
        seen.borrow_mut()
            .extend(st.drain().flatten().map(|s| s.state));
    }

    /// Starts a recv and cancels it at once.
    struct Canceller {
        rx: Slot<Recv>,
    }

    #[system]
    fn canceller(#[state] s: &mut Canceller, udp: &Cap<Udp>) {
        if let Some(buf) = s.rx.take_idle() {
            let rx = udp.recv(buf);
            rx.cancel();
            s.rx = Slot::Busy(rx);
        }
    }

    #[system]
    fn cmds(
        #[state] queue: &mut Rc<RefCell<Vec<SequenceCommand>>>,
        commands: &mut Output<SequenceCommand>,
    ) {
        for command in queue.borrow_mut().drain(..) {
            let _ = commands.write(&command);
        }
    }

    fn udp(id: &str) -> ResourceConfig {
        let bind = "127.0.0.1:0".into();
        ResourceConfig {
            id: id.into(),
            backing: Backing::Real(RealKind::Udp {
                bind,
                connect: None,
            }),
        }
    }

    fn listen(id: &str) -> ResourceConfig {
        let addr = "127.0.0.1:0".into();
        ResourceConfig {
            id: id.into(),
            backing: Backing::Real(RealKind::TcpListen { addr }),
        }
    }

    fn granted(id: &str, ty: &str, grants: &[(&str, &str)]) -> SystemConfig {
        let grant = |&(param, resource): &(&str, &str)| Grant {
            param: param.into(),
            resource: resource.into(),
        };
        SystemConfig {
            grants: grants.iter().map(grant).collect(),
            ..SystemConfig::new(id, ty)
        }
    }

    /// Everything the systems recorded.
    #[derive(Default)]
    struct Seen {
        results: Results,
        accepted: Accepted,
        status: Rc<RefCell<Vec<ResourceStatus>>>,
        queue: Rc<RefCell<Vec<SequenceCommand>>>,
        states: Rc<RefCell<Vec<u32>>>,
        /// The next sender asks for this many bytes, or `abc` when zero.
        ctor_len: Rc<core::cell::Cell<usize>>,
        /// The next sender closes its cap first.
        quit: Rc<core::cell::Cell<bool>>,
    }

    fn table(seen: &Seen, close: bool) -> SystemTable {
        let mut table = SystemTable::new();
        let (results, len, quit) = (
            seen.results.clone(),
            seen.ctor_len.clone(),
            seen.quit.clone(),
        );
        let make_sender = move |io: &IoInit<'_>| Sender {
            tx: Slot::Idle(match len.get() {
                0 => io.alloc_from(b"abc"),
                n => io.alloc(n),
            }),
            results: results.clone(),
            quit: quit.get(),
            closing: None,
        };
        table.register("sender", sender(), make_sender).unwrap();
        let make_echo = |io: &IoInit<'_>| Echo {
            tx: Slot::Idle(io.alloc_from(b"abc")),
            rx: Slot::Idle(io.alloc(16)),
            started: false,
        };
        table.register("echo", echo(), make_echo).unwrap();
        let accepted = seen.accepted.clone();
        let make_acceptor = move || Acceptor {
            pending: None,
            streams: Vec::new(),
            closing: Vec::new(),
            close,
            seen: accepted.clone(),
        };
        table
            .register("acceptor", acceptor(), make_acceptor)
            .unwrap();
        let make_pair = |io: &IoInit<'_>| Pair {
            tx: Slot::Idle(io.alloc_from(b"x")),
        };
        table.register("pair", pair(), make_pair).unwrap();
        let status = seen.status.clone();
        table
            .register("status_probe", status_probe(), move || status.clone())
            .unwrap();
        let queue = seen.queue.clone();
        table
            .register("cmds", cmds(), move || queue.clone())
            .unwrap();
        let states = seen.states.clone();
        table
            .register("channel_probe", channel_probe(), move || states.clone())
            .unwrap();
        let make_canceller = |io: &IoInit<'_>| Canceller {
            rx: Slot::Idle(io.alloc(8)),
        };
        table
            .register("canceller", canceller(), make_canceller)
            .unwrap();
        io_steps(&mut table);
        table
    }

    fn build(
        seen: &Seen,
        systems: Vec<SystemConfig>,
        resources: Vec<ResourceConfig>,
    ) -> Result<Coordinator, BuildError> {
        let mut systems = with_io(systems);
        systems.push(SystemConfig {
            inputs: vec![InputConfig::new("st", "io_complete", IO_STATUS_PORT)],
            ..SystemConfig::new("probe", "status_probe")
        });
        let config = CoordinatorConfig {
            systems,
            resources,
            ..CoordinatorConfig::default()
        };
        config.build(&table(seen, false))
    }

    fn answer(script: &Rc<RefCell<Script>>, f: impl Fn(&Sqe) -> Option<Cqe> + 'static) {
        script.borrow_mut().answer = Some(Box::new(f));
    }

    fn pushed(script: &Rc<RefCell<Script>>) -> Vec<Sqe> {
        script.borrow().pushed.clone()
    }

    fn steps(c: &mut Coordinator, from: i64, to: i64) {
        for t in from..to {
            c.step(Timestamp(t));
        }
    }

    #[test]
    fn ops_reach_the_os_with_their_resources_handles() {
        let os = script();
        answer(&os, |sqe| Some(Cqe::ok(sqe.token, Out::len(3))));
        let seen = Seen::default();
        let systems = vec![
            granted("one", "sender", &[("udp", "r1")]),
            granted("two", "sender", &[("udp", "r2")]),
        ];
        let mut c = build(&seen, systems, vec![udp("r1"), udp("r2")]).unwrap();
        c.step(Timestamp(1));
        let ops = pushed(&os);
        assert_eq!(ops.len(), 2);
        assert_eq!(
            (ops[0].handle, ops[1].handle),
            (RawHandle::new(0, 0), RawHandle::new(1, 0))
        );
        assert_eq!(
            os.borrow().registered,
            [FdClass::Datagram, FdClass::Datagram]
        );
        assert!(
            seen.results.borrow().is_empty(),
            "no result in the submitting cycle"
        );
        c.step(Timestamp(2));
        let results = seen.results.borrow();
        assert_eq!(
            results.as_slice(),
            [(Timestamp(2), Ok(3)), (Timestamp(2), Ok(3))]
        );
    }

    #[test]
    fn accepted_stream_is_a_derived_handle() {
        let os = script();
        answer(&os, |sqe| match sqe.op {
            Op::Accept { .. } => Some(Cqe::ok(sqe.token, Out::handle(RawHandle::new(77, 3)))),
            _ => Some(Cqe::ok(sqe.token, Out::UNIT)),
        });
        let seen = Seen::default();
        let systems = vec![granted("srv", "acceptor", &[("listen", "l")])];
        let config = CoordinatorConfig {
            systems: with_io(systems),
            resources: vec![listen("l")],
            ..CoordinatorConfig::default()
        };
        let mut c = config.build(&table(&seen, true)).unwrap();
        steps(&mut c, 1, 4);
        let first = seen.accepted.borrow()[0];
        assert_eq!(
            first,
            Ok(RawHandle::new(1, 1)),
            "the slot past the one grant"
        );
        let close = pushed(&os).into_iter().find(|s| s.op == Op::CLOSE).unwrap();
        assert_eq!(close.handle, RawHandle::new(77, 3));
    }

    #[test]
    fn grants_map_by_param_not_config_order() {
        let os = script();
        let seen = Seen::default();
        let systems = vec![granted("p", "pair", &[("b", "rb"), ("a", "ra")])];
        let mut c = build(&seen, systems, vec![udp("ra"), udp("rb")]).unwrap();
        c.step(Timestamp(1));
        assert_eq!(
            pushed(&os)[0].handle,
            RawHandle::new(1, 0),
            "`b` is granted `rb`"
        );
    }

    #[test]
    fn tables_freeze_and_idle_systems_cost_nothing() {
        let os = script();
        let seen = Seen::default();
        let systems = vec![granted("srv", "acceptor", &[("listen", "l")])];
        let mut c = build(&seen, systems, vec![listen("l")]).unwrap();
        let tables = os.borrow().tables.clone().unwrap();
        assert!(tables.get(TableId(0)).unwrap().arena().is_some());
        c.step(Timestamp(1));
        assert_eq!(pushed(&os).len(), 1, "only the accept");
        assert!(seen.status.borrow().is_empty() || seen.status.borrow()[0].completed == 0);
    }

    #[test]
    fn status_counts_ops_and_bytes() {
        let os = script();
        answer(&os, |sqe| match sqe.op {
            Op::Send { .. } => Some(Cqe::ok(sqe.token, Out::len(3))),
            Op::Recv { .. } => Some(Cqe::err(sqe.token, IoError::Os(Errno::ECONNREFUSED))),
            _ => None,
        });
        let seen = Seen::default();
        let systems = vec![granted("e", "echo", &[("udp", "u")])];
        let mut c = build(&seen, systems, vec![udp("u"), udp("idle")]).unwrap();
        steps(&mut c, 1, 4);
        let status = seen.status.borrow();
        assert_eq!(status.len(), 1, "the idle resource writes nothing");
        let s = status[0];
        assert_eq!(s.resource.to_str().unwrap(), "u");
        assert_eq!((s.submitted, s.completed, s.failed), (2, 2, 1));
        assert_eq!((s.bytes_out, s.bytes_in), (3, 0));
    }

    #[test]
    fn status_writes_at_most_sixteen_a_cycle() {
        let counters = vec![
            Counters {
                submitted: 1,
                ..Counters::default()
            };
            20
        ];
        let mut reported = vec![Counters::default(); 20];
        let first: Vec<_> = due(&counters, &reported, 0).collect();
        assert_eq!(first, (0..16).collect::<Vec<_>>());
        first.iter().for_each(|&at| reported[at] = counters[at]);
        let second: Vec<_> = due(&counters, &reported, 16).collect();
        assert_eq!(second, [16, 17, 18, 19]);
    }

    #[test]
    fn long_names_are_cut() {
        let long = "r".repeat(80);
        let name = crate::io::status::resource_name(&long);
        assert_eq!(name.to_str().unwrap().len(), 63);
    }

    #[test]
    fn dial_targets_reach_the_os_constructor() {
        let os = script();
        let seen = Seen::default();
        let dial = ResourceConfig {
            id: "ground".into(),
            backing: Backing::Real(RealKind::TcpConnect {
                addr: "127.0.0.1:2240".into(),
            }),
        };
        let systems = vec![granted("one", "sender", &[("udp", "u")])];
        let _c = build(&seen, systems, vec![udp("u"), dial]).unwrap();
        assert!(
            os.borrow().dials.is_empty(),
            "an ungranted dial is not opened"
        );
        assert_eq!(os.borrow().registered.len(), 1);
    }

    #[test]
    fn closed_derived_handle_is_stale() {
        let os = script();
        answer(&os, |sqe| match sqe.op {
            Op::Accept { .. } => Some(Cqe::ok(sqe.token, Out::handle(RawHandle::new(50, 0)))),
            _ => Some(Cqe::ok(sqe.token, Out::UNIT)),
        });
        let seen = Seen::default();
        let systems = vec![granted("srv", "acceptor", &[("listen", "l")])];
        let config = CoordinatorConfig {
            systems: with_io(systems),
            resources: vec![listen("l")],
            ..CoordinatorConfig::default()
        };
        let mut c = config.build(&table(&seen, true)).unwrap();
        steps(&mut c, 1, 6);
        let closes = pushed(&os).iter().filter(|s| s.op == Op::CLOSE).count();
        let accepts = seen.accepted.borrow().len();
        assert_eq!(closes, accepts, "each derived close reaches the Os once");
    }

    #[test]
    fn full_handle_map_refuses_and_closes() {
        let os = script();
        let next = Rc::new(RefCell::new(100));
        let n = next.clone();
        answer(&os, move |sqe| {
            *n.borrow_mut() += 1;
            let raw = RawHandle::new(*n.borrow(), 0);
            matches!(sqe.op, Op::Accept { .. }).then(|| Cqe::ok(sqe.token, Out::handle(raw)))
        });
        let seen = Seen::default();
        let systems = vec![granted("srv", "acceptor", &[("listen", "l")])];
        let mut c = build(&seen, systems, vec![listen("l")]).unwrap();
        steps(&mut c, 1, 2 + MAX_OPS as i64 + 1);
        let accepted = seen.accepted.borrow();
        assert_eq!(accepted.iter().filter(|r| r.is_ok()).count(), MAX_OPS);
        assert_eq!(accepted[MAX_OPS], Err(IoError::Refused));
        assert!(pushed(&os).iter().any(|s| s.op == Op::CLOSE));
    }

    fn command(kind: SequenceCommandKind) -> SequenceCommand {
        SequenceCommand {
            channel: "tx".into(),
            command: kind,
        }
    }

    /// A sender behind channel `tx`, fed by `cmds`.
    fn channeled(seen: &Seen) -> Coordinator {
        let systems = vec![
            SystemConfig::new("cmds", "cmds"),
            SystemConfig {
                inputs: vec![InputConfig::new("commands", "cmds", "commands")],
                adapter: Some(Adapter::Channel(ChannelConfig {
                    name: "tx".into(),
                    start: RunState::Running,
                })),
                ..granted("tx", "sender", &[("udp", "u")])
            },
            SystemConfig {
                inputs: vec![InputConfig::new("st", "tx", "channel_status")],
                ..SystemConfig::new("states", "channel_probe")
            },
        ];
        build(seen, systems, vec![udp("u")]).unwrap()
    }

    const QUIESCING: u32 = crate::channel::ChannelState::Quiescing as u32;
    const FAILED: u32 = crate::channel::ChannelState::Failed as u32;

    fn push(seen: &Seen, kind: SequenceCommandKind) {
        seen.queue.borrow_mut().push(command(kind));
    }

    fn last_state(seen: &Seen) -> u32 {
        *seen.states.borrow().last().unwrap()
    }

    #[test]
    fn reset_waits_for_the_op_in_flight() {
        let os = script();
        let seen = Seen::default();
        let mut c = channeled(&seen);
        c.step(Timestamp(1));
        let send = pushed(&os)[0];
        seen.queue
            .borrow_mut()
            .push(command(SequenceCommandKind::Stop));
        seen.queue
            .borrow_mut()
            .push(command(SequenceCommandKind::Reset));
        steps(&mut c, 2, 5);
        let cancel = pushed(&os)
            .into_iter()
            .find(|s| s.op.kind() == OpKind::Cancel)
            .unwrap();
        assert_eq!(cancel.op, Op::cancel(send.token));
        seen.queue
            .borrow_mut()
            .push(command(SequenceCommandKind::Start));
        steps(&mut c, 5, 7);
        assert_eq!(
            pushed(&os).len(),
            2,
            "no remake, so no new send, while quiescing"
        );

        // The send's cancel lands; the canceled send never reaches the new system.
        os.borrow_mut()
            .landed
            .push(Cqe::err(send.token, IoError::Canceled));
        os.borrow_mut()
            .landed
            .push(Cqe::ok(cancel.token, Out::UNIT));
        steps(&mut c, 7, 9);
        seen.queue
            .borrow_mut()
            .push(command(SequenceCommandKind::Start));
        steps(&mut c, 9, 12);
        let sends = pushed(&os)
            .iter()
            .filter(|s| s.op.kind() == OpKind::Send)
            .count();
        assert_eq!(sends, 2, "the remade system sends again");
        assert!(
            seen.results.borrow().is_empty(),
            "the canceled send reached no one"
        );
    }

    #[test]
    fn unanswered_cancel_keeps_the_system_quiescing() {
        let os = script();
        let seen = Seen::default();
        let mut c = channeled(&seen);
        c.step(Timestamp(1));
        seen.queue
            .borrow_mut()
            .push(command(SequenceCommandKind::Stop));
        seen.queue
            .borrow_mut()
            .push(command(SequenceCommandKind::Reset));
        steps(&mut c, 2, 30);
        let sends = pushed(&os)
            .iter()
            .filter(|s| s.op.kind() == OpKind::Send)
            .count();
        assert_eq!(sends, 1, "never remade while its op is in flight");
    }

    #[test]
    fn io_steps_take_no_channel_or_thread() {
        let seen = Seen::default();
        let mut systems = with_io(vec![granted("one", "sender", &[("udp", "u")])]);
        let submit = systems.iter_mut().find(|s| s.id == "io_submit").unwrap();
        submit.adapter = Some(Adapter::Channel(ChannelConfig {
            name: "io".into(),
            start: RunState::Running,
        }));
        let config = CoordinatorConfig {
            systems,
            resources: vec![udp("u")],
            ..CoordinatorConfig::default()
        };
        let err = config.build(&table(&seen, false)).err();
        assert_eq!(
            err,
            Some(BuildError::IoAdapter {
                id: "io_submit".into()
            })
        );

        let mut systems = with_io(vec![granted("one", "sender", &[("udp", "u")])]);
        let submit = systems.iter_mut().find(|s| s.id == "io_submit").unwrap();
        submit.adapter = Some(Adapter::Thread("io".into()));
        let config = CoordinatorConfig {
            systems,
            resources: vec![udp("u")],
            ..CoordinatorConfig::default()
        };
        assert!(matches!(
            config.build(&table(&seen, false)),
            Err(BuildError::Params { .. })
        ));
    }

    #[test]
    fn misplaced_io_steps_fail() {
        let seen = Seen::default();
        let fails = |systems: Vec<SystemConfig>| {
            let config = CoordinatorConfig {
                systems,
                resources: vec![udp("u"), udp("v")],
                ..CoordinatorConfig::default()
            };
            matches!(
                config.build(&table(&seen, false)),
                Err(BuildError::IoSystems { .. })
            )
        };
        let one = || granted("one", "sender", &[("udp", "u")]);
        let two = || granted("two", "sender", &[("udp", "v")]);
        assert!(fails(vec![one()]), "no io steps");
        let mut late = with_io(vec![one()]);
        late.push(two());
        assert!(fails(late), "a granted system after io_submit");
        let mut swapped = with_io(vec![one(), two()]);
        let complete = swapped.iter_mut().find(|s| s.id == "io_complete").unwrap();
        complete.outputs.reverse();
        assert!(fails(swapped), "io_complete outputs in another order");
    }

    #[test]
    fn steady_cycles_do_not_allocate() {
        let os = script();
        answer(&os, |sqe| Some(Cqe::ok(sqe.token, Out::len(3))));
        os.borrow_mut().pushed.reserve(4096);
        os.borrow_mut().landed.reserve(16);
        let seen = Seen::default();
        seen.results.borrow_mut().reserve(4096);
        let systems = vec![granted("one", "sender", &[("udp", "u")])];
        let mut c = build(&seen, systems, vec![udp("u")]).unwrap();
        steps(&mut c, 1, 10);
        seen.status.borrow_mut().reserve(4096);
        let count = super::super::allocations(|| steps(&mut c, 10, 1010));
        assert_eq!(count, 0);
    }

    #[test]
    fn quiescing_refuses_start_and_reports_its_state() {
        let os = script();
        let seen = Seen::default();
        let mut c = channeled(&seen);
        c.step(Timestamp(1));
        push(&seen, SequenceCommandKind::Stop);
        push(&seen, SequenceCommandKind::Reset);
        steps(&mut c, 2, 4);
        assert_eq!(last_state(&seen), QUIESCING);
        push(&seen, SequenceCommandKind::Start);
        steps(&mut c, 4, 30);
        let states = seen.states.borrow();
        assert!(
            states[states.len() - 26..].iter().all(|&s| s == QUIESCING),
            "{states:?}"
        );
        assert_eq!(
            pushed(&os)
                .iter()
                .filter(|s| s.op.kind() == OpKind::Send)
                .count(),
            1
        );
    }

    #[test]
    fn load_on_a_system_that_loads_nothing_acts_at_once() {
        let os = script();
        let seen = Seen::default();
        let mut c = channeled(&seen);
        c.step(Timestamp(1));
        let send = pushed(&os)[0];
        push(&seen, SequenceCommandKind::Stop);
        push(
            &seen,
            SequenceCommandKind::Load {
                name: "other".into(),
            },
        );
        steps(&mut c, 2, 4);
        assert!(
            pushed(&os).iter().all(|s| s.op.kind() != OpKind::Cancel),
            "no quiesce"
        );
        assert_ne!(last_state(&seen), QUIESCING);
        os.borrow_mut()
            .landed
            .push(Cqe::ok(send.token, Out::len(3)));
        push(&seen, SequenceCommandKind::Start);
        steps(&mut c, 4, 7);
        assert_eq!(
            seen.results.borrow().first().map(|r| r.1),
            Some(Ok(3)),
            "its op still lands"
        );
    }

    #[test]
    fn closed_grant_lives_again_after_a_reset() {
        let os = script();
        answer(&os, |sqe| Some(Cqe::ok(sqe.token, Out::len(3))));
        let seen = Seen::default();
        seen.quit.set(true);
        let mut c = channeled(&seen);
        steps(&mut c, 1, 3);
        assert_eq!(
            seen.results.borrow()[0].1,
            Err(IoError::Stale),
            "closed, then sent"
        );
        assert!(
            pushed(&os).is_empty(),
            "a grant's close never reaches the Os"
        );
        seen.quit.set(false);
        push(&seen, SequenceCommandKind::Stop);
        push(&seen, SequenceCommandKind::Reset);
        steps(&mut c, 3, 6);
        push(&seen, SequenceCommandKind::Start);
        steps(&mut c, 6, 9);
        let sends: Vec<_> = pushed(&os)
            .into_iter()
            .filter(|s| s.op.kind() == OpKind::Send)
            .collect();
        assert!(!sends.is_empty(), "the remade system's grant is live");
        assert_eq!(sends[0].handle, RawHandle::new(0, 0));
    }

    #[test]
    fn handle_opened_while_quiescing_is_closed() {
        let os = script();
        let seen = Seen::default();
        let systems = vec![
            SystemConfig::new("cmds", "cmds"),
            SystemConfig {
                inputs: vec![InputConfig::new("commands", "cmds", "commands")],
                adapter: Some(Adapter::Channel(ChannelConfig {
                    name: "tx".into(),
                    start: RunState::Running,
                })),
                ..granted("tx", "acceptor", &[("listen", "l")])
            },
        ];
        let mut c = build(&seen, systems, vec![listen("l")]).unwrap();
        c.step(Timestamp(1));
        let accept = pushed(&os)[0];
        push(&seen, SequenceCommandKind::Stop);
        push(&seen, SequenceCommandKind::Reset);
        steps(&mut c, 2, 4);
        let opened = RawHandle::new(88, 0);
        os.borrow_mut()
            .landed
            .push(Cqe::ok(accept.token, Out::handle(opened)));
        steps(&mut c, 4, 6);
        let close = pushed(&os).into_iter().find(|s| s.op == Op::CLOSE);
        assert_eq!(close.map(|s| s.handle), Some(opened));
    }

    #[test]
    fn remake_larger_than_the_arena_fails() {
        let os = script();
        answer(&os, |sqe| Some(Cqe::ok(sqe.token, Out::len(3))));
        let seen = Seen::default();
        let mut c = channeled(&seen);
        c.step(Timestamp(1));
        seen.ctor_len.set(4096);
        push(&seen, SequenceCommandKind::Stop);
        push(&seen, SequenceCommandKind::Reset);
        steps(&mut c, 2, 6);
        assert_eq!(last_state(&seen), FAILED);
    }

    #[test]
    fn wiring_checks_sources_not_just_names() {
        let seen = Seen::default();
        let fails = |systems: Vec<SystemConfig>| {
            let config = CoordinatorConfig {
                systems,
                resources: vec![udp("u"), udp("v")],
                ..CoordinatorConfig::default()
            };
            matches!(
                config.build(&table(&seen, false)),
                Err(BuildError::IoSystems { .. })
            )
        };
        let one = || granted("one", "sender", &[("udp", "u")]);
        let two = || granted("two", "sender", &[("udp", "v")]);
        let mut crossed = with_io(vec![one(), two()]);
        let submit = crossed.iter_mut().find(|s| s.id == "io_submit").unwrap();
        submit.inputs[0].from[0].system = "two".into();
        assert!(fails(crossed), "io_submit's `one` reads `two.io_sq`");
        let mut deaf = with_io(vec![one()]);
        deaf[0].inputs.clear();
        assert!(fails(deaf), "no io_cq edge");
    }

    #[test]
    fn untracked_cancels_are_not_counted() {
        let os = script();
        let seen = Seen::default();
        let systems = vec![granted("c", "canceller", &[("udp", "u")])];
        let mut c = build(&seen, systems, vec![udp("u")]).unwrap();
        steps(&mut c, 1, 3);
        assert_eq!(pushed(&os).len(), 2, "the recv and its cancel");
        let last = *seen.status.borrow().last().unwrap();
        assert_eq!((last.submitted, last.completed), (1, 0));
    }

    #[test]
    fn bytes_out_counts_what_was_written() {
        let os = script();
        answer(&os, |sqe| match sqe.op {
            Op::Send { .. } => Some(Cqe::ok(sqe.token, Out::len(2))),
            _ => None,
        });
        let seen = Seen::default();
        let systems = vec![granted("e", "echo", &[("udp", "u")])];
        let mut c = build(&seen, systems, vec![udp("u")]).unwrap();
        steps(&mut c, 1, 3);
        assert_eq!(
            seen.status.borrow().last().unwrap().bytes_out,
            2,
            "a short write"
        );
    }
}
