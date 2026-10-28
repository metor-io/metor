use metor_fsw_3_ring::NoWake;
use metor_proto_wkt::{SequenceChannelState, SequenceCommand, SequenceRegistry};

use super::*;
use crate::def::DefCx;
use crate::port::Output;
use crate::record::RecordSchema;
use crate::system::PortDef;
use crate::tests::utils::pair;

#[test]
fn test_wkt_records_keep_panel_ids() {
    assert_eq!(SequenceCommand::schema().packet_id(), [225, 247]);
    assert_eq!(SequenceRegistry::schema().packet_id(), [78, 109]);
}

#[test]
fn test_abort_requested_latches() {
    let (_ring, mut out, input) = pair::<AbortRequest>(4);
    let mut abort = Abort::new(input);
    assert!(!abort.requested());
    out.write(&AbortRequest {}).expect("room");
    assert!(abort.requested());
    assert!(abort.requested());
}

#[test]
fn test_abort_without_producer() {
    let mut abort = Abort::<NoWake>::new(Input::new(Vec::new()));
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
    assert_eq!(channel_state(RunState::Running, None).code(), 0);
    assert_eq!(channel_state(RunState::Stopped, None).code(), 1);
    assert_eq!(
        channel_state(RunState::Running, Some(&EndReason::Completed)),
        SequenceChannelState::Completed
    );
    assert_eq!(
        channel_state(RunState::Stopped, Some(&EndReason::Failed("x".into()))),
        SequenceChannelState::Failed
    );
    assert_eq!(
        channel_state(RunState::Stopped, Some(&EndReason::Unloaded)).code(),
        6
    );
}

#[test]
fn test_channel_status_layout() {
    let RecordSchema::Frame { metadata, .. } = ChannelStatus::schema() else {
        panic!("a frame")
    };
    let artifact = metadata
        .iter()
        .find(|m| m.name.ends_with("artifact"))
        .expect("an artifact component");
    assert!(artifact.is_string());
    let hashes = metadata.iter().filter(|m| m.name.contains("hash")).count();
    assert_eq!(hashes, 1);
    let names: Vec<_> = metadata.iter().map(|m| m.name.as_str()).collect();
    assert!(names.iter().any(|n| n.ends_with("accepts")), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("message")), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("pad")), "{names:?}");
    let port: PortDef = Output::<ChannelStatus>::def(&DefCx::empty(), STATUS_PORT);
    assert_eq!(port.max_len, size_of::<ChannelStatus>());
}

#[test]
fn test_channel_message_layout() {
    let RecordSchema::Frame { metadata, .. } = ChannelMessage::schema() else {
        panic!("a frame")
    };
    let [text] = metadata.as_slice() else {
        panic!("one component, got {metadata:?}")
    };
    assert!(text.name.ends_with("text"), "{}", text.name);
    assert!(text.is_string());
    let port: PortDef = Output::<ChannelMessage>::def(&DefCx::empty(), MESSAGE_PORT);
    assert_eq!(port.max_len, size_of::<ChannelMessage>());
    assert_eq!(size_of::<ChannelMessage>(), 8 + MESSAGE_LEN);
}
