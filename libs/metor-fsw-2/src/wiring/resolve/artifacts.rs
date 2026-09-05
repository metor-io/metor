//! Artifact loading, caching, manifest checks, and entry selection.

use std::collections::HashMap;
use std::sync::Arc;

use metor_fsw_2_core::SystemDescriptor;

use crate::dl::DlSystem;
use crate::wiring::pack_module;
use crate::wiring::{Artifact, LoadError, LoadErrorKind, ParamSource, Wiring, encode_value_params};

/// The per-resolve cache of opened packs, keyed by artifact id, so an
/// artifact serving several systems or occupants is opened (and its pack
/// constructed) exactly once.
#[derive(Default)]
pub(super) struct PackCache {
    packs: HashMap<String, crate::dl::DlPack>,
}

impl PackCache {
    /// The opened pack for `artifact_id`, opening it on first use.
    pub(super) fn open(
        &mut self,
        wiring: &Wiring,
        artifact_id: &str,
        owner: &str,
    ) -> Result<&crate::dl::DlPack, LoadError> {
        if !self.packs.contains_key(artifact_id) {
            let artifact = find_built_artifact(wiring, artifact_id, owner)?;
            let path = artifact
                .path
                .as_ref()
                .expect("checked by find_built_artifact");
            let pack = crate::dl::DlPack::open(path).map_err(|source| {
                LoadErrorKind::DlOpen {
                    system: owner.to_string(),
                    artifact: artifact_id.to_string(),
                    source: Box::new(source),
                }
                .bare()
            })?;
            self.packs.insert(artifact_id.to_string(), pack);
        }
        Ok(&self.packs[artifact_id])
    }
}

/// The per-resolve cache of described wasm artifacts, the [`PackCache`] twin:
/// bytes read once, the pack manifest decoded through a short-lived describe
/// instance, and, for a compiled Python pack, the expr manifest read
/// alongside, since edge synthesis and `@rng` seeding both key off it.
#[derive(Default)]
pub(super) struct WasmCache {
    pub(super) modules: HashMap<String, WasmModule>,
}

/// One described wasm artifact.
pub(super) struct WasmModule {
    pub(super) bytes: Arc<Vec<u8>>,
    pub(super) entries: Vec<metor_fsw_2_core::abi::PackEntryDesc>,
    /// The expr manifest a compiled Python pack also bakes; `None` for an
    /// ordinary Rust-authored pack.
    pub(super) expr: Option<metor_expr::Manifest>,
}

impl WasmCache {
    /// The described module for `artifact_id`, reading and describing it on
    /// first use.
    pub(super) fn open(
        &mut self,
        wiring: &Wiring,
        artifact_id: &str,
        owner: &str,
        max_memory: usize,
    ) -> Result<&WasmModule, LoadError> {
        if !self.modules.contains_key(artifact_id) {
            let bad = |detail: String| {
                LoadErrorKind::WasmSystem(
                    format!("`{owner}`: wasm artifact `{artifact_id}`: {detail}").into_boxed_str(),
                )
                .bare()
            };
            let artifact = find_built_artifact(wiring, artifact_id, owner)?;
            let path = artifact
                .path
                .as_ref()
                .expect("checked by find_built_artifact");
            let bytes =
                std::fs::read(path).map_err(|e| bad(format!("reading {}: {e}", path.display())))?;
            let mut pack = crate::wasm::WasmPack::open_with_memory_limit(
                &bytes,
                crate::coordinator::slot::WASM_SETUP_FUEL,
                max_memory,
            )
            .map_err(|e| bad(e.to_string()))?;
            let entries = pack.manifest().systems.clone();
            let expr = match pack.expr_manifest_bytes().map_err(|e| bad(e.to_string()))? {
                Some(manifest) => Some(
                    metor_expr::describe(&manifest)
                        .map_err(|e| bad(format!("expr manifest: {e}")))?,
                ),
                None => None,
            };
            self.modules.insert(
                artifact_id.to_string(),
                WasmModule {
                    bytes: Arc::new(bytes),
                    entries,
                    expr,
                },
            );
        }
        Ok(&self.modules[artifact_id])
    }
}

