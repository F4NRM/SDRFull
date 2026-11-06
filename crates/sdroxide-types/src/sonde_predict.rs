//! Where a radiosonde will come down: a simple, documented estimate.
//!
//! This is an **estimate, not a promise**. It knows only what the sonde's own
//! GPS has told the decoder, and the real answer depends on winds nobody has
//! measured above or below the balloon. The model, in order of what it uses:
//!
//! * **Falling** (climb rate below −1 m/s): the descent speed under a parachute
//!   scales with `1/sqrt(air density)`, so with an exponential atmosphere of
//!   scale height [`SCALE_HEIGHT_M`] it is `v(h) = v_sl · exp(h / 2H)`. The
//!   speed at sea level `v_sl` is fitted to the descent rate the sonde reports
//!   now, and the fall is integrated down to the ground in [`STEP_M`] steps.
//! * **Rising**: the balloon is assumed to burst at [`DEFAULT_BURST_M`] (or two
//!   kilometres above where it is, if it is already higher), at the climb rate
//!   it has now, then to fall as above from [`ASSUMED_SEA_LEVEL_FALL_MS`].
//! * **Wind**: while falling, the wind at each height is the one the sonde
//!   itself measured on the way up, where its track holds it (average ground
//!   velocity per [`BIN_M`] of altitude); anywhere else, the wind it feels now.
//!   While rising, the wind it feels now is held constant to the burst.
//! * **Ground**: the altitude it was first heard at, when that was low (the
//!   launch site), otherwise sea level.
//!
//! Pure functions on [`SondeTrack`], so they are tested without a radio.

use crate::{SondePoint, SondeTrack};

/// Exponential-atmosphere scale height, metres.
pub const SCALE_HEIGHT_M: f64 = 7_500.0;
/// Burst altitude assumed for a sonde still climbing: a typical RS41 flight.
pub const DEFAULT_BURST_M: f64 = 30_000.0;
/// Descent speed at sea level assumed when the sonde has not yet fallen.
pub const ASSUMED_SEA_LEVEL_FALL_MS: f64 = 5.0;
/// A climb rate below this is a fall under the parachute, not a float.
pub const FALLING_MS: f64 = -1.0;
/// Integration step, metres.
const STEP_M: f64 = 100.0;
/// Altitude bin of the measured wind profile, metres.
const BIN_M: f64 = 500.0;
/// A launch is "low" below this altitude: the first point is then the ground.
const LOW_START_M: f64 = 2_500.0;
/// Metres per degree of latitude.
const M_PER_DEG: f64 = 111_320.0;

/// Whether the estimate starts from a sonde going up or coming down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Still climbing: the burst height is assumed, so the answer is rough.
    Climbing,
    /// Already falling: the fall is fitted to the sonde's own descent.
    Falling,
}

/// The estimated landing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Landing {
    pub lat: f64,
    pub lon: f64,
    /// Seconds from the sonde's last frame to touchdown.
    pub eta_s: f64,
    /// The burst altitude assumed, when the sonde was still climbing.
    pub burst_alt_m: Option<f64>,
    pub basis: Basis,
}

/// Ground velocity (east, north) in m/s between two track points, if they are
/// far enough apart in time to mean anything.
fn velocity(a: &SondePoint, b: &SondePoint) -> Option<(f64, f64)> {
    let dt = (b.unix - a.unix) as f64;
    if dt < 5.0 {
        return None;
    }
    let north = (b.lat - a.lat) * M_PER_DEG / dt;
    let east = (b.lon - a.lon) * M_PER_DEG * ((a.lat + b.lat) / 2.0).to_radians().cos() / dt;
    Some((east, north))
}

/// The wind the sonde measured on its way up: mean ground velocity per
/// altitude bin, from the rising part of the track only.
fn ascent_profile(track: &[SondePoint]) -> Vec<(f64, f64, f64)> {
    // (sum east, sum north, count) per bin.
    let mut bins: std::collections::BTreeMap<i64, (f64, f64, u32)> = Default::default();
    for w in track.windows(2) {
        if w[1].alt_m <= w[0].alt_m {
            continue;
        }
        // Use a point ~10 s ahead where there is one, so the speed is smooth.
        if let Some((e, n)) = velocity(&w[0], &w[1]) {
            let bin = (f64::from(w[0].alt_m) / BIN_M).floor() as i64;
            let b = bins.entry(bin).or_insert((0.0, 0.0, 0));
            b.0 += e;
            b.1 += n;
            b.2 += 1;
        }
    }
    bins.into_iter()
        .map(|(k, (e, n, c))| ((k as f64 + 0.5) * BIN_M, e / f64::from(c), n / f64::from(c)))
        .collect()
}

