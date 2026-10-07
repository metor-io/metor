# IO chunk D: removed

Chunk D, the layer stack and its counting layer, is not in this slice.

- **Why.** Nothing in production used layers. Tests inject faults through
  models instead: a `ScriptedUart` that fails a write, gives a short read,
  or never answers.
- **What comes back later.** The layer mechanism (the stack, its verdicts,
  and a counting layer) returns with layers installed from config, through
  metor-expr or WASM (`TODO.md` §8).
- **Where `io_status` went.** Chunk C counts it inline in `io_submit` and
  `io_complete`, and owns the `ResourceStatus` record and its Python class
  (`18c-io-integration-impl.md`, C5 and C6).
