# Remove Noxpr and its unused support code

Reviewed and implemented 2026-09-06. The original review and plan are preserved below.

**Implementation result**

Deleted all 15 backend/JAX source files and the identified unused support APIs, error variants, `shared` feature, and `fn-traits` dependency. Retained array math, numerical mapping, dimension helpers needed by concatenation, and protocol conversion APIs. Updated source/package documentation and the expression compiler's obsolete comment. The lockfile change relative to the pre-implementation working tree removes only the `fn-traits` package and Nox's dependency on it.

Repaired the baseline by adding explicit types to ambiguous array test inputs and enabling serde's derive dependency feature. All 54 Nox unit tests passed with default and all features both before backend deletion and afterward. Existing tests cover scalar concatenation via `concat_many`, vector and matrix concatenation, numerical mapping, decompositions, and spatial operations; their assertions were preserved.

Public API removals include the ten listed error variants, `ReprMonad::{Map, map}`, `TensorItem::{Item, Tensor, Dim}`, `ReplaceDim::{Item, MAPPED_DIM}`, non-default-axis replacement implementations, `MappedDim`, `DimConcat`, `ConcatDims`, and `OwnedRepr::noop`. External code using these members must migrate; no compatibility stubs remain.

Validation used `--offline --locked` after the intentional lockfile update:

| Check | Result |
| --- | --- |
| Nox and nox-array, no default features / all features | Passed |
| Nox tests, default / all features | 54 passed in each configuration; doctests passed |
| Nox documentation and Clippy, all features | Passed; Clippy reports existing numerical-code warnings |
| `nox-frames` tests | 7 passed |
| `metor-proto` tests | 14 unit tests and 3 doctests passed |
| `metor-proto-wkt` tests | 17 passed |
| `metor-proto` check with `nox` enabled | Passed |
| `metor-expr` tests | 164 unit tests and 1 doctest passed |
| Separate `metor-expr-prelude` check for `wasm32-unknown-unknown` | Passed; checked-in WASM artifact unchanged |
| `cargo check --workspace --lib` | Passed, including metor-panel and ADCS consumers |
| `metor-adcs` tests | 15 passed; existing `yang_lqr::tests::test_control` assertion failed |
| `cargo check --workspace --all-targets` | Blocked by missing `HashMap` imports in untouched EQL tests at `libs/db/eql/src/lib.rs:907`, `961`, and `974` |
| Formatting, diff whitespace, and legacy-symbol search | Passed; no remaining legacy references outside this historical plan |

The ADCS failure was reproduced in a temporary harness using Nox source from Git HEAD before this implementation and the unchanged Yang LQR numerical code/test (only unrelated component metadata derives were omitted). Both versions produce `[-0.07981352519433647, 0.07978465981540937, -0.19859264724479217]`, while the existing assertion expects `[-0.07981352519433647, 0.15956931963081875, -0.19864202695933386]`. No ADCS or EQL implementation/test changes were made. These two unrelated failures prevent claiming an entirely green workspace suite.

Detailed command logs from this run are under `/private/tmp/nox-removal-*.log`; the baseline reproduction is `/private/tmp/nox-baseline-lqr.log`. Validation results above describe the completed implementation.

The code calls this subsystem `noxpr` (singular), with the expression type `Noxpr`. Delete it completely, including the JAX bridge, compilation/execution support, and support APIs whose only consumers belong to that subsystem. Preserve the working array math library and its consumers.

**What the code review established**

- [src/lib.rs](../../src/lib.rs) declares neither `noxpr` nor `jax`, and exports `ArrayRepr` as `DefaultRepr`. These files are already outside the compiled module tree, even with all declared features enabled.
- [Cargo.toml](../../Cargo.toml) declares only `std`, `shared`, and `serde`, with empty defaults. There are no XLA, JAX, CUDA, or Python features or dependencies to disable. `shared` is empty and has no source gates or workspace consumers. `fn-traits` remains a dependency, but its only source use is in the disconnected `noxpr/comp_fn.rs`.
- The 14 files in `src/noxpr/`, plus `src/jax.rs`, total 5,313 lines. Their graph, batching, lowering, execution, transfer, and Python APIs can be removed together.
- Repository searches, including hidden files outside Git/build output, found no downstream source consumers of `Noxpr`. The only mention outside Nox is commentary in [metor-expr/src/ir.rs](../../../metor-expr/src/ir.rs), which explicitly uses Nox as a kernel library and differential oracle, not as its IR.
- Several support traits have both live and dead members. Removing everything named “map,” “representation,” or “dimension” would break current array operations and downstream crates.

