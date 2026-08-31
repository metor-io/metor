//! Web-mercator math for the slippy map: positions, the camera, and tile
//! enumeration.
//!
//! Everything here is pure geometry over `f64`, deliberately free of gpui
//! types so it can be unit-tested headless. The conventions are OSM's:
//! a [`Mercator`] position is normalized to `[0,1]²` with x growing east
//! from the antimeridian and y growing south from ~85.05°N, and zoom `z`
//! divides the world into `2^z × 2^z` tiles of 256 px.

use std::f64::consts::PI;

/// Native pixel size of one slippy tile.
pub const TILE_PX: f64 = 256.0;

/// Deepest tile level the standard OSM servers publish.
pub const MAX_TILE_ZOOM: u8 = 19;

/// Latitude bound of the web-mercator projection; poleward of this the
/// projection diverges, so inputs clamp here.
const LAT_LIMIT_DEG: f64 = 85.051_128_779_806_59;

/// One slippy-map tile: `x`/`y` index at `zoom`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TileId {
    pub x: u32,
    pub y: u32,
    pub zoom: u8,
}

impl TileId {
    /// Tiles per axis at `zoom`: `2^zoom`.
    pub fn per_axis(zoom: u8) -> u32 {
        1 << zoom
    }

    /// The tile one level up that contains this one, or `None` at the root.
    ///
    /// Walking parents is how the map paints something useful while a tile
    /// downloads: an ancestor already in the cache is scaled up in its place.
    pub fn parent(self) -> Option<TileId> {
        (self.zoom > 0).then(|| TileId {
            x: self.x / 2,
            y: self.y / 2,
            zoom: self.zoom - 1,
        })
    }
}

/// A position in normalized web-mercator space, `[0,1]` on both axes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mercator {
    pub x: f64,
    pub y: f64,
}

/// Project geodetic degrees into normalized mercator space.
///
/// Latitude clamps to the projection's ±85.05° limit rather than diverging,
/// so a polar-orbit track stays drawable at the map edge.
pub fn project(lat_deg: f64, lon_deg: f64) -> Mercator {
    let lat = lat_deg.clamp(-LAT_LIMIT_DEG, LAT_LIMIT_DEG).to_radians();
    let lon = lon_deg.to_radians();
    Mercator {
        x: (1.0 + lon / PI) / 2.0,
        y: (1.0 - lat.tan().asinh() / PI) / 2.0,
    }
}

/// Recover geodetic degrees from a normalized mercator position.
pub fn unproject(m: Mercator) -> (f64, f64) {
    let lat = (PI * (1.0 - 2.0 * m.y)).sinh().atan();
    let lon = (2.0 * m.x - 1.0) * PI;
    (lat.to_degrees(), lon.to_degrees())
}

/// The map's viewpoint: a mercator center plus a fractional zoom.
///
/// At zoom `z` the whole world spans `2^z · 256` screen pixels, so the
/// fractional zoom is the one continuous knob the scroll wheel turns; the
/// integer tile level to fetch falls out of it in [`tile_zoom`](Self::tile_zoom).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Camera {
    pub center: Mercator,
    pub zoom: f64,
}

impl Camera {
    /// Total world width in screen pixels at this zoom.
    fn world_px(&self) -> f64 {
        self.zoom.exp2() * TILE_PX
    }

    /// The integer tile level drawn at this camera zoom, and the factor each
    /// tile's 256 px worth of world is scaled by in *logical* pixels.
    ///
    /// `bias` folds the display and the tile source into one number:
    /// `log2(scale_factor · 256 / source_image_px)`. A retina window biases
    /// deeper so tile pixels land ~1:1 on physical pixels; a source serving
    /// 512 px or @2x images biases shallower, because each of its tiles
    /// already carries the detail of a deeper level. Rounding (rather than
    /// flooring) keeps tiles within ±√2 of native resolution, the least
    /// blurry choice for a fractional zoom.
    pub fn tile_zoom(&self, bias: f64) -> (u8, f64) {
        let tz = (self.zoom + bias).round().clamp(0.0, MAX_TILE_ZOOM as f64) as u8;
        (tz, (self.zoom - tz as f64).exp2())
    }

