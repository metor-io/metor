//! The [`Ctor`] trait, which lets a plain fn build a system's state.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use crate::coordinator::{MakeError, Params};
use crate::io::{Io, IoInit};

/// Builds a system's state from its config params.
///
/// It is implemented for fns that take no params, fns that take a
/// deserializable params type, and fns that take params and may fail with a
/// [`MakeError`]. Each form may also take a trailing `&IoInit` to allocate
/// IO buffers. `Marker` keeps these impls from overlapping.
pub trait Ctor<S, Marker> {
    /// Builds the state.
    ///
    /// Decodes `params` first if the constructor takes them.
    fn make(&self, params: Params<'_>, io: &IoInit<'_>) -> Result<S, MakeError>;

    /// Returns the JSON Schema of the params type.
    ///
    /// Returns `None` if the constructor takes no params.
    fn schema() -> Option<Box<RawValue>>;

    /// Builds the state with an `IoInit` over `io`'s arena, or one that
    /// refuses every allocation when the system has no queue.
    ///
    /// A refused allocation fails the make with `IoMemory`, naming system `id`.
    fn make_with_io(&self, id: &str, params: Params<'_>, io: Option<&Io>) -> Result<S, ParamError> {
        let init = io.map_or_else(IoInit::empty, |io| IoInit::new(io.arena()));
        let state = self.make(params, &init)?;
        init.finish(id)?;
        Ok(state)
    }
}

/// Marks the constructor forms that take an `&IoInit`.
pub struct WithIo;

fn schema_of<P: JsonSchema>() -> Option<Box<RawValue>> {
    serde_json::value::to_raw_value(&schemars::schema_for!(P)).ok()
}

impl<S, F: Fn() -> S> Ctor<S, ()> for F {
    fn make(&self, _params: Params<'_>, _io: &IoInit<'_>) -> Result<S, MakeError> {
        Ok(self())
    }

    fn schema() -> Option<Box<RawValue>> {
        None
    }
}

impl<S, P: DeserializeOwned + JsonSchema, F: Fn(P) -> S> Ctor<S, (P,)> for F {
    fn make(&self, params: Params<'_>, _io: &IoInit<'_>) -> Result<S, MakeError> {
        Ok(self(params.decode()?))
    }

    fn schema() -> Option<Box<RawValue>> {
        schema_of::<P>()
    }
}

/// A constructor that can reject its params, such as one that binds a socket.
impl<S, P: DeserializeOwned + JsonSchema, F: Fn(P) -> Result<S, MakeError>> Ctor<S, (P, MakeError)>
    for F
{
    fn make(&self, params: Params<'_>, _io: &IoInit<'_>) -> Result<S, MakeError> {
        self(params.decode()?)
    }

    fn schema() -> Option<Box<RawValue>> {
        schema_of::<P>()
    }
}

impl<S, F: Fn(&IoInit<'_>) -> S> Ctor<S, (WithIo,)> for F {
    fn make(&self, _params: Params<'_>, io: &IoInit<'_>) -> Result<S, ParamError> {
        Ok(self(io))
    }

    fn schema() -> Option<Box<RawValue>> {
        None
    }
}

impl<S, P: DeserializeOwned + JsonSchema, F: Fn(P, &IoInit<'_>) -> S> Ctor<S, (P, WithIo)> for F {
    fn make(&self, params: Params<'_>, io: &IoInit<'_>) -> Result<S, ParamError> {
        Ok(self(params.decode()?, io))
    }

    fn schema() -> Option<Box<RawValue>> {
        schema_of::<P>()
    }
}

