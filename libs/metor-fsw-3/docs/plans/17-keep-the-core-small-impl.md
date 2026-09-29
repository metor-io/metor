# Keeping the core small: implementation plan

Implements `17-keep-the-core-small.md`. Five tasks, in order. Each ends
with:

- `cargo test -p metor-fsw-3 --no-fail-fast`;
- `cargo clippy --all-targets` clean;
- from T2 on, the `python/metor-config/tests` suite;
- one commit.

T1 also builds and tests `examples/adcs-fsw3`, whose tests import from
`cli`.

Order: T1 moves types out of `cli` with no behaviour change, so T2 and T3
have homes for what they use. T2 and T3 type the two `deploy/` passes. T4
counts failed writes. T5 rewrites DESIGN.md over the result. An
antagonistic review follows T5.

No task adds a re-export or alias at an old path. Callers move.

## T1. `cli` depends on the runtime

Files: `src/cli/{config,cargo,run,pack_dev,module,build}.rs`,
`src/deploy/{mod,index,slot,tests}.rs`, new `src/deploy/config.rs`,
`src/dl/{mod,slot,store}.rs`, new `src/dl/pack_ref.rs`,
`tests/{slot,run,module,pack_dev}.rs`, `tests/common/mod.rs`,
`examples/adcs-fsw3/tests/convergence.rs`.

1. `deploy/config.rs` takes `DeploymentConfig`, `TargetConfig`,
   `ConfigError`, and `CONFIG_VERSION` from `cli/config.rs`, with their
   tests. `deploy` re-exports them at `deploy::`.
2. `dl/pack_ref.rs` takes `PackKind`, `PackRef` (with `dylib`, `artifact`,
   and `open`), and from `cli/cargo.rs` `triple`, `cdylib_name`,
   `wasm_name`, and `WASM_TRIPLE`, with their tests. `dl` re-exports them
   at `dl::`.
3. `dl/store.rs` takes `copy_atomic` and `write_atomic`, with
   `test_copy_atomic_replaces_file`.
4. `cli/config.rs` is empty after steps 1 and 2 and is deleted. `cli/`
   keeps `cargo_build`, `wasm_build`, `PackConfig`, the TOML helpers, the
   module renderer, and the commands. They import from `deploy` and `dl`.
5. Tests and the ADCS example import from the new paths.

Check: `grep -rn "crate::cli" src --exclude-dir=cli` prints nothing.

Tests: none new. Every existing test passes unchanged apart from its
imports.

## T2. Links resolve through typed params

Files: `src/link/{mod,publish,subscribe,db}.rs`,
`src/deploy/{mod,error,tests}.rs`, new `src/deploy/part.rs`, `TODO.md`.

1. **`LinkParams`** in `link/mod.rs`:

   ```rust
   /// What every link is configured with.
   #[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
   pub struct LinkParams {
       pub transport: Transport,
       /// The target's telemetry namespace.
       #[serde(default)]
       pub namespace: Option<String>,
       /// This link's name, which tells one of a target's links from another.
       #[serde(default)]
       pub link: String,
   }
   ```

   `PublishParams`, `SubscribeParams`, and `DbParams` replace their three
   copies with `#[serde(flatten)] pub link: LinkParams`. The JSON they read
   does not change. Uses become `params.link.transport`. `DbParams` gains
   `Serialize`. Delete the "Link loops … repeat four fields" note from
   `TODO.md` §4 if the fold settles it. It also names the readiness race
   and status tail, which stay.
2. **Link types.** `link/mod.rs` names `PUBLISH_TYPE` (`fsw.publish`),
   `SUBSCRIBE_TYPE` (`fsw.subscribe`), and `DB_TYPE` (`fsw.db`, moved from
   `deploy`), plus `LINK_TYPES: [&str; 3]`. `register_builtins` registers
   under them.
3. **`deploy/part.rs`**:

   ```rust
   /// The fields of `T` in a system's params, and the rest untouched.
   #[derive(Serialize, Deserialize)]
   pub(crate) struct Part<T> {
       #[serde(flatten)]
       pub(crate) part: T,
       #[serde(flatten)]
       rest: serde_json::Map<String, Value>,
   }
   ```

   `Part::read(system: &SystemConfig) -> Result<Self, DeployError>` and
   `Part::write(self, system: &mut SystemConfig)`. A decode failure is the
   new `DeployError::Params { system, message }`, where the old code read
   `None` and skipped the system.