    /// Mercator → screen pixels within a viewport of `size`, center mapping
    /// to the viewport's middle.
    pub fn to_screen(&self, m: Mercator, size: (f64, f64)) -> (f64, f64) {
        let world = self.world_px();
        (
            (m.x - self.center.x) * world + size.0 / 2.0,
            (m.y - self.center.y) * world + size.1 / 2.0,
        )
    }

    /// Screen pixels → mercator, the inverse of [`to_screen`](Self::to_screen).
    pub fn from_screen(&self, px: (f64, f64), size: (f64, f64)) -> Mercator {
        let world = self.world_px();
        Mercator {
            x: self.center.x + (px.0 - size.0 / 2.0) / world,
            y: self.center.y + (px.1 - size.1 / 2.0) / world,
        }
    }

    /// Zoom by `factor`, holding the mercator point under `cursor` fixed —
    /// the scroll-wheel gesture every map user expects.
    pub fn zoom_at(&mut self, cursor: (f64, f64), size: (f64, f64), factor: f64) {
        let anchor = self.from_screen(cursor, size);
        self.zoom = (self.zoom + factor.log2()).clamp(1.0, MAX_TILE_ZOOM as f64);
        // Re-center so `anchor` lands back under the cursor at the new zoom.
        let world = self.world_px();
        self.center = Mercator {
            x: anchor.x - (cursor.0 - size.0 / 2.0) / world,
            y: anchor.y - (cursor.1 - size.1 / 2.0) / world,
        };
        self.clamp();
    }

    /// Keep the center on the map: y pinned inside the projection, x wrapped
    /// so panning across the antimeridian keeps going.
    pub fn clamp(&mut self) {
        self.center.x = self.center.x.rem_euclid(1.0);
        self.center.y = self.center.y.clamp(0.0, 1.0);
    }
}

/// A tile the viewport needs this frame, with where and how large to draw it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct VisibleTile {
    pub id: TileId,
    /// Screen position of the tile's top-left corner.
    pub origin: (f64, f64),
    /// On-screen edge length; `TILE_PX` scaled by the fractional zoom.
    pub size: f64,
}

/// Every tile intersecting a viewport of `size` logical pixels, at the tile
/// level the `bias` (see [`Camera::tile_zoom`]) selects.
///
/// X indices wrap modulo the tile count so the antimeridian is seamless;
/// rows off the top or bottom of the projection are simply absent, which is
/// what leaves the polar gap blank rather than glitched.
pub fn visible_tiles(cam: &Camera, size: (f64, f64), bias: f64) -> Vec<VisibleTile> {
    if size.0 <= 0.0 || size.1 <= 0.0 {
        return Vec::new();
    }
    let (tz, scale) = cam.tile_zoom(bias);
    let n = TileId::per_axis(tz) as i64;
    let draw = TILE_PX * scale;
    // Viewport top-left in world pixels at the drawn scale.
    let world = n as f64 * draw;
    let left = cam.center.x * world - size.0 / 2.0;
    let top = cam.center.y * world - size.1 / 2.0;

    let first_x = (left / draw).floor() as i64;
    let last_x = ((left + size.0) / draw).floor() as i64;
    let first_y = (top / draw).floor() as i64;
    let last_y = ((top + size.1) / draw).floor() as i64;

    let mut tiles = Vec::with_capacity(((last_x - first_x + 1) * (last_y - first_y + 1)) as usize);
    for iy in first_y..=last_y {
        if iy < 0 || iy >= n {
            continue;
        }
        for ix in first_x..=last_x {
            tiles.push(VisibleTile {
                id: TileId {
                    x: ix.rem_euclid(n) as u32,
                    y: iy as u32,
                    zoom: tz,
                },
                origin: (ix as f64 * draw - left, iy as f64 * draw - top),
                size: draw,
            });
        }
    }
    tiles
}

