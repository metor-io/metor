# Hardware config: implementation plan

Implements the core changes in `docs/hardware-config.md`: mux element paths,
calibrated mux mappings, mixed real and sim targets, and optional inputs in
composites. The hardware conventions themselves (leaves, parts, slots,
`DaqDriver`, the prop example) are user code and are not built here.

## Facts the plan relies on

- A plain `[T; N]` field is `N` leaves `x.0` … `x.{N-1}`
  (`metor-component/src/vtable.rs:45`), for primitives too: `f64`, `bool`,
  and the other scalars implement `AsVTable` (`metor-component/src/nox.rs:165`).
  Only `FrameArray<T, N>` (`metor-component/src/array.rs:90`) and nox
  `Tensor`/`V3`/`Matrix6` are one shaped leaf (`f64[3]`, `f64[6,6]`). Element
  paths in Rust are therefore needed only for shaped leaves. See design issue 1.
- `FieldTable` (`src/frame_fields.rs:23`) has public `leaves`, `len`,
  `stamp_at`. `Leaf.len` is `ty.size() * product(shape)`. `under`
  (`:71`) and `is_under` (`:113`) match a path or its `path.` children.
- `relative` (`src/mux/plan.rs:229`) borrows leaves from the table and returns
  them sorted by relative path. `field_copies` (`:239`) pairs dst and src
  leaves by relative path, type, and shape. `apply` (`:277`) copies `len`
  bytes and returns `false` when out of range. `Mux::take`
  (`src/mux/step.rs:108`) calls `apply(copy, record, staging)` per copy of
  the record's input.
- `Plan` (`plan.rs:63`) derives `PartialEq, Eq`; `Copy` (`:15`) derives
  `Copy, PartialEq, Eq`. `MuxError` (`:72`) maps to `MakeError::Mux` by
  message (`src/mux/mod.rs:24`).
- `FieldMap` (`src/mux/params.rs:54`) has `dst`, `input`, `src`. Params decode
  through `serde_ignored` and an unknown key is an error
  (`src/coordinator/params.rs:19`), so an old host refuses a config with
  `cal` rather than dropping it.
- `smallvec` is not a workspace dependency. Crates pin `1.11.2` with
  `const_generics`, `union` (`libs/stellarator/Cargo.toml:63`); the lock
  resolves `1.16.0`. metor-fsw-3 already pulls it in through
  `metor-proto-wkt`.
- Kani proofs sit in `#[cfg(kani)] mod proofs` next to the code
  (`src/rng.rs:86`, `src/channel/apply.rs:268`). `cfg(kani)` is allowed in the
  workspace lints. `mux` and `frame_fields` are not feature-gated.
- Python: `Field(port, path, signature)` (`metor_config/_fields.py:13`);
  `signature(fields, path)` (`:29`) returns a leaf's signature, a group's
  `{a:f64,b:f64[3]}`, or `None`. `Record.__init__` (`_model.py:35`) checks
  each mapping with `_check_mapping` (`:49`). `OutPort.field` (`:98`)
  resolves a path. `Mux` (`_builtins.py:477`) collects input ports with
  `_mux_inputs` (`:525`) and writes fields with `_fields` (`:567`).
- Stubgen: `Accessors::of` (`src/cli/port_class.rs:70`) builds `__init__`
  keywords and port classes from `(path, signature)` pairs; a leaf accessor
  returns `Field[Literal[sig]]` via `field_type` (`:207`); a group accessor
  returns a generated group class. `record.py.jinja:9` types each keyword
  `<annotation> | None = None` and the generated `__init__` has no `**kwargs`.
  `module.py.jinja:10` and `builtins.py.jinja:9` hold the imports.
  `_generated.py` is blessed by `METOR_BLESS=1 cargo test -p metor-fsw-3
  --test module`; `tests/golden/echo_pack.py` is compared by
  `tests/pack_dev.rs`.
- Pyright fixtures: `tests/typing/fields_{ok,bad}.py`, run by
  `test_field_typing` (`tests/module.rs:407`) against a stub of
  `Sensors { gyro_b: FrameArray<f64, 3>, css: FrameArray<f64, 6>, wheel }`.
  It skips when `pyright` and `uvx` are both missing, unless `CI` is set.
- Python tests are `unittest` classes, run with pytest from
  `python/metor-config`.
- `build_with` (`src/coordinator/build.rs:84`) runs `check_deterministic`
  (`:873`, clock only) before `Bindings::collect` (`:609`). Every grant names
  a real resource: `bind_grants` (`:638`) refuses a sim one with `SimGrant`.
  `CoordinatorConfig::default()` uses a wall clock.
- Drivers already stamp with cycle time: adcs `gps.rs:106` writes
  `Gps::from_best_xyz(seq.now(), ..)`. Nothing in the framework stamps for a
  driver. Rule 5 is a convention, not a code change.
