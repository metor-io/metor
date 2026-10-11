//! Thread placement for builds without OS threads. Every async system is
//! refused.

use std::sync::Arc;

use crate::async_system::AsyncSystem;
use crate::coordinator::{MakeCx, ParamError, Params, Step};

/// A stand-in for the thread registry. Builds without OS threads run no
/// threads.
#[derive(Default)]
pub struct Threads;

impl Threads {
    pub fn new() -> Self {
        Self
    }
}

/// Returns an error naming the system, since async systems need threads.
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