/// The two ways an artifact self-describes at resolve: a pack opened in
/// process, or the manifest a describe worker reported for a `process=#true`
/// system or slot (whose artifacts the host never dlopens).
pub(super) enum EntrySource<'a> {
    Opened {
        pack: &'a crate::dl::DlPack,
        artifact: &'a str,
    },
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Described {
        entries: &'a [metor_fsw_2_core::abi::PackEntryDesc],
        artifact: &'a str,
    },
}

impl EntrySource<'_> {
    /// The exported entry names, in manifest order.
    pub(super) fn entry_names(&self) -> Vec<String> {
        match self {
            EntrySource::Opened { pack, .. } => pack.system_names().map(str::to_string).collect(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            EntrySource::Described { entries, .. } => entries
                .iter()
                .map(|e| e.descriptor.name.to_string())
                .collect(),
        }
    }

    /// The sole exported entry name, in which case a spec may omit `type=`.
    pub(super) fn sole_entry(&self) -> Option<&str> {
        match self {
            EntrySource::Opened { pack, .. } => pack.sole_system(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            EntrySource::Described { entries, .. } => match entries {
                [only] => Some(only.descriptor.name.as_str()),
                _ => None,
            },
        }
    }

    /// The artifact id, for diagnostics.
    pub(super) fn artifact(&self) -> &str {
        match self {
            EntrySource::Opened { artifact, .. } => artifact,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            EntrySource::Described { artifact, .. } => artifact,
        }
    }
}

/// What [`resolve_occupant`] selected for registration: the loaded handle on
/// the dl path (the builder takes it by value), or the descriptor alone on
/// the process path (a worker owns the code; the host keeps the shape).
pub(super) enum OccupantEntry {
    Opened(DlSystem),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Described(SystemDescriptor),
}

impl OccupantEntry {
    /// The loaded handle behind an [`EntrySource::Opened`] resolve.
    pub(super) fn opened(self) -> DlSystem {
        match self {
            OccupantEntry::Opened(loaded) => loaded,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            OccupantEntry::Described(_) => unreachable!("selected from an opened pack"),
        }
    }

    /// The descriptor behind an [`EntrySource::Described`] resolve.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn described(self) -> SystemDescriptor {
        match self {
            OccupantEntry::Described(desc) => desc,
            OccupantEntry::Opened(_) => unreachable!("selected from a describe worker"),
        }
    }
}