**1. Establish a usable baseline**

Before implementation, record the working-tree diff and repeat the scoped checks below. The workspace already has unrelated modifications, including `Cargo.lock`; preserve those when updating dependencies.

Checks performed during this review, with `rustc 1.98.0 (88d9e12ae 2026-08-18)` and a target directory under `/private/tmp`:

- `cargo metadata --offline --locked --no-deps --format-version 1` succeeded. No workspace dependency on Nox requests a legacy feature.
- `cargo check -p nox -p nox-array --offline --locked` succeeded with the default features.
- `cargo check -p nox -p nox-array --all-features --offline --locked` failed with 13 errors because Nox uses serde derives without enabling the dependency's `derive` feature. A wider workspace build can mask this through feature unification.
- `cargo test -p nox -p nox-array --lib --offline --locked` failed before tests ran: 46 existing E0283/E0284 inference errors in `src/array/mod.rs`. Examples include `Array::eye()` at line 1833, `Array::from_diag(...)` at line 1843, and untyped `array!` values in later tests. The compiler reports multiple applicable `From` implementations for nested arrays.

Make a small prerequisite change adding explicit element/dimension types to the ambiguous test values, then run the tests to discover any runtime failures. Preserve their assertions. This makes the numerical tests useful for judging the cleanup; do not remove failing tests or change the public array conversion API as a shortcut. Keep any unrelated failures distinguishable from removal regressions.

Also enable `serde`'s `derive` dependency feature explicitly in the baseline repair, then repeat the isolated all-features check. Serialization is a retained feature, and its baseline must not depend on another crate enabling its required macros.

**2. Delete the disconnected backend in one change**

Delete all of `src/noxpr/` and `src/jax.rs`. No replacement backend or migration shim is required by the current module graph.

| Files | Functionality removed |
| --- | --- |
| `noxpr/node.rs` | Expression nodes and IDs, graph types, traversal/replacement, pretty printing, shape inference, and XLA lowering |
| `noxpr/batch.rs` | Graph batching, `vmap`, `vmap_with_dim`, and graph `scan` |
| `noxpr/builder.rs`, `comp_fn.rs` | Graph construction, `CompFn`, `FromBuilder`, and mutable-parameter scaffolding |
| `noxpr/comp.rs`, `client.rs`, `exec.rs` | XLA computations, PJRT clients, compilation, CPU/GPU execution |
| `noxpr/transfer.rs` | `TypedBuffer`, `AsTypedBuffer`, `FromTypedBuffers`, and host/device transfers |
| `noxpr/repr.rs`, `tensor.rs`, `scalar.rs`, `vector.rs` | The `Op` representation, graph tensor constructors/indexing, `Collapse`, nested graph tensors, and graph-only operations |
| `noxpr/py.rs`, `jax.rs` | Python tensor conversion, JAX tracing, and callable wrappers |
| `noxpr/mod.rs` | Backend module declarations and exports |

This also deletes the dormant tests inside these files. Do not port tests of the retired compiler or recreate graph-only operations such as `Vector<_, _, Op>::extend` in the array backend solely to preserve them.

**3. Remove dependencies, features, and dead errors**

In `Cargo.toml`, remove `fn-traits` and `shared = []`. Update the package description to describe the array/tensor math library; it currently promises XLA compilation. Let Cargo update the existing workspace lockfile and review the resulting diff for unrelated changes. Remove the `fn-traits` lock entry only if it has no remaining dependents.

The old files mention `xla`, `pyo3`, `numpy`, `boxcar`, and `paste`, but those are not direct Nox dependencies today. Do not add them to make the old code compile before deleting it, or remove workspace packages just because those names occur in deleted files. For example, `paste` remains in the math dependency graph.

From [src/error.rs](../../src/error.rs), remove these ten variants, whose workspace references are confined to their definitions or the deleted modules:

