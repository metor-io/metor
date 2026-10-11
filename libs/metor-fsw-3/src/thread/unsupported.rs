//! Placement in a build without OS threads, which refuses every async system.

use std::sync::Arc;

use crate::async_system::AsyncSystem;
use crate::coordinator::{MakeCx, ParamError, Params, Step};

/// `Threads` holds nothing: there are no groups to run.
#[derive(Default)]
pub struct Threads;

impl Threads {
    pub fn new() -> Self {
        Self
    }
}

/// Refuses the system `cx` names.
pub(crate) fn place<A, M>(_make: Arc<M>, cx: MakeCx<'_>) -> Result<Box<dyn Step>, ParamError>
where
    A: AsyncSystem + 'static,
    M: Fn(Params<'_>) -> Result<(A, A::State), ParamError> + Send + Sync + 'static,
{
    Err(ParamError::ThreadsUnsupported {
        system: cx.id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use metor_proto_wkt::LogEvent;

    use crate::coordinator::{MakeCx, ParamError, Params};
    use crate::tests::utils::{Imu, Nav, Recorder, ring, table};

    use super::Threads;

    #[test]
    fn test_place_refuses_naming_the_system() {
        let table = table(&Recorder::default());
        let entry = table.get("relay").expect("registered");
        let (imu, nav, log) = (ring::<Imu>(8, 1), ring::<Nav>(8, 1), ring::<LogEvent>(8, 1));
        let refused = (entry.make)(MakeCx {
            id: "relay_1",
            adapter: None,
            def: &entry.def,
            params: Params(&serde_json::Value::Null),
            inputs: vec![vec![&imu]],
            outputs: vec![&nav, &log],
            threads: &mut Threads::new(),
        });
        assert_eq!(
            refused.err(),
            Some(ParamError::ThreadsUnsupported {
                system: "relay_1".into()
            })
        );
    }
}
