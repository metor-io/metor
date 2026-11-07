//! `AlarmRaised` and `AlarmCleared` with fixed buffers, so an edge allocates
//! nothing. Each encodes to the same bytes as its wkt message.

use metor_proto_wkt::{AlarmCleared, AlarmRaised};
use serde::{Deserialize, Serialize};

use super::{Id, Msg, Severity};
use crate::record::{DecodeError, EncodeError, Payload, Record, RecordSchema, postcard};

/// `AlarmRaised` with fixed buffers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RaisedMsg {
    pub def_id: Id,
    pub occurrence: u64,
    pub severity: Severity,
    pub value: Option<f64>,
    pub message: Msg,
}

/// `AlarmCleared` with a fixed buffer.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ClearedMsg {
    pub def_id: Id,
    pub occurrence: u64,
}

impl Payload for RaisedMsg {
    type Read<'a> = Self;

    fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        postcard::encode(self, buf)
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        postcard::decode(bytes)
    }
}

impl Record for RaisedMsg {
    const NAME: &'static str = "alarm_raised";
    const MAX_LEN: usize = 256;
    const DEPTH: usize = 4;

    fn schema_with_root(_root: &str) -> RecordSchema {
        RecordSchema::postcard::<AlarmRaised>(Self::NAME)
    }
}

impl Payload for ClearedMsg {
    type Read<'a> = Self;

    fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        postcard::encode(self, buf)
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        postcard::decode(bytes)
    }
}

impl Record for ClearedMsg {
    const NAME: &'static str = "alarm_cleared";
    const MAX_LEN: usize = 128;
    const DEPTH: usize = 4;

    fn schema_with_root(_root: &str) -> RecordSchema {
        RecordSchema::postcard::<AlarmCleared>(Self::NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> Id {
        Id::try_from(s).expect("fits")
    }

    fn raised() -> RaisedMsg {
        let mut message = Msg::default();
        let _ = write!(message, "batt 61.0 C");
        RaisedMsg {
            def_id: id("ns.power.batt_hot"),
            occurrence: 7,
            severity: Severity::Warning,
            value: None,
            message,
        }
    }

    fn encode<T: Payload>(value: &T) -> Vec<u8> {
        let mut buf = [0; 512];
        value.encode(&mut buf).expect("fits").to_vec()
    }

    #[test]
    fn ids_and_schemas_match_wkt() {
        assert_eq!(
            RaisedMsg::schema().packet_id(),
            metor_proto::types::short_id([217, 65])
        );
        assert_eq!(
            ClearedMsg::schema().packet_id(),
            metor_proto::types::short_id([91, 221])
        );
        let wkt = RecordSchema::postcard::<AlarmRaised>("alarm_raised");
        assert_eq!(RaisedMsg::schema(), wkt);
        let wkt = RecordSchema::postcard::<AlarmCleared>("alarm_cleared");
        assert_eq!(ClearedMsg::schema(), wkt);
    }

    #[test]
    fn raised_bytes_decode_as_wkt_and_back() {
        let ours = raised();
        let wkt: AlarmRaised = ::postcard::from_bytes(&encode(&ours)).expect("decodes");
        assert_eq!(wkt.def_id, ours.def_id.as_str());
        assert_eq!(wkt.occurrence, 7);
        assert_eq!(wkt.severity, Severity::Warning);
        assert_eq!(wkt.value, None);
        assert_eq!(wkt.message, "batt 61.0 C");
        let back = ::postcard::to_allocvec(&wkt).expect("encodes");
        assert_eq!(RaisedMsg::decode(&back), Ok(ours));
    }

    #[test]
    fn cleared_bytes_decode_as_wkt_and_back() {
        let ours = ClearedMsg {
            def_id: id("ns.power.batt_hot"),
            occurrence: 9,
        };
        let wkt: AlarmCleared = ::postcard::from_bytes(&encode(&ours)).expect("decodes");
        assert_eq!(
            (wkt.def_id.as_str(), wkt.occurrence),
            ("ns.power.batt_hot", 9)
        );
        let back = ::postcard::to_allocvec(&wkt).expect("encodes");
        assert_eq!(ClearedMsg::decode(&back), Ok(ours));
    }

    #[test]
    fn largest_records_fit_max_len() {
        let mut message = Msg::default();
        let _ = write!(message, "{}", "m".repeat(128));
        let raised = RaisedMsg {
            def_id: id(&"i".repeat(96)),
            occurrence: u64::MAX,
            severity: Severity::Critical,
            value: Some(f64::MAX),
            message,
        };
        assert!(encode(&raised).len() <= RaisedMsg::MAX_LEN);
        let cleared = ClearedMsg {
            def_id: id(&"i".repeat(96)),
            occurrence: u64::MAX,
        };
        assert!(encode(&cleared).len() <= ClearedMsg::MAX_LEN);
    }

    #[test]
    fn overlong_wkt_id_is_a_decode_error() {
        let wkt = AlarmRaised {
            def_id: "i".repeat(97),
            occurrence: 1,
            severity: Severity::Info,
            value: None,
            message: String::new(),
        };
        let bytes = ::postcard::to_allocvec(&wkt).expect("encodes");
        assert_eq!(RaisedMsg::decode(&bytes), Err(DecodeError::Codec));
    }
}