4. **`deploy/mod.rs`**:
   - `link(system) -> Result<Option<Part<LinkParams>>, DeployError>` is
     `None` unless `LINK_TYPES` holds `system.ty`.
   - `peer_link` and `listen_port` take a `&LinkParams` and match on
     `Transport`. `listen_port` keeps `rsplit_once(':')` on the typed
     `addr`, so host names still parse.
   - `resolve` writes `Transport::Connect { addr }` through
     `Part<LinkParams>`, and `command_ids` through `Part<DbParams>`.
   - `unresolved` and `check_ports` go through `link`.
   - No `json!` and no `.get("..")` on params remain in the file.

Tests, in `deploy/tests.rs`:
- `tables` registers the test systems under `PUBLISH_TYPE` and
  `SUBSCRIBE_TYPE`, as it does `DB_TYPE` now. `envelope` names those types
  for `pub`, `cmds`, and `mirror`.
- New: a non-link system whose params hold a `transport.peer` is left
  untouched and does not count as unresolved.
- New: a link whose params do not decode fails with `DeployError::Params`
  naming it.
- New: resolving keeps a link's other params, such as `max_connections`
  and `conn_cap`.
- The existing tests pass, including `test_resolved_envelope_is_a_fixpoint`.

## T3. Slot candidates say where they come from

Files: `src/dl/slot.rs`, `src/dl/slot/tests.rs`, `src/deploy/slot.rs`,
`python/metor-config/metor_config/_builtins.py`,
`python/metor-config/tests/test_target.py`, `tests/slot.rs`.

1. **Types** in `dl/slot.rs`:

   ```rust
   #[derive(Serialize, Deserialize)]
   pub(crate) struct SlotParams {
       pub(crate) allow: Vec<Candidate>,
       #[serde(default)]
       pub(crate) initial: Option<String>,
   }

   #[derive(Serialize, Deserialize)]
   pub(crate) struct Candidate {
       pub(crate) name: String,
       pub(crate) ty: String,
       #[serde(default)]
       pub(crate) params: Value,
       pub(crate) source: Source,
   }

   #[derive(Serialize, Deserialize)]
   #[serde(rename_all = "snake_case")]
   pub(crate) enum Source {
       Pack(PackRef),
       Stored(Stored),
   }

   #[derive(Serialize, Deserialize)]
   pub(crate) struct Stored {
       pub(crate) path: PathBuf,
       pub(crate) hash: String,
       pub(crate) def: SystemDef,
       #[serde(default)]
       pub(crate) kind: PackKind,
       #[serde(flatten)]
       pub(crate) limits: Limits,
   }
   ```

2. **`def`** refuses any candidate whose source is `Pack`, naming it
   ("`{name}` is not resolved; run `metor build`"). This is the one gate.
   It takes the ports from the first candidate's `Stored::def`.
3. **`Resolved::of`** takes a `Stored`. The `Option` checks and the
   `unresolved` closure go. A `Pack` source past `def` is still an error
   return, not a panic.
4. **`deploy/slot.rs`** decodes `SlotParams`. A candidate with a `Pack`
   source is opened, its def computed, and its artifact stored as now,
   then replaced by `Source::Stored`. The pass encodes `SlotParams` back.
   `unresolved_slot` is "a candidate's source is `Pack`". `resolved`,
   `candidates`, `text`, and the `Map` edits are deleted. A decode failure
   is `DeployError::Slot`.
5. **Python.** `_candidate` emits `"source": {"pack": system._pack.to_json()}`.

Tests:
- `tests/slot.rs`: `candidate` and `wasm_candidate` emit
  `"source": {"stored": {..}}`. The stale-library tests edit
  `["source"]["stored"]["path"]` and `["hash"]`.
- `dl/slot/tests.rs`, new: `def` refuses a slot whose second candidate
  still has a `Pack` source, naming it.
- `deploy/tests.rs`, new: resolving an envelope whose slots are all
  `Stored` changes nothing.
- `test_target.py`: `allow[0]["source"]["pack"]` is the pack's JSON.

## T4. Failed writes are counted and reported

