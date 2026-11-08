//! Derive and attribute macros for metor-fsw-3.

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::quote;
use syn::Ident;

mod fields;
mod frame;
mod key;
mod record;
mod system_attr;
mod system_params;

/// Derives `Frame` for a struct sent as its raw bytes.
///
/// `Frame` is [`Fields`](macro@Fields) plus `Payload`, `Record`, a timestamp,
/// and the frame-id tag. Mark one field `#[fsw(timestamp)]`.
/// `#[fsw(name = "..")]` sets the record name, which defaults to the struct
/// name in snake_case. `#[fsw(depth = N)]` sets `Record::DEPTH`.
/// `#[fsw(max_len = N)]` bounds a frame holding a `FrameMap`; a fixed
/// frame's length is its size, and the attribute fails its build.
/// `#[frame(..)]` is an alias of `#[fsw(..)]`.
///
/// A frame used as a field of another frame is a plain group: the outer
/// frame's timestamp and frame id apply to its leaves. `Frame::STAMP` is the
/// timestamp field's offset and size.
#[proc_macro_derive(Frame, attributes(fsw, frame))]
pub fn frame_derive(input: TokenStream) -> TokenStream {
    frame::frame(input)
}

/// Derives `AsVTable` and `Metadatatize` for a group of components nested in
/// a frame.
///
/// The group has no name, id, or timestamp; the frame that holds it stamps
/// its leaves. Each field adds a path segment named after it, and an array
/// adds `0` … `N-1`. A field marked `timestamp` is a compile error. Field attributes:
/// `#[fsw(flatten)]` adds no segment, `#[fsw(rename = "y")]` names the
/// segment `y`, `#[fsw(skip)]` leaves the field out, and `#[fsw(unit = "..")]`
/// and `#[fsw(metadata(..))]` label its components. A field named `_x` is
/// skipped unless marked `#[fsw(skip = false)]`. `#[fsw(group)]` on the
/// struct emits a metadata entry for the group itself. Two leaves at one path
/// fail the derive where it sees them, and otherwise the build
/// (`BuildError::DuplicateLeaf`).
///
/// ```ignore
/// #[derive(Fields, IntoBytes, Immutable, KnownLayout, FromBytes)]
/// #[repr(C)]
/// pub struct Inner { pub x: f64, pub y: f64 }
/// ```
#[proc_macro_derive(Fields, attributes(fsw))]
pub fn fields_derive(input: TokenStream) -> TokenStream {
    fields::fields(input)
}

/// Derives `Payload` and `Record` for a type encoded with postcard.
///
/// `#[record(name = "..")]` sets the record name, which defaults to the type
/// name in snake_case. `max_len` sets the largest encoded size, which defaults
/// to the type's `MaxSize` bound. `depth` sets `Record::DEPTH`. A field marked
/// `#[record(timestamp)]` becomes the record's timestamp.
#[proc_macro_derive(Record, attributes(record))]
pub fn record_derive(input: TokenStream) -> TokenStream {
    record::record(input)
}

/// Derives `Param` and `ArrayParam` for a struct of system params.
///
/// Each field is a param, as a `#[system]` fn argument would be, and names
/// its ports under the field name; an array field names element `i` under
/// `{field}.{i}`. `#[fsw(rename = "..")]` renames that segment,
/// `#[fsw(flatten)]` on a field holding another struct drops it, and
/// `#[fsw(skip)]` or a leading `_` leaves the field out, filled by
/// `Default`. A struct passed as a fn argument adds no segment of its own.
///
/// `Seq`, `Timestamp`, and `Abort` are fn arguments only, and a tuple is no
/// field; the derive refuses them, and `flatten` on a field it can tell is a
/// leaf. An empty name, or two `Cap`s or `Rng`s of one name, fail the
/// system's definition.
///
/// The struct takes at most one lifetime. It is a param of cyclic fns and
/// sequences, or of async fns under `#[system_params(async)]`, whose inputs
/// are `Input<T, Notifier>`.
///
/// ```ignore
/// #[derive(SystemParams)]
/// struct Sensors<'a> {
///     fss: [&'a mut Input<Fss>; 8],
///     gyro: &'a Sample<Gyro>,
/// }
///
/// #[system]
/// fn sun(sensors: Sensors<'_>, est: &mut Output<SunVec>) { .. }
/// ```
#[proc_macro_derive(SystemParams, attributes(fsw, system_params))]
pub fn system_params_derive(input: TokenStream) -> TokenStream {
    system_params::system_params(input)
}

/// Turns a free fn into a system.
///
/// `#[system] fn nav(..)` becomes `fn nav() -> impl SystemFn`, which you
/// register with a table. Each parameter is a port, named after the parameter.
/// An array parameter `[P; N]` names element `i` `{name}.{i}`, and a
/// [`SystemParams`](macro@SystemParams) struct names its fields' ports
/// without the parameter's name. A tuple parameter is refused.
/// Mark one `&mut S` parameter `#[state]` to give the system state. Without
/// one, the state is `()`. The value `nav()` returns derefs to a fn pointer to
/// the original body, so tests can call it directly. The fn's doc comment
/// becomes the system's documentation.
///
/// An `async fn` whose last parameter is `stop: Stop` becomes an async system
/// instead. It has no fn pointer.
///
/// ```ignore
/// #[system]
/// fn nav(#[state] s: &mut NavState, now: Timestamp, est: &mut Output<Estimate>) { .. }
///
/// table.register("nav", nav(), NavState::new)?;
/// ```
#[proc_macro_attribute]
pub fn system(_attr: TokenStream, item: TokenStream) -> TokenStream {
    system_attr::system(item)
}

/// Turns an `async fn` into a sequence.
///
/// A sequence is an async fn that the runtime polls once per cycle until it
/// returns. `#[sequence] async fn arm(..)` becomes `fn arm() -> impl SequenceFn`.
/// Parameters are ports, as with `#[system]`. A `seq: &Seq` parameter gives the
/// sequence the current time, waits, and aborts. The fn must return
/// `Result<(), SeqError>`.
///
/// ```ignore
/// #[sequence]
/// async fn arm(seq: &Seq, cmd: &mut Output<Arm>) -> Result<(), SeqError> { .. }
///
/// table.register_sequence("arm", arm(), Default::default)?;
/// ```
#[proc_macro_attribute]
pub fn sequence(_attr: TokenStream, item: TokenStream) -> TokenStream {
    system_attr::sequence(item)
}

/// Returns the path generated code uses to reach `metor-fsw-3`.
///
/// Fails if the calling crate does not list `metor-fsw-3` in `[dependencies]`.
pub(crate) fn fsw_crate() -> syn::Result<proc_macro2::TokenStream> {
    match crate_name("metor-fsw-3") {
        Ok(FoundCrate::Itself) => Ok(quote!(::metor_fsw_3)),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, Span::call_site());
            Ok(quote!( #ident ))
        }
        Err(_) => Err(syn::Error::new(
            Span::call_site(),
            "metor-fsw-3 macros require `metor-fsw-3` in [dependencies]",
        )),
    }
}
