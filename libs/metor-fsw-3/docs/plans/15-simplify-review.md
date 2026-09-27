# metor-fsw-3 simplification review

## Decisions (2026-09-27)

- **D1:** approved.
- **D2:** keep the cross-process ring features (`wake.rs`, mmap, owner reclaim, `create_raw`). Take every other ring simplification in §3 and §4.
- **D3:** approved.
- **D4:** approved, where the compiler allows it.
- **D5:** reversed. Unit tests must not do IO, so keep the IO in integration tests.
  - Where an integration test duplicates a unit test that does IO (the TCP tests in `link/conn.rs`), delete the unit test.
  - Keep the `tests/link.rs` and `tests/deploy.rs` tests, but pull their scaffolding into `tests/common`.
  - Still delete `tests/pipeline.rs`, which does no IO and is covered by unit tests.
  - Still dedupe the two redial tests in `link.rs`.
- **D6:** approved.
- **§2–§5:** approved. Leave `DESIGN.md` alone.

Read-only review of the whole crate: `src/`, `ring/`, `macros/`, `tests/`, `python/`, and the docs.
Line counts are estimates. Every claim that something is dead was checked by grepping `libs/` and `examples/`.

Baseline (tracked files): `src` 22.5k, `ring` 4.6k, `macros` 1.3k, `tests` 4.6k, `python` 2.4k, `docs/plans` 7.7k.

| Bucket | Est. lines saved |
|---|---|
| Docs (completed plans, `REVIEW.md`) | ~7,750 |
| Golden `deploy.json` trim | ~2,400 (data) |
| Code: low risk | ~3,300 |
| Code: needs a decision (medium) | ~1,300 |

In total, about 4.5k of the roughly 35k lines of code (~13%) can go. The test suites account for about 60% of that.

---

## 1. Decisions for you

These are the big savings, but each one changes scope or coverage.

### D1. Delete `docs/plans/01–13` and `REVIEW.md` (~7,750)
All the slices have landed and the reviews are closed. Git keeps the history. Before deleting, move or fix these four references:
- `macros/src/system_attr.rs:44` and `tests/ui/system_on_impl.stderr` send users to `docs/plans/11-fn-systems.md`. Drop that clause and re-bless the `.stderr`.
- `DESIGN.md:99` points at `05-links.md` for the held-back shared-state design. Fold in a 3–5 line summary.
- `python/metor-config/pyproject.toml:1` cites `04-packs.md` in a comment. Drop it.
- `04-packs-review-fixes.md` T7 (no steady-state logging allocations) is still deferred and is **not** in `examples/adcs-fsw3/TODO.md`. Move it there.

Keep `14-wasm.md`, which is in progress.

### D2. Ring cross-process features (~580 dead, ~830 total dead API)
Nothing outside the ring uses these. Packs attach in-process with `attach_owned`.
- `ring/src/wake.rs` (futex / wait-on-address) plus the `libc` dep: ~242
- Dead-process reclaim (`reclaim_owner`, `owner_tag`, `ReaderSlot.owner`, and its tests and loom models): ~200. This changes the region format and needs a `VERSION` bump.
- The `mmap` feature (`create_mmap`/`attach_mmap`, the `memmap2` dep, 3 tests): ~140
- Async `View::read` plus 5 tests: ~145. Keep `WakeSink::wait_until`, which `link/subscribe.rs:124` uses.
- `create_raw` + 2 tests (~64) and `config_of` + its test (~45).

The question is whether cross-process rings are still on the roadmap. If they are, keep `wake`, mmap and reclaim together and cut only `read`, `config_of` and `try_read_into` (~250).

Watch `create_raw`: `libs/metor-expr/prelude/src/pack.rs` calls it on the **fsw-2** ring, so a wasm guest on the fsw-3 ring (`14-wasm.md`) would probably need it too.

### D3. Replace `tests/golden/deploy.json` (2,621 lines)
About 1,850 of its lines are baked peer `PortDef` schemas, and `src/deploy/tests.rs` already checks that resolution. There are two options:
- Strip `def` before comparing, which leaves a golden of about 200 lines.
- Replace the golden with targeted asserts.

Either way you lose the byte-for-byte pin on build output. There is no bless tool today.

### D4. Make port alignment a compile-time check (~100)
`T::ALIGN` is a const, yet `Output::new`/`Input::new` return `Result`, and about 60 production sites `.expect("supported alignment")`.
- Use `const { assert!(T::ALIGN <= PAYLOAD_ALIGNMENT) }` and make the constructors infallible.
- Delete `UnsupportedAlignment`.
- Move the reject test to trybuild.
- Keep `build.rs::check_alignment`, which checks pack and peer `PortDef` data.

