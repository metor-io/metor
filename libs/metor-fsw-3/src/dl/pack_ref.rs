//! A reference to a pack's built artifact, and the names cargo gives it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{DlPack, Pack, PackError};
use crate::wasm::{Limits, WasmPack};

/// How a pack is built and loaded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackKind {
    /// A shared library for this host.
    #[default]
    Dylib,
    /// A WebAssembly module, each system instance in a guest of its own.
    Wasm,
}

impl PackKind {
    fn is_dylib(&self) -> bool {
        *self == PackKind::Dylib
    }
}

/// One pack the config's types come from: its id prefix, its artifact stem,
/// the per-triple directory its generated module located, and its kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackRef {
    pub id: String,
    pub lib: String,
    pub libs: PathBuf,
    #[serde(default, skip_serializing_if = "PackKind::is_dylib")]
    pub kind: PackKind,
    /// A wasm pack's limits.
    #[serde(flatten)]
    pub limits: Limits,
}

impl PackRef {
    /// A dylib pack's reference.
    pub fn dylib(id: String, lib: String, libs: PathBuf) -> Self {
        Self {
            id,
            lib,
            libs,
            kind: PackKind::Dylib,
            limits: Limits::default(),
        }
    }

    /// The pack's library for this host, or its module.
    pub fn artifact(&self) -> PathBuf {
        match self.kind {
            PackKind::Dylib => self.libs.join(triple()).join(cdylib_name(&self.lib)),
            PackKind::Wasm => self.libs.join(WASM_TRIPLE).join(wasm_name(&self.lib)),
        }
    }

    /// Opens the artifact as its kind says.
    pub fn open(&self) -> Result<Pack, PackError> {
        let path = self.artifact();
        match self.kind {
            // SAFETY: a pack reference names a library `pack dev` built
            // against this ABI; `open` checks the version before calling
            // anything else.
            PackKind::Dylib => unsafe { DlPack::open(&path) }.map(Pack::Dl),
            PackKind::Wasm => {
                let bytes = std::fs::read(&path).map_err(PackError::Read)?;
                WasmPack::open(&bytes, &self.limits).map(Pack::Wasm)
            }
        }
    }
}

/// The host target triple, as cargo prints it.
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

/// The file name a cdylib of `stem` has on this host.
pub fn cdylib_name(stem: &str) -> String {
    format!(
        "{}{stem}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

/// The target a pack is built for as a module.
pub const WASM_TRIPLE: &str = "wasm32-unknown-unknown";

/// The file name a module of `stem` has.
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
        assert_eq!(pack.kind, PackKind::Wasm);
        let limits = Limits {
            memory: Some(1 << 20),
            fuel: Some(5_000),
        };
        assert_eq!(pack.limits, limits);
        assert_eq!(serde_json::to_value(&pack).expect("encodes"), json);
        let module = PathBuf::from("/l/wasm32-unknown-unknown/adcs_systems.wasm");
        assert_eq!(pack.artifact(), module);
    }

    #[test]
    fn test_pack_kind_defaults_to_dylib() {
        let json = serde_json::json!({ "id": "adcs", "lib": "adcs_systems", "libs": "/l" });
        let pack: PackRef = serde_json::from_value(json.clone()).expect("decodes");
        assert_eq!(pack.kind, PackKind::Dylib);
        assert_eq!(pack.limits, Limits::default());
        let written = serde_json::to_value(&pack).expect("encodes");
        assert_eq!(written, json);
    }

    #[test]
    fn test_missing_module_is_read_error() {
        let pack = PackRef {
            kind: PackKind::Wasm,
            ..PackRef::dylib("a".into(), "a".into(), "/no/such/dir".into())
        };
        assert!(matches!(pack.open(), Err(PackError::Read(_))));
    }
}
