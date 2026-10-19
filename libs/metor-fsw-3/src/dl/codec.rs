//! Encodes and decodes the JSON sent across the pack ABI.
//!
//! The host and packs of both kinds use the same encoding.

use crate::coordinator::{MakeError, ParamError};
use crate::def::{DefCx, DefError};
use crate::pack::DefStatus;
use crate::pack::def::{DefCxOwned, Instance, PackDef};
use crate::system::SystemDef;

use super::PackError;

/// The size of the buffer an export writes a descriptor or definition into.
pub(crate) const DESCRIPTOR_CAPACITY: usize = 1024 * 1024;

impl PackDef {
    /// Decodes the descriptor an export wrote.
    ///
    /// `bytes` is `None` when the export reported writing more than the buffer
    /// holds.
    pub(crate) fn decode(status: u32, bytes: Option<&[u8]>) -> Result<Self, PackError> {
        match DefStatus::try_from(status) {
            Ok(DefStatus::Ok) => serde_json::from_slice(bytes.ok_or(PackError::Decode)?)
                .map_err(|_| PackError::Decode),
            Ok(DefStatus::TooSmall) => Err(PackError::DescriptorTooLarge {
                capacity: DESCRIPTOR_CAPACITY,
            }),
            _ => Err(PackError::DescriptorStatus(status)),
        }
    }
}

impl SystemDef {
    /// Decodes the definition an export wrote, or the error it wrote in its
    /// place.
    pub(crate) fn decode(status: u32, bytes: Option<&[u8]>) -> Result<Self, DefError> {
        let bytes = bytes.ok_or_else(|| refused("the pack reported more than it wrote".into()))?;
        match DefStatus::try_from(status) {
            Ok(DefStatus::Ok) => serde_json::from_slice(bytes).map_err(|e| refused(e.to_string())),
            Ok(DefStatus::Refused) => {
                Err(serde_json::from_slice(bytes).unwrap_or_else(|e| refused(e.to_string())))
            }
            _ => Err(refused(format!(
                "definition export failed with status {status}"
            ))),
        }
    }
}

impl MakeError {
    /// Checks the status a `create` export returned, decoding the error it
    /// wrote when the type refused.
    ///
    /// `bytes` is `None` when the export reported writing more than the buffer
    /// holds.
    pub(crate) fn check_create(status: u32, bytes: Option<&[u8]>) -> Result<(), Self> {
        match DefStatus::try_from(status) {
            Ok(DefStatus::Ok) => Ok(()),
            Ok(DefStatus::Refused) => Err(match bytes {
                Some(bytes) => {
                    serde_json::from_slice(bytes).unwrap_or_else(|e| Self::Pack(e.to_string()))
                }
                None => Self::Pack("reported more than it wrote".into()),
            }),
            Ok(DefStatus::TooSmall) => Err(Self::Pack(format!(
                "error does not fit in {DESCRIPTOR_CAPACITY} bytes"
            ))),
            Ok(DefStatus::Encode) => Err(Self::Pack("error does not encode".into())),
            Ok(DefStatus::Panicked) => Err(Self::Pack("panicked".into())),
            Err(status) => Err(Self::Pack(format!("returned status {status}"))),
        }
    }
}

impl DefCxOwned {
    /// Encodes `cx` to send to a pack.
    pub(crate) fn encode(cx: &DefCx<'_>) -> Result<Vec<u8>, DefError> {
        serde_json::to_vec(&DefCxOwned::of(cx)).map_err(|e| refused(e.to_string()))
    }
}

impl Instance {
    /// Encodes `params` and this instance as a pack's `create` takes them.
    pub(crate) fn encode(
        &self,
        params: &serde_json::Value,
    ) -> Result<(Vec<u8>, Vec<u8>), MakeError> {
        let encode = |e: serde_json::Error| ParamError::Decode(e.to_string());
        let params = serde_json::to_vec(params).map_err(encode)?;
        let instance = serde_json::to_vec(self).map_err(encode)?;
        Ok((params, instance))
    }
}

fn refused(message: String) -> DefError {
    DefError::Pack { message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::SystemTable;

    /// Returns the descriptor the export writes for an empty table.
    fn empty_table() -> Vec<u8> {
        let mut bytes = vec![0; 4096];
        let mut written = 0;
        // SAFETY: the buffer and the length slot are writable, do not overlap, and
        // live for the whole call.
        let status = unsafe {
            crate::pack::def(
                SystemTable::new,
                bytes.as_mut_ptr(),
                bytes.len(),
                &mut written,
            )
        };
        assert_eq!(status, DefStatus::Ok as u32);
        bytes.truncate(written);
        bytes
    }

    #[test]
    fn test_descriptor_decodes() {
        let bytes = empty_table();
        let def = PackDef::decode(DefStatus::Ok as u32, Some(&bytes)).expect("decodes");
        assert!(def.systems.is_empty());
    }

    #[test]
    fn test_descriptor_export_errors() {
        assert!(matches!(
            PackDef::decode(DefStatus::TooSmall as u32, None),
            Err(PackError::DescriptorTooLarge {
                capacity: DESCRIPTOR_CAPACITY
            })
        ));
        for status in [2, 3, 99] {
            assert!(matches!(PackDef::decode(status, Some(b"{}")),
                Err(PackError::DescriptorStatus(found)) if found == status));
        }
    }

    #[test]
    fn test_descriptor_invalid_length_and_json() {
        for bytes in [None, Some(&b""[..]), Some(b"{")] {
            assert!(matches!(
                PackDef::decode(DefStatus::Ok as u32, bytes),
                Err(PackError::Decode)
            ));
        }
    }

    #[test]
    fn test_def_refusal_and_status() {
        let refusal = serde_json::to_vec(&DefError::FanIn { port: "out".into() }).expect("encodes");
        assert_eq!(
            SystemDef::decode(DefStatus::Refused as u32, Some(&refusal)),
            Err(DefError::FanIn { port: "out".into() })
        );
        assert!(matches!(
            SystemDef::decode(DefStatus::Encode as u32, Some(b"")),
            Err(DefError::Pack { message }) if message.contains("status 2")
        ));
        assert!(matches!(
            SystemDef::decode(DefStatus::Ok as u32, None),
            Err(DefError::Pack { .. })
        ));
    }

    #[test]
    fn test_create_status() {
        let refusal: MakeError = ParamError::UnknownKey("gian".into()).into();
        let bytes = serde_json::to_vec(&refusal).expect("encodes");
        let check =
            |status: DefStatus, bytes: Option<&[u8]>| MakeError::check_create(status as u32, bytes);
        assert_eq!(check(DefStatus::Ok, Some(b"")), Ok(()));
        assert_eq!(check(DefStatus::Refused, Some(&bytes)), Err(refusal));
        for (status, why) in [
            (DefStatus::Refused, None),
            (DefStatus::TooSmall, Some(&b""[..])),
            (DefStatus::Panicked, Some(&b""[..])),
        ] {
            assert!(
                matches!(check(status, why), Err(MakeError::Pack(_))),
                "{status:?}"
            );
        }
        assert_eq!(
            MakeError::check_create(9, None),
            Err(MakeError::Pack("returned status 9".into()))
        );
    }
}
