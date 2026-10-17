//! Encodes and decodes the JSON sent across the pack ABI.
//!
//! The host and packs of both kinds use the same encoding.

use crate::coordinator::ParamError;
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

impl ParamError {
    /// Decodes the error a failed `create` wrote, or a generic error if it
    /// wrote nothing.
    pub(crate) fn from_create_error(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return ParamError::Decode("pack create failed".to_string());
        }
        serde_json::from_slice(bytes).unwrap_or_else(|e| ParamError::Decode(e.to_string()))
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
    ) -> Result<(Vec<u8>, Vec<u8>), ParamError> {
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
    fn test_create_error() {
        let error = serde_json::to_vec(&ParamError::UnknownKey("gian".into())).expect("encodes");
        assert_eq!(
            ParamError::from_create_error(&error),
            ParamError::UnknownKey("gian".into())
        );
        assert_eq!(
            ParamError::from_create_error(b""),
            ParamError::Decode("pack create failed".into())
        );
    }
}