/// The wind at `h`: the nearest measured bin within one bin's reach, else
/// `fallback`.
fn wind_at(profile: &[(f64, f64, f64)], h: f64, fallback: (f64, f64)) -> (f64, f64) {
    profile
        .iter()
        .filter(|(alt, _, _)| (alt - h).abs() <= BIN_M)
        .min_by(|a, b| (a.0 - h).abs().total_cmp(&(b.0 - h).abs()))
        .map_or(fallback, |&(_, e, n)| (e, n))
}

fn advance(lat: f64, lon: f64, east: f64, north: f64, dt: f64) -> (f64, f64) {
    let lat2 = lat + north * dt / M_PER_DEG;
    let lon2 = lon + east * dt / (M_PER_DEG * lat.to_radians().cos().max(0.05));
    (lat2, lon2)
}

/// Estimate where `s` will land, from its latest frame and its track. `None`
/// when it has no position, no altitude, or is already on the ground.
pub fn predict(s: &SondeTrack) -> Option<Landing> {
    let (lat, lon) = (s.lat?, s.lon?);
    let alt = f64::from(s.alt_m?);
    let climb = f64::from(s.climb_ms.unwrap_or(0.0));
    // The wind it feels now, from its own ground speed and course.
    let now_wind = match (s.speed_ms, s.course_deg) {
        (Some(v), Some(c)) => {
            let c = f64::from(c).to_radians();
            (f64::from(v) * c.sin(), f64::from(v) * c.cos())
        }
        _ => (0.0, 0.0),
    };
    let ground = s
        .track
        .first()
        .map(|p| f64::from(p.alt_m))
        .filter(|a| *a < LOW_START_M)
        .unwrap_or(0.0);
    if alt <= ground + 20.0 {
        return None;
    }
    let profile = ascent_profile(&s.track);

    let (mut lat, mut lon, mut t) = (lat, lon, 0.0);
    let (mut h, v_sl, burst, basis);
    if climb < FALLING_MS {
        basis = Basis::Falling;
        burst = None;
        h = alt;
        v_sl = (-climb) / (alt / (2.0 * SCALE_HEIGHT_M)).exp();
    } else {
        basis = Basis::Climbing;
        let b = DEFAULT_BURST_M.max(alt + 2_000.0);
        burst = Some(b);
        // Up to the burst at the climb rate it has (5 m/s if it is not moving).
        let rate = if climb >= 1.0 { climb } else { 5.0 };
        let dt = (b - alt) / rate;
        (lat, lon) = advance(lat, lon, now_wind.0, now_wind.1, dt);
        t += dt;
        h = b;
        v_sl = ASSUMED_SEA_LEVEL_FALL_MS;
    }

    while h > ground {
        let step = STEP_M.min(h - ground);
        let mid = h - step / 2.0;
        let v = v_sl * (mid / (2.0 * SCALE_HEIGHT_M)).exp();
        let dt = step / v;
        // While falling, the wind it measured on the way up at this height; the
        // wind it feels now where its track holds none.
        let (e, n) = if basis == Basis::Falling {
            wind_at(&profile, mid, now_wind)
        } else {
            now_wind
        };
        (lat, lon) = advance(lat, lon, e, n, dt);
        t += dt;
        h -= step;
    }
    Some(Landing { lat, lon, eta_s: t, burst_alt_m: burst, basis })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sonde(alt: f32, climb: f32, speed: f32, course: f32, track: Vec<SondePoint>) -> SondeTrack {
        SondeTrack {
            model: "RS41".into(),
            serial: "N1".into(),
            freq_hz: 403e6,
            frame: 1,
            lat: Some(48.0),
            lon: Some(2.0),
            alt_m: Some(alt),
            speed_ms: Some(speed),
            course_deg: Some(course),
            climb_ms: Some(climb),
            num_sv: Some(9),
            battery_v: Some(2.7),
            last_unix: 1000,
            frames: 1,
            track,
        }
    }

    fn pt(lat: f64, lon: f64, alt: f32, unix: i64) -> SondePoint {
        SondePoint { lat, lon, alt_m: alt, unix }
    }

    #[test]
    fn a_fall_lands_downwind_in_a_plausible_time() {
        // 10 km up, falling 20 m/s, drifting east at 10 m/s.
        let s = sonde(10_000.0, -20.0, 10.0, 90.0, vec![]);
        let l = predict(&s).expect("a landing");
        assert_eq!(l.basis, Basis::Falling);
        assert!(l.burst_alt_m.is_none());
        // Falls faster aloft than at the ground, so the time is longer than
        // 10 km at the final speed and shorter than 10 km at 5 m/s.
        assert!(l.eta_s > 10_000.0 / 20.0 && l.eta_s < 10_000.0 / 5.0, "eta {}", l.eta_s);
        // Due east: same latitude, further east by about wind * time.
        assert!((l.lat - 48.0).abs() < 1e-6);
        let east_m = (l.lon - 2.0) * M_PER_DEG * 48.0f64.to_radians().cos();
        assert!((east_m - 10.0 * l.eta_s).abs() < 0.02 * east_m, "east {east_m}");
    }

    #[test]
    fn a_fall_fits_the_descent_rate_it_reports() {
        // The same height, a sonde falling twice as fast, comes down sooner.
        let slow = predict(&sonde(8_000.0, -15.0, 0.0, 0.0, vec![])).unwrap();
        let fast = predict(&sonde(8_000.0, -30.0, 0.0, 0.0, vec![])).unwrap();
        assert!(fast.eta_s < slow.eta_s * 0.6, "{} vs {}", fast.eta_s, slow.eta_s);
    }

    #[test]
    fn a_climbing_sonde_is_given_an_assumed_burst() {
        let s = sonde(12_000.0, 5.0, 20.0, 90.0, vec![]);
        let l = predict(&s).unwrap();
        assert_eq!(l.basis, Basis::Climbing);
        assert_eq!(l.burst_alt_m, Some(DEFAULT_BURST_M));
        // At least the climb to the burst, which is 18 km at 5 m/s.
        assert!(l.eta_s > 18_000.0 / 5.0);
        assert!(l.lon > 2.0, "carried east by the wind");
    }

    #[test]
    fn a_sonde_above_the_assumed_burst_is_given_a_little_more() {
        let l = predict(&sonde(33_000.0, 4.0, 0.0, 0.0, vec![])).unwrap();
        assert_eq!(l.burst_alt_m, Some(35_000.0));
    }

    #[test]
    fn a_falling_sonde_uses_the_wind_it_measured_on_the_way_up() {
        // Climbed 0 -> 6 km drifting west at about 2 m/s, and now falls with no
        // wind at all where it is. The low air it remembers is still west.
        let track: Vec<SondePoint> = (0..60)
            .map(|i| pt(48.0, 2.0 - 0.0003 * f64::from(i), (i * 100) as f32, i64::from(i) * 10))
            .collect();
        let mut s = sonde(6_000.0, -20.0, 0.0, 0.0, track);
        s.lon = Some(2.0);
        let l = predict(&s).unwrap();
        assert!(l.lon < 2.0, "carried west by the remembered low wind, got {}", l.lon);
    }

    #[test]
    fn nothing_to_predict_without_a_place_or_on_the_ground() {
        let mut s = sonde(10_000.0, -10.0, 0.0, 0.0, vec![]);
        s.lat = None;
        assert!(predict(&s).is_none());
        let on_the_ground = sonde(5.0, 0.0, 0.0, 0.0, vec![]);
        assert!(predict(&on_the_ground).is_none());
    }

    #[test]
    fn the_ground_is_where_a_low_start_was() {
        // Launched from a 1 200 m plateau: it lands there, not at sea level.
        let s = sonde(4_000.0, -10.0, 0.0, 0.0, vec![pt(48.0, 2.0, 1_200.0, 0)]);
        let high = predict(&s).unwrap();
        let sea = predict(&sonde(4_000.0, -10.0, 0.0, 0.0, vec![])).unwrap();
        assert!(high.eta_s < sea.eta_s, "{} vs {}", high.eta_s, sea.eta_s);
    }
}
