//! `Publish`, the link that sends a target's records to its peers.

use core::ops::{Deref, DerefMut};

use metor_fsw_3_ring::NoWake;
use metor_proto::types::{PacketId, table_id};

use crate::coordinator::MakeError;
use crate::def::{DefCx, DefError};
use crate::fn_system::{Cycle, Defs, Param, ParamNames, Views, Writers};
use crate::io::{Buf, Cap, Endpoint, IoInit};
use crate::log::Log;
use crate::port::{DynInputs, InputWake, Output};
use crate::record::RecordSchema;
use crate::system::PortDef;
use crate::{Seq, SeqError, sequence};

use super::batch::Batcher;
use super::conns::{Conns, ConnsMem};
use super::{LinkParams, LinkStatus, StatusReporter, check_conn_cap, failed, made_once, wire};

/// A link that sends every record wired into it to its peers over
/// metor-proto.
pub struct Publish {
    params: LinkParams,
    /// The connections' buffers and the batch's, until the body binds them.
    mem: Option<(ConnsMem, Buf)>,
}

impl Publish {
    /// Allocates the connections' buffers and the batch's.
    pub fn new(params: LinkParams, io: &mut IoInit<'_>) -> Result<Self, MakeError> {
        let conns = ConnsMem::make(io, params.sending())?;
        let scratch = io.alloc(params.conn_cap)?;
        Ok(Self {
            params,
            mem: Some((conns, scratch)),
        })
    }
}

/// Runs the `Publish` link.
///
/// Each new connection is first sent the announce, which describes every
/// input port's schema. After that, each cycle the link collects the records
/// waiting on its inputs into a batch and queues it on every connection.
#[sequence]
pub async fn publish(
    #[state] s: &mut Publish,
    seq: &Seq,
    ep: &Cap<Endpoint>,
    inputs: &mut PublishInputs,
    link_status: &mut Output<LinkStatus>,
    log: &mut Log,
) -> Result<(), SeqError> {
    let defs: Vec<PortDef> = inputs.iter_mut().map(|(def, _)| def.clone()).collect();
    let LinkParams {
        namespace, link, ..
    } = &s.params;
    let (announce, ids) = wire::announce(&defs, namespace.as_deref(), link);
    let (mem, scratch) = s.mem.take().ok_or_else(made_once)?;
    let mut conns = Conns::new(mem, ep, &announce, 0).map_err(failed)?;
    let mut batcher = Batcher::new(ids, scratch);
    let mut reporter = StatusReporter::default();
    loop {
        let turn = conns.poll(|_, _| {});
        if let Some(batch) = batcher.fill(inputs) {
            conns.broadcast(batch);
        }
        reporter.report(turn, &conns, 0, seq.now(), link_status, log);
        seq.next_cycle().await;
    }
}

/// A `Publish`'s inputs: one dynamic port per wired output.
///
/// A record is sent under its frame's table id, so the definition rejects two
/// frame ports whose tables share an id.
pub struct PublishInputs<W: InputWake = NoWake>(DynInputs<W>);

impl<W: InputWake> Deref for PublishInputs<W> {
    type Target = DynInputs<W>;
    fn deref(&self) -> &DynInputs<W> {
        &self.0
    }
}

impl<W: InputWake> DerefMut for PublishInputs<W> {
    fn deref_mut(&mut self) -> &mut DynInputs<W> {
        &mut self.0
    }
}

impl<D: InputWake> Param for PublishInputs<D> {
    type In<W: InputWake> = PublishInputs<W>;
    type Out = ();
    type Item<'a, W: InputWake> = &'a mut PublishInputs<W>;

    fn append_defs(
        names: &mut ParamNames<'_>,
        cx: &DefCx<'_>,
        defs: &mut Defs,
    ) -> Result<(), DefError> {
        let first = defs.inputs.len();
        <DynInputs<D> as Param>::append_defs(names, cx, defs)?;
        check_table_ids(&defs.inputs[first..])?;
        check_conn_cap(cx, &defs.inputs[first..])
    }

    fn bind_in<W: InputWake>(views: &mut Views<W>) -> PublishInputs<W> {
        PublishInputs(<DynInputs<D> as Param>::bind_in(views))
    }

    fn bind_out(names: &mut ParamNames<'_>, writers: &mut Writers) -> Result<(), MakeError> {
        <DynInputs<D> as Param>::bind_out(names, writers)
    }

    fn get<'a, W: InputWake>(
        input: &'a mut PublishInputs<W>,
        _output: &'a mut (),
        _cx: &mut Cycle<'a>,
    ) -> &'a mut PublishInputs<W> {
        input
    }
}

/// Rejects two frame ports whose tables share an id.
fn check_table_ids(ports: &[PortDef]) -> Result<(), DefError> {
    let mut seen: Vec<(PacketId, &str)> = Vec::with_capacity(ports.len());
    for port in ports {
        let RecordSchema::Frame { vtable, .. } = &port.schema else {
            continue;
        };
        let id = table_id(vtable);
        if let Some((_, other)) = seen.iter().find(|(known, _)| *known == id) {
            return Err(DefError::TableIdCollision {
                port: port.name.to_string(),
                other: other.to_string(),
            });
        }
        seen.push((id, &port.name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::DefCx;
    use crate::port::Input;
    use crate::tests::utils::{Fixed, Imu};

    use super::*;

    #[test]
    fn test_reject_colliding_tables() {
        let imu = Input::<Imu>::def(&DefCx::empty(), "plant.imu");
        let fixed = Input::<Fixed>::def(&DefCx::empty(), "cmds.fixed");
        let twin = PortDef {
            name: "copy.imu".into(),
            ..imu.clone()
        };
        assert_eq!(check_table_ids(&[imu.clone(), fixed.clone()]), Ok(()));
        assert_eq!(
            check_table_ids(&[imu, fixed.clone(), twin, fixed]),
            Err(DefError::TableIdCollision {
                port: "copy.imu".into(),
                other: "plant.imu".into(),
            }),
            "messages may share an id; tables may not"
        );
    }
}
