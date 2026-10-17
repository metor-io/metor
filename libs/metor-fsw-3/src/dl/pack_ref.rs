//! Locates a pack's build output, using the file names cargo gives it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{DlPack, Pack, PackError};
use crate::wasm::{Limits, WasmPack};

/// How a pack is built and loaded.
///
/// It is written as a `kind` key, which a dylib leaves out, and a wasm pack's
/// limits beside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "KindFields", into = "KindFields")]
pub enum PackKind {
    /// A shared library for this host.
    #[default]
    Dylib,
    /// A WebAssembly module. Each system instance runs in its own guest,
    /// within these limits.
    Wasm(Limits),
}

/// The keys a [`PackKind`] is written as.
#[derive(Clone, Copy, Serialize, Deserialize)]
struct KindFields {
    #[serde(default, skip_serializing_if = "KindTag::is_dylib")]
    kind: KindTag,
    #[serde(flatten)]
    limits: Limits,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum KindTag {
    #[default]
    Dylib,
    Wasm,
}

impl KindTag {
    fn is_dylib(&self) -> bool {
        *self == KindTag::Dylib
    }
}

impl TryFrom<KindFields> for PackKind {
    type Error = &'static str;

    fn try_from(fields: KindFields) -> Result<Self, &'static str> {
        match fields.kind {
            KindTag::Dylib if fields.limits != Limits::default() => {
                Err("a dylib pack takes no `memory` or `fuel`")
            }
            KindTag::Dylib => Ok(PackKind::Dylib),
            KindTag::Wasm => Ok(PackKind::Wasm(fields.limits)),
        }
    }
}

impl From<PackKind> for KindFields {
    fn from(kind: PackKind) -> Self {
        match kind {
            PackKind::Dylib => KindFields {
                kind: KindTag::Dylib,
                limits: Limits::default(),
            },
            PackKind::Wasm(limits) => KindFields {
                kind: KindTag::Wasm,
                limits,
            },
        }
    }
}

/// Locates one pack whose system types a config uses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackRef {
    /// The prefix the pack's types are registered under.
    pub id: String,
    /// The file stem of the pack's build output.
    pub lib: String,
    /// The directory with one subdirectory per target triple.
    pub libs: PathBuf,
    #[serde(flatten)]
    pub kind: PackKind,
}

impl PackRef {
    /// Creates a reference to a dylib pack.
    pub fn dylib(id: String, lib: String, libs: PathBuf) -> Self {
        Self {
            id,
            lib,
            libs,
            kind: PackKind::Dylib,
        }
    }

    /// Returns the path of the pack's build output.
    ///
    /// That is the library for this host, or the wasm module.
    pub fn artifact(&self) -> PathBuf {
        match self.kind {
            PackKind::Dylib => self.libs.join(triple()).join(cdylib_name(&self.lib)),
            PackKind::Wasm(_) => self.libs.join(WASM_TRIPLE).join(wasm_name(&self.lib)),
        }
    }

    /// Opens the pack's build output as the kind of pack it is.
    pub fn open(&self) -> Result<Pack, PackError> {
        let path = self.artifact();
        match self.kind {
            // SAFETY: a pack reference points at a library that `pack dev`
            // built against this ABI. `open` checks the version before calling
            // anything else.
            PackKind::Dylib => unsafe { DlPack::open(&path) }.map(Pack::Dl),
            PackKind::Wasm(limits) => {
                let bytes = std::fs::read(&path).map_err(PackError::Read)?;
                WasmPack::open(&bytes, &limits).map(Pack::Wasm)
            }
        }
    }
}

/// Returns the host's target triple, spelled the way cargo spells it.
pub fn triple() -> String {
    let arch = std::env::consts::ARCH;
    #[cfg(target_os = "macos")]
    let rest = "apple-darwin";
    #[cfg(all(target_os = "linux", target_env = "musl"))]
    let rest = "unknown-linux-musl";
    #[cfg(all(target_os = "linux", not(target_env = "musl")))]
    let rest = "unknown-linux-gnu";
    format!("{arch}-{rest}")
}

/// Returns the file name cargo gives a cdylib called `stem` on this host.
pub fn cdylib_name(stem: &str) -> String {
    format!(
        "{}{stem}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

/// The target triple a wasm pack is built for.
pub const WASM_TRIPLE: &str = "wasm32-unknown-unknown";

/// Returns the file name of a wasm module called `stem`.
pub fn wasm_name(stem: &str) -> String {
    format!("{stem}.wasm")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wasm_pack_ref_round_trips() {
        let json = serde_json::json!({
            "id": "adcs", "lib": "adcs_systems", "libs": "/l",
            "kind": "wasm", "memory": 1 << 20, "fuel": 5_000,
        });
        let pack: PackRef = serde_json::from_value(json.clone()).expect("decodes");
        let limits = Limits {
            memory: Some(1 << 20),
            fuel: Some(5_000),
        };
        assert_eq!(pack.kind, PackKind::Wasm(limits));
        assert_eq!(serde_json::to_value(&pack).expect("encodes"), json);
        let module = PathBuf::from("/l/wasm32-unknown-unknown/adcs_systems.wasm");
        assert_eq!(pack.artifact(), module);
    }

    #[test]
    fn test_pack_kind_defaults_to_dylib() {
        let json = serde_json::json!({ "id": "adcs", "lib": "adcs_systems", "libs": "/l" });
        let pack: PackRef = serde_json::from_value(json.clone()).expect("decodes");
        assert_eq!(pack.kind, PackKind::Dylib);
        let written = serde_json::to_value(&pack).expect("encodes");
        assert_eq!(written, json);
    }

    #[test]
    fn test_dylib_with_limits_is_refused() {
        let json = serde_json::json!({ "id": "a", "lib": "a", "libs": "/l", "fuel": 5 });
        assert!(serde_json::from_value::<PackRef>(json).is_err());
    }

    #[test]
    fn test_missing_module_is_read_error() {
        let pack = PackRef {
            kind: PackKind::Wasm(Limits::default()),
            ..PackRef::dylib("a".into(), "a".into(), "/no/such/dir".into())
        };
        assert!(matches!(pack.open(), Err(PackError::Read(_))));
    }
}