This also removes ~60 panics, which the style guide wants gone.

### D5. Integration tests that duplicate unit tests (~350)
- **`tests/link.rs`:**
  - `test_oversized_packet_closes_connection`, `test_disconnect_frees_slot` and `test_refuse_peer_past_limit` repeat unit tests in `src/link/conn.rs:321-391`. Each spins up a whole TCP target.
  - `test_reconnect_checks_the_new_peer` is covered by `link.rs:767` and `inbox.rs:314`.
  - `link.rs:432` and `:1022` both test redialing a dropped dial.
- **`tests/deploy.rs:338` `test_peer_announcing_another_table_is_faulted`:** ~190 lines counting its `Impostor`/`Source`/`Until` scaffolding. It reaches the same `peer_schema_mismatch` fault as `link.rs:767`.
- **`tests/pipeline.rs` (220):** it does no IO, so it is really a unit test. `run.rs` `test_run_stop` and `fn_system/tests.rs::test_pipeline_matches_trait` already cover it, and its `AfterCycles` is a copy of the one in `src/tests/utils.rs`.

Suggestion: keep one TCP smoke test per behavior class and delete the rest.

### D6. `#[derive(SystemInputs/SystemOutputs)]` is only used by tests (~160)
Every production and example system is a `#[system]` fn. The derive can go if these move to `#[system]`:
- the test systems in `src/tests/utils.rs`
- the tests in `system.rs`
- `pipeline.rs`

---

## 2. Changes that pay off across the crate (low risk, ~600)

| Change | Where | Saves |
|---|---|---|
| `InputConfig::new(port, system, output)`, mirroring `PortRef::new` | ~48 four-line literals in `build.rs`, `fn_system/tests`, `sequence/tests`, `thread/tests`, `tests/*.rs`. `thread/tests.rs::reading` and `tests/link.rs::reading` are identical, and `adapter/tests.rs:262` already has a private `input()` | ~130 |
| `tests::utils::{ring::<T>(depth, readers), pair::<T>(), fan_in::<T>()}` | `create_in_memory(Config{ring_capacity(..)})` + `try_new(..).expect(..)` appears 32×. `port/mod.rs::stamped_pair` is `message_pair_two::<Stamped>` word for word. `ring<R>()` is copied into 7 test modules | ~200 |
| `tests/common/mod.rs` for integration tests | Copied helpers: `fixture()` (dl, slot), `Ping` (3 copies), `Imu`+`Source` (link, deploy), `dial()` (3 copies), `metor()`/`Running`/`free_ports` (deploy, sequences, run). Also add `poll_until(deadline, f)` (13 hand-rolled loops), `within(fut)` and `dial_fsw(addr)` (11 `let … else panic`) | ~200 |
| `LogEvent::line(now, level, msg, fields)` | 5 copies of the 9-field literal with `source:"", span/file/line: None` (`thread/mod.rs` ×2, `log.rs` ×2, one test) | ~45 |
| One fallible closure for table registrations | ~37 × `table\n.register(..)\n.expect(..)` in `tests/utils.rs`, `sequence/tests.rs`, `adapter/tests.rs` and `cli/config.rs` | ~65 |

---

## 3. Production-code simplifications (low risk)

### Core (`port/`, `coordinator/`, `fn_system/`, `system.rs`, `thread/`)
- **`fn_system/param.rs:572-705`:** rustfmt spreads 17 tuple-impl macro calls over 134 lines. One recursive macro that peels a list (`A 0, B 1, …`), or a `#[rustfmt::skip]` block, fixes it. ~115
- **`fn_system/set.rs:72-101` + `def.rs:46-69`:** `input_counts`/`output_counts` build a fake `DefCx` to recompute defs just to get per-parameter counts. Only one dynamic parameter can bind, so its count is `bindings.len() - declared.len()`. Delete `of_inputs`/`of_outputs`. ~50 (med)
- **`fn_system/param.rs:50-158`:**
  - `Views` forwards `next`/`take`/`is_empty` to `Bindings`.
  - The crate-private `Defs` has four `push_*` methods plus getters. Use `pub(crate)` fields and one push. ~35
- **`coordinator/table.rs`:**
  - `same_record` duplicates `PortDef::same_record`.
  - `TableEntry::def_for` just forwards to `def_fn`.
  - The local `static_def` duplicates `tests::utils::static_def`. ~20 prod, plus tests below
