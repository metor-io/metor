# Links: review

Range `ce41d4c8..b550955a` (17 commits, slice 4). Validated by reading every
file in the diff plus stellarator's `recv_growable`/`WaitQueue::wait_for`/`run`
and fsw-2's `set_announces`/`prefix_vtable`; `cargo test -p metor-fsw-3`,
`cargo clippy --all-targets`, and the 24 Python tests pass.

## Findings

1. **High: a peer picks the size of the link's receive buffer.**
   `src/link/conn.rs:205-215` (`read_half`) reads with `PacketStream::next_grow`,
   whose `recv_growable` (`stellarator/src/io.rs:118-119`) does
   `buf.grow(len + 4)` for whatever `u32` length prefix arrives, and
   `Vec::grow` is `resize(new_len, 0)`. Scenario: any client sends the four
   bytes `ff ff ff ff`; the link thread zero-fills a 4 GiB `Vec`. On a flight
   box that is an OOM abort of the whole process, from either link, and
   `Publish` does not even want inbound bytes. Fix: wrap the buffer in a type
   implementing `GrowableBuf` that refuses to grow past a cap (say the
   largest routed `max_len + PACKET_OVERHEAD` for `Subscribe`, `RECV_BUF` for
   `Publish`); `try_slice` then fails with `BufferOverflow`, `read_half`
   returns, and the connection closes.

2. **High: a link never notices a connection closing unless traffic wakes it.**
   `src/link/publish.rs:95-98`, `src/link/subscribe.rs:106-109`: `prune()` and
   the `has_free() -> want()` re-arm run only after an `Event`, and nothing in
   `serve` (`conn.rs:172-181`) wakes the loop when a socket ends. Scenario A:
   listening `Subscribe`, `max_connections` peers connect and disconnect
   without sending a routed id; the last iteration saw `has_free() == false`
   and never called `want()`; with no open connection there is no
   `Inbound` event, so the source stays parked (`transport.rs:185-190`) and
   the target accepts no commands until stop. Scenario B: dialing `Subscribe`
   whose `Publish` peer restarts; the announce packets are unmatched ids that
   `accept` ignores without stirring (`subscribe.rs:240`), so the slot is never
   pruned and it never redials. `Publish` survives only because telemetry
   supplies `Ready` events. Fix: give `Connections` a `WaitQueue` that `serve`
   wakes after `outbox.close()`, race it as a fourth `Event::Closed` in both
   `next()`s (which also lets `prune` run only then).

3. **Medium: `Output<Bytes>::write_bytes` skips the port's bound.**
   `src/port.rs:117-121` calls `try_write` without `bounded`, and
   `Inbox::accept` (`subscribe.rs:252`) bounds only against the largest routed
   port. Scenario: `Subscribe([Ping, Note])` (8 and 64 bytes); a peer sends a
   `Ping`-id message of 60 bytes; it lands on `cmds.ping`, whose ring and the
   consumer's mirror were sized for 8-byte records, and `Thread::execute` copies
   it on. The Kani-proved `bounded` invariant does not hold for dynamic
   outputs. Fix: store `max_len` from the binding's `PortDef` in
   `Output<Bytes>` (or keep it beside the id in `routes`) and apply `bounded`.

4. **Medium: a constructor that panics takes its whole thread group down.**
   `src/thread/group.rs:119` calls `member.launch.launch(..)` inside the
   group's root future with no unwind guard; maitake has no `catch_unwind`
   and `stellarator::run` propagates, so the thread dies and
   `finished.store(true)` (`group.rs:77`) never runs. Scenario: a
   `Publish::new` that panics (or any user `Ctor`). Effects: the `add` waiter
   gets "thread stopped before it built the system", the panic message lost;
   every sibling already on the group loses its task without its `panicked`
   slot being set, so its adapter reports healthy while its mirrors drop;
   `Drop for GroupHandle` spins the full `JOIN_TIMEOUT` before detaching. Fix:
   `catch_unwind(AssertUnwindSafe(|| launch(..)))` mapping the payload to
   `ParamError::Decode(panic_message(..))`, and replace `finished` with
   `JoinHandle::is_finished()`.

