//! The JSON every pack kind sends across the ABI and reads back.

use crate::coordinator::ParamError;
use crate::def::{DefCx, DefError};
use crate::pack::DefStatus;
use crate::pack::def::{DefCxOwned, Instance, PackDef};
use crate::system::SystemDef;

use super::PackError;

/// The bytes the host lends an export to write a descriptor or definition into.
pub(crate) const DESCRIPTOR_CAPACITY: usize = 1024 * 1024;

/// The descriptor an export wrote, `None` when it reported writing past the
/// buffer.
pub(crate) fn decode_descriptor(status: u32, bytes: Option<&[u8]>) -> Result<PackDef, PackError> {
    if status == DefStatus::TooSmall as u32 {
        return Err(PackError::DescriptorTooLarge {
            capacity: DESCRIPTOR_CAPACITY,
        });
    }
    if status != DefStatus::Ok as u32 {
        return Err(PackError::DescriptorStatus(status));
    }
    serde_json::from_slice(bytes.ok_or(PackError::Decode)?).map_err(|_| PackError::Decode)
}

/// One type's definition, or the refusal, an export wrote.
pub(crate) fn decode_def(status: u32, bytes: Option<&[u8]>) -> Result<SystemDef, DefError> {
    let bytes = bytes.ok_or_else(|| refused("the pack reported more than it wrote".into()))?;
    if status == DefStatus::Refused as u32 {
        return Err(serde_json::from_slice(bytes).unwrap_or_else(|e| refused(e.to_string())));
    }
    if status != DefStatus::Ok as u32 {
        return Err(refused(format!(
            "definition export failed with status {status}"
        )));
    }
    serde_json::from_slice(bytes).map_err(|e| refused(e.to_string()))
}

/// The error a failed `create` wrote, or names the failure it could not.
pub(crate) fn decode_create_error(bytes: &[u8]) -> ParamError {
    if bytes.is_empty() {
        return ParamError::Decode("pack create failed".to_string());
    }
    serde_json::from_slice(bytes).unwrap_or_else(|e| ParamError::Decode(e.to_string()))
}

/// A definition context, owned for the crossing.
pub(crate) fn encode_def_cx(cx: &DefCx<'_>) -> Result<Vec<u8>, DefError> {
    serde_json::to_vec(&DefCxOwned::of(cx)).map_err(|e| refused(e.to_string()))
}

/// The params and instance `create` takes.
pub(crate) fn encode_instance(
    params: &serde_json::Value,
    instance: &Instance,
) -> Result<(Vec<u8>, Vec<u8>), ParamError> {
    let encode = |e: serde_json::Error| ParamError::Decode(e.to_string());
    let params = serde_json::to_vec(params).map_err(encode)?;
    let instance = serde_json::to_vec(instance).map_err(encode)?;
    Ok((params, instance))
}

fn refused(message: String) -> DefError {
    DefError::Pack { message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::SystemTable;

    /// An empty table's descriptor, as the export writes it.
    fn empty_table() -> Vec<u8> {
        let mut bytes = vec![0; 4096];
        let mut written = 0;
        // SAFETY: the buffer and length slot are writable, separate, and live for the call.
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
        let def = decode_descriptor(DefStatus::Ok as u32, Some(&bytes)).expect("decodes");
        assert!(def.systems.is_empty());
    }

    #[test]
    fn test_descriptor_export_errors() {
        assert!(matches!(
            decode_descriptor(DefStatus::TooSmall as u32, None),
            Err(PackError::DescriptorTooLarge {
                capacity: DESCRIPTOR_CAPACITY
            })
        ));
        for status in [2, 3, 99] {
            assert!(matches!(decode_descriptor(status, Some(b"{}")),
                Err(PackError::DescriptorStatus(found)) if found == status));
        }
    }

    #[test]
    fn test_descriptor_invalid_length_and_json() {
        for bytes in [None, Some(&b""[..]), Some(b"{")] {
            assert!(matches!(
                decode_descriptor(DefStatus::Ok as u32, bytes),
                Err(PackError::Decode)
            ));
        }
    }

    #[test]
    fn test_def_refusal_and_status() {
        let refusal =
            serde_json::to_vec(&DefError::Unresolved { port: "out".into() }).expect("encodes");
        assert_eq!(
            decode_def(DefStatus::Refused as u32, Some(&refusal)),
            Err(DefError::Unresolved { port: "out".into() })
        );
        assert!(matches!(
            decode_def(DefStatus::Encode as u32, Some(b"")),
            Err(DefError::Pack { message }) if message.contains("status 2")
        ));
        assert!(matches!(
            decode_def(DefStatus::Ok as u32, None),
            Err(DefError::Pack { .. })
        ));
    }

    #[test]
    fn test_create_error() {
        let error = serde_json::to_vec(&ParamError::UnknownKey("gian".into())).expect("encodes");
        assert_eq!(
            decode_create_error(&error),
            ParamError::UnknownKey("gian".into())
        );
        assert_eq!(
            decode_create_error(b""),
            ParamError::Decode("pack create failed".into())
        );
    }
}