- Composites (`sphw/fsw3-composite`, `python/metor-config/metor_config/_composite.py`):
  `composite` (`:32`) checks arguments with `inspect.signature(fn).bind` and
  defers the body. Parameters are plain Python values; nothing inspects
  annotations. A `Sources[T] | None` parameter already works at runtime.
  A system input given `None` fails in `_sources` (`_model.py:320`) with a
  bare `TypeError` from `list(None)`.

## A. Mux element paths

### A1. Rust lowering

1. `src/frame_fields.rs`: add

   ```rust
   /// Why an element path does not resolve.
   #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub enum ElementError {
       /// No leaf is a proper prefix, a segment is not a canonical decimal
       /// index, or there are more indices than dimensions.
       Unknown,
       /// An index at or past its dimension, of length `len`.
       Index { len: usize },
   }

   impl FieldTable {
       /// The element of a shaped leaf at `path`, as its own leaf.
       pub fn element(&self, path: &str) -> Result<Leaf, ElementError>;
   }

   /// `(offset, len)` in bytes of the element at `indices` of a `shape` of
   /// `size`-byte elements, relative to the leaf; `None` past a dimension.
   fn locate(shape: &[usize], size: usize, indices: &[usize]) -> Option<(usize, usize)>;

   /// A canonical decimal index: digits only, no leading zero but "0".
   fn index(segment: &str) -> Option<usize>;
   ```

   `element` finds the one leaf with `path.starts_with(leaf.path + ".")`,
   parses the rest with `index`, checks the count against the rank, and calls
   `locate`. The leaf it returns has `path = path`, `shape = shape[k..]`,
   `offset = leaf.offset + off`, `len`. `locate` uses checked arithmetic.
   `Index` reports the first index past its dimension.
2. `src/mux/plan.rs`:
   - `MuxError::Index { port: String, path: String, len: usize }`,
     `"`{port}.{path}`: index past a dimension of {len}"`.
   - `relative(table, path) -> Result<Vec<(&'a str, Cow<'a, Leaf>)>, ElementError>`:
     `under` as today; when empty, `table.element(path)` gives one owned leaf
     at relative path `""`. `Unknown` gives an empty list, so the caller
     keeps `UnknownField`.
   - `field_copies` maps `ElementError::Index` to `MuxError::Index` with the
     side's port (output port for dst, `map.input` for src). `Copy` and
     `apply` are unchanged.
3. Bounds are checked at make by `element`; a hand-written config gets the
   same errors as Python.
4. Overlapping destinations (a leaf and its element, an element twice) are
   not refused in Rust: copies run in order and the later wins. Python
   refuses them (A2.4).
5. Tests:
   - `src/frame_fields/tests.rs`, over a hand-built `FieldTable` with
     `p: f64[6,6]` and `v: f64[4]`:
     - nominal: `v.2` is `f64` at `offset + 16`, `len 8`; `p.1` is `f64[6]`
       at `offset + 48`; `p.1.2` is `f64` at `offset + 64`;
     - edge: `v.3` (last); index 0 of `f64[1]`; struct-array leaf `css.0`
       is still a leaf, not an element;
     - failure: `v.4` is `Index { len: 4 }`; `v.x`, `v.+1`, `v.01`, `v.-1`
       are `Unknown`; `a.0` on a scalar is `Unknown`; `p.1.2.0` is `Unknown`;
       `w.0` with no leaf `w` is `Unknown`.
   - `src/mux/plan/tests.rs`, through `Plan::resolve` with `Pair`
     (`b: FrameArray<f64, 3>`) and a new test frame with a
     `FrameArray<f64, 4>` and a scalar `f64`:
     - nominal: `b.1` to a scalar dst gives one 8-byte copy at the right
       offsets; a scalar src to dst `b.0`; a whole leaf and an element
       mapped to different leaves of one output;
     - edge: `b.2`; `a` still resolves as a whole leaf;
     - failure: `b.3` is `MuxError::Index` on the src side and on the dst
       side; `b.one` is `UnknownField`; `b.0` to a `f64[3]` dst is
       `Mismatch`.
   - Kani in `frame_fields.rs`: `locate_in_bounds`. For any shape of rank ≤ 2
     with dims ≤ 8, size in `{1, 8}`, and indices of length ≤ rank: when
     `locate` is `Some((off, len))`, `off + len <= size * product(shape)`;
     it is `Some` exactly when every index is below its dimension.
6. `docs/muxes.md` "Semantics" and "Lowering": element rules (an element of
   `ty[d0, ..]` is `ty[d1, ..]`; each index drops one dimension; same
   separator as struct arrays; one byte copy per element; `Index` at make).

### A2. Python and stubgen

