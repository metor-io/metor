# Qualified frame schemas

A follow-up to slice 4, decided 2026-09-22. A frame port's schema is
built under the port's announced root when the type computes its def,
so what a `PortDef` carries is what a link announces. The host patches
nothing.

## Why

A `PortDef`'s frame schema was built from the type alone, with leaves
`{record}.{field}`, and crossed the pack ABI as data. `Publish` then
rebuilt the announced names `{namespace}.{instance}.{port}.{field}` on
the host by rewriting the vtable's id blobs in place (`wire::reroot`),
since no type was left to ask. Two places knew the naming rule, and one
of them worked on bytes.

## Shape

```rust
pub struct DefCx<'a> {
    /// The instance this def is for, and the target's namespace.
    pub id: &'a str,
    pub namespace: Option<&'a str>,
    ..
}

impl DefCx<'_> {
    /// Where port `port`'s components hang: `{namespace}.{id}.{port}`,
    /// skipping the parts the context lacks.
    pub fn root(&self, port: &str) -> String;
}

trait Record {
    fn schema() -> RecordSchema;
    /// What a port rooted at `root` announces; a frame builds its vtable
    /// and metadata there, a message ignores it.
    fn schema_at(root: &str) -> RecordSchema { Self::schema() }
}

impl RecordSchema {
    pub fn frame<T: Frame>(root: &str) -> Self;   // vtable_fields(root), metadata(root)
}

impl<T: Record> Output<T> { pub fn def(cx: &DefCx<'_>, name: &'static str) -> PortDef; }
impl<T: Record> Input<T>  { pub fn def(cx: &DefCx<'_>, name: &'static str) -> PortDef; }
```

- The frame derive stops chaining the record name into the component
  path, so `vtable_fields(root)` yields `{root}.{field}`. `schema()` is
  `frame::<Self>(Self::NAME)`, today's record-relative form, and
  `schema_at(root)` is `frame::<Self>(root)`.
- `def` builds `schema: T::schema_at(&cx.root(name))`. The `defs` a
  bundle derive emits pass its `cx` through. `DefCx::empty()` has no id
  or namespace, so the descriptor's schemas root at the port name.
- `PortDef::same_record` compares schemas for messages only. A frame's
  identity is its record name, id, length, alignment, and depth; two
  ports of one frame type now carry different vtables by design.
- `DynInputs` clone the producer's def, so a link's input carries the
  producer's announced schema. A static input's frame schema roots at
  its own port and nothing reads it.
- `CoordinatorConfig` gains `namespace: Option<String>`; build hands
  it and the instance id to every `DefCx`. The links keep their
  `namespace` param for `LinkInfo`, and Python sets both from
  `Target(namespace=)`.
- `Publish::announce` announces each frame def's vtable and metadata as
  they are, under `schema.packet_id()`. `reroot`, `rename`, `leaf_ids`,
  and `root` are deleted.

## ABI

Version 6. The `def` export's context payload (`DefCxOwned`) gains `id`
and `namespace`; the signature and the result are unchanged. A pack
built against 5 would read the new fields as defaults and announce
unqualified ids, which is the mismatch the version check refuses.

## Tasks

1. `DefCx` and `DefCxOwned` gain `id` and `namespace` with `root()`;
   `CoordinatorConfig.namespace`; build passes both; Python emits the
   coordinator `namespace`; ABI 6 in the crate, `metor_config._config`,
   `metor-fsw-abi`, and the golden module.
2. `Record::schema_at`, `RecordSchema::frame(root)`, the frame derive's
   path, `def(cx, name)` on both ports and in the bundle derive,
   `same_record` for frames, every call site.
3. `announce` from the def's schema; the four re-rooting functions and
   their tests go; `05-links.md` wire names and `TODO.md` updated; the
   link integration tests pass the namespace through the config.

Task 1 is one commit. Tasks 2 and 3 land together, since a qualified
schema and the host's re-rooting cannot share a tree. Tests and clippy
green, fmt on touched files only.