/// Select `entry` from an artifact's self-description (or its sole entry
/// when the spec named none) and encode `params` against the entry's
/// exported `Params` schema, the one open, select, and encode path behind
/// `resolve_dl`, `resolve_proc`, and both of `resolve_slot`'s occupant
/// loops.
///
/// `require_reloadable` is set only for slot occupants, which are
/// instantiated on every load; a wired system is instantiated once, so a
/// non-reloadable `.state(...)` entry is legal there. `owner` names the
/// `system` or `slot` instance in diagnostics.
pub(super) fn resolve_occupant(
    source: &EntrySource<'_>,
    entry: Option<&str>,
    params: &ParamSource,
    owner: &str,
    require_reloadable: bool,
) -> Result<(OccupantEntry, Vec<u8>), LoadError> {
    let name = match entry {
        Some(name) => name,
        None => source.sole_entry().ok_or_else(|| {
            LoadErrorKind::PackTypeRequired {
                system: owner.to_string(),
                artifact: source.artifact().to_string(),
                available: source.entry_names().join(", "),
            }
            .bare()
        })?,
    };
    let pack_system = |source: crate::dl::DlError| {
        LoadErrorKind::PackSystem {
            system: owner.to_string(),
            source: Box::new(source),
        }
        .bare()
    };
    let not_reloadable = || {
        LoadErrorKind::OccupantNotReloadable {
            slot: owner.to_string(),
            occupant: name.to_string(),
        }
        .bare()
    };
    match source {
        EntrySource::Opened { pack, .. } => {
            let loaded = pack.system(name).map_err(pack_system)?;
            if require_reloadable && !loaded.reloadable() {
                return Err(not_reloadable());
            }
            let params = encode_occupant_params(
                params,
                loaded.params_schema(),
                loaded.params_default(),
                owner,
            )?;
            Ok((OccupantEntry::Opened(loaded), params))
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        EntrySource::Described { entries, .. } => {
            let meta = entries
                .iter()
                .find(|e| e.descriptor.name == name)
                .ok_or_else(|| {
                    pack_system(crate::dl::DlError::UnknownPackSystem {
                        name: name.to_string(),
                        available: source.entry_names(),
                    })
                })?;
            if require_reloadable && !meta.reloadable {
                return Err(not_reloadable());
            }
            let params = encode_occupant_params(
                params,
                &meta.params_schema,
                meta.params_default.as_deref(),
                owner,
            )?;
            Ok((OccupantEntry::Described(meta.descriptor.clone()), params))
        }
    }
}

/// Resolve a [`ParamSource`] to canonical postcard bytes against an entry's
/// exported `Params` schema and declared defaults. A value tree is
/// schema-encoded (the host never links the type), producing the same bytes
/// a typed builder's `Postcard` source carries; an absent config takes the
/// defaults verbatim.
pub(super) fn encode_occupant_params(
    params: &ParamSource,
    schema: &postcard_schema::schema::owned::OwnedNamedType,
    defaults: Option<&[u8]>,
    owner: &str,
) -> Result<Vec<u8>, LoadError> {
    Ok(match params {
        ParamSource::None => defaults.unwrap_or_default().to_vec(),
        ParamSource::Postcard(bytes) => bytes.clone(),
        ParamSource::Value(value) => encode_value_params(value, schema, owner, defaults)?,
    })
}

/// Find an [`Artifact`] by id and require its built `path`, the shared front
/// of the dl and process resolve paths.
pub(super) fn find_built_artifact<'w>(
    wiring: &'w Wiring,
    artifact_id: &str,
    owner: &str,
) -> Result<&'w Artifact, LoadError> {
    let artifact = wiring
        .artifacts
        .iter()
        .find(|a| a.id == artifact_id)
        .ok_or_else(|| {
            LoadErrorKind::UnknownArtifact {
                system: owner.to_string(),
                artifact: artifact_id.to_string(),
            }
            .bare()
        })?;
    if artifact.path.is_none() {
        return Err(LoadErrorKind::ArtifactNotBuilt {
            artifact: artifact_id.to_string(),
        }
        .bare());
    }
    Ok(artifact)
}

/// Enforce generated-stub freshness: for each artifact whose stub module
/// recorded a `manifest_hash`, compare it against the live pack manifest and
/// fail with [`LoadErrorKind::StaleStubs`] on a mismatch. Artifacts with no
/// recorded hash (builder-authored, hand-written `pack()` handles) or no built
/// path yet are skipped; the dlopen path still opens them.
pub(super) fn check_manifest_hashes(wiring: &Wiring) -> Result<(), LoadError> {
    for artifact in &wiring.artifacts {
        let Some(recorded) = artifact.manifest_hash.as_deref() else {
            continue;
        };
        let Some(path) = artifact.path.as_deref() else {
            continue;
        };
        // Prefer the build driver's sidecar (no dlopen); otherwise describe in
        // process. A describe failure is not a staleness verdict, so let the
        // later dlopen path surface it.
        let Some(bytes) =
            crate::dl::manifest_sidecar_bytes(path).or_else(|| crate::dl::describe_raw(path).ok())
        else {
            continue;
        };
        if pack_module::manifest_hash(&bytes) != recorded {
            return Err(LoadErrorKind::StaleStubs {
                artifact: artifact.id.clone(),
            }
            .bare());
        }
    }
    Ok(())
}
