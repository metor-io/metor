//! Registers a program's types in an fsw-3 [`SystemTable`].

use std::sync::Arc;

use metor_fsw_3::coordinator::{DynType, MakeCx, MakeError, SystemKind};
use metor_fsw_3::serde_json::value::RawValue;
use metor_fsw_3::{BuildError, DefCx, Input, Step, SystemTable};

use crate::cyclic::CyclicStep;
use crate::desc::{Kind, ParamDesc, Prim, TypeDesc};
use crate::instance::Instance;
use crate::ports::{Rings, system_def};
use crate::program::Program;
use crate::sequence::sequence_step;

/// Registers every type of `program` in `table`, under its own name.
pub fn register(table: &mut SystemTable, program: &Program) -> Result<(), BuildError> {
    for (index, ty) in program.desc.types.iter().enumerate() {
        table.register_dyn(&ty.name, dyn_type(program, index, ty)?)?;
    }
    Ok(())
}

/// Returns a table with every type of `program`.
pub fn table(program: &Program) -> Result<SystemTable, BuildError> {
    let mut table = SystemTable::new();
    register(&mut table, program)?;
    Ok(table)
}

/// Describes type `index` of `program` to the table.
fn dyn_type(program: &Program, index: usize, ty: &TypeDesc) -> Result<DynType, BuildError> {
    let refused = |source| BuildError::TypeDef {
        ty: ty.name.clone(),
        source,
    };
    let def = system_def(&program.desc, ty, &DefCx::empty()).map_err(refused)?;
    let desc = program.desc.clone();
    let make_program = program.clone();
    let redlines: Arc<[&'static str]> = ty
        .entry
        .redlines
        .iter()
        .map(|redline| &*Box::leak(redline.name.clone().into_boxed_str()))
        .collect();
    Ok(DynType {
        kind: match ty.kind {
            Kind::Cyclic => SystemKind::Cyclic,
            Kind::Sequence => SystemKind::Sequence,
        },
        def,
        def_fn: Box::new(move |cx| system_def(&desc, &desc.types[index], cx)),
        doc: ty.doc.clone().into(),
        schema: params_schema(&ty.params),
        // fsw-3's table holds makes in an `Arc`; a program's steps stay on
        // the thread that made them.
        #[allow(clippy::arc_with_non_send_sync)]
        make: Arc::new(move |cx| make(&make_program, index, &redlines, cx)),
    })
}

/// Makes one instance over the rings in `cx`.
fn make(
    program: &Program,
    index: usize,
    redlines: &Arc<[&'static str]>,
    mut cx: MakeCx<'_>,
) -> Result<Box<dyn Step>, MakeError> {
    let instance = Instance::new(program.clone(), index, cx.params.0)?;
    let (mut inputs, outputs) = cx.take_bindings();
    let kind = instance.desc().kind;
    let unbound = || MakeError::Pack("the rings do not match the definition".into());
    let abort = match kind {
        Kind::Sequence if !inputs.is_empty() => Some(Input::new(inputs.remove(0).views)),
        Kind::Sequence => return Err(unbound()),
        Kind::Cyclic => None,
    };
    let (rings, log) = Rings::bind(inputs, outputs).ok_or_else(unbound)?;
    Ok(match abort {
        Some(abort) => sequence_step(instance, rings, abort, log, redlines.clone()),
        None => Box::new(CyclicStep::new(instance, rings, log)),
    })
}

/// Returns the JSON Schema of a type's params, or `None` if it has none.
fn params_schema(params: &[ParamDesc]) -> Option<Box<RawValue>> {
    if params.is_empty() {
        return None;
    }
    let properties: serde_json::Map<String, serde_json::Value> = params
        .iter()
        .map(|param| {
            let ty = match param.prim {
                Prim::Bool => "boolean",
                prim if prim.is_float() => "number",
                _ => "integer",
            };
            let schema = serde_json::json!({ "type": ty, "default": param.default });
            (param.name.clone(), schema)
        })
        .collect();
    let schema = serde_json::json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });
    RawValue::from_string(schema.to_string()).ok()
}