5. **Medium: accept errors spin.** `src/link/transport.rs:194-207`: an
   `accept()` error yields `None`, `continue` re-enters `wait_for` whose
   predicate is already true, and `accept` is retried immediately, forever,
   with no log. Scenario: `EMFILE`/`ENFILE` on the host. Confirmed CPU spin
   on the link thread; whether sibling tasks starve depends on the reactor
   turning between completions (unconfirmed; a test with an fd limit would
   show it). Fix: on error, `or(sleep(BACKOFF_INITIAL), stop.wait())` and one
   `log`/`tracing` line, as the dialer does.

6. **Low: steady-state allocation on the cycle thread while a mirror drops.**
   `src/thread/mod.rs:161-191` allocates a `Vec` and one `String` per port,
   plus `write_line`'s `message.to_string()`, every cycle the counts are
   nonzero; `tests/link.rs:540-584` shows drops persisting for thousands of
   cycles under a simulated clock. Same class as the deferred T7, but new
   code. Fix: keep a fixed `fields` `Vec` and a small stack formatter, or
   rate-limit the line to once per N cycles. Related: `batch_cap`
   (`publish.rs:159-163`) reserves `BATCH_DEPTH = 8` cycles per port while a
   mirror holds `MIRROR_FACTOR * ring_depth = 32`; a stall longer than eight
   cycles regrows `batch` (the "never outgrows" test writes one record per
   cycle). Reserve from the mirror's capacity or `pending_cap`.

7. **Low: two dynamic outputs on one record id.** `subscribe.rs:80,162`:
   `outputs: [{port: a, record: ping}, {port: b, record: ping}]` builds (names
   differ), announces the id twice in `command_ids`, and `deliver` routes only
   to the first. Fix: reject a duplicate wire id in `routes` with a fault, or
   fan out.

8. **Low: wall stamps under a simulated clock.** `publish.rs:99`,
   `subscribe.rs:111` stamp `link_status` with `Timestamp::now()`, and
   `fn_system/mod.rs:86` / `log.rs` `LogScope` stamp async log lines the same
   way, while every other record carries the cycle's time. A sim run's
   telemetry has two timelines. Fix: carry the cycle stamp across in the
   mirror (the adapter knows `now`) or document it.

9. **Low: `publish.rs` and `subscribe.rs` duplicate the loop.** The `Event`
   enum, `next()`, and the `prune / has_free / want / status` tail are the
   same code twice; `PublishParams`/`SubscribeParams` repeat four fields.
   Fix: one `LinkLoop` helper in `conn.rs` taking the per-link event future
   and handler, and a shared `LinkParams` flattened into both.

10. **Low: a `Mutex` lock per cycle.** `thread/mod.rs:200` locks `panicked`
    every `execute` for a value set at most once. Fix: an `AtomicBool` beside
    the slot, checked first.

11. **Low: an undocumented panic.** `thread/mod.rs:291-297`:
    `create_in_memory` panics when `capacity * MIRROR_FACTOR` overflows the
    region bound that `allocate_ring` checked for the real ring. Fix: use
    `checked_region_len` and return `ParamError`, or add the `PANIC Safety`
    line with the bound argument.

12. **Low: docs claim what the code does not do.** `docs/plans/05-links.md`:
    "Thread adapter" still describes `Thread<S: AsyncSystem>` owning "the
    thread ... a stop flag and the join handle", detecting a panic when "the
    join handle completes", and joining on drop; the code has a non-generic
    `Thread`, a shared `GroupHandle`, and a `Panicked` slot, and only the last
    adapter on a group joins. "Thread placement" says a pack's async system
    "ignores `thread`", contradicting "Build changes" and `pack/mod.rs:173`.
    "Python" says a `Publish` handle exposes `pub.plant.imu`; `SystemHandle`
    resolves only `_outputs`. "Tests" promises a counting-allocator assert and
    Kani proofs for the pending buffer and `MIRROR_FACTOR`; neither exists.
    "Transport" shows `Listen { addr }` without `max_connections`. Fix the
    text.