impl<S, P, F> Ctor<S, (P, WithIo, ParamError)> for F
where
    P: DeserializeOwned + JsonSchema,
    F: Fn(P, &IoInit<'_>) -> Result<S, ParamError>,
{
    fn make(&self, params: Params<'_>, io: &IoInit<'_>) -> Result<S, ParamError> {
        self(params.decode()?, io)
    }

    fn schema() -> Option<Box<RawValue>> {
        schema_of::<P>()
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;
    use crate::coordinator::ParamError;

    #[derive(Deserialize, JsonSchema)]
    struct Port {
        port: u16,
    }

    fn build(params: Port) -> Result<u16, MakeError> {
        match params.port {
            0 => Err(ParamError::Decode("port zero".into()).into()),
            port => Ok(port),
        }
    }

    /// Calls `build` as a fallible constructor.
    ///
    /// The turbofish is needed because `Fn(P) -> S` also matches `build`.
    fn ctor(params: &serde_json::Value) -> Result<u16, MakeError> {
        Ctor::<u16, (Port, MakeError)>::make(&build, Params(params), &IoInit::empty())
    }

    #[test]
    fn test_fallible_constructor() {
        assert_eq!(ctor(&json!({ "port": 7 })), Ok(7));
    }

    #[test]
    fn test_constructor_error() {
        assert_eq!(
            ctor(&json!({ "port": 0 })),
            Err(ParamError::Decode("port zero".into()).into())
        );
    }

    #[derive(Deserialize, JsonSchema)]
    struct Len {
        len: usize,
    }

    struct Bufs(Vec<crate::io::Buf>);

    fn with_io(io: &IoInit<'_>) -> Bufs {
        Bufs(vec![io.alloc(4)])
    }

    fn sized(p: Len, io: &IoInit<'_>) -> Bufs {
        Bufs(vec![io.alloc(p.len)])
    }

    fn sized_or_fail(p: Len, io: &IoInit<'_>) -> Result<Bufs, ParamError> {
        match p.len {
            0 => Err(ParamError::Decode("zero".into())),
            len => Ok(Bufs(vec![io.alloc(len)])),
        }
    }

    fn make<M, C: Ctor<Bufs, M>>(
        ctor: C,
        params: &serde_json::Value,
    ) -> Result<Vec<usize>, ParamError> {
        let arena = std::rc::Rc::new(crate::io::Arena::new());
        let init = IoInit::new(&arena);
        let bufs = ctor.make(Params(params), &init)?;
        init.finish("a")?;
        Ok(bufs.0.iter().map(|b| b.capacity()).collect())
    }

    #[test]
    fn test_io_forms_allocate() {
        assert_eq!(make(with_io, &serde_json::Value::Null), Ok(vec![4]));
        assert_eq!(make(sized, &json!({ "len": 7 })), Ok(vec![7]));
        assert_eq!(make(sized_or_fail, &json!({ "len": 3 })), Ok(vec![3]));
        assert!(make(sized_or_fail, &json!({ "len": 0 })).is_err());
        assert!(<fn(Len, &IoInit<'_>) -> Bufs as Ctor<Bufs, (Len, WithIo)>>::schema().is_some());
    }

    #[test]
    fn test_plain_forms_ignore_io() {
        let arena = std::rc::Rc::new(crate::io::Arena::new());
        let init = IoInit::new(&arena);
        assert_eq!(
            Ctor::<u16, ()>::make(&|| 5u16, Params(&json!(null)), &init),
            Ok(5)
        );
        assert_eq!(init.finish("a"), Ok(()));
    }

    #[test]
    fn test_bad_params_skip_an_io_constructor() {
        let ran = core::cell::Cell::new(false);
        let ctor = |p: Len, io: &IoInit<'_>| {
            ran.set(true);
            sized(p, io)
        };
        assert!(matches!(
            make(ctor, &json!({ "len": "x" })),
            Err(ParamError::Decode(_))
        ));
        assert!(!ran.get());
    }

    #[test]
    fn test_invalid_params_skip_constructor() {
        assert!(matches!(
            ctor(&json!({ "port": "eight" })),
            Err(MakeError::Params(ParamError::Decode(_)))
        ));
        let schema = <fn(Port) -> Result<u16, MakeError> as Ctor<u16, (Port, MakeError)>>::schema();
        assert!(schema.expect("a params schema").get().contains("port"));
    }
}