1. `metor_config/_fields.py`:
   - `parse(sig: str) -> tuple[str, tuple[int, ...]]` and
     `render(ty: str, dims: tuple[int, ...]) -> str`.
   - `signature(fields, path)`: after the leaf and group cases, try
     `_element(fields, path)`: the leaf that is a proper prefix, canonical
     decimal segments, at most rank many. An index at or past its dimension
     raises `ConfigError("`{path}`: index {i} is past dimension {d}")`; any
     other miss returns `None`.
   - `class ArrayField(Field[S], Generic[S, E])` with
     `def __getitem__(self, i: int) -> E`: refuses a non-`int`, a `bool`,
     and a negative index with `ConfigError`, then returns
     `self.port.field(f"{self.path}.{i}")`. `Field` has no `__getitem__`.
     Hand-written, not generated; stubgen only writes its type arguments.
   - `ArrayField.__iter__ = None`, so `__getitem__` does not make a field
     iterable (`list(field)` raises `TypeError` as today).
   - `OutPort.field` returns an `ArrayField` for a shaped leaf or element,
     or a group whose children are `0..N`.
2. `OutPort.field` and `Record.__init__` accept element paths through
   `signature` with no further change.
3. Destination mappings in `Record.__init__` (`_model.py:35`):
   - a value that is a `Mapping[int, Field]` expands to `{key}.{i}` per
     entry; keys go through the same index check as `__getitem__`;
   - `_check_overlap(paths)`: a path equal to or under another raises
     `ConfigError("`{record}` maps `{a}` and `{b}`")`.
   - `BoardCmd(open={0: f})` works through the generated `__init__`. The
     by-path form `BoardCmd(**{"open.3": f})` is not supported.
4. Stubgen (`src/cli/port_class.rs`):
   - `fn element_type(signature: &str) -> String`: for `ty[d0]` →
     `Field[Literal["ty"]]`; for `ty[d0, d1, ..]` →
     `ArrayField[Literal["ty[d1,..]"], element_type(..)]`.
   - `fn array_type(signature) -> Option<String>`:
     `ArrayField[Literal[sig], element_type(sig)]` for a shaped leaf.
   - `accessor` (`:109`): a shaped leaf returns `array_type`, with body
     `ArrayField({port}, {path}, {sig})`.
   - `Accessors::of` (`:70`): a shaped leaf's keyword annotation is
     `Field[Literal[sig]] | Mapping[int, <element keyword>]`, where the
     element keyword is `field_type` of the element signature.
   - `record.py.jinja`, `module.py.jinja:10`, `builtins.py.jinja:9`: import
     `ArrayField` and `Mapping`.
   - If design issue 1 is settled as recommended, an index-run group
     (children exactly `0..N`, one shared signature) gets the same
     treatment: base class `ArrayField[Literal[group_sig], field_type(child)]`
     and a `Mapping[int, ..]` keyword.
   - Bless `_generated.py`; update `tests/golden/echo_pack.py`.
5. `docs/muxes.md` "Python API": `raw.v[5]`, `cov.p[1][2]`,
   `raw.field("v.5")`, `BoardCmd(open={0: cmd.sv101})`, the overlap rule, the
   `ArrayField` stub shape.
6. Tests:
   - `tests/test_fields.py`:
     - nominal: `plant.imu.<shaped>[1]` has path `x.1` and the element
       signature; `field("x.1")` is the same; a record maps an element dst
       from a scalar src; `open={0: f, 1: g}` expands to two paths;
     - edge: last index; a 2-D `[1][2]`; a struct-array path still maps;
     - failure: index past the dimension (`__getitem__`, `field`, and a
       mapping key); a negative index; `True` as an index; indexing a
       scalar; indexing a group; a leaf and its element both mapped; an
       element mapped twice through a mapping and a path.
     `adcs_stub.py` gains a frame with a `FrameArray` leaf if none fits.
   - `tests/test_mux.py`: a mux with element src and dst writes
     `"src": "x.1"`, `"dst": "y.0"`.
   - `port_class.rs` unit tests: `f64[4]` accessor returns
     `ArrayField[Literal["f64[4]"], Field[Literal["f64"]]]`; `f64[6,6]`
     nests; keyword annotation carries `Mapping[int, ..]`; scalars and
     groups unchanged.
   - Pyright: `fields_ok.py` gains `plant.sensors.gyro_b[0]` passed where
     `Field[Literal["f64"]]` is expected and `Body(omega_b={0: plant.sensors.css[1]})`;
     `fields_bad.py` gains `Body(omega_b={0: plant.sensors.gyro_b})  # expect-error`
     and `x: Field[Literal["f64[3]"]] = plant.sensors.gyro_b[0]  # expect-error`.

## B. Calibrated mux mappings

