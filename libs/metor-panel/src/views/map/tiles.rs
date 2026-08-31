//! Shared tile fetcher and cache behind every map view.
//!
//! One [`TileStore`] entity serves the whole app: map views ask it for tiles
//! during paint and observe it for repaints, so a tile downloaded for one
//! window lights up every map showing that area. Fetching happens on one
//! dedicated thread running a current-thread tokio runtime and gpui's own
//! reqwest fork: one pooled client multiplexes every download over HTTP/2
//! (a per-tile TLS handshake is what makes a map trickle in), throttled to
//! the polite two connections when a job targets the public OSM servers.
//! A disk cache under the user cache directory sits in front of the
//! network, which is also what keeps previously-seen areas working offline.
//!
//! GPU lifetime is the subtle part: every [`RenderImage`] that reaches a
//! window's sprite atlas must eventually pass through `window.drop_image`,
//! but the store has no window. Evictions therefore park images in an
//! orphan list that map views drain inside their paint pass — the same
//! arrangement the 3D viewer uses for retired frames.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, unbounded};
use gpui::{App, AppContext as _, Context, Entity, RenderImage};
use image::Frame;
use smallvec::SmallVec;
use tokio::sync::{Semaphore, mpsc};

use super::mercator::TileId;

/// The default basemap: Mapbox's outdoors style as 512 px @2x raster tiles
/// — the same source, style, and public token metor-ui ships. Served images
/// are 1024 px, so the map draws them shallower (see `Camera::tile_zoom`)
/// and a retina display gets native-density pixels without oversampling.
const MAPBOX_STYLE: &str = "outdoors-v12";
const MAPBOX_TOKEN: &str =
    "pk.eyJ1Ijoic3BodyIsImEiOiJjbWZ0eW4zbXAwb2Z1MmtvZHFsMjlnc2JzIn0.mf3qBgeCNJFyx9h6gZAQTg";

/// Standard OpenStreetMap raster tiles, for configs that point `tile_url`
/// at OSM explicitly.
pub const OSM_TILE_URL: &str = "https://tile.openstreetmap.org/{z}/{x}/{y}.png";

/// One raster tile provider, resolved from a map's `tile_url` config.
///
/// The template is the identity: it substitutes `{z}/{x}/{y}`, names the
/// disk-cache bucket, and its served image size feeds the zoom bias. A
/// custom template is assumed to serve classic 256 px tiles.
pub struct TileSource {
    pub template: String,
    /// Physical pixel edge of one served image.
    pub image_px: f64,
    pub attribution: &'static str,
}

/// The provider a map's `tile_url` names; empty means the Mapbox default.
pub fn source_for(tile_url: &str) -> TileSource {
    if tile_url.is_empty() {
        TileSource {
            template: format!(
                "https://api.mapbox.com/styles/v1/mapbox/{MAPBOX_STYLE}/tiles/512/{{z}}/{{x}}/{{y}}@2x?access_token={MAPBOX_TOKEN}"
            ),
            image_px: 1024.0,
            attribution: "© Mapbox © OpenStreetMap",
        }
    } else {
        TileSource {
            template: tile_url.to_string(),
            image_px: 256.0,
            attribution: "© OpenStreetMap contributors",
        }
    }
}

impl TileSource {
    /// Tile-level bias for this source on a display of `scale_factor` (see
    /// `Camera::tile_zoom`): denser output pixels want deeper tiles, denser
    /// source images want shallower ones.
    pub fn zoom_bias(&self, scale_factor: f64) -> f64 {
        (scale_factor.max(0.5) * 256.0 / self.image_px).log2()
    }

    /// Stable disk-cache bucket for this provider, so switching templates
    /// never serves one provider's tiles under another's name.
    fn cache_bucket(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        self.template.hash(&mut hasher);
        hasher.finish()
    }
}

/// Ready tiles kept in memory. At 256 KiB of BGRA per tile this caps the
/// atlas footprint near 96 MiB — several full screens at any zoom.
const MEMORY_TILE_CAP: usize = 384;

