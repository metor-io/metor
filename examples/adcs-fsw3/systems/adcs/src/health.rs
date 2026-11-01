//! Sensor health: the sun sensors against the orbit's sunlight, and the
//! magnetometer against the field strength in low Earth orbit.

use adcs_contracts::{Gps, Magnetometer, SunSensors, epoch_at, in_earth_shadow, sun_dir_eci};
use metor_fsw_3::{AlarmCell, AlarmSpec, Alarms, Input, Msg, Timestamp, system};

use crate::ctrl::magnitude;
use crate::sun_from_css;

/// Every sun sensor head dark while the orbit puts the spacecraft in sunlight.
/// Two seconds at 120 Hz ride out GPS error at the shadow's edge.
pub const CSS_DARK: AlarmSpec = AlarmSpec::warning("css_dark").debounce(240);

/// A field magnitude outside 15 to 70 µT, the field at 400 km with margin.
pub const MAG_FIELD: AlarmSpec = AlarmSpec::warning("mag_field").debounce(12);

/// A field magnitude outside 5 to 100 µT: a dead or saturated sensor.
pub const MAG_DEAD: AlarmSpec = AlarmSpec::critical("mag_dead").debounce(12);

/// The health checks' alarms, and the epoch's zero.
#[derive(Default)]
pub struct Health {
    /// The first sun sensor stamp with a fix, the epoch's zero as nav takes it.
    first: Option<Timestamp>,
    css_dark: AlarmCell,
    mag_field: AlarmCell,
    mag_dead: AlarmCell,
}

impl Health {
    /// Whether the spacecraft is in sunlight at `stamp`, at `gps`'s position.
    fn sunlit(&mut self, stamp: Timestamp, gps: &Gps) -> bool {
        let first = *self.first.get_or_insert(stamp);
        let sun = sun_dir_eci(epoch_at((stamp - first).as_secs_f64()));
        !in_earth_shadow(&gps.pos_eci, &sun)
    }
}

#[system]
/// Checks the newest sun sensor and magnetometer samples. A check with no new
/// sample, or the sun check with no fix, holds its alarm.
pub fn health(
    #[state] s: &mut Health,
    css: &mut Input<SunSensors>,
    mag: &mut Input<Magnetometer>,
    gps: &mut Input<Gps>,
    alarms: &mut Alarms,
) {
    let gps = gps.latest().ok().flatten().map(|g| *g);
    let css = css.drain().flatten().last().copied();
    let dark = css.and_then(|c| {
        let gps = gps.as_ref()?;
        Some(s.sunlit(c.timestamp, gps) && sun_from_css(&c.css).is_none())
    });
    alarms.check_with(&mut s.css_dark, &CSS_DARK, dark, |m| {
        write!(m, "all sun sensors dark in sunlight")
    });

    let b = mag.drain().flatten().last().map(|m| magnitude(&m.mag_b));
    let field = |m: &mut Msg| write!(m, "|B| {:.1} uT", b.unwrap_or(f64::NAN) * 1e6);
    let outside = |lo: f64, hi: f64| b.map(|b| !(lo..=hi).contains(&b));
    alarms.check_with(&mut s.mag_field, &MAG_FIELD, outside(15e-6, 70e-6), field);
    alarms.check_with(&mut s.mag_dead, &MAG_DEAD, outside(5e-6, 100e-6), field);
}
