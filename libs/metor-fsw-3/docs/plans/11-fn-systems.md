# Fn systems

Decided 2026-09-22. `#[system]` goes on a free fn and rewrites it into a
fn that returns the system. The impl-block form is retired.

## Why

`#[system]` sits on an impl block today because port names are the
parameter identifiers of `execute`, and only a macro can read them. An
attribute on a method emits impl items only and cannot see `Self`, so it
cannot write `impl SystemFn for Self`. The block was the one place with
the method, the type, and room to emit a trait impl.

That forces a wrapper block on every system and a unit struct on every
stateless one. A blanket `SystemFn` over `Fn` types would need no
attribute, but it cannot recover parameter names or doc comments, and
by-type naming is ruled out because two ports of one record type are
ordinary.

An attribute on a free fn can emit any items, so it keeps names and docs
and drops the wrapper.

## Shape

```rust
/// One filter cycle.
#[system]
fn nav(
    #[state] s: &mut NavState,
    now: Timestamp,
    sensors: &mut Input<Sensors>,
    attitude_estimate: &mut Output<AttitudeEstimate>,
) { .. }

#[system]
fn mode(mode_cmd: &mut Output<ModeCmd>) { .. }

table.register("nav", nav(), NavState::new)?;
table.register("mode", mode(), Default::default)?;
```

The expansion:

```rust
fn nav() -> impl SystemFn<State = NavState>
          + Deref<Target = fn(&mut NavState, Timestamp, &mut Input<Sensors>, &mut Output<AttitudeEstimate>)>
{
    fn inner(s: &mut NavState, now: Timestamp, ..) { .. }
    struct Nav;
    impl Ports for Nav {
        type State = NavState;
        type Params = (Timestamp, Input<Sensors>, Output<AttitudeEstimate>);
        const NAME: &'static str = "nav";
        const NAMES: &'static [&'static str] = &["now", "sensors", "attitude_estimate"];
        const DOC: &'static str = "One filter cycle.";
    }
    impl SystemFn for Nav {
        fn call(s: &mut NavState, (now, sensors, attitude_estimate): /* Item */) {
            inner(s, now, sensors, attitude_estimate)
        }
    }
    impl Deref for Nav {
        type Target = fn(&mut NavState, Timestamp, &mut Input<Sensors>, &mut Output<AttitudeEstimate>);
        fn deref(&self) -> &Self::Target { &(inner as _) }
    }
    Nav
}
```

- The returned type is a unique ZST, so `NAME`, `NAMES`, and `DOC` stay
  associated consts. `System::def(cx)`, `Defs::of::<S>`, and the table's
  `def_fn` are unchanged.
- The struct is local to the fn body, so no name leaks into the module.
- `NAME` is the fn name. The attribute still reads identifiers and doc
  comments only and never classifies a type.
- `Deref` to a fn pointer makes the value callable: `nav()(&mut s, now,
  &mut sensors, &mut est)`. Elided lifetimes in the pointer type are
  higher-ranked, and `&(inner as _)` promotes to `'static`. Only direct
  calls go through the pointer; `SystemFn::call` calls `inner` directly.
- A generic fn expands to a generic struct holding `PhantomData`, called
  as `nav::<T>()`.

## State

The state parameter carries `#[state]` and must be `&mut S`. At most one.
Without it the system is stateless, `State = ()`, and it registers with
`Default::default`.

A blanket `Param for &mut T` would overlap the existing `Param` impls, so
the state is marked, not inferred.

## Traits

```rust
pub trait Ports: Sized + 'static {
    type State: 'static;
    type Params: Param;
    const NAME: &'static str;
    const NAMES: &'static [&'static str];
    const DOC: &'static str = "";
}

pub trait SystemFn: Ports {
    fn call(state: &mut Self::State, items: <Self::Params as Param>::Item<'_, NoWake>);
}

pub trait AsyncSystemFn: Ports {
    async fn call(state: &mut Self::State, items: <Self::Params as Param>::Item<'_, Notifier>, stop: Stop);
}
```

`FnSystem<S>` keeps `PhantomData<S>` and sets `System::State =
S::State`. `register` takes the system value as a type witness, plus the
ctor:

```rust
pub fn register<S: SystemFn, M, C: Ctor<S::State, M> + 'static>(
    &mut self, ty: &str, _system: S, ctor: C,
) -> Result<(), BuildError>;
```

## Async

`#[system] async fn x(.., stop: Stop)` expands the same way to
`fn x() -> impl AsyncSystemFn<State = ..>`, without `Deref`: a fn pointer
cannot name the async fn's future without boxing. Tests drive it through
`AsyncSystemFn::call`. The trailing `stop: Stop` rule and its error stay
as they are.

## Expansion rules

- The user body sits in a fn nested beside the struct, so it sees their
  names. The macro names them `__metor_body` and `__MetorSystem`, not
  `inner` and `Nav` as sketched above.
- The rewrite occupies the value namespace only, so `mod nav; pub use
  nav::nav;` keeps working. The ADCS example uses that layout.
- Items in a fn body cannot see the outer fn's generics. The inner fn and
  struct redeclare the generics and bounds; the struct holds
  `PhantomData`. Edition 2024 return-position `impl Trait` captures them.
- Visibility, `#[doc]`, `#[cfg]`, `#[deprecated]`, and `#[must_use]` go on
  the outer fn. Lint attrs such as `#[allow(clippy::too_many_arguments)]`
  go on `inner`.
- The user body keeps its spans, so completion and go-to-def inside it
  work.
- On a validation error the macro emits the original fn unchanged plus
  `compile_error!`, so the IDE keeps its types.
- Hover on `nav` shows the rewritten signature. The `Deref` target in it
  lists the ports.

## Migration

Every `#[system] impl` block in the crate, its tests, the echo-pack
fixture, and `examples/adcs-fsw3` becomes a `#[system] fn`. State
methods other than `execute` stay in their own impl block. `trybuild`
cases move to the fn form: no `#[state]` on a non-`&mut` parameter, two
`#[state]` parameters, a non-`Param` parameter, `run` without `stop`.

## Rejected

Surveyed 2026-09-22 against utoipa, specta, tauri, pyo3, Rocket, Salvo,
actix, dropshot, poem-openapi, axum, bevy, bevy_reflect, wasm-bindgen,
uniffi, and inventory/linkme.

- Blanket `SystemFn` over `Fn` (axum, bevy): no parameter names, no docs.
  bevy_reflect's `ArgInfo::name` is set by hand for the same reason.
- A struct named after the fn (Rocket, Salvo, dropshot): `mod nav { pub
  struct nav; } pub use nav::nav;` is E0255, and a unit struct cannot be
  generic. `const nav: __nav` has the same problems plus a hidden type.
- A mangled companion item found by a wrapper macro (utoipa
  `__path_nav`, specta and tauri `macro_rules!`, pyo3 `mod nav`):
  registration needs `system!(nav)`, `macro_rules!` companions need
  `#[macro_export]` at the crate root, and pyo3's `mod` collides like a
  struct does.
- A value-level `FnSystem { names, doc, call }`: moves names off the type,
  so `System::def` and `SystemInputs::defs` would need `&self` across the
  trait path.
- Link-time metadata (wasm-bindgen, uniffi, inventory): runtime only, no
  consts on a type.
