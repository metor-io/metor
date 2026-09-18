//! The metor-proto bytes a link speaks.

use metor_proto::types::{PACKET_HEADER_LEN, PacketId, PacketTy};

/// Appends one length-prefixed packet, the framing a `LenPacket` builds.
pub(crate) fn append_packet(batch: &mut Vec<u8>, ty: PacketTy, id: PacketId, payload: &[u8]) {
    batch.extend_from_slice(&((PACKET_HEADER_LEN + payload.len()) as u32).to_le_bytes());
    batch.push(ty as u8);
    batch.extend_from_slice(&id);
    batch.push(0);
    batch.extend_from_slice(payload);
}

#[cfg(test)]
mod tests {
    use metor_proto::types::{IntoLenPacket, LenPacket};

    use super::*;

    #[test]
    fn a_packet_frames_as_its_len_packet_does() {
        let mut batch = Vec::new();
        append_packet(&mut batch, PacketTy::Table, [7, 9], b"body");
        let mut expected = LenPacket::table([7, 9], 4);
        expected.extend_from_slice(b"body");
        assert_eq!(batch, expected.into_len_packet().inner);
    }

    #[test]
    fn an_empty_payload_is_the_header_alone() {
        let mut batch = Vec::new();
        append_packet(&mut batch, PacketTy::Msg, [0, 1], b"");
        assert_eq!(batch, vec![4, 0, 0, 0, PacketTy::Msg as u8, 0, 1, 0]);
    }

    #[test]
    fn packets_append_back_to_back() {
        let mut batch = Vec::new();
        append_packet(&mut batch, PacketTy::Msg, [0, 1], b"a");
        append_packet(&mut batch, PacketTy::Msg, [0, 2], b"bb");
        assert_eq!(batch.len(), 8 + 1 + 8 + 2);
        assert_eq!(&batch[8..9], b"a");
    }
}