13. **Low: component ids diverge from fsw-2 when a port is not named after
    its frame.** `wire.rs:129-189` roots leaves at `{ns}.{producer}.{port}`;
    fsw-2 (`core/src/registry.rs:85`, `init.rs:333`) roots at
    `{ns}.{instance}.{frame}`. For `nav.est` carrying frame `nav_est` the panel
    sees `cube_sat.nav.est.x`, not `cube_sat.nav.nav_est.x`, so saved panel
    layouts keyed by fsw-2 names miss. The design chose this; confirm it is
    intended and say so in "Wire names".

14. **Low: Python accepts arguments it ignores.** `_builtins.py:100-105`
    drops `max_connections` silently for `connect=`, and `Subscribe([Ping,
    Ping])` only fails at build as `DuplicateOutput`. Fix: raise `ConfigError`
    in the builder for both.

## Not findings

- `wire::announce` bytes match `set_announces` packet for packet: `LinkInfo`
  first, `VTableMsg` then one `SetComponentMetadata` per component per table,
  `SetMsgMetadata` with `schema.name` and empty metadata for postcard; only
  the table/msg interleaving order differs, which no consumer parses.
- `reroot` rewrites exactly the eight-byte `Op::Data` leaf blobs
  `prefix_vtable` rewrites; msg ids use `Msg::ID` for postcard and
  `msg_id(NAME)` otherwise, as designed.
- Reader accounting: the adapter's view on a real input ring is the one slot
  `count_readers` counted for that edge; each mirror has `max_readers: 1` and
  one view; the real output ring's single writer goes to the adapter.
- `Thread::execute` does not block, read errors stop the copy loop quietly,
  and `scratch` is sized from the completed `cx.def` (dynamic ports included).
- Drop order: adapter-side mirror handles drop before the `Arc<GroupHandle>`;
  the task's own handles keep the mirror backing alive; a cancelled task
  holds no writer claim the cycle side needs.
- `Incoming::want`/`source` handshake cannot lose a wake: `wait_for`
  subscribes before testing the predicate, and `connected` is polled before
  `ready` in `next()`, so a parked stream is always taken next poll.
- `Outbox`: whole-batch-or-nothing against `cap`, two `cap`-sized buffers per
  connection after the first swap, no allocation per batch afterwards.
- `Inbox` slots never grow (`accept` checks before `extend`), `inbound_cap = 0`
  drops everything without indexing.
- `Dialer` honors stop during connect and backoff; `GroupHandle::add` is only
  called from the cycle thread, so it cannot self-deadlock; a group thread
  that exited before a job is pushed drops the `report` sender and `add`
  returns an error rather than hanging.
- Nothing `Rc` or `!Send` crosses to the group thread: `AsyncLaunch` carries
  `Arc<M>`, an owned `Value`, and ring handles; `Running` is built on the
  group thread and stays there.
- Dynamic ports: fan-in rejected, unknown/conflicting record names rejected,
  `status` reserved after completion, `link_status`/`log` collisions caught by
  `check_port_names`; `ParamError::Thread` crosses a pack as JSON and maps to
  `ThreadOnCyclic`.
- ABI 4 is pinned in `pack/mod.rs`, `pack/tests.rs`, `metor-fsw-abi`,
  `_config.py` (Rust-tested), and the golden `echo_pack.py`.
- `_finalize` is idempotent; `all=True` lists only earlier systems; the golden
  exercises listen transports, a handle plus a port, and `thread`.