Needs A1 for shaped channels. Split-array channels (`[f64; 32]`) are scalar
leaves already.

### B1. Rust

1. `Cargo.toml`: `smallvec = { version = "1.11.2", features = ["const_generics", "union"] }`.
2. New `src/mux/cal.rs`:

   ```rust
   #[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
   #[serde(rename_all = "lowercase")]
   pub enum TcKind { K, J, T, E }

   #[derive(Clone, Debug, PartialEq)]
   pub enum Cal {
       Poly(SmallVec<[f64; 2]>),
       Table(Box<[(f64, f64)]>),
       Thermocouple { kind: TcKind },
       Ratiometric { excitation: u32, span: (f64, f64), eng: (f64, f64) },
   }

   impl Cal {
       /// `None` when empty or a coefficient is not finite.
       pub fn poly(coeffs: &[f64]) -> Option<Self>;
       /// `None` when empty, not strictly increasing in `x`, or not finite.
       pub fn table(points: &[(f64, f64)]) -> Option<Self>;
       /// `None` when `span` has zero width or a value is not finite.
       pub fn ratiometric(excitation: u32, span: (f64, f64), eng: (f64, f64)) -> Option<Self>;
       /// Evaluates at `x`; `record` is the source record, read by `Ratiometric`.
       pub fn eval(&self, x: f64, record: &[u8]) -> f64;
   }

   /// Horner, ascending coefficients.
   pub(crate) fn poly(coeffs: &[f64], x: f64) -> f64;
   /// Linear interpolation, clamped to the end points; NaN for a NaN `x`.
   pub(crate) fn table(points: &[(f64, f64)], x: f64) -> f64;
   pub(crate) fn thermocouple(kind: TcKind, volts: f64) -> f64;
   pub(crate) fn ratiometric(v: f64, excitation: f64, span: (f64, f64), eng: (f64, f64)) -> f64;
   /// An `f64` at `at`, little-endian.
   pub(crate) fn read_f64(record: &[u8], at: u32) -> Option<f64>;
   ```

   `table` finds the segment with `partition_point`. `ratiometric` returns
   NaN when the excitation cannot be read. A NaN source gives NaN in every
   kind; no kind panics.
3. New `src/mux/cal/its90.rs`: the NIST ITS-90 inverse polynomials
   (mV → °C) as `const` slices, with their ranges in mV:

   ```rust
   pub(super) struct Range { pub lo: f64, pub hi: f64, pub d: &'static [f64] }
   pub(super) const K: &[Range] = &[..];  // -5.891..0, 0..20.644, 20.644..54.886
   pub(super) const J: &[Range] = &[..];  // -8.095..0, 0..42.919, 42.919..69.553
   pub(super) const T: &[Range] = &[..];  // -5.603..0, 0..20.872
   pub(super) const E: &[Range] = &[..];  // -8.825..0, 0..76.373
   ```

   Source: NIST ITS-90 Thermocouple Database (NIST SRD 60; Burns et al.,
   NIST Monograph 175, 1993), the "inverse coefficients" table for each
   type. Transcribe the `d0..dn` columns exactly, with a comment naming the
   table. Ship K first, with J, T, E in the same PR (common in prop test
   stands; the code per type is one table). `thermocouple` converts V to mV,
   picks the range, and evaluates with `poly`. Outside every range it returns
   NaN; see design issue 5. B, N, R, S are later additions of one table each.
4. `src/mux/params.rs`:

   ```rust
   #[derive(Clone, Debug, Deserialize, JsonSchema)]
   pub struct FieldMap {
       pub dst: String,
       pub input: String,
       pub src: String,
       #[serde(default)]
       pub cal: Option<CalParam>,
   }

   #[derive(Clone, Debug, Deserialize, JsonSchema)]
   #[serde(rename_all = "snake_case")]
   pub enum CalParam {
       Poly(Vec<f64>),
       Table(Vec<(f64, f64)>),
       Thermocouple { kind: TcKind },
       Ratiometric { excitation: SourceParam, span: (f64, f64), eng: (f64, f64) },
   }

   /// One field of one input.
   #[derive(Clone, Debug, Deserialize, JsonSchema)]
   pub struct SourceParam { pub input: String, pub src: String }
   ```

   `CalParam` is the serde form; `Cal` is built from it at make, so
   `smallvec` needs no serde or schema feature.