- **`coordinator/build.rs`:**
  - `check_port_names` is two identical loops.
  - `resolve_edges` repeats the lookup in the `instance_def` closure, and `from_port` exists only because that closure lacks the port.
  - `allocate_rings` and `RingSpec::new` are one-line wrappers. ~30
- **Placement errors defined twice:** `ParamError::{Thread, Channel, SlotChannel}` map 1:1 onto `BuildError::{ThreadOnCyclic, …}`. Separately, `ParamError::Decode` is used for failures that are not decode errors (`thread/mod.rs:89,362`, `group.rs:127,169`). ~20 (med, changes error shape)
- **`FnSystem<S>` / `FnAsyncSystem<S>`:** the same `PhantomData` type twice. Merge them, and keep a single `LOG_PORT` (defined in both `set.rs:18` and `thread/mod.rs:102`). ~15
- **`SystemDef::new_async`:** it is `new` with a different bound. Make `new` generic over `W: WakeSink`. ~10
- **Dead or test-only items:**
  - Unused: `Records::get` and `Coordinator::entry_names`.
  - Only called from tests: `StopHandle::is_set`, `Latest::read` (already covered by `Deref`), and `Default` for `DynInputs`/`DynOutputs`.
  - `Coordinator.threads` has `#[allow(dead_code)]` and holds only `Weak`s.
  - ~50 in total.
- **`thread/mod.rs:200-216` `take_drops`:** two identical loops. ~10

### Links, packs, deploy
- **`pack/raw.rs:76-97` `RawPort`:** it has the same layout as `RawSlice` and re-implements `as_slice`. Use `RawSlice::of(&rings)`. The ABI layout does not change. ~35 (unsafe contract, low-med)
- **`PublishParams`, `SubscribeParams`, `DbParams`:** each copies its fields into a second struct through `new()`. Store the params struct directly. ~25
- **`LinkStatus` / `DbStatus`:**
  - Derive `Default` instead of writing `new`.
  - `changed` becomes `Self { timestamp: o.timestamp, ..*self } != *o`.
  - ~20
- **`dl/mod.rs` and `pack/mod.rs`:** each side of the ABI repeats its export-buffer code twice. Add `call_export(..)` on the host side and `out_buf(dst, cap)` on the guest side. ~18
- **`deploy/slot.rs::SlotCx`:** mirrors `pack::def::DefCxOwned`. ~15
- **`deploy/`:**
  - `envelope.targets[at.target].coordinator.systems[at.system]` appears about 7 times. Add `At::get` and `At::get_mut`.
  - `build_error` is used once and hand-inlined 3 more times.
  - ~20
- **Serde defaults written twice:** each is both a `const` (used only by tests) and a `fn` that returns it (`DEFAULT_CONN_CAP`, `MAX_CONNECTIONS`, `DEFAULT_INBOUND_CAP`). ~9
- **Small items:**
  - `Publish` hand-inlines the `report()` logic that subscribe factors out.
  - `db.rs:261-271` has an unreachable `Sink::Refused` arm.
  - `dl::Pack::{systems, library}` are redundant accessors.
  - `PackFns` is re-exported but has no outside users.
  - ~20

### Sequences, channel, log, record, cli
- **`channel/records.rs` + `log.rs:15-36`:** four hand-written postcard `Record` impls that differ only in their consts. Add a `PostcardRecord` trait with a blanket impl (preferred over a macro). ~40 (med, needs a coherence check)
- **`sequence/seq.rs`:**
  - `Sleep` duplicates `WaitFor`'s poll logic; wrap it instead.
  - Trivial private getters: `Clock::now/aborted`, `Node::name`, `Redline::node`.
  - ~25
- **`log.rs`:**
  - `LogScope::poll` and `LogPort::fault` repeat `with_log_port`'s unsafe slot swap. Route both through it so the unsafe code lives in one place.
  - `layer()` only returns the unit struct `LogLayer`.
  - `write` and `emit` both set the timestamp.
  - ~28
- **`channel/adapter/mod.rs`:**
  - The `Outer` struct and `split` are just three `pop()`s used once.
  - `def()` re-spells the three port defs that `ports()` already lists.
  - ~23
- **`record.rs`:**
  - `pub mod bytes` holds two identity fns with a single caller.
  - The `Msg` arm of `PartialEq` can compare tuples.
  - ~22
