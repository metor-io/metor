# Keeping the core small

fsw-3's advantage over fsw-2 is one shape: the coordinator steps a list of
`Box<dyn Step>` and knows nothing else. Threads, channels, wasm, and slots
are table entries that return a step. Links are plain async systems. fsw-2
lost this shape one special case at a time, until the coordinator knew about
workers, boundaries, reloads, and registries.

This doc states the rules that keep the shape, and four changes where fsw-3
has started to slip.

## Rules

1. The coordinator loop names no adapter, system type, or record beyond
   `SystemStatus`.
2. Config is typed at one gate. A build pass reads and writes a system's
   params through the serde types its runtime decodes, never by probing JSON
   keys.
3. `cli` depends on the runtime. No runtime module imports `crate::cli`.
4. A failed write is reported. It may be dropped, but not silently.

## 1. `deploy/` resolves typed params

Now:

- `deploy/mod.rs:35` and `:41` write `params["transport"]` and
  `params["command_ids"]` as `json!` values.
- `listen_port` walks `transport.listen.addr` and splits on `:`.
- `unresolved` treats any system whose params have `transport.peer` as a
  link, whatever its type.
- `deploy/slot.rs:80-85` decides a candidate is resolved by which keys are
  present. `dl/slot.rs:38-53` makes `path`, `hash`, and `def` optional to
  match, then errors at runtime with "run `metor build`".

The change:

- The three links' shared fields fold into one `LinkParams { transport,
  namespace, link }`, flattened into `PublishParams`, `SubscribeParams`,
  and `DbParams`. `TODO.md` §4 holds this fold "until a third link
  arrives"; `Db` is the third.
- A link is a system whose `ty` is one of the link types, not one whose
  params hold a `transport` key.
- `deploy/` edits a link's params through a typed part:

  ```rust
  /// The fields of `T` in a system's params, and the rest untouched.
  #[derive(Serialize, Deserialize)]
  struct Part<T> {
      #[serde(flatten)]
      part: T,
      #[serde(flatten)]
      rest: serde_json::Map<String, Value>,
  }
  ```

  It decodes `Part<LinkParams>` to read a listen address or replace a
  `Transport::Peer` with a `Transport::Connect`, and `Part<DbParams>` to set
  a db's `command_ids`. The listen port comes from `Transport::Listen`'s
  `addr`.
- A slot candidate gets one typed field that says where it comes from:

  ```rust
  #[serde(rename_all = "snake_case")]
  enum Source {
      Pack(PackRef),
      Stored { path: PathBuf, hash: String, def: SystemDef, kind: PackKind, #[serde(flatten)] limits: Limits },
  }
  ```

  `deploy/` decodes the slot's whole `SlotParams`, maps each `Pack` to a
  `Stored`, and encodes it back. `DlSlot`'s `def` refuses a `Pack` source
  once, at build. `Resolved` then needs no `Option`s. Python's `DlSlot`
  emits `source: {pack: ..}`.

Done when `deploy/` has no `.get("..")` on params and no `json!` writes into
them.

## 2. `cli` depends on the runtime, not the reverse

Now `dl/slot.rs:16` imports `cli::config::PackKind` and `dl/store.rs:31`
calls `cli::cargo::copy_atomic`. `deploy/` imports `DeploymentConfig` from
`cli::config`.

Moves:

| Item | From | To |
|---|---|---|
| `DeploymentConfig`, `TargetConfig`, `ConfigError`, `CONFIG_VERSION` | `cli/config.rs` | `deploy/config.rs` |
| `PackRef`, `PackKind`, `PackRef::open`, `artifact` | `cli/config.rs` | `dl/` |
| `triple`, `cdylib_name`, `wasm_name`, `WASM_TRIPLE` | `cli/cargo.rs` | beside `PackRef` |
| `copy_atomic`, `write_atomic` | `cli/cargo.rs` | `dl/store.rs` |

`cli/` keeps cargo invocation, `PackConfig`, the module renderer, and the
commands.

Done when a search for `crate::cli` outside `src/cli/` finds nothing.

## 3. Failed writes are counted and reported

Now `Output::write` returns `SendError`. The doc examples, the channel
adapter, and the coordinator's status writes discard it with `let _ =`. A
full ring loses records with no trace. fsw-2 counted failed publishes and
logged one `publish_dropped` fault per cycle.

The change:

- Every `Output` counts its failed writes into a shared counter. Bind hands
  all of a fn system's outputs one counter, the way `Views` carries the
  samplers `Sample` parameters push. A sequence moves its outputs into its
  future, so the counter has to be shared rather than read off the outputs
  after the call.
- After the call, `FnSystem::execute` and the sequence step take the count
  and write one `publish_dropped` fault on `log` when it is nonzero.
- `write` still returns the error for a caller that reacts to it. `let _ =`
  stays a valid idiom, since the drop now surfaces.
- A trait-path `System` reports its own drops.
- Nothing allocates after bind.

The count does not go on `SystemStatus`. The coordinator cannot see a
step's outputs, and rule 1 keeps it that way.

An async system's writes land in its mirror rings, whose copies already
report `mirror_dropped`. Its own failed writes into a full mirror are left
for later.

## 4. DESIGN.md matches the code

Rewrite it after changes 1 to 3 land. It is stale in these places:

- The `System` sketch shows `from_views` and `SystemState::from_config`,
  which do not exist.
- Rings point at `../metor-fsw-2/ring/lib.rs`, and shared state points at a
  deleted `docs/plans/05-links.md`.
- The ABI section describes fn pointers "roughly". The real ABI is eight
  symbols over JSON.
- "The last line … is slice 7" and other slice-status language.
- The dynamic-frames and Yoke section describes nothing that exists.

## Not in scope

- A backward-edge check. Slice 1 fixed "no backward-edge marking; `loop()`
  is a builder device". A hand-written config that reads a later system
  gets last cycle's value.
- The positional bind contract, also fixed in slice 1.
- fsw-2 features not yet in fsw-3: alarms, process isolation, discovery,
  replay, bundles, and presets. Each comes back as a system or an adapter,
  under the rules above.
- Load and remake on the cycle thread, and allocation in the log path.
  They are known debts, tracked separately.

## Order

2, then 1, then 3, then 4. Change 2 is mechanical and gives 1 its homes.
Change 4 describes the result.