5. `src/mux/plan.rs`:
   - `Copy` gains `pub cal: Option<u16>`, an index into `Plan::cals`.
   - `Plan` gains `pub cals: Vec<Cal>` and derives `PartialEq` only (`Cal`
     holds `f64`).
   - `MuxError` gains:
     - `CalType { port: String, path: String }`: "`{port}.{path}` is not a
       scalar f64; a calibration maps f64 to f64";
     - `CalInput { dst: String, input: String }`: "`{dst}`'s excitation is
       in `{input}`, not the mapping's input";
     - `CalTable { dst: String }`: "`{dst}`'s calibration is empty,
       unsorted, or has a zero-width span";
     - `TooManyCals(usize)`; see design issue 4.
   - `resolve` threads `&mut Vec<Cal>` through `out_spec` (`:120`).
   - `field_copies` (`:239`): with `map.cal`, `calibrated_copy(map, input,
     src, dst, port, cals) -> Result<Copy, MuxError>`: exactly one leaf each
     side, each `PrimType::F64` with an empty shape, else `CalType`; then
     `lower(param, map, src) -> Result<Cal, MuxError>`, push, index with a
     `u16` check.
   - `lower` resolves `Ratiometric`'s excitation: `input` must equal
     `map.input` (`CalInput`), its path must resolve to one scalar `f64`
     (`CalType`), and its offset is the `u32`.
   - `apply(c: &Copy, cals: &[Cal], src: &[u8], dst: &mut [u8]) -> bool`
     (`:277`): `None` → `copy_bytes` (today's body); `Some(i)` →
     `calibrate(cal, c, src, dst)`: `read_f64` at `c.src`, `eval`, write 8
     bytes LE at `c.dst`. `false` when out of range or `i` has no cal.
6. `src/mux/step.rs:117`: `apply(copy, &self.plan.cals, record, &mut state.staging)`.
   The only change there; see design issue 3.
7. Tests in `src/mux/cal/tests.rs`:
   - nominal: `Poly([offset, gain])` at three points; a cubic; a table
     between points; type K at NIST reference points (0.000 mV → 0 °C,
     4.096 mV → 100 °C, 20.644 mV → 500 °C, 41.276 mV → 1000 °C, −3.554 mV
     → −100 °C) within 0.06 °C; one point per range for J, T, E from the
     NIST forward tables; ratiometric mid-span;
   - edge: table below the first and above the last point clamps; a
     one-point table; a one-coefficient `Poly`; a `Poly` of five
     coefficients (spills past inline storage); each range boundary of K;
     NaN in, NaN out for every kind; excitation 0 gives a non-finite
     result without panicking;
   - failure: `Cal::poly(&[])`, `Cal::table` unsorted, with a repeated `x`,
     with a NaN; `Cal::ratiometric` with equal span ends; thermocouple
     past the top range is NaN.
8. Tests in `src/mux/plan/tests.rs` (frame with scalar `f64`, a
   `FrameArray<f64, 4>`, and an `i32`):
   - nominal: a calibrated `v.1` → scalar dst gives a `Copy` with
     `cal: Some(0)` and `plan.cals[0]`; a calibrated and a plain mapping in
     one output; `apply` on a byte buffer writes the evaluated value;
     ratiometric with excitation `v.3` of the same input;
   - failure: calibrated to an `i32` dst, from an `i32` src, to a
     `f64[3]` dst (`CalType`); excitation from another input (`CalInput`);
     excitation that is a `f64[4]` (`CalType`); an unsorted table and an
     empty poly in params (`CalTable`); `apply` with a cal index past
     `cals` returns `false`.
9. `tests/mux.rs`: a calibrated mapping through a built coordinator: a
   source emits volts, the output holds engineering values each cycle.
10. Kani in `cal.rs`:
    - `poly_bounded_is_finite`: any `[f64; 3]`, any length `0..=3`, any
      `x`, all within `±1e3`; the result is finite.
    - `table_never_panics`: any `[(f64, f64); 3]` with strictly increasing
      finite `x`, any length `1..=3`, any `x`; when `x <= x0` the result is
      `y0`, when `x >= xn` it is `yn`.
    - `read_f64_in_bounds`: any `at`, a 16-byte record; `Some` exactly when
      `at + 8 <= 16`.
11. `docs/muxes.md`: a mapping is a byte copy or a calibration; the kinds,
    the scalar `f64` rule, the same-input rule for excitation, NaN
    propagation, out-of-range thermocouples, and that `Volts` stay in
    telemetry.

### B2. Python and stubgen

1. New `metor_config/_cal.py`:

   ```python
   @dataclass(frozen=True)
   class Cal:
       _param: Any  # the `CalParam` JSON, with a Field for excitation

       @staticmethod
       def poly(coeffs: Sequence[float]) -> Cal: ...
       @staticmethod
       def linear(*, gain: float, offset: float) -> Cal: ...
       @staticmethod
       def two_point(*, volts: tuple[float, float], eng: tuple[float, float]) -> Cal: ...
       @staticmethod
       def table(points: Sequence[tuple[float, float]]) -> Cal: ...
       @staticmethod
       def thermocouple(kind: Literal["K", "J", "T", "E"]) -> Cal: ...
       @staticmethod
       def ratiometric(*, excitation: Field[Literal["f64"]],
                       span: tuple[float, float], eng: tuple[float, float]) -> Cal: ...

       def to_json(self) -> Any: ...

   @dataclass(frozen=True)
   class Calibrated:
       source: Field[Literal["f64"]]
       cal: Cal
   ```

   Every constructor refuses a non-finite number (JSON cannot carry NaN or
   inf). `poly` refuses an empty list; `two_point` equal volts; `table` an
   empty list and repeated `x`, and sorts by `x`; `thermocouple` an unknown
   kind; `ratiometric` equal span ends and an excitation that is not a
   scalar `f64` field. `to_json` writes
   `{"poly": [..]}`, `{"table": [[x, y], ..]}`,
   `{"thermocouple": {"kind": "k"}}`, or
   `{"ratiometric": {"excitation": {"input": port.name, "src": path}, "span": [..], "eng": [..]}}`.
2. `_model.py`: `Record._mapping: dict[str, Field[Any] | Calibrated]`.
   `_check_mapping` accepts a `Calibrated` when both the destination and
   `source.signature` are `"f64"`, and a ratiometric excitation on the same
   port as `source`; each else raises `ConfigError`. A mapping value may be a
   `Calibrated` inside a `Mapping[int, ..]` too.
3. `_builtins.py`: `_mux_inputs` (`:525`) reads `c.source.port` for a
   `Calibrated`. `_fields` (`:567`) writes `"cal": c.cal.to_json()` for a
   `Calibrated` and omits the key otherwise.
4. `__init__.py` exports `Cal` and `Calibrated` (and `ArrayField` from A2).
5. Stubgen: a scalar `f64` keyword is typed
   `Field[Literal["f64"]] | Calibrated`, and an `f64` array's mapping value
   `Field[Literal["f64"]] | Calibrated`. The templates import `Calibrated`.
   Bless `_generated.py`, update `echo_pack.py`. See design issue 6.
6. Tests:
   - `tests/test_cal.py`: each constructor's JSON; `two_point((0.5, 4.5),
     (0, 1000))` is `poly([-125, 250])`; `table` sorts; failure for each
     refusal above.
   - `tests/test_mux.py`: a calibrated mapping writes `cal`; a plain one
     does not; ratiometric writes the excitation source; a calibrated
     mapping to an `f64[3]` dst, from an `f64[3]` src, and with excitation
     on another port raise `ConfigError`; the calibrated source's port is
     a mux input.
   - Pyright: `fields_ok.py` passes `Calibrated(plant.sensors.gyro_b[0],
     Cal.linear(gain=2.0, offset=0.0))` to a scalar `f64` keyword;
     `fields_bad.py` passes `Calibrated(plant.sensors.gyro_b, ..)` (an
     `f64[3]` source) `# expect-error`.

