//! The Storage page: how much disk the session DB and the per-peer caches
//! take, and a way to clear the caches no panel holds.
//!
//! Node files are sparse, so a size is a walk of every file's allocated
//! blocks rather than a length read. That runs off the UI thread each time
//! the page opens, and the rows read the latest result as it lands.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{App, SharedString};
use metor_db::peer_cache::disk_usage;
use metor_proto_wkt::InstanceId;

use super::commands::SessionGlobal;
use super::{SessionStorage, StorageDirectory};
use crate::inspector::rows::{CommandRow, InspectorRow, NavRow, RowAction, TextRow};

/// The latest measurement, shared between the rows that show it and the
/// task that takes it.
#[derive(Default)]
pub(super) struct Usage {
    session: Option<u64>,
    /// Each cached peer and its size, largest first.
    peers: Vec<(InstanceId, u64)>,
    measuring: bool,
}

pub(super) type SharedUsage = Arc<Mutex<Usage>>;

pub(super) fn summary(cx: &App) -> SharedString {
    let Some(state) = cx.try_global::<SessionGlobal>() else {
        return SharedString::default();
    };
    let usage = state.usage.lock().unwrap();
    match usage.session {
        Some(session) => {
            let cache: u64 = usage.peers.iter().map(|(_, size)| size).sum();
            format!("session {} · cache {}", bytes(session), bytes(cache)).into()
        }
        None => "Disk usage and peer cache".into(),
    }
}

pub(super) fn rows(cx: &App) -> Vec<Box<dyn InspectorRow>> {
    measure(cx);
    let Some(state) = cx.try_global::<SessionGlobal>() else {
        return Vec::new();
    };
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![Box::new(
        TextRow::readonly("Session database".into(), "Measuring…".into()).live(Arc::new(|cx| {
            let usage = usage(cx)?;
            let session = usage.lock().unwrap().session?;
            Some(bytes(session))
        })),
    )];
    let Some(cache) = state.peer_cache.clone() else {
        rows.push(Box::new(TextRow::readonly(
            "Peer cache".into(),
            "No cache directory".into(),
        )));
        return rows;
    };
    rows.push(Box::new(
        TextRow::readonly("Peer cache".into(), "Measuring…".into()).live(Arc::new(|cx| {
            let usage = usage(cx)?;
            let usage = usage.lock().unwrap();
            usage.session?;
            let total: u64 = usage.peers.iter().map(|(_, size)| size).sum();
            Some(format!("{} in {} peers", bytes(total), usage.peers.len()).into())
        })),
    ));
    rows.push(Box::new(NavRow::with_dynamic_summary(
        "Cached peers",
        Box::new(|cx| {
            usage(cx)
                .map(|usage| usage.lock().unwrap().peers.len().to_string().into())
                .unwrap_or_default()
        }),
        Box::new(peer_rows),
    )));
    rows.push(Box::new(TextRow::readonly(
        "Cache directory".into(),
        cache.root().display().to_string().into(),
    )));
    rows.push(Box::new(CommandRow::action(
        "Clear peer cache",
        Arc::new(move |_, cx| {
            let cache = cache.clone();
            let task = cx
                .background_executor()
                .spawn(async move { cache.prune(0, Duration::ZERO, None) });
            cx.spawn(async move |cx| {
                let removed = task.await;
                cx.update(|cx| {
                    if let Some(usage) = usage(cx) {
                        let mut usage = usage.lock().unwrap();
                        usage
                            .peers
                            .retain(|(id, _)| !removed.iter().any(|dir| dir_is(dir, *id)));
                    }
                    measure(cx);
                    cx.refresh_windows();
                });
            })
            .detach();
            RowAction::Handled
        }),
    )));
    rows
}

/// One row per cached peer, marking the one this session mirrors into.
fn peer_rows(cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let Some(state) = cx.try_global::<SessionGlobal>() else {
        return Vec::new();
    };
    let current = match &state.storage {
        Some(SessionStorage {
            directory: StorageDirectory::Cached(cached),
            ..
        }) => Some(cached.path().to_owned()),
        _ => None,
    };
    let usage = state.usage.lock().unwrap();
    if usage.peers.is_empty() {
        return vec![Box::new(TextRow::readonly(
            "No cached peers".into(),
            SharedString::default(),
        ))];
    }
    usage
        .peers
        .iter()
        .map(|(id, size)| {
            let mut label = id.to_string();
            if current.as_deref().is_some_and(|dir| dir_is(dir, *id)) {
                label.push_str(" (this session)");
            }
            Box::new(TextRow::readonly(label.into(), bytes(*size))) as Box<dyn InspectorRow>
        })
        .collect()
}

/// Start a measurement unless one is already running.
fn measure(cx: &App) {
    let Some(state) = cx.try_global::<SessionGlobal>() else {
        return;
    };
    let usage = state.usage.clone();
    {
        let mut usage = usage.lock().unwrap();
        if usage.measuring {
            return;
        }
        usage.measuring = true;
    }
    let session = state
        .storage
        .as_ref()
        .map(|storage| storage.directory.path().to_owned())
        .unwrap_or_else(|| state.db.path.clone());
    let root = state
        .peer_cache
        .as_ref()
        .map(|cache| cache.root().to_owned());
    let task = cx
        .background_executor()
        .spawn(async move { (disk_usage(&session), root.map(|root| peers(&root))) });
    cx.spawn(async move |cx| {
        let (session, peers) = task.await;
        *usage.lock().unwrap() = Usage {
            session: Some(session),
            peers: peers.unwrap_or_default(),
            measuring: false,
        };
        cx.update(|cx| cx.refresh_windows());
    })
    .detach();
}

/// Every peer directory under `root` and its size, largest first.
fn peers(root: &Path) -> Vec<(InstanceId, u64)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut peers: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let id = entry.file_name().to_str()?.parse::<InstanceId>().ok()?;
            Some((id, disk_usage(&entry.path())))
        })
        .collect();
    peers.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
    peers
}

fn dir_is(dir: &Path, id: InstanceId) -> bool {
    dir.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse::<InstanceId>().ok())
        == Some(id)
}

fn usage(cx: &App) -> Option<SharedUsage> {
    Some(cx.try_global::<SessionGlobal>()?.usage.clone())
}

fn bytes(n: u64) -> SharedString {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024. && unit + 1 < UNITS.len() {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B").into()
    } else {
        format!("{value:.1} {}", UNITS[unit]).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sizes_read_in_binary_units() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(20 << 30), "20.0 GiB");
    }

    #[test]
    fn peers_are_listed_largest_first_and_strangers_skipped() {
        let root = tempfile::tempdir().unwrap();
        let cache = metor_db::peer_cache::PeerCache::new(root.path().to_owned());
        let small = cache.open(InstanceId([1; 16])).unwrap().unwrap();
        let large = cache.open(InstanceId([2; 16])).unwrap().unwrap();
        std::fs::write(large.path().join("ballast"), vec![1u8; 64 * 1024]).unwrap();
        std::fs::create_dir(root.path().join("not-a-peer")).unwrap();
        let listed: Vec<_> = peers(root.path()).into_iter().map(|(id, _)| id).collect();
        assert_eq!(listed, vec![InstanceId([2; 16]), InstanceId([1; 16])]);
        assert!(dir_is(small.path(), InstanceId([1; 16])));
        assert!(!dir_is(&PathBuf::from("/tmp/x"), InstanceId([1; 16])));
    }
}