- **`cli/`:**
  - `release: bool` has one caller, which passes `false`.
  - `Cargo.toml` is parsed twice (`cargo_package`, `cargo_lib`).
  - Toml `.get()` calls are hand-chained.
  - `run_children` and `supervise` can merge.
  - Three `PackDevError::Write` mappers do the same thing.
  - ~34

### Ring and macros
- **Ring errors:** 5 hand-written `Display`/`Error` impls (~70 lines) whose messages repeat the variant docs. Use `thiserror`, which the main crate already depends on. `ReadError` has a single variant. ~45
- **`try_read_into`:** only tests call it, in 46 places, each a two-line assert pattern. ~40
- **`try_latest`:** return `Option<&[u8]>` directly and drop `ReadGrant.release_at`. ~15
- **`Backing`:** `base()`/`len()` getters on a `pub(super)` struct. `cfg(test)`-only imports and `reader_count` live in production `lib.rs`. ~22
- **`verify.rs`:** paired harnesses share their setup, and the symbolic data-fill loop is duplicated. Merge them; every proof stays. ~35
- **Verification docs:** merge `KANI.md`, `LOOM.md` and `MIRI.md` into one `VERIFY.md`. ~30
- **macros:**
  - `system_attr.rs:421-528`: `sync_call`, `async_call` and `sequence_call` each re-destructure `CallParts` and recompute `state_ty()` 4× per expansion. Use one `call_impl` that matches on kind. ~35
  - `system_inputs`/`system_outputs` are wrappers that differ only by `Dir`. ~15
  - `frame.rs:93-107` builds `Input::Struct`, then destructures it with `unreachable!`. Reordering the construction removes a panic.

---

## 4. Tautological and duplicate tests (~1,200)

**Tests of constants, derives, serde or the type system:**
- **`fn_system/tests.rs`:**
  - `test_ports_consts`, `test_attribute_type_and_ports`, `test_execute_doc_comment` and `test_generic_system_two_records` only check `NAME`/`NAMES`/`DOC`. Line 571 compares `DOC` to itself.
  - `test_default_constructors` only asserts `cycle()==1`.
  - Line 346 checks `Nav::ID == def.id`.
- **`frame.rs` test module:** fully duplicates `record.rs`. ~27
- **`record.rs`:**
  - `test_postcard_schema_id` is the third test of `Msg::ID`.
  - `test_raw_bytes_passthrough` tests identity fns.
  - `ID == ComponentId::new(NAME)` checks the trait default.
- **`channel/tests.rs`:**
  - A postcard round trip of a foreign struct.
  - `size_of == 8+4+4+64+32`, which the `IntoBytes` derive already guarantees.
  - `test_oversized_event_refused` repeats `record.rs`.
- **`port/mod.rs`:**
  - `test_shared_write_round_trips` and `test_closure_and_caller_write_in_order` only restate that `write` takes `&self`.
  - Line 804 asserts `Fixed.timestamp()==None`.
- **`pack/def.rs`:**
  - `test_empty_table_serialization` and `test_truncated_descriptor` test serde_json itself.
  - `test_system_registration_order` hard-codes 16 fixture names.
- **`link/wire.rs`:** `test_announce_frame_and_message` and `test_msg_streams_request_each_id` rebuild the expected bytes with the same calls as the code under test.
- **Smaller ones:**
  - `link/mod.rs::test_fsw_registers_builtins` checks that `format!("{BUILTIN}.…")` starts with `BUILTIN`.
  - `transport.rs::test_decode_connect_address` tests the serde derive.
  - `dl/store.rs` tests blake3 and `?` propagation.
  - `cli/cargo.rs::test_host_artifact_names`.
  - `cli/config.rs::test_hosts_default_empty`.
  - `inbox.rs::test_announce_storage_is_bounded`.
- **Ring:**
  - `test_writer_is_send`.
  - `test_config_reports_geometry`.
  - `test_shared_borrows_*`, which restate `&self`.
- **Python:** `test_deployment.py:251 test_est_record_is_a_frame` asserts a class attribute of the stub.

**Duplicates:**
- **`port/mod.rs` async tests:**
  - The `DynInputs` pair (`test_any_ready_wakes_per_port`, `*_cancel_and_retry`) repeats the `Input` pair, because `DynInputs::poll_ready` only calls `Input::poll_ready`.
  - Waker/Context setup is repeated 5×.
  - ~55
