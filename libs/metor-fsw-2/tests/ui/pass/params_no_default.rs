//! A `#[system]` params type without `Default` (or `Serialize`) still
//! compiles: the defaults probe in the generated `BuildSystem` resolves to
//! the blanket `None` fallback instead of demanding the bounds. The probe is
//! exercised from a consumer crate here, both ways.

use metor_fsw_2::{AsyncContext, BuildSystem, Timestamp, system};

#[derive(serde::Deserialize, postcard_schema::Schema)]
struct BareParams {
    gain: f64,
}

struct Bare {
    gain: f64,
}

#[system]
impl Bare {
    fn new(p: BareParams) -> Self {
        Self { gain: p.gain }
    }

    fn execute(&mut self, _now: Timestamp) {
        let _ = self.gain;
    }
}

#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Default)]
struct DefaultedParams {
    gain: f64,
}

struct Defaulted;

#[system]
impl Defaulted {
    fn new(_p: DefaultedParams) -> Self {
        Self
    }

    fn execute(&mut self, _now: Timestamp) {}
}

#[derive(Default)]
struct Cooperative;

#[system]
impl Cooperative {
    async fn run(&mut self, context: &AsyncContext) {
        let _ = context.is_cancelled();
    }
}

fn main() {
    assert_eq!(<Bare as BuildSystem>::params_default_blob(), None);
    assert_eq!(
        <Defaulted as BuildSystem>::params_default_blob(),
        Some(postcard::to_allocvec(&DefaultedParams::default()).unwrap()),
    );
    let _: Cooperative = <Cooperative as BuildSystem>::new(());
    let _ = metor_fsw_2::Pack::new()
        .system_type::<Bare, _>("bare")
        .system_type::<Defaulted, _>("defaulted");
}