/// How long a failed tile stays failed before a repaint may retry it.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Concurrent downloads in flight — a browser's worth. Fine for Mapbox (a
/// commercial API on the user's own token); the public OSM servers ask for
/// less, which [`OSM_IN_FLIGHT_CAP`] enforces per request rather than by
/// starving every other provider down to two lanes.
const MAX_IN_FLIGHT: usize = 16;

/// The OSM usage policy's concurrency cap for a polite client, applied to
/// any job whose URL points at openstreetmap.org.
const OSM_IN_FLIGHT_CAP: usize = 2;

/// Queued-job bound: a screenful or two beyond what is in flight. A full
/// queue sheds the request instead of stacking up tiles the user has
/// already panned away from.
const QUEUE_CAP: usize = 64;

enum TileState {
    /// Queued or downloading; the entry itself is what dedups requests.
    Pending,
    Ready(Arc<RenderImage>),
    Failed(Instant),
}

struct TileJob {
    id: TileId,
    url: String,
    /// Disk-cache bucket of the provider the URL came from.
    bucket: u64,
}

struct TileResult {
    id: TileId,
    image: Option<Arc<RenderImage>>,
}

/// Hands the shared [`TileStore`] entity to any map view.
pub struct GlobalTileStore(pub Entity<TileStore>);

impl gpui::Global for GlobalTileStore {}

/// The shared tile store, or `None` if it was never initialized (tests).
pub fn try_global(cx: &App) -> Option<Entity<TileStore>> {
    cx.try_global::<GlobalTileStore>().map(|g| g.0.clone())
}

pub struct TileStore {
    tiles: HashMap<TileId, TileState>,
    /// Ready tiles in touch order, oldest first.
    lru: VecDeque<TileId>,
    /// Evicted images awaiting a window to release their atlas slot.
    orphaned: Vec<Arc<RenderImage>>,
    job_tx: mpsc::Sender<TileJob>,
}

impl TileStore {
    pub fn init(cx: &mut App) {
        let (job_tx, job_rx) = mpsc::channel::<TileJob>(QUEUE_CAP);
        let (done_tx, done_rx) = unbounded::<TileResult>();
        std::thread::Builder::new()
            .name("tile-fetch".into())
            .spawn(move || run_fetcher(job_rx, done_tx))
            .expect("spawn tile fetcher");

        let store = cx.new(|_| TileStore {
            tiles: HashMap::new(),
            lru: VecDeque::new(),
            orphaned: Vec::new(),
            job_tx,
        });

        // Marry the blocking result channel to gpui: a background task
        // parks on `recv`, and arrivals land on the main thread as entity
        // updates whose `notify` fans out to every observing map. Results
        // install in batches — one wakeup, one repaint per burst — so a
        // screenful arriving at once costs one frame, not one frame each.
        cx.spawn({
            let store = store.downgrade();
            async move |cx| {
                loop {
                    let batch = cx
                        .background_executor()
                        .spawn({
                            let done_rx = done_rx.clone();
                            async move {
                                let Ok(first) = done_rx.recv() else {
                                    return Vec::new();
                                };
                                let mut batch = vec![first];
                                while let Ok(more) = done_rx.try_recv() {
                                    batch.push(more);
                                }
                                batch
                            }
                        })
                        .await;
                    if batch.is_empty() {
                        break;
                    }
                    let installed = store.update(cx, |store, cx| {
                        for msg in batch {
                            store.install(msg);
                        }
                        cx.notify();
                    });
                    if installed.is_err() {
                        break;
                    }
                }
            }
        })
        .detach();

        cx.set_global(GlobalTileStore(store));
    }