## C. Mixed real and sim targets

Independent of A and B. No new build checks: a simulated clock with a real
device, and `--deterministic` with a real resource, are user errors and are
documented, not refused.

1. Tests:
   - `tests/io.rs` (`through_the_io_steps`, next to
     `datagram_to_itself_completes_a_recv`, `:345`):
     - `mixed_target_delivers_real_and_sim`: the UDP `pinger` on a real
       loopback socket and the echo-pack `gps` driver with a model over a
       `SimUart` in one config, `Clock::Wall`, run with
       `common::after_cycles` until both a ping and a fix arrive, bounded
       by `common::within`;
     - `model_holding_a_real_resource_runs`: the `pinger` placed between
       `io_submit` and `io_complete`; its datagram completes.
     - `simulated_clock_with_a_real_resource_builds`: the `pinger` on a
       real socket under `Clock::Simulated` is not refused at build.
   - `python/metor-config/tests/test_io.py`: next to
     `test_real_and_sim_resources_mix` (`:261`),
     `test_model_may_hold_a_real_resource`: `fsw.sim("rig", Gps(uart=fsw.resource("p", Tty(..))))`
     renders with the grant and sits between the io steps.
2. Docs:
   - `TODO.md:168`: replace with "A paced simulated clock: fixed `dt` for
     models, paced to wall for real devices. Until then a mixed target runs
     on a wall clock, and a simulated clock with a real device is unpaced."
   - `DESIGN.md` "IO" (`:272`): a mixed target holds real and sim
     resources; it runs on a wall clock; models step once per cycle and
     read the cycle time; a model may hold a real resource; a simulated
     clock with a real device is unpaced, and a run holding a real resource
     is not deterministic; drivers stamp records with the cycle time and put
     device time in a field.
   - `docs/muxes.md` `max_age` bullet: stamps are cycle times.

## D. Optional inputs in composites

