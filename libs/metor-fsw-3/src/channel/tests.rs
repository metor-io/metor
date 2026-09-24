use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
use metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind, SequenceRegistry,
};

use super::*;
use crate::def::DefCx;
use crate::port::{Output, ring_capacity};
use crate::record::RecordSchema;
use crate::system::PortDef;

fn ring<T: Record>() -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: ring_capacity(T::MAX_LEN, 4).expect("valid capacity"),
        max_readers: 1,
    })
}

#[test]
fn test_wkt_records_keep_panel_ids() {
    use metor_proto::types::Msg;
    assert_eq!(
        SequenceCommand::schema().packet_id(),
        <SequenceCommand as Msg>::ID
    );
    assert_eq!(SequenceCommand::schema().packet_id(), [225, 247]);
    assert_eq!(SequenceChannelEvent::schema().packet_id(), [133, 5]);
    assert_eq!(SequenceRegistry::schema().packet_id(), [78, 109]);
    assert!(matches!(AbortRequest::schema(), RecordSchema::Msg { .. }));
}

#[test]
fn test_command_round_trip() {
    let command = SequenceCommand {
        channel: "mode".into(),
        command: SequenceCommandKind::Load {
            name: "commissioning".into(),
        },
    };
    let mut buf = [0; SequenceCommand::MAX_LEN];
    let bytes = command.encode(&mut buf).expect("fits");
    let back = SequenceCommand::decode(bytes).expect("decodes");
    assert_eq!(back.channel, "mode");
    assert!(matches!(back.command, SequenceCommandKind::Load { name } if name == "commissioning"));
}

#[test]
fn test_oversized_event_refused() {
    let event = SequenceChannelEvent {
        channel: "mode".into(),
        kind: SequenceEventKind::Failed {
            reason: "r".repeat(SequenceChannelEvent::MAX_LEN),
        },
    };
    let mut buf = [0; SequenceChannelEvent::MAX_LEN];
    assert!(event.encode(&mut buf).is_err());
}

#[test]
fn test_abort_requested_latches() {
    let ring = ring::<AbortRequest>();
    let mut abort =
        Abort::new(Input::try_new(vec![ring.view(NoWake).expect("slot")]).expect("aligned"));
    let mut out =
        Output::<AbortRequest>::try_new(ring.writer(NoWake).expect("writer")).expect("aligned");
    assert!(!abort.requested());
    out.write(&AbortRequest {}).expect("room");
    assert!(abort.requested());
    assert!(abort.requested());
}

#[test]
fn test_abort_without_producer() {
    let mut abort = Abort::<NoWake>::new(Input::try_new(Vec::new()).expect("no views"));
    assert!(!abort.requested());
}

/// Counts aborts on its own port.
#[crate::system]
fn aborts(abort: &mut Abort, seen: &mut Output<AbortRequest>) {
    if abort.requested() {
        let _ = seen.write(&AbortRequest {});
    }
}

fn def_of<S: crate::SystemFn>(_system: S) -> crate::SystemDef {
    <crate::FnSystem<S> as crate::System>::def(&DefCx::empty()).expect("a static definition")
}

#[test]
fn test_abort_param_declares_port() {
    let def = def_of(aborts());
    let names: Vec<_> = def.inputs.iter().map(|port| port.name.as_ref()).collect();
    assert_eq!(names, [ABORT_PORT]);
    assert_eq!(def.inputs[0].record, AbortRequest::NAME);
}

#[test]
fn test_channel_state_codes() {
    assert_eq!(ChannelState::of(RunState::Running, None).code(), 0);
    assert_eq!(ChannelState::of(RunState::Stopped, None).code(), 1);
    assert_eq!(
        ChannelState::of(RunState::Running, Some(&End::Completed)),
        ChannelState::Completed
    );
    assert_eq!(
        ChannelState::of(RunState::Stopped, Some(&End::Failed("x".into()))),
        ChannelState::Failed
    );
    assert_eq!(
        ChannelState::of(RunState::Stopped, Some(&End::Unloaded)).code(),
        6
    );
}

#[test]
fn test_channel_status_layout() {
    assert_eq!(size_of::<ChannelStatus>(), 8 + 4 + 4 + 64 + 32);
    let RecordSchema::Frame { metadata, .. } = ChannelStatus::schema() else {
        panic!("a frame")
    };
    let artifact = metadata
        .iter()
        .find(|m| m.name.ends_with("artifact"))
        .expect("an artifact component");
    assert!(artifact.is_string());
    let port: PortDef = Output::<ChannelStatus>::def(&DefCx::empty(), "channel_status");
    assert_eq!(port.max_len, size_of::<ChannelStatus>());
}