    /// The tile if it is ready, requesting it if nothing has yet.
    ///
    /// Called from a map's paint pass for every visible tile: a hit touches
    /// the LRU, a miss enqueues one download and returns `None` until the
    /// store notifies. A shed request (full queue) or a fresh failure also
    /// return `None`; the failure retries once [`RETRY_AFTER`] has passed.
    pub fn request(&mut self, id: TileId, source: &TileSource) -> Option<Arc<RenderImage>> {
        match self.tiles.get(&id) {
            Some(TileState::Ready(image)) => {
                let image = image.clone();
                self.touch(id);
                return Some(image);
            }
            Some(TileState::Pending) => return None,
            Some(TileState::Failed(at)) if at.elapsed() < RETRY_AFTER => return None,
            Some(TileState::Failed(_)) | None => {}
        }
        let job = TileJob {
            id,
            url: tile_url(&source.template, id),
            bucket: source.cache_bucket(),
        };
        if self.job_tx.try_send(job).is_ok() {
            self.tiles.insert(id, TileState::Pending);
        } else {
            // Queue full mid-pan: forget the entry so a calmer frame retries.
            self.tiles.remove(&id);
        }
        None
    }

    /// The tile if it is ready, with no side effects — the probe the
    /// parent-fallback walk uses, which must never fetch ancestors.
    pub fn ready(&self, id: TileId) -> Option<Arc<RenderImage>> {
        match self.tiles.get(&id) {
            Some(TileState::Ready(image)) => Some(image.clone()),
            _ => None,
        }
    }

    /// Evicted images whose atlas slots the calling window should release
    /// via `window.drop_image`.
    pub fn take_orphans(&mut self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut self.orphaned)
    }

    fn install(&mut self, result: TileResult) {
        match result.image {
            Some(image) => {
                self.tiles.insert(result.id, TileState::Ready(image));
                self.touch(result.id);
                while self.lru.len() > MEMORY_TILE_CAP {
                    let Some(evict) = self.lru.pop_front() else {
                        break;
                    };
                    if let Some(TileState::Ready(image)) = self.tiles.remove(&evict) {
                        self.orphaned.push(image);
                    }
                }
            }
            None => {
                self.tiles
                    .insert(result.id, TileState::Failed(Instant::now()));
            }
        }
    }

    fn touch(&mut self, id: TileId) {
        if let Some(pos) = self.lru.iter().position(|t| *t == id) {
            self.lru.remove(pos);
        }
        self.lru.push_back(id);
    }
}

fn tile_url(template: &str, id: TileId) -> String {
    template
        .replace("{z}", &id.zoom.to_string())
        .replace("{x}", &id.x.to_string())
        .replace("{y}", &id.y.to_string())
}

fn cache_path(id: TileId, bucket: u64) -> Option<PathBuf> {
    Some(
        dirs::cache_dir()?
            .join("metor")
            .join("tiles")
            .join(format!("{bucket:016x}"))
            .join(id.zoom.to_string())
            .join(id.x.to_string())
            .join(format!("{}.png", id.y)),
    )
}

/// The fetcher thread: a current-thread tokio runtime dispatching every
/// job as its own task, so downloads overlap over one HTTP/2 connection
/// per host while decode and cache IO run on the blocking pool.
///
/// The [`MAX_IN_FLIGHT`] permit is taken *before* a job is accepted, which
/// is what turns the bounded queue into real backpressure: past sixteen
/// downloads plus sixty-four queued, `request` sheds instead of piling up
/// tiles the user has panned away from.
fn run_fetcher(mut job_rx: mpsc::Receiver<TileJob>, done_tx: Sender<TileResult>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tile runtime");
    runtime.block_on(async move {
        // Providers require a real, identifying agent string.
        let client = reqwest::Client::builder()
            .user_agent(format!(
                "metor-panel/{} (+{})",
                env!("CARGO_PKG_VERSION"),
                option_env!("CARGO_PKG_REPOSITORY").unwrap_or("https://metor.io")
            ))
            .build()
            .expect("build tile client");
        let in_flight = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
        // The public OSM servers ask polite clients for two connections;
        // jobs pointing there wait for a slot rather than being shed.
        let osm = Arc::new(Semaphore::new(OSM_IN_FLIGHT_CAP));
        while let Some(job) = job_rx.recv().await {
            let permit = in_flight
                .clone()
                .acquire_owned()
                .await
                .expect("semaphore never closes");
            let client = client.clone();
            let done_tx = done_tx.clone();
            let osm = osm.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _osm_permit = if job.url.contains("openstreetmap.org") {
                    Some(osm.acquire_owned().await.expect("semaphore never closes"))
                } else {
                    None
                };
                let image = fetch_tile(&client, &job).await;
                if image.is_none() {
                    tracing::warn!(url = %job.url, "tile fetch failed");
                }
                let _ = done_tx.send(TileResult { id: job.id, image });
            });
        }
    });
}