Edits files from `sphw/fsw3-composite` (worktree
`/Users/sphw/code/metor/metor-composite`), the PR's base. The runtime already accepts
`Sources[T] | None`; this stage states it and tests it.

1. `docs/composites.md` "Inputs and params": an input may be optional,
   `Sources[T] | None`; the body decides what `None` means, such as adding
   a model only when given truth. A system input never takes `None`.
2. `_model.py:320` `_sources`: `None` raises
   `ConfigError("input `{port}` takes ports and loops, got None")` instead
   of the `TypeError` from `list(None)`.
3. Tests:
   - `tests/test_composite.py`: a composite with
     `truth: Sources[Imu] | None = None` adds its model when given a port
     and not when given `None`; both render; a body that passes `None` to a
     system input raises `ConfigError`.
   - `tests/typing/composite_ok.py`: the optional composite called with a
     port, with `None`, and with `world and world.imu` for a
     `NamedTuple | None` world.
   - `tests/typing/composite_bad.py`: the optional input given a port of
     another record `# expect-error`.

## One PR

One branch, `sphw/fsw3-hw-config`, rebased onto `sphw/fsw3-composite`,
since D edits files that exist only there and the design's examples use
composites. If composites merge first, the PR retargets to `main` with no
change. One commit per stage, in this order, each leaving every suite green:

1. docs: `docs/hardware-config.md`, this plan
2. A1: Rust element paths
3. A2: Python element paths and stubgen
4. B1: Rust calibration
5. B2: Python calibration and stubgen
6. C: mixed-target tests and docs
7. D: optional composite inputs

## Size

Estimated added lines, excluding the two design docs (about 1,400 lines,
already written):

| Stage | Code | Tests | Docs | Total |
|---|---:|---:|---:|---:|
| A1 Rust element paths | 105 | 245 | 25 | 375 |
| A2 Python element paths, stubgen | 190 | 220 | 30 | 440 |
| B1 Rust calibration | 270 (of which ITS-90 tables 120) | 410 | 40 | 720 |
| B2 Python calibration, stubgen | 165 | 180 | — | 345 |
| C Mixed real and sim | — | 110 | 25 | 135 |
| D Optional composite inputs | 5 | 75 | 10 | 90 |
| **Total** | **~735** | **~1,240** | **~130** | **~2,100** |

- Code is about a third of the PR; tests are more than half, from the
  nominal, edge, and failure cases each stage lists, plus about 75 lines of
  Kani harnesses.
- Without the ITS-90 coefficient tables, which are data, new logic is about
  615 lines.
- Regenerated stubs (`_generated.py`, `tests/golden/echo_pack.py`) add
  about 10 changed lines. Each has under five shaped or scalar `f64` fields.
- About 25 lines change in existing Rust (`plan.rs`, `params.rs`,
  `step.rs`); the rest is new functions and files.
- The estimate is ±30%. B1's tests are the most uncertain, since the count
  of NIST reference points per range is set during transcription.

## Commands

From `libs/metor-fsw-3`:

```
cargo test -p metor-fsw-3 --lib -- frame_fields mux cli::port_class
cargo test -p metor-fsw-3 --test mux --test module --test pack_dev
cargo test -p metor-fsw-3 --test io
METOR_BLESS=1 cargo test -p metor-fsw-3 --test module    # regenerate _generated.py
cargo test -p metor-fsw-3                                 # default features: host, db
cargo clippy -p metor-fsw-3 --all-targets
cargo kani -p metor-fsw-3 --lib --no-default-features --harness locate_in_bounds
cargo kani -p metor-fsw-3 --lib --no-default-features --harness poly_bounded_is_finite
cargo kani -p metor-fsw-3 --lib --no-default-features --harness table_never_panics
cargo kani -p metor-fsw-3 --lib --no-default-features --harness read_f64_in_bounds
(cd python/metor-config && python -m pytest tests)
```

`test_field_typing` needs `pyright` or `uvx` on `PATH`; it skips without
them. Run it once with `CI=1` to make sure it did not skip. The io tests in
C use `PollOs` and run on macOS; run `tests/io_uring.rs` through `orb` on
nixos only if `io/host` changes.

## End-to-end check

1. In a scratch copy of `examples/adcs-fsw3/target.py` (not committed), add
   a mux:

   ```python
   Mux(hk=Housekeeping(
       omega_b={0: Calibrated(nav.attitude_estimate.omega_b[2],
                              Cal.linear(gain=2.0, offset=1.0))},
       b_hat_b={1: nav.attitude_estimate.p[1][1]},
   ), on="cycle")
   ```

   `omega_b` is a `V3` (`f64[3]`) and `p` a `Matrix6` (`f64[6,6]`).
   `metor build` it; check the JSON has `"src": "omega_b.2"`,
   `"dst": "omega_b.0"`, `cal`, and `"src": "p.1.1"`.