/// Where an ancestor tile must be drawn so that `child`'s quadrant of it
/// lands exactly on `child`'s screen rect.
///
/// This is the substitute-blit for a tile still downloading: the whole
/// ancestor is painted `2^dz` times oversize and the canvas content mask
/// clips it down to the child's cell.
pub fn fallback_rect(
    child: TileId,
    ancestor: TileId,
    child_origin: (f64, f64),
    child_size: f64,
) -> ((f64, f64), f64) {
    let dz = child.zoom - ancestor.zoom;
    let offset_x = child.x - (ancestor.x << dz);
    let offset_y = child.y - (ancestor.y << dz);
    (
        (
            child_origin.0 - offset_x as f64 * child_size,
            child_origin.1 - offset_y as f64 * child_size,
        ),
        child_size * (dz as f64).exp2(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_island_is_the_middle_of_the_map() {
        let m = project(0.0, 0.0);
        assert!((m.x - 0.5).abs() < 1e-12 && (m.y - 0.5).abs() < 1e-12);
    }

    #[test]
    fn projection_round_trips() {
        for &(lat, lon) in &[(0.0, 0.0), (45.0, -122.5), (-60.0, 179.0), (85.0, -179.9)] {
            let (lat2, lon2) = unproject(project(lat, lon));
            assert!((lat - lat2).abs() < 1e-9, "lat {lat} -> {lat2}");
            assert!((lon - lon2).abs() < 1e-9, "lon {lon} -> {lon2}");
        }
    }

    #[test]
    fn polar_latitudes_clamp_instead_of_diverging() {
        let m = project(90.0, 0.0);
        assert!(m.y.abs() < 1e-9, "north pole clamps to the top edge: {m:?}");
        assert!(project(-90.0, 0.0).y > 1.0 - 1e-9);
    }

    #[test]
    fn a_parent_holds_four_children() {
        let child = TileId {
            x: 3,
            y: 2,
            zoom: 5,
        };
        assert_eq!(
            child.parent(),
            Some(TileId {
                x: 1,
                y: 1,
                zoom: 4
            })
        );
        assert_eq!(
            TileId {
                x: 0,
                y: 0,
                zoom: 0
            }
            .parent(),
            None
        );
    }

    #[test]
    fn screen_projection_round_trips_through_the_camera() {
        let cam = Camera {
            center: project(37.0, -122.0),
            zoom: 7.3,
        };
        let size = (640.0, 480.0);
        let m = project(36.5, -121.0);
        let px = cam.to_screen(m, size);
        let back = cam.from_screen(px, size);
        assert!((m.x - back.x).abs() < 1e-12 && (m.y - back.y).abs() < 1e-12);
    }

    #[test]
    fn zooming_holds_the_point_under_the_cursor() {
        let mut cam = Camera {
            center: Mercator { x: 0.5, y: 0.5 },
            zoom: 5.0,
        };
        let size = (800.0, 600.0);
        let cursor = (200.0, 450.0);
        let anchor = cam.from_screen(cursor, size);
        cam.zoom_at(cursor, size, 1.5);
        let after = cam.from_screen(cursor, size);
        assert!((anchor.x - after.x).abs() < 1e-12);
        assert!((anchor.y - after.y).abs() < 1e-12);
        assert!(cam.zoom > 5.0);
    }

    #[test]
    fn zoom_stays_within_the_tile_pyramid() {
        let mut cam = Camera {
            center: Mercator { x: 0.5, y: 0.5 },
            zoom: 18.9,
        };
        cam.zoom_at((0.0, 0.0), (100.0, 100.0), 64.0);
        assert_eq!(cam.zoom, MAX_TILE_ZOOM as f64);
        cam.zoom_at((0.0, 0.0), (100.0, 100.0), 1e-9);
        assert_eq!(cam.zoom, 1.0);
    }

    #[test]
    fn an_integer_zoom_draws_tiles_at_native_size() {
        let cam = Camera {
            center: Mercator { x: 0.5, y: 0.5 },
            zoom: 2.0,
        };
        // A 512² viewport at zoom 2 spans two native tiles per axis, but the
        // viewport edges straddle tile boundaries, so three columns and rows
        // intersect.
        let tiles = visible_tiles(&cam, (512.0, 512.0), 0.0);
        assert_eq!(tiles.len(), 9);
        assert!(tiles.iter().all(|t| t.size == TILE_PX));
        assert!(tiles.iter().all(|t| t.id.zoom == 2));
        // The center tile sits flush with the viewport middle.
        assert!(tiles.iter().any(|t| t.origin == (256.0, 256.0)
            && t.id
                == TileId {
                    x: 2,
                    y: 2,
                    zoom: 2
                }));
    }

    #[test]
    fn a_retina_display_fetches_one_level_deeper_at_half_the_size() {
        let cam = Camera {
            center: Mercator { x: 0.5, y: 0.5 },
            zoom: 2.0,
        };
        // 2× display, 256 px source: bias = log2(2·256/256) = 1.
        let (tz, scale) = cam.tile_zoom(1.0);
        assert_eq!(tz, 3);
        assert_eq!(scale, 0.5);
        // Same logical viewport, four times the tiles, each half the
        // logical edge — so each tile pixel lands on one physical pixel.
        let tiles = visible_tiles(&cam, (512.0, 512.0), 1.0);
        assert!(tiles.iter().all(|t| t.id.zoom == 3 && t.size == 128.0));
        // The tile pyramid still bottoms out at its deepest level.
        let deep = Camera {
            center: Mercator { x: 0.5, y: 0.5 },
            zoom: MAX_TILE_ZOOM as f64,
        };
        assert_eq!(deep.tile_zoom(1.0).0, MAX_TILE_ZOOM);
    }

    #[test]
    fn a_hidpi_source_fetches_shallower_at_double_the_size() {
        let cam = Camera {
            center: Mercator { x: 0.5, y: 0.5 },
            zoom: 4.0,
        };
        // 2× display, 512 px @2x source (1024 px images):
        // bias = log2(2·256/1024) = -1 — each tile already holds a level's
        // worth of extra detail, so one level up drawn twice the size still
        // lands ~1:1 on physical pixels.
        let (tz, scale) = cam.tile_zoom(-1.0);
        assert_eq!(tz, 3);
        assert_eq!(scale, 2.0);
        let tiles = visible_tiles(&cam, (512.0, 512.0), -1.0);
        assert!(tiles.iter().all(|t| t.id.zoom == 3 && t.size == 512.0));
    }

    #[test]
    fn columns_wrap_across_the_antimeridian() {
        let cam = Camera {
            // Centered on the antimeridian itself.
            center: Mercator { x: 0.0, y: 0.5 },
            zoom: 2.0,
        };
        let tiles = visible_tiles(&cam, (512.0, 256.0), 0.0);
        // Both easternmost and westernmost columns appear.
        assert!(tiles.iter().any(|t| t.id.x == 3));
        assert!(tiles.iter().any(|t| t.id.x == 0));
        assert!(tiles.iter().all(|t| t.id.x < 4));
    }

    #[test]
    fn rows_beyond_the_poles_are_absent() {
        let cam = Camera {
            center: Mercator { x: 0.5, y: 0.0 },
            zoom: 2.0,
        };
        let tiles = visible_tiles(&cam, (256.0, 512.0), 0.0);
        // Everything above y=0 is off the projection; only real rows remain.
        assert!(!tiles.is_empty());
        assert!(tiles.iter().all(|t| t.id.y < 4));
    }

    #[test]
    fn a_fallback_blit_covers_the_childs_cell() {
        let child = TileId {
            x: 5,
            y: 6,
            zoom: 3,
        };
        let parent = child.parent().unwrap();
        let ((ox, oy), size) = fallback_rect(child, parent, (100.0, 200.0), 256.0);
        // Child (5,6) is the (1,0) quadrant of parent (2,3): the parent's
        // blit starts one child-width west of the child.
        assert_eq!((ox, oy), (100.0 - 256.0, 200.0));
        assert_eq!(size, 512.0);

        let grandparent = parent.parent().unwrap();
        let ((ox, oy), size) = fallback_rect(child, grandparent, (100.0, 200.0), 256.0);
        assert_eq!((ox, oy), (100.0 - 256.0, 200.0 - 2.0 * 256.0));
        assert_eq!(size, 1024.0);
    }
}