`WrongAxisLen`, `UnbatchableArgument`, `VmapArgsEmpty`, `VmapInAxisMismatch`, `GetTupleElemWrongType`, `OutOfBoundsAccess`, `ScanWrongArgCount`, `ScanMissingArg`, `ScanShapeMismatch`, and `InvertFailed`.

Retain `InvalidConcatDims`, `Cholesky`, and `SizeOverflow`. Array concatenation constructs the first; the faer-backed decomposition code uses the latter two, including implicit conversion from `CholeskyError`. LU inversion remains supported despite the unused `InvertFailed` variant.

Keep `thiserror`, `faer`, `inplace_it`, `libm`, `num-traits`, `typenum`, `seq-macro`, `smallvec`, `zerocopy`, `serde`, and `approx`: they support compiled numerical, dimension, storage, serialization, or comparison code. In particular, `approx` provides public trait implementations, so it is not merely a test dependency.

**4. Trim support APIs that become unused**

Do this after the file deletion, with compile checks between each group. The precise removal boundary is:

| Location | Delete or simplify | Preserve |
| --- | --- | --- |
| `src/repr.rs`; implementations in `tensor.rs`, `array/mod.rs`, `quaternion.rs`, `spatial.rs` | Remove `ReprMonad::Map` and its representation-converting `map` method from the trait and all seven implementations. The remaining forwarding call in the quaternion implementation disappears with that method. | `ReprMonad::{Elem, Dim, inner, into_inner, from_inner}`; these serve actual buffer and protocol conversions. |
| `src/tensor.rs`, `TensorItem` | Remove associated types `Item`, `Tensor<D>`, and `Dim`, and the corresponding blanket-implementation entries. Their higher-order tensor role belongs to graph batching/collapse; no live consumers were found. Update the documentation that promises tensors of quaternions or nested tensors. | `TensorItem::Elem` and the existing bounds using it. It still connects tensor storage to its element type; eliminating the entire trait would require a separate API rewrite. |
| `src/tensor.rs`, `ReplaceDim` | Remove `Item`, `MAPPED_DIM`, and their implementation entries; remove the unused `MappedDim<T, D>` convenience alias. Remove `impl_map_inner!` and its invocations, which generate the non-default-axis graph mapping implementations. | `Mapped`, `DefaultMap`, `DefaultMappedDim`, `ReplaceDim::{MappedDim, ReplaceMappedDim}`, the `ReplaceMappedDim` alias, and implementations supporting first-axis concatenation. |
| `src/tensor.rs`, dimension concatenation | Remove `DimConcat`, `ConcatDims`, and their implementations, including the two implementation blocks embedded in `impl_map!`. Their consumers are the removed nested-tensor `Collapse` implementation. | `NonTupleDim`. Despite its current documentation linking to `DimConcat`, it is used in broadcasting, indexing, matrix operations, and ADCS. Rewrite its documentation. |
| `src/repr.rs`, `src/array/repr.rs`, `Tensor::clone` | Replace `R::noop(&self.inner)` with `self.inner.clone()`, then remove `OwnedRepr::noop` and its remaining implementation. | Existing tensor clone behavior and bounds. `Repr::Inner` already requires `Clone`; the array implementation of `noop` only calls `clone`. |

Do not confuse the removed representation-converting `ReprMonad::map` with `Tensor::map`, `Array::map`, or `OwnedRepr::map`. Those implement live numerical mapping and must remain. Likewise, `ConcatDims` is obsolete, but `ConcatDim` and `ConcatManyDim` in `array/mod.rs` actively depend on the retained dimension-replacement machinery.

The public support API changes are source-breaking for external callers that use the deleted members, even though workspace searches found no live users. Record them in the change description. There is no need to preserve unused compatibility stubs for a complete deletion.

**5. Remove obsolete comments and backend-specific exceptions**

- Remove the crate-wide `clippy::arc_with_non_send_sync` allowance in `src/lib.rs`; the Arc-based graph is gone. Retain the recursion limit unless a separate check establishes a lower limit works for the live dimension machinery.
- Rewrite the array module's “non-XLA backend” introduction and scalar module's host/client description around current behavior.
- Remove the empty scalar test module containing only the commented-out graph log test, and the commented-out XLA `extend` test in `src/vector.rs`.
- Remove the stale commented-out `try_inverse` test in `src/matrix.rs`; retain the working LU inversion tests in `src/array/mod.rs`.
- Update mapping and tensor-item documentation to match the smaller traits. Rename the surviving mapping macro if that makes its concatenation-only purpose clearer, without changing the generated live implementations.
- Update the comment in `metor-expr/src/ir.rs` to say Nox supplies kernels and the differential oracle without referring to a graph layer that no longer exists.