- **Single tests covered elsewhere:**
  - `fn_system/tests.rs::test_param_error_reports_system` and `test_reject_multiple_dynamic_producers` repeat `build.rs:789` and `:848`.
  - `thread/group.rs::test_write_wakes_executor` repeats `port/mod.rs::test_write_wakes_port_across_threads`.
- **`system.rs` tests:**
  - `HandIn` exists only to compare its defs against `DerivedIn`.
  - `test_manual_bundle_binding` overlaps `test_bind_field_order`.
  - `test_unit_bundles_empty` tests `()`.
- **`coordinator/`:**
  - `Twice` is defined in both `tests.rs` and `run.rs`.
  - The `Coordinator { … }` literal is built twice.
  - `table.rs::test_register_replaces_type` is covered by `test_replacement_preserves_order`.
- **`sequence/`:**
  - `seq.rs::Ran` is the same type as `tests.rs::Trace`, and `ms()`/`at()` are duplicated.
  - The def is built 3× across `step_of`, `step_over` and `step_on_imu`.
  - `redline.rs::test_unnamed_redline` repeats `tests.rs::test_unnamed_redline_fault`.
- **`tests/slot.rs`:**
  - `test_unknown_load_keeps_occupant` and `test_reset_restarts_loaded` repeat tests in `adapter/tests.rs`.
  - `test_stale_baked_ports_refused` and `test_other_type_refused` hit the same path.
  - The tempdir setup is repeated 7×.
- **`tests/sequences.rs:228` `test_build_stores_candidates`:** duplicates `slot.rs:460`. See also §5.
- **`tests/pack_dev.rs:20`:** its assertion is repeated inside `:83`.
- **`tests/ui/pass/system_{nav,generic,relay}.rs` (~150):** in-crate tests cover the same expansion via `extern crate self`. `system_reexport.rs` keeps the out-of-crate case covered.
- **`macros/src/system_attr/tests.rs`:**
  - About 10 reject cases have trybuild twins that also check the message and span.
  - `tokens()` is just `to_string()`.
- **`ring/src/tests.rs`:**
  - `test_raw_attach_swap_reacquire`, `test_writer_claim_freed_on_drop` and `test_latest_pin_backpressures_writer` are covered by other tests.
  - The seven attach-rejection tests should become one table-driven test.
  - Three tests are word-for-word copies of the loom models; share each body so Miri still runs it.
  - About 15 section-banner comments can go.
- **Python:**
  - Five classes repeat the same setUp/tearDown.
  - About 12 deployment tests rebuild the same `Target` + plant + pub fixture.
  - `_record_names` and `_candidate_names` duplicate each other.
  - `Copy` can be a frozen dataclass.
- **`src/tests/utils.rs`:**
  - `Recorder` has four private `push_*` wrappers.
  - `AfterCycles` can be a `poll_fn`.
  - `ControlLaw`/`StatusWatch` can become `#[system]` fns.
  - Six `Imu { .. }` literals should use `Imu::new`.

---

## 5. Things found along the way (not about line count)

- **Bug:** `python/metor-config/tests/test_target.py:453` has `if __name__ == "__main__": unittest.main()` in the middle of the file. Running the file directly exits before `slotted()` / `ChannelTest` are defined, so those tests don't run. Move it to the end.
- **Test hygiene:** `tests/sequences.rs::test_build_stores_candidates` writes into the fixture's own `.metor/store` instead of a tempdir. 17 stale dylibs have piled up there (gitignored).
- **`ring/.claude/settings.local.json`:** a stray local settings file.
- **`DESIGN.md` is stale:**
  - It links the ring to `../metor-fsw-2/ring`.
  - Its `System` trait shape, `system(nav).with_state(..)` and `InputConfig { system, port }` examples no longer exist.
  - The pyproject paths point at fsw-2.
  - It describes the ABI as one symbol; there are actually 6 exports.
  - The `FrameList`/`Yoke` dynamic frames are unimplemented.
  - It still calls slot 7 upcoming.

  Fix this rather than cut it. It is worth doing together with D1.
- **Comments:**
  - `coordinator/mod.rs:1-5` uses mannered prose ("meat and potatoes").
  - `system.rs:87` has a truncated doc sentence.
  - `build.rs:1-4` has typos.

---

## Suggested order

1. **D1 + DESIGN.md fixes + the Python `__main__` bug.** Zero risk, and the biggest line count.
2. **§2 crate-wide helpers.** They shrink everything that follows.
3. **§4 test deletions and merges.** Mechanical, one commit per module.
4. **§3 production simplifications,** by area.
5. **D2–D6,** each as a separate decision.
