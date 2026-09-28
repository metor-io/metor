# ADCS example: a sequence pack that runs as a module

Unblocks T12 of `14-wasm-impl.md`. The ADCS example gets a small pack of
sequences that builds as a module. The convergence test runs it in the
loop, beside the native packs.

This replaces a full sim/flight split, which `examples/adcs-fsw3/TODO.md`
still plans as step 1. Nothing here gets in its way.

## Why a new pack

The `adcs` pack cannot build for wasm32. `adcs-contracts` links WMM
(through bindgen) and SOFA (through `nox-frames`), and `hifitime` pulls
`wasm-bindgen`. Of the four systems, `plant` and `nav` use that code, and
`ctrl` and `mode` do not. A new pack that needs only frames and pure math
builds as a module once the C sits behind a feature.

The example has no sequences yet, so the new pack's first sequence takes
over what `mode` does, and more: it sequences the boot.

## Contracts

`adcs-contracts` gains `default = ["environment"]`. The feature gates:

- the `environment` and `orbit` modules;
- the `Epoch` and `MagneticModel` re-exports;
- the `wmm`, `nox-frames`, and `hifitime` dependencies.

Everything else stays in the default build, with no change: frames,
params, `mtq`, `pointing`, `spacecraft`, `DT`, `V3`, and `Quat`. The
`adcs` pack keeps the default. The new pack takes
`default-features = false`.

## The pack

`systems/seqs`, crate `adcs-seqs-3`, pack id `seqs`, distribution
`adcs-seqs`, module `adcs_seqs`, `[tool.metor.pack] wasm = true`.

```rust
/// Detumbles on the magnetorquers until the estimated rate settles, then
/// holds nadir. Commands safe if aborted.
#[sequence]
async fn commission(
    seq: &Seq,
    #[state] params: &mut CommissionParams,
    estimate: &Sample<AttitudeEstimate>,
    mode_cmd: &mut Output<ModeCmd>,
) -> Result<(), SeqError> {
    let _safe = seq.on_abort(|seq| {
        let _ = mode_cmd.write(&ModeCmd::safe().stamped(seq.now()));
    });
    let _ = mode_cmd.write(&ModeCmd::detumble().stamped(seq.now()));
    let settled = || estimate.get().is_some_and(|e| e.omega_b.norm() < params.rate);
    let _ = seq.wait_for(params.detumble_timeout(), settled).await;
    let _ = mode_cmd.write(&ModeCmd::idle().stamped(seq.now()));
    Ok(())
}
```

- `CommissionParams { rate: f64, detumble_timeout_s: f64 }`.
- A timeout does not abort. Commissioning goes on to nadir either way, as
  a boot sequence would on a slow detumble.
- `ctrl` keeps the last law it was sent, so a sequence that ends after its
  last command leaves the loop pointing.

The exact shape of the sequence params (`#[state]` or a params struct)
follows what `#[sequence]` supports today. The implementer checks
`src/sequence` and the echo-pack's `count_by`.

## Targets

- **`sim.py` is unchanged.** It stays the native baseline, with `mode`.
- **`target.py`:** the `fsw` target replaces `mode` with `commission` from
  `adcs_seqs` and adds `fsw.wasm(adcs_seqs.PACK)`, so the deployment runs
  the sequence as a module. The sequence gets `channel="commission"`, so
  it can be stopped, aborted, and reset from the ground like any other.

## Tests

`convergence.rs` keeps its two runs, linked and loaded, and adds two
that swap `mode` for `commission`:

- **`sequenced (linked)`:** `adcs_seqs::pack()` registered statically.
- **`sequenced (module)`:** the staged `.wasm` opened through
  `PackRef::open`.

Assertions:

- Both sequenced runs converge on nadir under the existing bounds. If
  detumble-then-nadir needs more than today's 2400 cycles, the test gets a
  longer run for these two. The measured settling cycle goes beside the
  constant.
- The two sequenced runs command bit-identical torque streams. The
  sequence only compares a rate and writes fixed commands, and `ctrl` runs
  native in both, so there is no `libm` difference to tolerate.
- The module run's `commission` ends `Completed`, and its log carries
  ctrl's "pointing law changed" lines for detumble and then nadir.

`deploy.rs` runs `target.py` as now.

`tests/seqs.rs`, unit-level, over the linked pack:

- A rate above the threshold holds detumble. Dropping below it switches
  to nadir and completes.
- A timeout goes on to nadir.
- An abort mid-detumble writes `safe` and ends `Aborted`.

`adcs-seqs-3` builds for wasm32 in CI through `pack dev`. The `pack dev`
wasm build also checks that the module's descriptor matches the dylib's.

## Tasks

1. **S1. Contracts feature.** The `adcs` pack and every test are
   unchanged and green. `adcs-contracts-3` with `default-features = false`
   builds for wasm32.
2. **S2. The pack.** The crate, `pyproject.toml`, `commission`, its unit
   tests, the example's `pyproject.toml` and `uv.lock`, and `target.py`.
3. **S3. The gate.**
   - The two sequenced convergence runs.
   - Measure `commission`'s setup and step fuel, and revisit the wasm
     defaults if needed.
   - Write the "Shipped deviations" section of `14-wasm.md`.

Each task ends with the adcs-fsw3 tests, the metor-fsw-3 tests, clippy,
and one commit.

## Risks

- **Two `ModeCmd` classes in Python.** `adcs_pack` and `adcs_seqs` each
  generate a `ModeCmd` record class. `ctrl`'s `mode=` taking
  `commission.mode_cmd` may be two classes for one record, and pyright
  could refuse it. The host matches by record id, so the config is right
  either way. If pyright refuses it, a `metor-config` follow-up can let a
  generated module reuse a record class another installed pack declares.
- **`nox` on wasm32.** Frames use `nox` tensors. If `nox` without default
  features does not build for wasm32, S1 finds out first.
