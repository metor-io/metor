use impeller2::{
    error::Error,
    vtable::{
        VTable,
        builder::{FieldBuilder, schema, vtable},
    },
};
use nox::Body;

pub trait AsVTable {
    fn populate_vtable_fields(builder: &mut Vec<FieldBuilder>) -> Result<(), Error>;
    fn as_vtable() -> VTable {
        let mut fields = vec![];
        Self::populate_vtable_fields(&mut fields).expect("vtable failed to form");
        vtable(fields)
    }
}

impl AsVTable for Body {
    fn populate_vtable_fields(builder: &mut Vec<FieldBuilder>) -> Result<(), Error> {
        use impeller2::vtable::builder::{component, field};
        builder.push(field!(
            Body::pos,
            schema(impeller2::types::PrimType::F64, &[7], component("pos"))
        ));
        builder.push(field!(
            Body::vel,
            schema(impeller2::types::PrimType::F64, &[6], component("vel"))
        ));
        builder.push(field!(
            Body::accel,
            schema(impeller2::types::PrimType::F64, &[6], component("accel"))
        ));
        builder.push(field!(
            Body::inertia,
            schema(impeller2::types::PrimType::F64, &[6], component("inertia"))
        ));
        Ok(())
    }
}
