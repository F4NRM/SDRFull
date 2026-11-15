//! **The audio spectrum**: what the receiver's own audio looks like, in a strip
//! under the waterfall.
//!
//! SDR# draws one beside the main spectrum, and it is the one picture this
//! program did not have: the panadapter shows the band, which says nothing about
//! a hum from the sound card, a whistle from a switched-mode supply, or how much
//! the filter's skirt is taking off. The strip is the demodulated audio, 50 Hz to
//! 16 kHz, in the same log spacing the station's own level meter uses.
//!
//! The bands arrive in [`sdroxide_types::AudioLevel::bands`] — 48 of them, each
//! `0` (the floor, -80 dB) to `255` (full scale) — so there is no new wire and no
//! FFT here: the station already measures this for the meter that draws it, and
//! this is the same reading drawn as a shape rather than as 48 bars.
//!
//! Asked for by the operator on 2026-11-15, under the SPEC chip.

use eframe::egui;

use crate::theme;

/// How tall the strip is drawn: enough for a readable skirt, little enough that a
/// laptop's waterfall loses nothing worth missing.
pub const STRIP_H: f32 = 56.0;
/// The height of the frequency scale under the plot, in points.
const SCALE_H: f32 = 12.0;
/// The bands the station sends, from [`sdroxide_types::AUDIO_BANDS`] — the shape
/// is the same length, and a shorter one is drawn as far as it goes.
const BANDS: usize = sdroxide_types::AUDIO_BANDS;
/// The band edges, in hertz, log-spaced exactly as the station's meter makes
/// them (`AUDIO_BANDS_LO_HZ` .. `AUDIO_BANDS_HI_HZ`).
const LO_HZ: f32 = sdroxide_types::AUDIO_BANDS_LO_HZ;
const HI_HZ: f32 = sdroxide_types::AUDIO_BANDS_HI_HZ;
/// Where the frequency labels sit, in hertz: three octaves apart is as much as
/// fits a strip, and these are the ones an operator reads.
const LABELS: [(f32, &str); 5] =
    [(100.0, "100"), (500.0, "500"), (1_000.0, "1k"), (4_000.0, "4k"), (10_000.0, "10k")];

/// The x a band's **left edge** takes in a strip of `w` points, for band `i`.
pub fn band_x(i: usize, w: f32) -> f32 {
    w * i as f32 / BANDS as f32
}

/// The x of a frequency, on the strip's log scale. The band edges are log-spaced
/// from [`LO_HZ`] to [`HI_HZ`], so a frequency maps linearly onto the band index.
pub fn hz_x(hz: f32, w: f32) -> f32 {
    let pos = (hz.max(LO_HZ) / LO_HZ).ln() / (HI_HZ / LO_HZ).ln();
    w * pos.clamp(0.0, 1.0)
}

/// Draw the strip in `rect`.
///
/// `bands` is the station's latest reading, or `None` while none has arrived
/// (the strip is drawn either way: a flat floor is what silence looks like, and
/// a strip that vanished would be a control that silently did nothing).
pub fn show(ui: &mut egui::Ui, rect: egui::Rect, bands: Option<&[u8]>) {
    let p = ui.painter();
    let plot = egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.max.y - SCALE_H));
    p.rect_filled(rect, 0.0, theme::INPUT_BG());
    p.line_segment(
        [egui::pos2(plot.min.x, plot.max.y), egui::pos2(plot.max.x, plot.max.y)],
        egui::Stroke::new(1.0, theme::LINE()),
    );
    // A light grid at the labelled frequencies, under the bars, so the shape can
    // be read against something.
    for (hz, _) in LABELS {
        let x = plot.min.x + hz_x(hz, plot.width());
        p.line_segment(
            [egui::pos2(x, plot.min.y), egui::pos2(x, plot.max.y)],
            egui::Stroke::new(1.0, theme::LINE()),
        );
    }
    if let Some(bands) = bands {
        let bw = plot.width() / BANDS as f32;
        for (i, &v) in bands.iter().take(BANDS).enumerate() {
            if v == 0 {
                continue;
            }
            let h = plot.height() * f32::from(v) / 255.0;
            let x = plot.min.x + band_x(i, plot.width());
            // A bar per band, the top of it a shade brighter: the same shape the
            // waterfall's own trace makes, one row of it.
            let bar = egui::Rect::from_min_max(
                egui::pos2(x + 0.5, plot.max.y - h),
                egui::pos2(x + bw - 0.5, plot.max.y),
            );
            p.rect_filled(bar, 0.0, theme::CYAN());
            let cap = egui::Rect::from_min_max(
                egui::pos2(bar.min.x, bar.min.y),
                egui::pos2(bar.max.x, bar.min.y + 1.0),
            );
            p.rect_filled(cap, 0.0, theme::TEXT_STRONG());
        }
    }
    // The scale: the labelled frequencies, in the strip's own small text.
    for (hz, text) in LABELS {
        let x = plot.min.x + hz_x(hz, plot.width());
        p.text(
            egui::pos2(x + 2.0, plot.max.y + 1.0),
            egui::Align2::LEFT_TOP,
            text,
            egui::FontId::proportional(9.0),
            theme::gray(140),
        );
    }
    // "AUDIO" at the left, so the strip is never mistaken for a second band: it
    // is the only row in this program that is not frequency on the x axis.
    p.text(
        egui::pos2(rect.min.x + 4.0, plot.min.y + 2.0),
        egui::Align2::LEFT_TOP,
        "AUDIO",
        egui::FontId::proportional(9.0),
        theme::CYAN_DIM(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The strip reads left to right in the order the ear does, and a decade is
    /// the same width wherever it sits: the band edges are log-spaced.
    #[test]
    fn the_scale_is_logarithmic_and_lands_inside_the_strip() {
        let w = 480.0;
        assert_eq!(hz_x(LO_HZ, w), 0.0, "the lowest band starts at the left");
        assert!((hz_x(HI_HZ, w) - w).abs() < 0.01, "and the highest ends at the right");
        assert!(hz_x(200.0, w) > hz_x(100.0, w));
        // One decade, the same width at either end: 100→1000 and 1000→10000.
        let low = hz_x(1_000.0, w) - hz_x(100.0, w);
        let high = hz_x(10_000.0, w) - hz_x(1_000.0, w);
        assert!((low - high).abs() < 0.01, "decades equal: {low} vs {high}");
        // Anything outside the range is clamped rather than drawn off the strip.
        assert_eq!(hz_x(10.0, w), 0.0);
        assert_eq!(hz_x(30_000.0, w), w);
    }

    /// Every band gets its own column, and the last one ends at the right edge —
    /// a band drawn one pixel wide would read as a single line rather than a
    /// spectrum.
    #[test]
    fn the_bands_tile_the_strip() {
        let w = 480.0;
        assert_eq!(band_x(0, w), 0.0);
        assert!((band_x(BANDS, w) - w).abs() < 0.01);
        for i in 1..BANDS {
            assert!(band_x(i, w) > band_x(i - 1, w));
        }
        // Ten points a bar at 480: wider than the one-point cap drawn on top.
        assert!(w / BANDS as f32 > 2.0);
    }
}