Files: `src/port/mod.rs`, `src/fn_system/{param,set,mod}.rs`,
`src/sequence/step.rs`, `src/fn_system/tests.rs`, `src/sequence/tests.rs`.

1. **`Output`** gains `dropped: Arc<AtomicU32>`. `Output::new` and
   `Output::bytes` make their own counter. `Output::counting(writer,
   dropped)` shares one. `write` and `write_bytes` add one, saturating, on
   any error before returning it.
2. **`Writers`** becomes a struct, like `Views`:

   ```rust
   /// "Writers" are bound outputs, plus the counter their failed writes share.
   pub struct Writers {
       pub(crate) bindings: Bindings<OutputBinding>,
       pub(crate) dropped: Arc<AtomicU32>,
   }
   ```

   `Output`'s `bind_out` uses `Output::counting`. `DynOutputs` gains a
   `counting` constructor that its `bind_out` uses. `SystemOutputs::bind`
   keeps its signature.
3. **`OutSet`** keeps the counter beside `log`, and gains
   `report_dropped(&mut self, now)`. It swaps the count to zero, and when
   the count was nonzero writes `log.fault(now, "publish_dropped",
   "{n} writes dropped")`. The log port's own output counts into its own
   counter, not the shared one, so a full log ring does not report
   through itself.
4. **Callers.** `FnSystem::execute` calls `outputs.report_dropped(now)`
   after the call. `SequenceStep` keeps the counter from `OutSet` and
   reports after each poll, and in `fault`.

Tests:
- `port` unit: a write to a full ring bumps the counter once and returns
  `SendError::Ring`. An oversize write bumps it and returns `Oversize`. A
  good write leaves it at zero.
- `fn_system/tests.rs`:
  - a system that writes into a full ring logs one `publish_dropped`
    line per cycle with the count;
  - a cycle with no failed write logs nothing;
  - two outputs failing in one cycle give one line with their sum.
- `sequence/tests.rs`: a sequence writing into a full ring logs
  `publish_dropped` after the poll.
- A test pins that bind allocates the counter once and a cycle allocates
  nothing on the success path. Use the counting allocator that
  `tests/wasm_alloc.rs` uses, if it applies.

## T5. DESIGN.md matches the code

Files: `DESIGN.md`.

Rewrite in house style: short sections, no slice or status language, no
future tense. Each section says what exists and names the module that
holds it. Keep the document the same length or shorter.

Sections:

1. **Systems, rings, and records.** A system is a fn over ports. A ring is
   single-producer and multi-reader, in a `metor-fsw-3-ring` crate that
   works across processes. `Record` covers frames and messages. A frame is
   a `#[repr(C)]` value with an optional timestamp. A message carries a
   codec. Fan-in picks the newest by timestamp.
2. **Authoring.** `#[system]` fns, `#[state]`, ctors and JSON params,
   `Sample`, the `log` output, async systems with `Stop`, and
   `#[sequence]` with `Seq`, `on_abort`, and `redline!`. Use the crate docs'
   examples; do not restate their rules.
3. **The coordinator.** `CoordinatorConfig`, `SystemTable`, build into one
   ring per output plus `status`, and `step` in list order. `loop()` is how
   a system reads a later one's output. `Step` has five methods, and why.
4. **Adapters.** Thread, channel, `DlSlot`, and wasm, each a table entry
   that returns a step.
5. **Packs and the ABI.** The eight `metor_fsw_*` symbols over JSON, and
   one ABI for dylibs and wasm guests. `export_pack!`, and the store and
   hash check.
6. **Config and build.** The Python `Target` and `Deployment`, the config
   v2 envelope, and what `metor build` resolves (peers, db command ids,
   slot sources).
7. **Links and gateways.** `Publish`, `Subscribe`, `Transport`, and `Db`.

Delete:
- the `type System` sketch;
- the fsw-2 ring path;
- the `05-links.md` reference;
- the "roughly" ABI sketch;
- the dynamic-frames and Yoke section, which describes nothing that
  exists;
- the NixOS renderer paragraph, which stays in `TODO.md`.

Check: every type and module the document names exists (`grep` each).

## Review

After T5, another model reviews the five commits antagonistically against
`17-keep-the-core-small.md`, and in particular against its four rules.
Findings are fixed in follow-up commits before the slice closes.