2. `metor run` the sim with that mux for a few hundred cycles; confirm in
   the db or with a tap that `hk.omega_b.0 == 2 · omega_b.2 + 1`,
   `hk.b_hat_b.1 == p[1][1]`, and that unmapped elements stay zero.
3. Edit an index past its dimension in the JSON and `metor run` it: the
   build fails with `MuxError::Index` through `MakeError::Mux`.
4. For C: a scratch adcs sim with one real `Udp` resource added runs on a
   wall clock with its `SimUart` GPS.

## Constraints

- Allocation only at make: `relative`'s owned element leaves, `Plan::cals`,
  `SmallVec` spill, and `Box<[(f64, f64)]>` are all built in `Plan::resolve`.
  `apply` and `Cal::eval` allocate nothing.
- No panics: `locate` and `read_f64` use checked arithmetic and `get`;
  `table` uses `partition_point` and `get`; `u16` cal indices are checked at
  make. No `unwrap` in new non-test code.
- Short functions: `element` = find prefix leaf + parse + `locate`;
  `field_copies` delegates the calibrated case to `calibrated_copy`;
  `apply` dispatches to `copy_bytes` or `calibrate`.

## Risks

- Float proofs in Kani can be slow. Keep harness inputs to three points or
  coefficients; if a harness does not finish, prove no-panic over the index
  logic with `kani::assume(x.is_finite())` and leave value properties to
  unit tests.
- The crate may not build under `cargo kani --no-default-features` today
  (existing proofs in `src/` have no documented command). Check first; if it
  fails, move the pure functions to a dependency-free module the harness can
  reach, or run with the features the existing proofs use.
- Coefficient transcription: a wrong digit gives plausible numbers. Test
  every range of every shipped kind against a NIST forward-table point, not
  only K.
- `Field.__getitem__` changes iteration semantics; `__iter__ = None`
  prevents silent iteration.
- Generated stubs change for every pack with a shaped leaf or an `f64`
  scalar. Packs regenerate with `metor pack dev`; a stale stub still runs,
  it only types less.

## Design issues found

1. **Plain arrays are already split.** The doc says `v: [f64; 32]` is one
   leaf `v` of shape `[32]` and `v.5` fails today. A plain `[T; N]` is `N`
   leaves `v.0` … `v.31` (`vtable.rs:45`, `nox.rs:165`), so `v.5` already
   resolves in Rust and in `OutPort.field`. The Rust element path is needed
   for `FrameArray` and nox tensors only. In Python, `raw.v` for a plain
   array is a group, so `raw.v[5]` and `BoardCmd(open={0: f})` (with
   `open: [bool; 16]`; there is no `FrameArray<bool, _>`, `bool` is not an
   `Element`) hit the doc's "a group raises" rule. Recommended: `__getitem__`
   delegates to `port.field("{path}.{i}")` (A2.1), which serves both forms,
   and stubgen types an index-run group as `ArrayField` (A2.4, last bullet).
   Resolved: the doc now describes both forms.
2. **`BoardCmd(**{"open.3": f})` does not reach `Record.__init__`.**
   Resolved: the by-path destination form is dropped; `open={3: f}` stays.
3. **`step.rs` changes by one line.** `apply` needs the plan's cals, so
   `Mux::take` passes `&self.plan.cals` (`step.rs:117`). Nothing else in
   `step.rs` changes.
4. **Cal index width.** `Copy.cal: Option<u16>` caps a mux at 65 536
   calibrations; the plan adds `MuxError::TooManyCals` (not in the doc).
   `Cal::ratiometric` with a zero-width span is folded into `CalTable`.
5. **Thermocouple out of range** is unspecified. The plan returns NaN
   outside the NIST ranges (an open or shorted junction reads as no data,
   not as a clamped temperature). `Table` clamps, per the doc.
6. **Stubgen must type `Calibrated`.** The doc covers stubgen for elements
   only. Without B2.5, `FeedPressures(pt101=Calibrated(..))` fails pyright,
   since the keyword is `Field[Literal["f64"]] | None`.
7. **`FieldTable::element` returns `Result`, not `Option`.** `Option<Leaf>`
   cannot tell an unknown path from an index past a dimension, which
   `MuxError::Index` needs.
8. **Optional composite inputs need no runtime change.** `@composite`
   stores arguments and binds the signature only; stage D is docs, a clearer
   `ConfigError` for `None` reaching a system input, and tests. `need(..)`
   in the sketches is user code.
9. **Ratiometric cal holds a field.** `Cal.ratiometric(excitation=..)` takes
   a port field, so a cert file keyed by serial number cannot hold a
   ratiometric cal before its DAQ exists. Setups would build it in the
   binding function. Minor; no change planned.
