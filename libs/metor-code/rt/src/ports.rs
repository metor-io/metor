//! An instance's fsw-3 ports: their definitions, and copying between the
//! rings and the instance's blocks.

use core::sync::atomic::AtomicU32;
use std::sync::Arc;

use metor_fsw_3::fn_system::LOG_PORT;
use metor_fsw_3::log::LogPort;
use metor_fsw_3::metor_proto_wkt::LogEvent;
use metor_fsw_3::system::{InputBinding, OutputBinding};
use metor_fsw_3::{Bytes, DefCx, DefError, Input, Output, PortDef, SystemDef};

use crate::desc::{Kind, ProgramDesc, TypeDesc};
use crate::instance::Instance;
use crate::schema::port_def;

/// Returns the definition of an instance of `ty` under `cx`.
///
/// A sequence's inputs start with `abort`, and every type's outputs end
/// with `log`.
pub fn system_def(desc: &ProgramDesc, ty: &TypeDesc, cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
    let port = |p: &crate::desc::PortDesc| {
        let frame = desc.frame_of(p).ok_or_else(|| DefError::Pack {
            message: format!("port `{}` names no frame", p.name),
        })?;
        Ok(port_def(frame, cx, &p.name))
    };
    let mut inputs: Vec<PortDef> = match ty.kind {
        Kind::Sequence => vec![metor_fsw_3::sequence::abort_def(cx)],
        Kind::Cyclic => Vec::new(),
    };
    inputs.extend(ty.inputs.iter().map(port).collect::<Result<Vec<_>, _>>()?);
    let mut outputs: Vec<PortDef> = ty.outputs.iter().map(port).collect::<Result<_, _>>()?;
    let log = outputs.len();
    outputs.push(Output::<LogEvent>::def(cx, LOG_PORT));
    Ok(SystemDef {
        name: ty.name.clone().into(),
        inputs,
        outputs,
        log: Some(log),
        grants: Vec::new(),
        io: None,
    })
}

/// The rings of an instance's compiled ports.
pub struct Rings {
    inputs: Vec<Input<Bytes>>,
    outputs: Vec<Output<Bytes>>,
    /// Counts failed writes, which the step reports once per cycle.
    pub dropped: Arc<AtomicU32>,
}

impl Rings {
    /// Binds the compiled ports, returning them and the `log` port.
    ///
    /// `inputs` and `outputs` are the bindings of the compiled ports and
    /// `log`, in definition order, with any `abort` input taken off.
    pub fn bind(inputs: Vec<InputBinding>, mut outputs: Vec<OutputBinding>) -> Option<(Rings, LogPort)> {
        let log = outputs.pop()?;
        let dropped = Arc::new(AtomicU32::new(0));
        let rings = Rings {
            inputs: inputs.into_iter().map(|b| Input::new(b.views)).collect(),
            outputs: outputs
                .into_iter()
                .map(|b| {
                    let max_len = b.def.max_len;
                    Output::<Bytes>::with_dropped_counter(b.writer, max_len, dropped.clone())
                })
                .collect(),
            dropped,
        };
        Some((rings, LogPort::new(Output::new(log.writer))))
    }

    /// Copies the newest record of each input into the input block.
    ///
    /// An input that has had no record keeps its zeroes.
    pub fn refresh(&mut self, instance: &mut Instance) {
        for (index, input) in self.inputs.iter_mut().enumerate() {
            let Ok(Some(latest)) = input.latest() else {
                continue;
            };
            let bytes: &[u8] = latest.into_inner();
            if let Some(dst) = instance.input_mut(index) {
                let len = dst.len().min(bytes.len());
                dst[..len].copy_from_slice(&bytes[..len]);
            }
        }
    }

    /// Writes every output the instance marked, stamped with `now`.
    pub fn flush(&mut self, instance: &mut Instance, now: i64) {
        for (index, output) in self.outputs.iter_mut().enumerate() {
            if !instance.take_written(index) {
                continue;
            }
            instance.stamp(index, now);
            if let Some(bytes) = instance.output(index) {
                let _ = output.write(bytes);
            }
        }
    }
}
