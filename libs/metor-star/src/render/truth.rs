//! What the renderer drew: the brightest stars on the sensor and counts.

/// The most stars a [`Truth`] lists.
pub const MAX_TRUTH: usize = 32;
/// A star is detectable when its peak pixel reaches this SNR.
pub const DETECT_SNR: f32 = 5.0;

/// One star's centroid at mid-exposure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TruthStar {
    /// Pixel position, with pixel centres at integer + ½.
    pub u: f64,
    pub v: f64,
    /// Band magnitude.
    pub mag: f32,
    /// The Hipparcos number, or `u32::MAX`.
    pub cat_id: u32,
}

impl TruthStar {
    pub const NONE: TruthStar = TruthStar {
        u: 0.0,
        v: 0.0,
        mag: 0.0,
        cat_id: u32::MAX,
    };
}

#[derive(Debug, Clone, PartialEq)]
pub struct Truth {
    /// The brightest stars on the sensor, brightest first. Entries past
    /// `count` are [`TruthStar::NONE`].
    pub stars: [TruthStar; MAX_TRUTH],
    pub count: usize,
    /// Stars that put at least one count on the sensor.
    pub n_rendered: u32,
    /// Stars on the sensor at mid-exposure whose peak pixel reaches
    /// [`DETECT_SNR`].
    pub n_detectable: u32,
}

impl Truth {
    pub fn empty() -> Truth {
        Truth {
            stars: [TruthStar::NONE; MAX_TRUTH],
            count: 0,
            n_rendered: 0,
            n_detectable: 0,
        }
    }

    pub fn brightest(&self) -> &[TruthStar] {
        &self.stars[..self.count.min(MAX_TRUTH)]
    }

    /// Keeps `star` if it is among the [`MAX_TRUTH`] brightest offered so
    /// far. Of equal magnitudes the one offered first ranks first.
    pub fn offer(&mut self, star: TruthStar) {
        let at = self.brightest().partition_point(|s| s.mag <= star.mag);
        let end = (self.count + 1).min(MAX_TRUTH);
        let Some(tail) = self.stars.get_mut(at..end).filter(|t| !t.is_empty()) else {
            return;
        };
        tail.rotate_right(1);
        if let Some(slot) = tail.first_mut() {
            *slot = star;
        }
        self.count = end;
    }
}

/// The SNR test for [`Truth::n_detectable`], for a still star centred on a
/// pixel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detector {
    /// The share of a star's electrons in its peak pixel.
    pub peak_fraction: f32,
    /// Sky, dark and read-noise variance per pixel (e⁻²).
    pub noise_var: f32,
}

impl Detector {
    pub fn detects(&self, electrons: f32) -> bool {
        let peak = electrons * self.peak_fraction;
        peak > 0.0 && peak >= DETECT_SNR * libm::sqrtf(peak + self.noise_var)
    }
}