**Working features that remain in scope to preserve**

Keep `Repr`, `OwnedRepr`, `ArrayRepr`, `DefaultRepr`, and the representation type parameters. `ViewRepr` is a separate borrowed representation in [src/array/view.rs](../../src/array/view.rs); `nox-frames` and `metor-adcs` also expose working generic APIs over `OwnedRepr`. Deleting representation polymorphism would be a broader redesign, not unused-code cleanup.

Keep the [nox-array crate](../../array/src/lib.rs). It defines borrowed `ArrayView` storage used by Nox, directly by `metor-proto`, and by the separate WASM [metor-expr-prelude crate](../../../metor-expr/prelude/Cargo.toml). Preserve array views, dynamic dimensions, fixed arrays, slicing, broadcasting, concatenation/stacking, map/row iteration, numerical decompositions, spatial math, and integrators. Keep `std`, `serde`, and default `no_std` behavior.

Keep the conversion portion of `ReprMonad`: [metor-proto/src/nox_impls.rs](../../../metor-proto/src/nox_impls.rs) uses its element/dimension metadata and buffer access for tensors, quaternions, and spatial types. The `metor-expr` tests also import it to access numerical reference results. The current expression compiler and its tensor kernels are independent of Noxpr and remain supported.

**6. Validate the result**

Run these from the workspace root after the prerequisite test repair and removals. Use the existing lockfile after the intentional dependency update.

```sh
cargo check -p nox -p nox-array --locked --no-default-features
cargo check -p nox -p nox-array --locked --all-features
cargo test -p nox -p nox-array --locked
cargo test -p nox -p nox-array --locked --all-features
cargo doc -p nox -p nox-array --locked --all-features --no-deps
cargo clippy -p nox -p nox-array --locked --all-targets --all-features
cargo test -p nox-frames -p metor-adcs -p metor-proto -p metor-proto-wkt --locked
cargo check -p metor-proto --locked --features nox
cargo test -p metor-expr --locked
cargo check --workspace --all-targets --locked
```

The workspace check covers the additional consumers found by metadata: `metor-component`, `metor-db`, `metor-proto-cli`, `metor-db-tests`, `metor-panel`, `adcs-contracts`, and `adcs-systems`. Report unrelated workspace/platform failures separately rather than treating them as evidence that the Nox changes passed. Consult the existing `metor-expr` build setup for WASM prerequisites; its prelude is a separate workspace, so the root workspace check alone does not validate that guest artifact.

Use the existing tests to verify broadcasting and concatenation shapes, row mapping, LU/Cholesky, quaternion/spatial operations, and the expression compiler's differential results. Because the plan trims dimension macros, ensure coverage includes scalar/vector and matrix concatenation. Add a focused case only if existing tests do not exercise a retained implementation affected by the edit. Preserve serde/zerocopy derives and confirm protocol conversion code compiles.

Finally, search source, manifests, build configuration, and maintained documentation for `noxpr`, `Noxpr`, `CompFn`, `FromBuilder`, `TypedBuffer`, `JaxTracer`, `xla`, `jax`, `cuda`, `fn-traits`, and the deleted support members. Review hits in context: this plan is intentionally historical, and unrelated uses elsewhere in the workspace are not deletion targets. Confirm there are no source includes or feature gates that resurrect the removed backend. Check the final diff for unrelated formatting or lockfile changes.

Completion means all 15 disconnected source files are gone, the identified dead support members and feature/dependency declarations are removed, current documentation describes array math, and retained numerical/protocol consumers pass the checks above or have explicitly documented pre-existing/environmental blockers. Merely leaving Noxpr unexported does not satisfy this plan.

Suggested implementation sequence: one small baseline test/serde repair commit, one backend/dependency/error deletion commit, and one support-API/documentation cleanup commit, followed by the full validation. No replacement compiler work is needed.