/// Fetch one tile: disk cache, then network.
///
/// Decoding runs on tokio's blocking pool, never inline: a hi-dpi PNG
/// costs tens of milliseconds of CPU, and a screenful decoded serially on
/// the fetcher thread would stall every download behind it. On the pool,
/// decodes spread across cores and overlap the network waits.
async fn fetch_tile(client: &reqwest::Client, job: &TileJob) -> Option<Arc<RenderImage>> {
    let path = cache_path(job.id, job.bucket);
    if let Some(path) = &path {
        let path = path.clone();
        let cached = tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&path).ok()?;
            decode_tile(&bytes)
        })
        .await
        .ok()
        .flatten();
        if let Some(image) = cached {
            return Some(image);
        }
    }

    let response = client.get(&job.url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let bytes = response.bytes().await.ok()?;
    tokio::task::spawn_blocking(move || {
        let image = decode_tile(&bytes)?;
        if let Some(path) = path {
            write_cached(&path, &bytes);
        }
        Some(image)
    })
    .await
    .ok()
    .flatten()
}

/// Persist tile bytes via temp-file-and-rename so a crash mid-write can
/// never leave a torn file behind for a later launch to decode.
fn write_cached(path: &std::path::Path, bytes: &[u8]) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Decode tile bytes into the BGRA frame gpui's atlas expects — the same
/// swizzle gpui's own image loader performs.
fn decode_tile(bytes: &[u8]) -> Option<Arc<RenderImage>> {
    let mut data = image::load_from_memory(bytes).ok()?.into_rgba8();
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(
        Frame::new(data),
        1,
    ))))
}

impl TileStore {
    /// Observe the store from a map view so tile arrivals repaint it.
    pub fn observe<V: 'static>(cx: &mut Context<V>) -> Option<gpui::Subscription> {
        let store = try_global(cx)?;
        Some(cx.observe(&store, |_, _, cx| cx.notify()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_substitute_tile_coordinates() {
        let id = TileId {
            x: 2,
            y: 3,
            zoom: 5,
        };
        assert_eq!(
            tile_url(OSM_TILE_URL, id),
            "https://tile.openstreetmap.org/5/2/3.png"
        );
        assert_eq!(
            tile_url("https://example.com/{z}/{x}/{y}@2x.png", id),
            "https://example.com/5/2/3@2x.png"
        );
    }

    #[test]
    fn an_empty_config_url_means_the_mapbox_default() {
        let source = source_for("");
        assert!(source.template.contains("api.mapbox.com"));
        assert!(source.template.contains("@2x"));
        assert_eq!(source.image_px, 1024.0);
        // 1024 px images on a 2× display: one level shallower than the
        // camera zoom; on a 1× display, two.
        assert_eq!(source.zoom_bias(2.0), -1.0);
        assert_eq!(source.zoom_bias(1.0), -2.0);
        // A custom template is classic 256 px tiles: retina oversamples.
        assert_eq!(source_for(OSM_TILE_URL).zoom_bias(2.0), 1.0);
        // Distinct providers never share a cache bucket.
        assert_ne!(
            source.cache_bucket(),
            source_for(OSM_TILE_URL).cache_bucket()
        );
    }
}
