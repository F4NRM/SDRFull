//! HF weather facsimile (WEFAX / radiofax) receiver.
//!
//! The charts the meteorological services broadcast on short wave — surface
//! analyses, wave heights, ice edges, satellite composites — are sent as
//! frequency-modulated facsimile inside an SSB channel, to a scheme that has
//! not changed since the 1970s and is documented in ITU-R Recommendation
//! M.1170 and in every navy's radio-facsimile schedule.
//!
//! # The signal
//!
//! The picture is a subcarrier that swings between two frequencies: **1500 Hz
//! is black, 2300 Hz is white**, centred on 1900 Hz with ±400 Hz of deviation.
//! That is the same discriminator geometry as SSTV, which is why this module
//! and [`crate::sstv`] demodulate identically and diverge only afterwards.
//!
//! What arrives is a continuous raster, not a framed image:
//!
//! * a **start tone** — the black/white subcarrier alternating at 300 Hz (for
//!   [`Ioc::I576`]) or 675 Hz ([`Ioc::I288`]) for about five seconds, which is
//!   what says a transmission is beginning and at which index of cooperation;
//! * a **phasing signal** — around thirty seconds of lines that are black
//!   except for a white pulse of 5 % centred on the line boundary. The pulse is the
//!   only thing that says where a line *starts*; without it the chart arrives
//!   correctly but cut vertically and wrapped, which is the classic radiofax
//!   failure;
//! * the **picture**, at 60, 90, 120, 180 or 240 lines per minute — 120 is what
//!   nearly every service uses for charts;
//! * a **stop tone**, the subcarrier alternating at 450 Hz for five seconds.
//!
//! # Finding a transmission
//!
//! Two ways, and the second is the one a real channel needs:
//!
//! * the **start tone**, whether the subcarrier alternating or a plain audio
//!   tone, which arms the receiver; the page is announced once the phasing
//!   behind it locks, so a false tone leaves no trace on screen;
//! * the **phasing signal alone**. A start tone can sit under a carrier or a
//!   fade and never be heard, while the phasing signal — the same line, the same
//!   pulse, at the line rate — is the one thing on a fax channel that repeats
//!   exactly. Lines are stacked so a weak signal's scattered pulse stands out,
//!   and a few seconds of it begins a page with no tone at all.
//!
//! A **carrier left standing on the channel** starts nothing — it is steady, and
//! neither the alternation nor a pulse is — and ends a page that has run out of
//! chart, whatever frequency it has drifted to. A stop tone must be *periodic*
//! at its pitch, not merely cross mid-grey that often: noise on a weak chart
//! does, and a real recording was cut short that way.
//!
//! Black and white are not taken on trust either. A receiver a little off tune,
//! or Doppler on the path, moves the whole subcarrier, so the phasing signal is
//! used to re-measure where they are before the chart is read.
//!
//! Line length comes from the *index of cooperation*: pixels per line is
//! `IOC × π`, so IOC 576 gives 1809 and IOC 288 gives 904. Nothing in the
//! signal states the height — a chart runs for as long as the station keeps
//! sending, which is why the receiver ends on the stop tone or on the
//! operator's say-so rather than at a known row count.
//!
//! # Why the timing is the hard part
//!
//! A line at 120 LPM is half a second, which at a 12 kHz tap is six thousand
//! samples for 1809 pixels. An error of one part in ten thousand in the assumed
//! sample rate — well within a sound card's tolerance — walks the image sideways
//! by most of a line over a fifteen-minute chart. That is what [`WefaxRx::set_slant_ppm`]
//! trims, and why the pixel clock here is accumulated in fractional samples and
//! never by adding a rounded per-line integer.

use std::f64::consts::{PI, TAU};

use std::sync::Arc;

use num_complex::Complex32;
use realfft::{RealFftPlanner, RealToComplex};

use crate::fir::{bandpass_taps, ComplexFir};

/// Subcarrier centre, Hz. Black is 400 Hz below it and white 400 Hz above.
pub const CENTRE_HZ: f64 = 1900.0;
/// Black frequency, Hz.
pub const BLACK_HZ: f64 = 1500.0;
/// White frequency, Hz.
pub const WHITE_HZ: f64 = 2300.0;

/// Index of cooperation: the drum-facsimile parameter that survives as the
/// definition of how many pixels there are in a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ioc {
    /// 576 — every weather chart. 1809 pixels per line.
    #[default]
    I576,
    /// 288 — the half-resolution mode, and what the older satellite-imagery
    /// schedules use. 904 pixels per line.
    I288,
}

impl Ioc {
    pub const ALL: [Ioc; 2] = [Ioc::I576, Ioc::I288];

    pub fn value(self) -> u32 {
        match self {
            Ioc::I576 => 576,
            Ioc::I288 => 288,
        }
    }

    /// Pixels per line, `IOC × π` truncated.
    ///
    /// Truncated rather than rounded because that is what the figures every
    /// schedule and every other decoder quotes come from: 576 π is 1809.56 and
    /// the line is 1809 pixels, 288 π is 904.78 and the line is 904.
    pub fn width(self) -> u16 {
        (self.value() as f64 * PI) as u16
    }

    /// The start tone that announces this index of cooperation, Hz.
    pub fn start_tone_hz(self) -> f64 {
        match self {
            Ioc::I576 => 300.0,
            Ioc::I288 => 675.0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Ioc::I576 => "IOC 576",
            Ioc::I288 => "IOC 288",
        }
    }
}

/// Lines per minute. 120 is what the weather services broadcast charts at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lpm {
    L60,
    L90,
    #[default]
    L120,
    L180,
    L240,
}

impl Lpm {
    pub const ALL: [Lpm; 5] = [Lpm::L60, Lpm::L90, Lpm::L120, Lpm::L180, Lpm::L240];

    pub fn value(self) -> f64 {
        match self {
            Lpm::L60 => 60.0,
            Lpm::L90 => 90.0,
            Lpm::L120 => 120.0,
            Lpm::L180 => 180.0,
            Lpm::L240 => 240.0,
        }
    }

    /// Seconds per scan line.
    pub fn line_secs(self) -> f64 {
        60.0 / self.value()
    }

    pub fn label(self) -> &'static str {
        match self {
            Lpm::L60 => "60 LPM",
            Lpm::L90 => "90 LPM",
            Lpm::L120 => "120 LPM",
            Lpm::L180 => "180 LPM",
            Lpm::L240 => "240 LPM",
        }
    }
}

/// The stop tone, Hz — the same whatever the index of cooperation.
pub const STOP_TONE_HZ: f64 = 450.0;

/// Longest chart the receiver will build before ending it itself.
///
/// A stop tone is what normally ends a transmission, but it is the first thing
/// lost to a fade, and a receiver that kept appending rows to a dead carrier
/// would grow an image without limit. Twenty minutes at 120 LPM is 2400 lines,
/// comfortably longer than any scheduled chart.
pub const MAX_LINES: u16 = 2600;

/// What the receiver reports as it goes.
#[derive(Debug, Clone, PartialEq)]
pub enum WefaxEvent {
    /// A transmission has begun; a new image starts. `auto` distinguishes a
    /// start tone from the operator pressing START.
    Start { ioc: Ioc, lpm: Lpm, auto: bool },
    /// The phasing pulse has been located, `skew` samples into the line.
    Phased { skew: u32 },
    /// A finished scan line: `gray` is [`Ioc::width`] bytes, 0 black … 255 white.
    Line { y: u16, gray: Vec<u8> },
    /// The transmission ended. `by_tone` distinguishes a stop tone from the
    /// operator, a fade, or the length limit.
    Complete { lines: u16, by_tone: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Not receiving. Listening for a start tone if auto-start is on.
    Idle,
    /// Receiving, but still looking for the phasing pulse that says where a
    /// line begins.
    Phasing,
    /// Building the picture.
    Image,
}

/// Detector for the alternating black/white tones that bracket a transmission.
///
/// The start and stop signals are not audio tones — they are the *picture*
/// alternating between black and white at 300, 675 or 450 Hz. So they are
/// measured after the discriminator, on the demodulated brightness, by counting
/// how often it crosses mid-grey. A 300 Hz square wave crosses 600 times a
/// second; nothing in a weather chart does that for seconds on end.
#[derive(Debug, Default)]
struct ToneDetect {
    /// Crossings seen in the window so far, and how long the window has run.
    crossings: u32,
    samples: u32,
    above: bool,
    /// Consecutive windows whose rate matched, in units of the window length.
    matched: f32,
    /// Samples in the window so far that sat near black and near white.
    dark: u32,
    bright: u32,
    /// Whether the window that just completed swung the full range.
    swing: bool,
    /// The levels of the window in progress, kept so a completed window can be
    /// measured for periodicity: see [`LevelSpectrum`].
    win: Vec<f32>,
}

impl ToneDetect {
    /// Feed one demodulated sample; returns the measured crossing rate once a
    /// window completes.
    fn push(&mut self, level: f32, window: u32) -> Option<f64> {
        let above = level > 0.5;
        // Hysteresis-free is fine here: the start signal is full-swing black to
        // white, and mid-grey noise between windows costs at most one crossing.
        if above != self.above {
            self.crossings += 1;
            self.above = above;
        }
        if self.samples == 0 {
            self.win.clear();
        }
        self.samples += 1;
        self.dark += (level < 0.25) as u32;
        self.bright += (level > 0.75) as u32;
        self.win.push(level);
        if self.samples < window {
            return None;
        }
        let rate = self.crossings as f64;
        // A real start signal spends about half its time at each extreme. A
        // steady carrier whose level merely wobbles about mid-grey crosses the
        // threshold just as often and never gets near either.
        self.swing = self.dark * 4 >= self.samples && self.bright * 4 >= self.samples;
        self.crossings = 0;
        self.samples = 0;
        self.dark = 0;
        self.bright = 0;
        Some(rate)
    }

    /// The levels of the window `push` has just completed. Valid until the next
    /// `push`.
    fn window(&self) -> &[f32] {
        &self.win
    }

    fn reset(&mut self) {
        self.win.clear();
        self.crossings = 0;
        self.samples = 0;
        self.matched = 0.0;
        self.dark = 0;
        self.bright = 0;
        self.swing = false;
    }
}

/// What a window of plain audio held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioTone {
    /// A pure tone at the start-signal pitch, [`START_AUDIO_HZ`].
    Start,
    /// A pure tone at the stop-signal pitch, [`STOP_TONE_HZ`].
    Stop,
    Other,
}

/// Scale a threshold that must be *reached*: a share of a window's energy.
///
/// `2 − k` and not `1 / k`. Dividing is the obvious form and it breaks at the
/// low end: the reference purity is 0.6, so `−5` would ask for 1.5 — a share no
/// window can hold — and the detector would stop hearing a start or a stop tone
/// at all, which silences the mode instead of steadying it. `2 − k` keeps every
/// fraction inside `(0, 1)`, moves the reference 0.6 to 0.24 at `+5` and 0.96 at
/// `−5`, is monotonic, and is exactly `1.0` at the reference setting.
fn fraction_threshold(f: f32, k: f64) -> f32 {
    f * (2.0 - k) as f32
}

/// Detector for a start or stop signal sent as a literal audio tone.
///
/// The standard describes both as the subcarrier alternating black/white, which
/// [`ToneDetect`] measures after the discriminator. A transmission can also
/// carry them as a plain 300 / 450 Hz tone, which lies outside the subcarrier's
/// passband and so is invisible to the discriminator (it reads as noise).
/// This looks at the raw audio instead: one FFT per window, the strongest line
/// between [`AUDIO_TONE_LO_HZ`] and [`AUDIO_TONE_HI_HZ`], and the share of the
/// window's energy that line holds. A tone is *pure* or it is not a tone — a
/// chart, noise or a voice spread their energy and never reach
/// [`AUDIO_TONE_PURITY`].
struct AudioToneDetect {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    buf: Vec<f32>,
    spec: Vec<Complex32>,
    scratch: Vec<Complex32>,
    hz_per_bin: f64,
    /// Threshold multiplier from the operator's sensitivity setting: the purity
    /// is scaled by [`fraction_threshold`] and the pitch tolerance multiplied,
    /// so `+` believes a fainter tone and `−` demands a cleaner one. Exactly
    /// `1.0` at the reference setting, so the default reading is untouched.
    scale: f64,
}

impl AudioToneDetect {
    fn new(rate: f64) -> Self {
        let n = ((rate * TONE_WINDOW_S) as usize).max(16);
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(n);
        let spec = fft.make_output_vec();
        let scratch = fft.make_scratch_vec();
        let window = (0..n).map(|i| 0.5 - 0.5 * (TAU * i as f64 / n as f64).cos() as f32).collect();
        AudioToneDetect {
            fft,
            window,
            buf: Vec::with_capacity(n),
            spec,
            scratch,
            hz_per_bin: rate / n as f64,
            scale: 1.0,
        }
    }

    /// Set the threshold multiplier (see [`WefaxRx::sens_k`]).
    fn set_scale(&mut self, scale: f64) {
        self.scale = scale;
    }

    /// Share of the low audio a line must hold to be a start or stop tone.
    fn purity_threshold(&self) -> f32 {
        fraction_threshold(AUDIO_TONE_PURITY, self.scale)
    }

    fn reset(&mut self) {
        self.buf.clear();
    }

    /// Feed one audio sample; the verdict on the window once it is full.
    fn push(&mut self, a: f32) -> Option<AudioTone> {
        self.buf.push(a);
        if self.buf.len() < self.window.len() {
            return None;
        }
        for (b, w) in self.buf.iter_mut().zip(&self.window) {
            *b *= w;
        }
        let ok = self.fft.process_with_scratch(&mut self.buf, &mut self.spec, &mut self.scratch);
        self.buf.clear();
        if ok.is_err() {
            return Some(AudioTone::Other);
        }
        let power: Vec<f32> = self.spec.iter().map(|c| c.norm_sqr()).collect();
        let lo = (AUDIO_TONE_LO_HZ / self.hz_per_bin) as usize;
        let hi = ((AUDIO_TONE_HI_HZ / self.hz_per_bin) as usize).min(power.len() - 1);
        // Purity is judged against the low part of the audio only. A carrier on
        // the channel, or the subcarrier itself, sits near 2 kHz and dominates
        // the total energy, so a perfectly clean 300 Hz tone beside one is a
        // twentieth of the whole — and was never heard.
        let top = ((AUDIO_TONE_BAND_HZ / self.hz_per_bin) as usize).min(power.len() - 1);
        let total: f32 = power[1.min(top)..=top].iter().sum();
        if total <= 1e-9 || lo >= hi {
            return Some(AudioTone::Other);
        }
        let peak = (lo..=hi).max_by(|&a, &b| power[a].total_cmp(&power[b]))?;
        // A Hann window spreads a line over a few bins either side.
        let held: f32 =
            power[peak.saturating_sub(3)..=(peak + 3).min(power.len() - 1)].iter().sum();
        if held < total * self.purity_threshold() {
            return Some(AudioTone::Other);
        }
        let hz = peak as f64 * self.hz_per_bin;
        Some(if (hz - START_AUDIO_HZ).abs() <= START_AUDIO_TOL_HZ * self.scale {
            AudioTone::Start
        } else if (hz - STOP_TONE_HZ).abs() <= STOP_AUDIO_TOL_HZ * self.scale {
            AudioTone::Stop
        } else {
            AudioTone::Other
        })
    }
}

/// The share of a window's brightness variation that lies near one pitch.
///
/// One FFT per completed [`ToneDetect`] window — a few thousand points twice a
/// second — so the check costs nothing next to the discriminator.
struct LevelSpectrum {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    buf: Vec<f32>,
    spec: Vec<Complex32>,
    scratch: Vec<Complex32>,
    hz_per_bin: f64,
}

impl LevelSpectrum {
    fn new(rate: f64, window_secs: f64) -> Self {
        let n = ((rate * window_secs) as usize).max(16);
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(n);
        let spec = fft.make_output_vec();
        let scratch = fft.make_scratch_vec();
        let window = (0..n).map(|i| 0.5 - 0.5 * (TAU * i as f64 / n as f64).cos() as f32).collect();
        LevelSpectrum {
            fft,
            window,
            buf: Vec::with_capacity(n),
            spec,
            scratch,
            hz_per_bin: rate / n as f64,
        }
    }

    /// Of everything that varies in `levels`, the share within [`PERIODIC_TOL`]
    /// of `hz`. Zero when the window is the wrong length or does not vary.
    fn share(&mut self, levels: &[f32], hz: f64) -> f32 {
        if levels.len() != self.window.len() {
            return 0.0;
        }
        let mean = levels.iter().sum::<f32>() / levels.len() as f32;
        self.buf.clear();
        self.buf.extend(levels.iter().zip(&self.window).map(|(l, w)| (l - mean) * w));
        if self.fft.process_with_scratch(&mut self.buf, &mut self.spec, &mut self.scratch).is_err()
        {
            return 0.0;
        }
        let total: f32 = self.spec.iter().skip(1).map(|c| c.norm_sqr()).sum();
        if total <= 1e-9 {
            return 0.0;
        }
        let lo = ((hz * (1.0 - PERIODIC_TOL)) / self.hz_per_bin) as usize;
        let hi =
            (((hz * (1.0 + PERIODIC_TOL)) / self.hz_per_bin) as usize).min(self.spec.len() - 1);
        if lo >= hi {
            return 0.0;
        }
        self.spec[lo..=hi].iter().map(|c| c.norm_sqr()).sum::<f32>() / total
    }
}

/// The last few lines, stacked, to see a phasing signal through the noise.
///
/// A phasing signal is the one thing on a fax channel that repeats exactly: the
/// same black line with the same white pulse, at the line rate. On a weak
/// signal no *single* line shows it — one real recording's pulse arrives as a
/// scatter of white and black blips whose edges wander by a tenth of a line from
/// one line to the next — but laid on top of each other the noise averages away
/// and the pulse stands out as a clean block. So the lock is taken on the
/// average of a run of lines, not on each line alone, and a run only counts
/// when nearly every line in it really does carry the pulse: a chart's fixed
/// graphic would pass the average, but not the line-by-line vote on a signal
/// that is not there.
///
/// The lines have to be cut on one clock — one line length apart — so that the
/// pulse lands in the same place in each. Both callers do that already.
#[derive(Debug, Default)]
struct PhasingStack {
    lines: std::collections::VecDeque<Vec<f32>>,
}

impl PhasingStack {
    fn reset(&mut self) {
        self.lines.clear();
    }

    /// Add a line; once `want` are held, where the picture begins in the line
    /// just added (samples from its start) and where the pulse is, if they make
    /// a phasing signal.
    fn push(&mut self, line: &[f32], want: usize) -> Option<(u32, PulseRegion)> {
        self.lines.push_back(line.to_vec());
        while self.lines.len() > want {
            self.lines.pop_front();
        }
        if self.lines.len() < want {
            return None;
        }
        // One clock cuts lines a sample apart in length, not more.
        let n = self.lines.iter().map(Vec::len).min()?;
        let mut profile = vec![0.0f32; n];
        for l in &self.lines {
            for (p, v) in profile.iter_mut().zip(l) {
                *p += v;
            }
        }
        let k = self.lines.len() as f32;
        profile.iter_mut().for_each(|p| *p /= k);
        let pulse = pulse_region(&profile, false)?;
        // The vote: the pulse has to be in nearly every line, or the average is
        // a few bright lines and a chart.
        let present = self.lines.iter().filter(|l| pulse.stands_out(&l[..n])).count();
        if present + 1 < self.lines.len() {
            return None;
        }
        Some((pulse.picture_start(n), pulse))
    }
}

/// The weather-fax receiver.
pub struct WefaxRx {
    rate: f64,

    // Discriminator, identical in shape to the SSTV one: down-mix to complex
    // baseband at the subcarrier centre, low-pass, then differentiate the phase.
    mix_ph: f32,
    mix_inc: f32,
    lpf: ComplexFir,
    prev: Complex32,
    have_prev: bool,
    inst_hz: f64,
    /// Where the chart's own excursions are centred, Hz: nominal until a
    /// page has shown both black and white. See [`WefaxRx::subcarrier_hz`].
    tune_hz: f64,
    /// The darkest and brightest frequency of each of the last lines.
    tune_lo: Vec<f32>,
    tune_hi: Vec<f32>,
    in_level: f32,

    // The picture's own discriminator: a wider band-pass than the detection
    // one (so edges are not rounded off), and a phase step kept per sample
    // instead of a frequency, so a pixel is the angle of the *sum* of the steps
    // across it. That weights each sample by its own signal strength: a weak or
    // crashed sample barely moves a pixel that has strong ones beside it.
    pic_lpf: ComplexFir,
    pic_prev: Complex32,
    pic_have_prev: bool,
    /// Recent mean length of a phase step, which a single step is capped to
    /// twice of, so an impulse cannot outvote the signal it lands on.
    pic_mag: f32,
    /// The phase steps of the samples since the line being built began (and the
    /// last one before it), and the sample position of the first of them.
    px_buf: Vec<Complex32>,
    px_start: f64,
    /// Whether the picture goes through the S-shaped contrast curve.
    contrast: bool,
    /// Whether a pixel with nothing to say takes its neighbour's value.
    fill: bool,

    // Settings.
    ioc: Ioc,
    lpm: Lpm,
    auto_start: bool,
    auto_stop: bool,
    /// Sample-clock trim in parts per million. Positive stretches the line,
    /// which corrects an image slanting to the left.
    slant_ppm: f64,
    /// How far the input's real sample rate is from `rate`, in ppm, as measured
    /// by whoever delivers it. Kept apart from `slant_ppm`: that is a trim and
    /// the auto-slant's to move, this is a fact about the stream that follows
    /// the receiver's gain, and the auto-slant measures what is left.
    clock_ppm: f64,
    /// Auto-slant: a least-squares fit of the phasing pulse's position against
    /// the phasing-line index. The pulse sits at the same place in every
    /// phasing line, so the slope of that fit *is* the line-clock error — and
    /// a fit over the whole phasing signal is a far steadier number than the
    /// walk between two lines twenty apart.
    slant_fit: SlantFit,
    /// Phasing lines seen so far, to index the fit without trusting
    /// `phase_samples` (the line length changes as the trim does).
    phasing_lines: u32,
    /// The correction that put the trim where it is, in parts per million, and
    /// the number of phasing lines the fit behind it covered. Zero lines means
    /// no correction was ever applied and the trim is the operator's own.
    ///
    /// Kept because the trim is otherwise invisible: a page that leaned and a
    /// page that was corrected to the same tilt look identical in the picture,
    /// and the two floors (25 ppm from twenty lines, 10 from fifty) mean the
    /// difference between "it acted" and "it never had enough signal to act" is
    /// a number nobody could see. Reported with each page (#444).
    last_slant: (f64, u32),
    /// The operator's own trim: where every page starts. The auto-slant adds to
    /// it for the page it measured and is dropped at the next one, so a wrong
    /// measurement on a weak phasing signal cannot lean the pages after it.
    slant_base: f64,
    /// The trim before the last auto correction, that correction, and the
    /// `last_slant` it replaced; held until the next window of phasing lines
    /// has checked the correction made the walk smaller, not larger.
    slant_pending: Option<(f64, f64, (f64, u32))>,
    /// No more auto-slant on this page: the last correction was undone, by the
    /// check above or by the operator.
    slant_locked: bool,
    /// Windows in a row that walked the other way from the correction last
    /// made; one is a mis-measure, two in a row undo the correction.
    slant_contra: u8,
    /// The lines drawn since the picture-based slant last measured, kept to
    /// measure the next window from: see [`WefaxRx::image_slant`].
    lean_rows: Vec<Vec<u8>>,
    /// The walks the last windows measured, in parts per million.
    lean_hist: Vec<f64>,
    /// The operator reverted the auto-slant on this page: the picture-based one
    /// stays off as well. A pulse-based correction undone by its own check does
    /// not turn it off.
    lean_off: bool,
    /// Measure the picture's lean on a thread of its own. The measurement is
    /// a few hundred milliseconds of arithmetic every 80 lines; run inside
    /// [`WefaxRx::process`] it stops the audio for that long, and a decoder fed
    /// live has its samples arrive late (the stream's clock is read from when
    /// they do) and a radio's buffers overrun. Off by default, so a recording
    /// decoded offline gives the same picture every time.
    lean_background: bool,
    /// The measurement running on that thread, if one is.
    lean_job: Option<std::sync::mpsc::Receiver<Option<(f64, f64)>>>,
    /// Whether the receiver corrects the slant by itself at all, from the
    /// phasing signal and from the picture. On unless the operator turned it off.
    auto_slant: bool,
    /// The last line drawn, to stand in for a line the receiver lost.
    last_line: Vec<u8>,
    /// Lost lines repeated in a row.
    lost_run: u32,

    phase: Phase,
    /// Demodulated brightness of the line being built, one entry per input
    /// sample since the line began.
    line_buf: Vec<f32>,
    /// Fractional sample position where the current line started, so the line
    /// clock never accumulates rounding error.
    line_start: f64,
    line_no: u16,
    sample_idx: u64,

    // Phasing.
    /// Samples spent phasing, so a transmission whose phasing signal was missed
    /// still produces a (misaligned) chart rather than nothing.
    phase_samples: u64,
    /// Whether the picture in progress was begun by a start tone rather than
    /// by the operator.
    auto_begun: bool,
    /// Consecutive completed lines that looked like a phasing signal *while a
    /// picture was being built*, and where the last one's pulse sat. See
    /// [`WefaxRx::note_phasing_line`].
    rephase_lines: u32,
    rephase_skew: f64,
    /// Whether any line of the picture in progress was not blank, and the
    /// white lines in a row since, tolerating short fades. See
    /// [`BLANK_END_S`].
    had_content: bool,
    blank_run: u32,
    blank_gap: u32,
    /// Seconds the subcarrier has been absent, in the tone windows that measure
    /// its share of the band. A page whose carrier is gone is over: between two
    /// transmissions a station is silent for minutes, and a page that runs
    /// through that gap draws noise.
    carrier_gone_s: f64,
    /// The subcarrier band's mean power as it has been, which the drop is
    /// measured against. Only ever follows a window that is *not* far below it,
    /// so a gap cannot walk the reference down into the noise it is being
    /// measured against.
    carrier_ref: f32,
    /// The last window's power against that reference, for the log: 1.0 is a
    /// carrier where it has been, 0.01 is one twenty decibels down.
    carrier_ratio: f32,

    start_det: ToneDetect,
    stop_det: ToneDetect,
    /// Consecutive matching windows, for start and stop respectively.
    start_run: u32,
    stop_run: u32,
    /// Windows missed in a row since the last stop-tone match.
    stop_miss: u32,
    /// Which IOC the start tone that armed us implied.
    start_ioc: Ioc,

    /// Plain-audio start and stop tones, see [`AudioToneDetect`].
    audio_tone: AudioToneDetect,
    audio_start_run: u32,
    audio_stop_run: u32,
    audio_stop_miss: u32,
    /// An automatic start that has not yet been confirmed by its phasing
    /// signal: nothing has been announced, so a carrier that was never a
    /// transmission leaves no trace.
    start_pending: bool,
    /// Audio energy and the part of it inside the subcarrier's band, over the
    /// start-detector window in progress.
    band_raw: f32,
    band_sub: f32,

    /// Periodicity of a completed tone-detector window, see [`LevelSpectrum`].
    level_spec: LevelSpectrum,
    /// The run of phasing lines being counted: while locking, and while idle.
    phasing_stack: PhasingStack,
    /// Lines of phasing signal that must be in the run to lock.
    lock_lines: u32,
    /// While idle: the brightness and the subcarrier frequency of the line
    /// going by, on a clock of its own, and where that line ends.
    ///
    /// A transmission can begin at any moment and its line clock is unknown, so
    /// the idle line is cut at the line rate from wherever the receiver was
    /// switched on. Taken round the circle of the line, a phasing signal looks
    /// the same from any starting point, which is what makes this possible.
    idle_levels: Vec<f32>,
    idle_hz: Vec<f32>,
    idle_end: f64,
    /// The subcarrier frequency of each sample of the line being built, to
    /// measure black and white from when the line proves to be phasing.
    hz_buf: Vec<f32>,
    /// Where black and white actually are, Hz: nominal until a phasing signal
    /// says otherwise.
    black_hz: f64,
    white_hz: f64,
    /// Phasing lines at the head of a page begun by its phasing signal alone
    /// are the rest of that signal, not chart, and are dropped; how many have
    /// been so far.
    lead_phasing: Option<PulseRegion>,
    lead_skipped: u32,
    /// How many of those leading lines are drawn all the same, as a thin strip
    /// marking the top of the page: [`LEAD_PHASING_KEEP`] after a start tone,
    /// none for a page begun by phasing alone.
    lead_keep: u32,
    /// No page is begun by phasing alone before this sample: see [`IDLE_HOLD_S`].
    idle_hold_until: u64,
    /// The operator's receive sensitivity, `-5..=+5`. See [`WefaxRx::sens_k`].
    sensitivity: i8,
}

/// How long a tone-detector window is, in seconds. Long enough that a chart's
/// own black/white detail cannot fill one, short enough that several fit in the
/// five-second start signal.
const TONE_WINDOW_S: f64 = 0.5;
/// Matching windows needed to accept a start signal while *idle* (≈1.5 s).
const TONE_RUN: u32 = 3;
/// Seconds an automatic start may wait for its phasing signal. A real
/// transmission shows its phasing a few seconds after the start tone ends. A
/// start tone that never does was the chart's own detail or noise, and it
/// yields no picture rather than one drawn on no phase at all.
const AUTO_PHASE_TIMEOUT_S: f64 = 20.0;
/// Matching windows needed to accept a stop signal (≈3.5 s). A stop tone is
/// five seconds long; requiring most of it keeps a noise burst or a fade from
/// ending a chart whose signal is merely dipping (issue #496).
const STOP_RUN: u32 = 7;
/// Windows in a row a stop tone may miss before the run starts over, so a fade
/// or a noise burst inside the tone does not throw away the windows already
/// matched.
const STOP_MISS_TOL: u32 = 2;
/// The share of a window's variation that sits at the stop pitch above which
/// the window counts as the stop tone even when it did not swing the whole
/// range. A chart's own detail never gets near it: a real tone reads 0.5-0.95.
const STOP_SHARE_SURE: f32 = 0.5;
/// How long an automatically begun page may carry nothing but a white carrier,
/// after chart content, before it is ended. Stations keep a white carrier up
/// after a chart; if the stop tone was missed it is the only way out of the
/// picture, and it would otherwise hide every later start tone.
///
/// **Two minutes, not one.** A chart's own footer is very nearly white — the
/// wind-scale box of a GYA chart ran a minute and a half of near-white before
/// the transmission ended — and a minute closed the page in the middle of it
/// (bench, 2026-11-03). The stop tone ends a page long before this; this rule
/// is the fallback for a station that keeps a carrier up instead, and being
/// slow costs a listener nothing.
const BLANK_END_S: f64 = 120.0;
/// Brightness a line's mean must reach to count as blank (white carrier).
const BLANK_MEAN: f32 = 0.9;
/// Brightness below which a sample is **ink** rather than white paper.
const INK_LEVEL: f32 = 0.25;
/// How many ink samples a line needs before it counts as content.
///
/// One is too few: a noisy white carrier throws the odd dark speckle and those
/// must not keep a page open for ever. Two is enough to catch what a chart
/// actually puts in its own whitespace — the ruled border of a scale box, a
/// caption — because a vertical rule runs through *every* line of the block it
/// encloses (bench, 2026-11-03).
const INK_SAMPLES: usize = 2;
/// Non-blank lines in a row (fading) that restart the blank count.
const BLANK_GAP_LINES: u32 = 6;
/// How far a measured crossing rate may be from the nominal one, as a fraction.
const TONE_TOL: f64 = 0.18;
/// Pitch of a start signal sent as a plain audio tone, Hz — the 300 Hz of
/// [`Ioc::I576`].
const START_AUDIO_HZ: f64 = 300.0;
/// How far a start or stop tone may sit from its nominal pitch, Hz. Propagation
/// and a receiver a little off tune move a tone in the audio by tens of hertz;
/// the two windows do not overlap, so the start and stop tones cannot be
/// mistaken for one another.
const START_AUDIO_TOL_HZ: f64 = 75.0;
const STOP_AUDIO_TOL_HZ: f64 = 75.0;
/// The part of the audio a tone's purity is judged against, Hz.
const AUDIO_TONE_BAND_HZ: f64 = 1000.0;
/// The band searched for an audio start or stop tone, Hz.
const AUDIO_TONE_LO_HZ: f64 = 200.0;
const AUDIO_TONE_HI_HZ: f64 = 520.0;
/// Share of a window's energy a tone must hold to count as one.
const AUDIO_TONE_PURITY: f32 = 0.6;
/// Share of a window's audio energy that must lie in the subcarrier's band for
/// an alternating start signal to be believed.
const SUBCARRIER_SHARE: f32 = 0.25;
/// Share of a window's brightness variation that must sit at the tone's own
/// pitch, within [`PERIODIC_TOL`], for an alternation to be believed.
///
/// Counting mid-grey crossings cannot tell a start or stop signal from noise:
/// a weak chart's discriminator noise crosses mid-grey four to five hundred
/// times a second, which is the stop tone's rate, and one real recording was cut
/// that way twenty seconds before its own end. A square wave puts four fifths of
/// its energy on its fundamental; that recording's noise put a tenth there.
const PERIODIC_SHARE: f32 = 0.3;
/// How far from its nominal pitch the energy of an alternation may sit and
/// still count, as a fraction. A sound card and a receiver a little off tune
/// move it by a few percent either way, and propagation by more.
const PERIODIC_TOL: f64 = 0.15;
/// Lines in a row, with their phasing pulse in one place, that lock the line
/// clock on a transmission that announced itself with a start tone.
const LOCK_LINES: u32 = PHASE_LINES;
/// Seconds of phasing signal that begin a transmission on their own, with no
/// start tone at all.
///
/// A start tone is five seconds of the subcarrier alternating and a fade, or a
/// carrier standing on the channel, swallows it — and one real recording's
/// start tone sat under a carrier and was never there at all. The phasing signal
/// that follows is the one thing on a fax channel that is *periodic at the line
/// rate*: black but for a white pulse, in the same place line after line. The
/// standard asks for thirty seconds of it and stations send anything from five
/// up, so this is as short as the shortest of them allows. Three seconds of lines
/// with the pulse in one place is not something a chart does, and it is what a
/// fade between two transmissions cannot imitate.
const IDLE_PHASING_S: f64 = 3.0;
/// The steadiest a line may be, as a standard deviation of its brightness, and
/// still be a carrier rather than a picture.
///
/// A carrier on the channel is steady at whatever frequency it sits — above
/// white, on it, or somewhere in the middle of the scale if it has drifted —
/// and so reads as a line with no detail in it. White is the commonest case and
/// not the only one.
const FLAT_STD: f32 = 0.04;
/// How long after a page ends the phasing signal alone may not begin another,
/// seconds. Charts end on black bars and a station's own return-to-start signal
/// can look like a few seconds of phasing; neither is a transmission starting.
/// A real next transmission announces itself by tone, or is minutes away.
const IDLE_HOLD_S: f64 = 15.0;
/// Brightness a flat line's mean must reach to count as the bright end of the
/// scale. A flat *dark* line is a stop signal's black, or a chart's solid black
/// bar, and neither of those is a carrier left on after the end.
const FLAT_MEAN: f32 = 0.45;
/// The span the black and white references may be re-measured within, Hz.
///
/// A receiver off tune moves the whole subcarrier, so black does not sit at
/// 1500 Hz and white at 2300; the phasing signal shows where they actually are.
/// The limits are wide on purpose — a hundred and fifty hertz of tuning error
/// or Doppler is ordinary — and narrow enough that a line which is not phasing
/// at all cannot move the scale.
const BLACK_RANGE_HZ: (f64, f64) = (1300.0, 1700.0);
const WHITE_RANGE_HZ: (f64, f64) = (2100.0, 2500.0);
const SPAN_RANGE_HZ: (f64, f64) = (600.0, 1000.0);
/// Lines the tuning centre is read over, and the span between black and white
/// (Hz) a reading must show to be believed. Black is 1500 Hz and white 2300:
/// a window that shows less than 600 Hz of it has seen only paper or only ink,
/// and its middle is not the subcarrier's.
const TUNE_WINDOW_LINES: usize = 40;
const TUNE_SPAN_HZ: (f64, f64) = (600.0, 1100.0);

/// A calibration window widened — or narrowed — about its own centre by `k`.
///
/// At `k = 1.0` the arithmetic is exact, so each window is the constant itself:
/// black, white and the span are a centre ±200 Hz. See [`WefaxRx::sens_k`].
fn cal_range(range: (f64, f64), k: f64) -> (f64, f64) {
    let mid = 0.5 * (range.0 + range.1);
    let half = 0.5 * (range.1 - range.0) * k;
    (mid - half, mid + half)
}
/// Give up looking for the phasing pulse after this long and start the picture
/// anyway — a misaligned chart beats no chart.
const PHASE_TIMEOUT_S: f64 = 35.0;
/// Phasing lines to average the skew over before locking.
const PHASE_LINES: u32 = 5;
/// How long a transmission's own phasing signal lasts, in seconds.
///
/// The standard is thirty: a five-second start tone, then thirty seconds of
/// phasing, then the chart. The decoder takes its lock on the *first* phasing
/// pulse, so the opening thirty seconds of every page are the rest of that
/// signal — and the signal is black lines with a white pulse, which is exactly
/// what the rephase rule looks for. This is the guard that keeps it from cutting
/// the page it has just started: see [`WefaxRx::note_phasing_line`].
const PHASING_SIGNAL_S: f64 = 30.0;
/// Lines of the phasing signal left at the head of a page, so the top of the
/// picture still shows where the signal ended and the chart began. The rest of
/// its half minute (a lock takes a few lines of it) is dropped, which is what
/// other WEFAX programs do with it.
const LEAD_PHASING_KEEP: u32 = 3;
/// Phasing-looking lines in a row, in the middle of a picture, that mean the
/// next transmission has begun on top of it.
///
/// Eight is four seconds at 120 LPM — short enough to lose almost none of the
/// new chart's thirty-second phasing signal, long enough that a moment's worth
/// of chart that happens to look like phasing is not enough on its own. Four
/// was not (issue #496), but note what the length does and does not buy: a
/// graphic *fixed* in the chart holds its shape for the whole page, so no run
/// length rejects it. That is [`phasing_skew_strict`]'s job; this only keeps a
/// passing shape from cutting the page on a line or two of it.
const REPHASE_LINES: u32 = 8;
/// Share of a *mid-picture* line that may be bright outside the phasing pulse
/// and still count as phasing. A real phasing line is black but for one narrow
/// pulse; a chart's own thick black bar with white lettering (the water
/// temperature chart's longitude labels, #444) is the same shape line after
/// line but carries several bright patches. The run-length test cannot tell
/// them apart — a graphic fixed in the chart lasts the whole page — so this is
/// the property that does.
const REPHASE_BRIGHT_OUTSIDE: f32 = 0.12;
/// Least-squares sums of a phasing pulse's position against its line index:
/// `n`, `Σi`, `Σp`, `Σip` and `Σi²`.
///
/// The slope is the line-clock error in samples per line. Accumulated rather
/// than stored, because a page reads a line every half second for as long as
/// the phasing lasts and the whole fit is five numbers.
#[derive(Clone, Copy, Default)]
struct SlantFit {
    n: f64,
    sx: f64,
    sy: f64,
    sxy: f64,
    sxx: f64,
    syy: f64,
    /// The last position added, unwrapped (see [`SlantFit::unwrap`]).
    last: Option<f64>,
}

impl SlantFit {
    /// `pos`, a position in a line of `n` samples, moved to the whole-number
    /// lap nearest the last one added.
    ///
    /// A pulse that sits near the line boundary reads as `n - 3`, then `2`, then
    /// `n - 1` as it walks: a straight drift that the fit takes for a jump of a
    /// whole line. Following it round the circle keeps the drift a drift. The
    /// first position is taken as it is.
    ///
    /// A position further from the last than a clock could have moved it in one
    /// line (`max_step`) is not a drift but a chart line that passed the pulse
    /// test, or the lock's own longer first line: `None`, and it leaves the fit
    /// as it was. Until a few positions are in, though, the baseline is not yet
    /// trusted and the new position replaces it.
    fn unwrap(&mut self, pos: f64, n: f64, max_step: f64) -> Option<f64> {
        let Some(last) = self.last else {
            self.last = Some(pos);
            return Some(pos);
        };
        let step = (pos - last + n / 2.0).rem_euclid(n) - n / 2.0;
        if step.abs() > max_step {
            if self.n < 3.0 {
                *self = Self { last: Some(pos), ..Self::default() };
                return Some(pos);
            }
            return None;
        }
        self.last = Some(last + step);
        Some(last + step)
    }

    /// Standard error of the slope, samples per line: how far the pulse's
    /// positions scatter around the fitted line, over the spread of the lines.
    fn slope_error(&self) -> f64 {
        let sxx = self.sxx - self.sx * self.sx / self.n;
        if self.n < 3.0 || sxx <= 0.0 {
            return f64::INFINITY;
        }
        let sxy = self.sxy - self.sx * self.sy / self.n;
        let syy = self.syy - self.sy * self.sy / self.n;
        let sse = (syy - sxy * sxy / sxx).max(0.0);
        (sse / (self.n - 2.0) / sxx).sqrt()
    }

    fn add(&mut self, i: f64, p: f64) {
        self.n += 1.0;
        self.sx += i;
        self.sy += p;
        self.sxy += i * p;
        self.sxx += i * i;
        self.syy += p * p;
    }

    /// Samples of walk per line, once there are enough lines to say.
    fn slope(&self, need: u32) -> Option<f64> {
        if self.n < f64::from(need) {
            return None;
        }
        let den = self.n * self.sxx - self.sx * self.sx;
        if den.abs() < f64::EPSILON {
            return None;
        }
        Some((self.n * self.sxy - self.sx * self.sy) / den)
    }
}

/// Seconds without the subcarrier that end a page.
///
/// Long enough to ride a deep fade on a night path — a chart that ends early
/// because the band dropped for a minute is the complaint #426 was — and short
/// enough to cut the silence between two transmissions, which is minutes.
const CARRIER_GONE_S: f64 = 90.0;
/// How far the subcarrier band's **power** must fall below where it has been,
/// as a fraction, before the carrier counts as gone: 10 dB.
///
/// The first attempt tested the subcarrier band's *share* of the audio — what
/// the tone detectors use — and the bench threw it out: over a real gap every
/// one of 1687 windows still read "in band", because a receiver's audio is
/// already band-limited and noise fills the share. What collapses when a
/// station goes off is the power in that band, by tens of dB.
const CARRIER_DROP: f32 = 0.1;
/// Time constant of the reference that drop is measured against, in seconds:
/// long enough to ride a chart's own modulation, short enough to settle within
/// a page.
const CARRIER_REF_S: f32 = 20.0;
/// Phasing lines the auto-slant measures the drift over. Twenty is ten seconds
/// at 120 LPM, well inside the thirty-second phasing signal, and long enough
/// that a few parts-per-million of line-clock error move the pulse several
/// samples — which is what makes the slant measurable at all.
const SLANT_PROBE_LINES: u32 = 12;
/// The smallest drift the auto-slant will act on once the whole phasing signal
/// has been fitted, in parts per million.
///
/// A chart of DWD on 3855 (bench, 2026-11-03 14:07) came out with its frame
/// border walking 51 px to the right over 1354 lines: twenty-one parts per
/// million, under the twenty-five a twenty-line measurement can act on, so
/// nothing was applied and the page leaned. Fitting the whole signal brings the
/// measurement's own noise down to a couple of parts per million, and this is
/// the floor that then becomes useful. Five now: the whole signal of the page
/// of 2026-11-12 (54 lines) read +6.4 ppm and the page's clock was 6 off.
const SLANT_MIN_PPM: f64 = 5.0;
/// The smallest drift the auto-slant will act on from a short fit, in parts per
/// million. Below this the pulse's own edge-wander swamps the measurement, and
/// "correcting" it would only add a wobble the operator never asked for.
///
/// Sixty, not twenty-five: the first dozen lines of that page read +32 ppm
/// while the signal as a whole said +6, the early lines being pulled by the
/// receiver settling, and the trim they set leaned the first 880 lines of the
/// page by 26 ppm. A slant worth acting on at a dozen lines is a large one.
const SLANT_COARSE_MIN_PPM: f64 = 60.0;
/// Phasing lines the fit must cover before a correction under
/// [`SLANT_COARSE_MIN_PPM`] is believed. The phasing signal is thirty seconds —
/// sixty lines at 120 LPM, and the lock takes five — so this is most of it.
const SLANT_FINE_LINES: u32 = 50;
/// The largest line-clock error the auto-slant will believe, in parts per
/// million.
///
/// A sound card's crystal and the sending station's own clock are a few hundred
/// ppm apart at the very worst, so nothing this large is a clock error: it is a
/// *different* white patch matched by mistake, or noise that moved the measured
/// pulse. Acting on it is worse than doing nothing. On a noisy phasing signal
/// the drift measurement came out at **47 500 ppm**, the trim took the line
/// 4.75 % short, and a chart that should have been merely grainy came out as a
/// page of bands — issue #239, where the decoder was worse than leaving the
/// clock alone.
const SLANT_MAX_PPM: f64 = 2_000.0;
/// Lines the picture-based slant measures over at the least. A lean of ten
/// parts per million moves the last of them a pixel from the first, which the
/// columns' sharpness resolves; a shorter stretch is a chart's texture.
const LEAN_BLOCK: usize = 160;
/// How much sharper than the typical candidate the best lean must score.
const LEAN_CONFIDENCE: f64 = 1.15;
/// Lines a stretch is measured from, how many more are added between two
/// measurements, and the most it grows to.
const LEAN_STEP_LINES: usize = 80;
/// What each half of a stretch must score, and how far from the whole's lean
/// (parts per million) its own may be.
const LEAN_HALF_CONFIDENCE: f64 = 1.1;
const LEAN_HALF_PPM: f64 = 30.0;
const LEAN_MAX_LINES: usize = 480;
/// Stretches whose leans are averaged, the smallest average worth correcting
/// (parts per million), how far apart the stretches may be against it, the
/// largest lean one may name and the grid it is searched on.
const LEAN_WINDOWS: usize = 2;
const LEAN_MIN_PPM: f64 = 10.0;
const LEAN_MAX_PPM: f64 = 300.0;
const LEAN_STEP_PPM: f64 = 10.0;
const LEAN_STEADY: f64 = 0.5;
/// The share of the average corrected at a time.
const LEAN_GAIN: f64 = 0.9;
/// The largest input clock error [`WefaxRx::set_clock_ppm`] follows.
const CLOCK_MAX_PPM: f64 = 1_000.0;
/// How many standard errors a fitted walk must stand above before it is
/// believed to be a clock error and not the pulse's scatter.
const SLANT_SIGNIFICANCE: f64 = 3.0;
/// How far a phasing line's pulse must stand above the line's own median
/// brightness to be read as a pulse rather than as a chart's own bright area.
///
/// Measured on a real, weak chart (GHI, 2026-11-01): the body of a phasing line
/// sat at a median near 0.3 with noise on it, and the pulse still reached 1.00,
/// so a contrast of 0.35 is comfortably met by a real pulse and not by the
/// smeared bright areas a chart produces.
const PHASING_CONTRAST: f32 = 0.35;
/// How far two phasing lines' pulses may sit apart, as a fraction of a line,
/// and still be read as the same phasing signal. The line clock drifts by parts
/// per million between lines, so a real signal's pulse barely moves; what this
/// rejects is *moving* picture content, which is most of it.
///
/// It was once reasoned that a picture would not put its white patch in the
/// same place twice running. A chart's fixed graphic does exactly that, for as
/// many lines as it is on the page — which is how pictures were being cut in
/// two (issue #496) — so this tolerance narrows the field and does not close
/// it. The shape test in [`phasing_skew_strict`] is what closes it.
const REPHASE_TOL: f64 = 0.02;

impl WefaxRx {
    pub fn new(rate: f64) -> Self {
        WefaxRx {
            rate,
            mix_ph: 0.0,
            mix_inc: (TAU as f32) * CENTRE_HZ as f32 / rate as f32,
            // ±1100 Hz passes the ±400 Hz deviation with room for a badly
            // tuned receiver, and rejects the rest of the SSB channel.
            lpf: ComplexFir::new(bandpass_taps(129, -1100.0, 1100.0, rate)),
            prev: Complex32::new(0.0, 0.0),
            have_prev: false,
            inst_hz: CENTRE_HZ,
            tune_hz: CENTRE_HZ,
            tune_lo: Vec::new(),
            tune_hi: Vec::new(),
            in_level: 0.0,
            pic_lpf: ComplexFir::new(bandpass_taps(129, -PIC_HALF_BW_HZ, PIC_HALF_BW_HZ, rate)),
            pic_prev: Complex32::new(0.0, 0.0),
            pic_have_prev: false,
            pic_mag: 0.0,
            px_buf: Vec::new(),
            px_start: 0.0,
            contrast: true,
            fill: true,
            ioc: Ioc::default(),
            lpm: Lpm::default(),
            auto_start: true,
            auto_stop: true,
            slant_ppm: 0.0,
            clock_ppm: 0.0,
            slant_fit: SlantFit::default(),
            phasing_lines: 0,
            last_slant: (0.0, 0),
            slant_base: 0.0,
            slant_pending: None,
            slant_locked: false,
            slant_contra: 0,
            lean_rows: Vec::new(),
            lean_hist: Vec::new(),
            lean_off: false,
            lean_background: false,
            lean_job: None,
            auto_slant: true,
            last_line: Vec::new(),
            lost_run: 0,
            phase: Phase::Idle,
            line_buf: Vec::new(),
            line_start: 0.0,
            line_no: 0,
            sample_idx: 0,
            phase_samples: 0,
            auto_begun: false,
            rephase_lines: 0,
            rephase_skew: 0.0,
            start_det: ToneDetect::default(),
            stop_det: ToneDetect::default(),
            start_run: 0,
            stop_run: 0,
            stop_miss: 0,
            had_content: false,
            blank_run: 0,
            carrier_gone_s: 0.0,
            carrier_ref: 0.0,
            carrier_ratio: 1.0,
            blank_gap: 0,
            start_ioc: Ioc::default(),
            audio_tone: AudioToneDetect::new(rate),
            audio_start_run: 0,
            audio_stop_run: 0,
            audio_stop_miss: 0,
            start_pending: false,
            band_raw: 0.0,
            band_sub: 0.0,
            level_spec: LevelSpectrum::new(rate, TONE_WINDOW_S),
            phasing_stack: PhasingStack::default(),
            lock_lines: LOCK_LINES,
            idle_levels: Vec::new(),
            idle_hz: Vec::new(),
            idle_end: 0.0,
            hz_buf: Vec::new(),
            black_hz: BLACK_HZ,
            white_hz: WHITE_HZ,
            lead_phasing: None,
            lead_skipped: 0,
            lead_keep: 0,
            idle_hold_until: 0,
            sensitivity: 0,
        }
    }

    pub fn ioc(&self) -> Ioc {
        self.ioc
    }

    pub fn lpm(&self) -> Lpm {
        self.lpm
    }

    /// Set the index of cooperation. Ignored mid-picture: changing the line
    /// length halfway would leave the top and the bottom of one image at
    /// different widths.
    pub fn set_ioc(&mut self, ioc: Ioc) {
        if self.phase == Phase::Idle {
            self.ioc = ioc;
        }
    }

    pub fn set_lpm(&mut self, lpm: Lpm) {
        if self.phase == Phase::Idle {
            self.lpm = lpm;
            self.reset_idle();
        }
    }

    /// Whether a start tone begins a picture on its own.
    pub fn set_auto_start(&mut self, on: bool) {
        self.auto_start = on;
    }

    /// Whether a stop tone ends one.
    pub fn set_auto_stop(&mut self, on: bool) {
        self.auto_stop = on;
    }

    /// Sample-clock trim, parts per million.
    pub fn set_slant_ppm(&mut self, ppm: f32) {
        self.slant_ppm = ppm as f64;
        self.slant_base = ppm as f64;
        self.slant_pending = None;
    }

    /// Undo the auto-slant on the page in progress: the trim goes back to the
    /// operator's own and the auto-slant stays off until the next page. The
    /// lines already drawn are not touched, only the ones still to come.
    /// Let the receiver correct the slant by itself, or leave the line clock to
    /// the operator's own trim alone. Takes effect on the lines still to come.
    pub fn set_auto_slant(&mut self, on: bool) {
        self.auto_slant = on;
    }

    /// Whether the receiver corrects the slant by itself.
    pub fn auto_slant(&self) -> bool {
        self.auto_slant
    }

    pub fn revert_slant(&mut self) {
        self.slant_ppm = self.slant_base;
        self.slant_pending = None;
        self.slant_contra = 0;
        self.slant_locked = true;
        self.lean_off = true;
        self.slant_fit = SlantFit::default();
        self.phasing_lines = 0;
        self.last_slant = (0.0, 0);
    }

    /// The auto-slant is off for this page because a correction was undone.
    pub fn slant_reverted(&self) -> bool {
        self.slant_locked
    }

    /// The input's real sample rate against its nominal one, in ppm (negative:
    /// it runs slow), as measured by whoever delivers it. The line length
    /// follows at once, on top of the trim and whatever the auto-slant found.
    pub fn set_clock_ppm(&mut self, ppm: f64) {
        self.clock_ppm = ppm.clamp(-CLOCK_MAX_PPM, CLOCK_MAX_PPM);
    }

    /// Measure the picture's lean on a thread of its own instead of inside
    /// [`Self::process`]. For a decoder fed live: see the field `lean_background`.
    pub fn set_background_lean(&mut self, on: bool) {
        self.lean_background = on;
    }

    /// Wait for the measurement on its thread and take it in (tests).
    #[cfg(test)]
    fn wait_for_lean(&mut self) {
        if let Some(job) = self.lean_job.take() {
            if let Ok(measured) = job.recv() {
                self.take_lean(measured);
            }
        }
    }

    /// The input clock error last given to [`Self::set_clock_ppm`].
    pub fn clock_ppm(&self) -> f64 {
        self.clock_ppm
    }

    /// The sample-clock trim now in force.
    pub fn slant_ppm(&self) -> f32 {
        self.slant_ppm as f32
    }

    /// The correction that put [`Self::slant_ppm`] where it is: the parts per
    /// million applied, and the phasing lines the fit behind it covered. Zero
    /// lines means nothing was applied.
    pub fn slant_correction(&self) -> (f32, u32) {
        (self.last_slant.0 as f32, self.last_slant.1)
    }

    /// The subcarrier band's power against where it has been: 1.0 is a carrier
    /// at the level it has held, 0.01 is one twenty decibels down. What the
    /// carrier-gone rule reads, and what the log quotes while a page runs on.
    pub fn carrier_ratio(&self) -> f32 {
        self.carrier_ratio
    }

    /// The operator's receive sensitivity, `-5..=+5`; see [`WefaxRx::sens_k`].
    pub fn sensitivity(&self) -> i8 {
        self.sensitivity
    }

    /// Set the receive sensitivity, `-5..=+5`, clamped.
    ///
    /// A fax is frequency modulated, so signal strength does not change the
    /// greys — this moves the decoder's *thresholds* instead. See
    /// [`WefaxRx::sens_k`] for what is scaled and what deliberately is not.
    pub fn set_sensitivity(&mut self, sensitivity: i8) {
        self.sensitivity = sensitivity.clamp(-5, 5);
        let k = self.sens_k();
        self.audio_tone.set_scale(k);
    }

    /// Threshold multiplier from the sensitivity setting: `1 + 0.12·s`, so
    /// `0.4` at `-5` and `1.6` at `+5`.
    ///
    /// `+` is more sensitive: a threshold said to *accept* something is widened
    /// (the tone pitch tolerance, the audio start/stop window, the phasing
    /// agreement and the black/white calibration window are all multiplied),
    /// and one that must be *reached* is lowered (the audio tone's purity, the
    /// subcarrier share and the periodic share, through
    /// [`fraction_threshold`]). `−` does the opposite: it waits for a cleaner
    /// signal.
    ///
    /// Exactly `1.0` at the reference setting, and multiplying by it is exact,
    /// so `0` reproduces the decoder's own behaviour bit for bit.
    fn sens_k(&self) -> f64 {
        1.0 + 0.12 * f64::from(self.sensitivity)
    }

    /// How far a measured crossing rate may sit from the nominal one.
    fn tone_tol(&self) -> f64 {
        TONE_TOL * self.sens_k()
    }

    /// How far from its nominal pitch a phasing pulse may repeat, as a fraction.
    fn rephase_tol(&self) -> f64 {
        REPHASE_TOL * self.sens_k()
    }

    /// Share of a window's audio energy that must lie in the subcarrier's band.
    fn subcarrier_share(&self) -> f32 {
        fraction_threshold(SUBCARRIER_SHARE, self.sens_k())
    }

    /// Share of a window's brightness variation that must sit at the tone's own
    /// pitch.
    fn periodic_share(&self) -> f32 {
        fraction_threshold(PERIODIC_SHARE, self.sens_k())
    }

    /// Smoothed input level, for the panel's tuning meter.
    pub fn level(&self) -> f32 {
        self.in_level
    }

    /// Where the chart's excursions are centred, for the tuning indicator: a
    /// correctly tuned receiver puts black at 1500 Hz and white at 2300, so
    /// their middle at 1900. Read over the last [`TUNE_WINDOW_LINES`] lines of a
    /// page that has shown both, and 1900 Hz (nothing to correct) when there is
    /// no such page.
    ///
    /// It used to be the frequency of the pixel being decoded. That is the
    /// picture's own content: +400 Hz on white paper, -400 on ink, +420 on a
    /// carrier standing on the channel, and a dial moved by it chased the page
    /// (bench, 2026-11-12: the dial walked 335 Hz and the lines it crossed came
    /// out as grey and black bands).
    pub fn subcarrier_hz(&self) -> f64 {
        self.tune_hz
    }

    /// Fold one line's frequencies into [`WefaxRx::subcarrier_hz`].
    fn note_tuning(&mut self, hz: &[f32]) {
        if hz.len() < 16 {
            return;
        }
        let mut sorted: Vec<f32> = hz.to_vec();
        sorted.sort_by(f32::total_cmp);
        let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
        self.tune_lo.push(at(0.04));
        self.tune_hi.push(at(0.96));
        if self.tune_lo.len() > TUNE_WINDOW_LINES {
            self.tune_lo.remove(0);
            self.tune_hi.remove(0);
        }
        if self.tune_lo.len() < TUNE_WINDOW_LINES / 2 {
            return;
        }
        // The window's dark end is the lowest tenth of the lines' darkest, its
        // bright end the highest tenth of the brightest: a line of paper or of
        // ink does not move either.
        let end = |v: &[f32], q: f64| {
            let mut v = v.to_vec();
            v.sort_by(f32::total_cmp);
            f64::from(v[((v.len() - 1) as f64 * q) as usize])
        };
        let (lo, hi) = (end(&self.tune_lo, 0.1), end(&self.tune_hi, 0.9));
        if (TUNE_SPAN_HZ.0..=TUNE_SPAN_HZ.1).contains(&(hi - lo)) {
            self.tune_hz = 0.5 * (lo + hi);
        }
    }

    pub fn receiving(&self) -> bool {
        self.phase != Phase::Idle && !self.start_pending
    }

    /// True while still hunting for the phasing pulse.
    pub fn phasing(&self) -> bool {
        self.phase == Phase::Phasing && !self.start_pending
    }

    /// Lines emitted for the picture in progress.
    pub fn lines(&self) -> u16 {
        self.line_no
    }

    /// Begin a picture now, without waiting for a start tone.
    ///
    /// The usual way to catch a chart that was already running when you tuned
    /// to it — which, on a fifteen-minute transmission, is most of the time.
    pub fn start_manual(&mut self, out: &mut Vec<WefaxEvent>) {
        self.finish(false, out);
        self.begin(self.ioc, false, out);
    }

    /// End the picture in progress, keeping what has arrived so far.
    pub fn stop_manual(&mut self, out: &mut Vec<WefaxEvent>) {
        self.finish(false, out);
    }

    /// Nudge the line alignment by `pixels`, for a chart whose phasing pulse
    /// was missed or misread. Positive moves the image right.
    pub fn nudge_phase(&mut self, pixels: i32) {
        let spp = self.samples_per_line() / self.ioc.width() as f64;
        self.line_start -= pixels as f64 * spp;
    }

    /// Samples in one scan line, including the slant trim.
    fn samples_per_line(&self) -> f64 {
        self.rate * self.lpm.line_secs() * (1.0 + self.slant_ppm * 1e-6) * (1.0 + self.clock_ppm * 1e-6)
    }

    fn begin(&mut self, ioc: Ioc, auto: bool, out: &mut Vec<WefaxEvent>) {
        self.arm(ioc, auto);
        out.push(WefaxEvent::Start { ioc, lpm: self.lpm, auto });
    }

    /// Arm for an automatic start without announcing it: [`WefaxEvent::Start`]
    /// is sent when the phasing signal locks, and nothing at all is sent if it
    /// never does.
    fn begin_pending(&mut self, ioc: Ioc) {
        self.arm(ioc, true);
        self.start_pending = true;
    }

    fn arm(&mut self, ioc: Ioc, auto: bool) {
        self.start_pending = false;
        self.ioc = ioc;
        self.phase = Phase::Phasing;
        self.line_buf.clear();
        self.px_buf.clear();
        self.line_start = self.sample_idx as f64;
        self.line_no = 0;
        self.phase_samples = 0;
        self.slant_fit = SlantFit::default();
        self.phasing_lines = 0;
        self.slant_ppm = self.slant_base;
        self.slant_pending = None;
        self.slant_locked = false;
        self.slant_contra = 0;
        self.last_slant = (0.0, 0);
        self.lean_rows.clear();
        self.lean_hist.clear();
        self.lean_off = false;
        self.lean_job = None;
        self.last_line.clear();
        self.lost_run = 0;
        self.auto_begun = auto;
        self.phasing_stack.reset();
        self.lock_lines = LOCK_LINES;
        self.hz_buf.clear();
        self.reset_tuning();
        self.lead_phasing = None;
        self.lead_skipped = 0;
        self.lead_keep = 0;
        self.reset_idle();
        self.rephase_lines = 0;
        self.had_content = false;
        self.blank_run = 0;
        self.carrier_gone_s = 0.0;
        self.carrier_ref = 0.0;
        self.carrier_ratio = 1.0;
        self.blank_gap = 0;
        self.start_det.reset();
        self.start_run = 0;
        self.stop_det.reset();
        self.stop_run = 0;
        self.stop_miss = 0;
        self.reset_audio_tones();
    }

    fn reset_audio_tones(&mut self) {
        self.audio_tone.reset();
        self.audio_start_run = 0;
        self.audio_stop_run = 0;
        self.audio_stop_miss = 0;
        self.band_raw = 0.0;
        self.band_sub = 0.0;
    }

    fn finish(&mut self, by_tone: bool, out: &mut Vec<WefaxEvent>) {
        if self.phase == Phase::Idle {
            return;
        }
        let lines = self.line_no;
        let announced = !self.start_pending;
        self.start_pending = false;
        self.phase = Phase::Idle;
        self.line_buf.clear();
        self.px_buf.clear();
        self.rephase_lines = 0;
        self.start_det.reset();
        self.stop_det.reset();
        self.start_run = 0;
        self.stop_run = 0;
        self.stop_miss = 0;
        self.reset_audio_tones();
        self.reset_idle();
        self.hz_buf.clear();
        self.reset_tuning();
        self.black_hz = BLACK_HZ;
        self.white_hz = WHITE_HZ;
        self.lead_phasing = None;
        self.idle_hold_until = self.sample_idx + (IDLE_HOLD_S * self.rate) as u64;
        if announced {
            out.push(WefaxEvent::Complete { lines, by_tone });
        }
    }

    /// Forget the tuning centre: a new page is read from scratch.
    fn reset_tuning(&mut self) {
        self.tune_hz = CENTRE_HZ;
        self.tune_lo.clear();
        self.tune_hi.clear();
    }

    /// Start the idle line, and the run of phasing lines counted on it, over.
    fn reset_idle(&mut self) {
        self.idle_levels.clear();
        self.idle_hz.clear();
        self.idle_end = 0.0;
        self.phasing_stack.reset();
    }

    /// Brightness of a subcarrier frequency against where black and white
    /// actually are.
    fn level_of(&self, hz: f64) -> f32 {
        brightness_between(hz, self.black_hz, self.white_hz)
    }

    /// Re-measure black and white from a line that is known to be phasing.
    ///
    /// A receiver a little off tune, or Doppler on the path, moves the whole
    /// subcarrier: black is not at 1500 Hz and white not at 2300, and a chart
    /// read against the nominal pair comes out too dark or washed out. The
    /// phasing signal is black with a white pulse, so it shows where both are.
    /// Anything outside the (wide) ranges is left alone: it was not phasing.
    fn calibrate(&mut self, line: &[f32], hz: &[f32]) {
        if line.len() != hz.len() || line.is_empty() {
            return;
        }
        let (mut black, mut blacks, mut white, mut whites) = (0.0f64, 0u32, 0.0f64, 0u32);
        for (&l, &h) in line.iter().zip(hz) {
            if l < 0.3 {
                black += f64::from(h);
                blacks += 1;
            } else if l > 0.7 {
                white += f64::from(h);
                whites += 1;
            }
        }
        // Enough of each to be a measurement, not a sample.
        if blacks * 4 < line.len() as u32 || whites * 200 < line.len() as u32 {
            return;
        }
        let (black, white) = (black / f64::from(blacks), white / f64::from(whites));
        let span = white - black;
        // The windows move with the sensitivity setting; at the reference they
        // are the constants themselves.
        let k = self.sens_k();
        let ok = |v: f64, r: (f64, f64)| {
            let (lo, hi) = cal_range(r, k);
            v >= lo && v <= hi
        };
        if ok(black, BLACK_RANGE_HZ) && ok(white, WHITE_RANGE_HZ) && ok(span, SPAN_RANGE_HZ) {
            self.black_hz = black;
            self.white_hz = white;
            self.tune_hz = 0.5 * (black + white);
        }
    }

    /// Feed audio; push any decoded events.
    pub fn process(&mut self, audio: &[f32], out: &mut Vec<WefaxEvent>) {
        let mut mixed = Vec::with_capacity(audio.len());
        for &a in audio {
            self.in_level += 0.001 * (a.abs() - self.in_level);
            let z = Complex32::new(a * self.mix_ph.cos(), -a * self.mix_ph.sin());
            self.mix_ph += self.mix_inc;
            if self.mix_ph > std::f32::consts::TAU {
                self.mix_ph -= std::f32::consts::TAU;
            }
            mixed.push(z);
        }
        let mut bb = Vec::with_capacity(audio.len());
        self.lpf.process(&mixed, &mut bb);
        let mut pic = Vec::with_capacity(audio.len());
        self.pic_lpf.process(&mixed, &mut pic);

        for (i, z) in bb.into_iter().enumerate() {
            let a = audio.get(i).copied().unwrap_or(0.0);
            let step = self.pic_step(pic.get(i).copied().unwrap_or_default());
            self.band_raw += a * a;
            self.band_sub += z.norm_sqr();
            if let Some(tone) = self.audio_tone.push(a) {
                self.on_audio_tone(tone, out);
            }
            let raw_hz = if self.have_prev {
                let d = z * self.prev.conj();
                CENTRE_HZ + (d.arg() as f64) * self.rate / TAU
            } else {
                CENTRE_HZ
            };
            self.prev = z;
            self.have_prev = true;
            // Light smoothing: enough to take the edge off the discriminator's
            // noise without rounding the black/white transitions the phasing
            // pulse is found by.
            self.inst_hz += 0.5 * (raw_hz - self.inst_hz);

            let level = self.level_of(self.inst_hz);
            self.sample_idx += 1;

            match self.phase {
                Phase::Idle => self.step_idle(level, out),
                Phase::Phasing | Phase::Image => self.step_receiving(level, step, out),
            }
        }
    }

    /// Switch the S-shaped contrast curve, for before/after measurements.
    #[cfg(test)]
    fn set_contrast(&mut self, on: bool) {
        self.contrast = on;
    }

    /// Switch the filling of empty pixels, for measurements.
    #[cfg(test)]
    fn set_fill(&mut self, on: bool) {
        self.fill = on;
    }

    /// Re-cut the picture band-pass, for measurements.
    #[cfg(test)]
    fn set_pic_band(&mut self, half_hz: f64) {
        self.pic_lpf = ComplexFir::new(bandpass_taps(129, -half_hz, half_hz, self.rate));
    }

    /// The picture discriminator's phase step for one sample, capped.
    fn pic_step(&mut self, z: Complex32) -> Complex32 {
        let d = if self.pic_have_prev { z * self.pic_prev.conj() } else { Complex32::default() };
        self.pic_prev = z;
        self.pic_have_prev = true;
        let m = d.norm();
        let cap = PIC_CRASH * self.pic_mag;
        if self.pic_mag > 0.0 && m > cap {
            // Slowly, so a signal that really has grown is not blanked for good.
            self.pic_mag += 0.0002 * (m - self.pic_mag);
            return Complex32::default();
        }
        self.pic_mag += 0.002 * (m - self.pic_mag);
        d
    }

    /// Draw the line that began at `from` (in samples) from the phase steps in
    /// hand, then drop the steps before the next one.
    fn picture(&mut self, from: f64) -> Vec<u8> {
        let origin = from - self.px_start - PIC_DELAY_SAMPLES;
        let sums =
            picture_line(&self.px_buf, origin, self.samples_per_line(), self.ioc.width() as usize);
        let (gray, share) = self.shade(&sums);
        self.conceal(gray, share)
    }

    /// Stand-in for a lost line. A line with almost nothing of its own in it was
    /// lost by the receiver (an AGC pump, a fade): drawn as it came it is a
    /// black stripe, so the line above stands in, for a few lines and no more,
    /// so a long fade shows as one instead of as a smear.
    fn conceal(&mut self, mut gray: Vec<u8>, share: f32) -> Vec<u8> {
        if self.fill
            && share < PIC_LOST_SHARE
            && self.lost_run < PIC_LOST_RUN
            && self.last_line.len() == gray.len()
        {
            self.lost_run += 1;
            gray.clone_from(&self.last_line);
        } else {
            if share >= PIC_LOST_SHARE {
                self.lost_run = 0;
            }
            self.last_line.clone_from(&gray);
        }
        gray
    }

    /// Brightness of each pixel's summed phase step, 0..255.
    ///
    /// A pixel the crash blanker emptied (less than a third of the signal it
    /// should hold) or whose steps point every way (no coherence, which is what
    /// noise on a weak pixel looks like) has nothing to say: it takes the
    /// middle one of the nearest pixel that does on each side, up to
    /// [`PIC_FILL_PX`] away, and the pixel the line above has there. It is left
    /// as it is when none is that close and there is no line above.
    fn shade(&self, sums: &[(Complex32, f32)]) -> (Vec<u8>, f32) {
        let expect = self.samples_per_line() as f32 / sums.len().max(1) as f32 * self.pic_mag;
        let valid = |&(s, m): &(Complex32, f32)| {
            !self.fill || (m >= PIC_MIN_WEIGHT * expect && s.norm() >= PIC_COHERENCE * m)
        };
        let level = |&(s, _): &(Complex32, f32)| -> u8 {
            if s.norm_sqr() <= 0.0 {
                return 0;
            }
            let hz = CENTRE_HZ + f64::from(s.arg()) * self.rate / TAU;
            let mut l = self.level_of(hz);
            if self.contrast {
                l = 0.5 * l + 0.5 * l * l * (3.0 - 2.0 * l);
            }
            (l * 255.0).round().clamp(0.0, 255.0) as u8
        };
        let ok: Vec<bool> = sums.iter().map(valid).collect();
        // Nearest valid pixel on each side, by one pass each way.
        let n = sums.len();
        let mut left = vec![usize::MAX; n];
        let mut last = usize::MAX;
        for i in 0..n {
            if ok[i] {
                last = i;
            }
            left[i] = last;
        }
        let mut right = vec![usize::MAX; n];
        last = usize::MAX;
        for i in (0..n).rev() {
            if ok[i] {
                last = i;
            }
            right[i] = last;
        }
        let share = ok.iter().filter(|&&v| v).count() as f32 / n.max(1) as f32;
        let gray = (0..n)
            .map(|i| {
                if ok[i] {
                    return level(&sums[i]);
                }
                let dl = if left[i] == usize::MAX { usize::MAX } else { i - left[i] };
                let dr = if right[i] == usize::MAX { usize::MAX } else { right[i] - i };
                // What the pixel's neighbours say: the nearest good pixel on
                // each side, and what the line above had in the same place.
                // Copying the nearest side alone drew the black edge of a
                // frame out into the white beside it, a dash of up to
                // [`PIC_FILL_PX`] pixels; the line above breaks the tie.
                let side = |d: usize, j: usize| (d <= PIC_FILL_PX).then(|| level(&sums[j]));
                let l = side(dl, left[i]);
                let r = side(dr, right[i]);
                let up = (self.last_line.len() == n).then(|| self.last_line[i]);
                // A gap wider than a few pixels is a burst of noise, not an
                // edge: the two sides are too far apart to say what is between
                // them, and drawn from them it is a horizontal streak out of
                // every stroke. A picture is far more alike from one line to
                // the next than from one end of a gap to the other.
                if let Some(u) = up.filter(|_| dl.saturating_add(dr) > PIC_FILL_NEAR) {
                    return u;
                }
                match (l, r, up) {
                    (Some(l), Some(r), Some(u)) => l.max(r.min(u)).min(r.max(l.min(u))),
                    (Some(l), Some(r), None) => if dl <= dr { l } else { r },
                    (Some(a), None, Some(u)) | (None, Some(a), Some(u)) => {
                        // One side only: it counts when the line above agrees
                        // with it, and the line above when it does not.
                        if a.abs_diff(u) < 96 { a } else { u }
                    }
                    (Some(a), None, None) | (None, Some(a), None) => a,
                    (None, None, Some(u)) => u,
                    (None, None, None) => level(&sums[i]),
                }
            })
            .collect();
        (gray, share)
    }

    /// Forget the phase steps before sample `from`.
    fn trim_steps(&mut self, from: f64) {
        let cut = ((from - self.px_start).floor().max(0.0) as usize).min(self.px_buf.len());
        self.px_buf.drain(..cut);
        self.px_start += cut as f64;
    }

    /// A window of plain audio has been classified.
    ///
    /// Idle, a start tone held for [`TONE_RUN`] windows arms an automatic start
    /// — announced only once the phasing signal confirms it. Receiving, a stop
    /// tone held for [`STOP_RUN`] windows ends the page, exactly as the
    /// alternating stop signal does. A start tone mid-picture is ignored, for
    /// the reason given at [`WefaxRx::start_tone`].
    fn on_audio_tone(&mut self, tone: AudioTone, out: &mut Vec<WefaxEvent>) {
        if self.phase == Phase::Idle {
            if !self.auto_start {
                return;
            }
            if tone == AudioTone::Start {
                self.audio_start_run += 1;
                if self.audio_start_run >= TONE_RUN {
                    self.begin_pending(Ioc::I576);
                }
            } else {
                self.audio_start_run = 0;
            }
        } else if self.auto_stop {
            if tone == AudioTone::Stop {
                self.audio_stop_run += 1;
                self.audio_stop_miss = 0;
                if self.audio_stop_run >= STOP_RUN {
                    self.line_no = self.line_no.saturating_sub(2);
                    self.finish(true, out);
                }
            } else if self.audio_stop_run > 0 {
                self.audio_stop_miss += 1;
                if self.audio_stop_miss > STOP_MISS_TOL {
                    self.audio_stop_run = 0;
                    self.audio_stop_miss = 0;
                }
            }
        }
    }

    /// Listening for a start tone.
    fn step_idle(&mut self, level: f32, out: &mut Vec<WefaxEvent>) {
        if !self.auto_start {
            return;
        }
        if let Some(ioc) = self.start_tone(level, TONE_RUN) {
            self.begin_pending(ioc);
            return;
        }
        self.idle_phasing(level, out);
    }

    /// A phasing pulse found by a lock, moved into the coordinates of the lines
    /// cut from the lock on, which begin right after it: the pulse ends such a
    /// line, as far back from the end as the picture's start (the middle of the
    /// pulse, see [`PulseRegion::picture_start`]) sits beyond the pulse's own
    /// start.
    ///
    /// Taken against a whole line, not against the one that locked: the first
    /// line of a start-tone page carries the samples that came before the
    /// signal and is longer.
    fn aligned_pulse(&self, pulse: PulseRegion) -> PulseRegion {
        let n = (self.samples_per_line().round() as usize).max(1);
        let back = (pulse.width / 2 + pulse.width % 2) % n;
        PulseRegion { start: (n - back) % n, width: pulse.width }
    }

    /// Listening for the phasing signal itself, with no start tone needed.
    ///
    /// The line going by is cut at the line rate on a clock of its own and
    /// tested for the phasing shape, taken round the circle so the starting
    /// point does not matter. [`IDLE_PHASING_S`] of lines with the pulse in one
    /// place begins a page, locked on that pulse. A carrier standing on the
    /// channel, silence, a voice and a chart all fail the test line after line.
    fn idle_phasing(&mut self, level: f32, out: &mut Vec<WefaxEvent>) {
        if self.sample_idx < self.idle_hold_until {
            return;
        }
        let spl = self.samples_per_line();
        if self.idle_end == 0.0 {
            self.idle_end = self.sample_idx as f64 + spl;
        }
        self.idle_levels.push(level);
        self.idle_hz.push(self.inst_hz as f32);
        if (self.sample_idx as f64) < self.idle_end {
            return;
        }
        self.idle_end += spl;
        let line = std::mem::take(&mut self.idle_levels);
        let hz = std::mem::take(&mut self.idle_hz);
        let line_s = spl / self.rate;
        let need = ((IDLE_PHASING_S / line_s).ceil() as usize).max(LOCK_LINES as usize);
        let Some((skew, pulse)) = self.phasing_stack.push(&line, need) else {
            return;
        };
        self.calibrate(&line, &hz);
        self.arm(self.ioc, true);
        // `arm` starts the page over; the phasing just measured is its first
        // reading of the tuning.
        self.tune_hz = 0.5 * (self.black_hz + self.white_hz);
        self.phase = Phase::Image;
        self.line_start = self.sample_idx as f64 + f64::from(skew);
        self.lead_phasing = Some(self.aligned_pulse(pulse));
        out.push(WefaxEvent::Start { ioc: self.ioc, lpm: self.lpm, auto: true });
        out.push(WefaxEvent::Phased { skew });
    }

    /// Feed the start-signal detector one sample; `Some(ioc)` once a whole
    /// start signal has been heard. `required` matching windows are demanded.
    ///
    /// Run only while idle. Mid-picture a start signal is not acted on: a
    /// chart's own periodic detail (dotted graticule lines, hatching) alternates
    /// at the same rate and cut healthy pictures into pieces. The next
    /// transmission is found by its phasing signal instead, see
    /// [`WefaxRx::note_phasing_line`].
    fn start_tone(&mut self, level: f32, required: u32) -> Option<Ioc> {
        let window = (self.rate * TONE_WINDOW_S) as u32;
        let rate = self.start_det.push(level, window)?;
        let in_band = self.band_sub >= self.band_raw * self.subcarrier_share();
        self.band_raw = 0.0;
        self.band_sub = 0.0;
        // Crossings per window → crossings per second → the alternation rate,
        // which is half the crossing rate.
        let alt_hz = rate / TONE_WINDOW_S / 2.0;
        let (tone_tol, periodic_share) = (self.tone_tol(), self.periodic_share());
        let candidate = Ioc::ALL.into_iter().find(|i| {
            let want = i.start_tone_hz();
            in_band && self.start_det.swing && (alt_hz - want).abs() <= want * tone_tol
        });
        let hit = candidate.filter(|i| {
            self.level_spec.share(self.start_det.window(), i.start_tone_hz()) >= periodic_share
        });
        match hit {
            Some(ioc) if ioc == self.start_ioc => {
                self.start_run += 1;
                if self.start_run >= required {
                    self.start_run = 0;
                    return Some(ioc);
                }
            }
            Some(ioc) => {
                self.start_ioc = ioc;
                self.start_run = 1;
            }
            None => self.start_run = 0,
        }
        None
    }

    /// Whether `line` continues a run of phasing lines long enough to mean the
    /// next transmission has started on top of the picture being built.
    ///
    /// The pulses have to line up: [`REPHASE_LINES`] in a row with the patch in
    /// the same place, to within the line clock's own drift.
    ///
    /// The run is not on its own what tells a phasing signal from a picture —
    /// a graphic fixed in the chart repeats for as long as it is on the page,
    /// however long the run is asked to be (issue #496). What carries it is the
    /// *shape* test in [`phasing_skew_strict`]; the run and the consistency
    /// narrow the window a passing shape has to survive.
    ///
    /// **And the run has to be past the page's own phasing signal.**
    ///
    /// The lock is taken on the transmission's *first* phasing pulse and the
    /// standard signal runs for thirty seconds after it, so the opening of every
    /// page is black lines with a white pulse — exactly the shape this looks
    /// for. Ungated, the rule cut the page it had just started, over and over:
    /// measured on an off-air recording (`Sigidwiki`'s *WFax_IntroNORTHWOOD*, a
    /// start tone followed by its phasing signal), one transmission came out as
    /// **four** pages of eight to eleven lines, cut every seven seconds, every
    /// cut inside the phasing — a chart that could never come out whole.
    ///
    /// The gate is **time, not content**, and that was measured rather than
    /// assumed. The obvious test — wait until a line the strict test *rejects*
    /// has been drawn — does not work: the strict test turns away some of the
    /// phasing signal's own lines (the partial line the lock leaves at the head
    /// of the page, and others a few seconds in), so the gate opens while the
    /// signal is still running. [`PHASING_SIGNAL_S`] is the standard's own
    /// number, and it costs nothing real: the next chart's phasing is minutes
    /// away, never seconds.
    /// Measure the line clock's drift over the phasing signal and correct the
    /// slant from the pulse's movement across it.
    ///
    /// The pulse sits at the same place in every phasing line; if the line clock
    /// is a little long or short it walks a few samples per line, and that walk
    /// *is* the slant. Fitted over every phasing line seen rather than measured
    /// between two of them, so a weak line's edge-wander averages out instead of
    /// turning into a wobble — and so a slant too small for a twenty-line
    /// measurement to name is still corrected once the whole signal has been
    /// read (see [`SLANT_FINE_LINES`]).
    fn probe_slant(&mut self, line: &[f32]) {
        if self.slant_locked || !self.auto_slant {
            return;
        }
        let Some(pos) = phasing_skew_strict(line) else { return };
        self.phasing_lines += 1;
        let n = line.len() as f64;
        let max_step = n * SLANT_MAX_PPM * 1e-6 + 2.0;
        let Some(pos) = self.slant_fit.unwrap(f64::from(pos), n, max_step) else { return };
        self.slant_fit.add(f64::from(self.phasing_lines), pos);
        let Some(slope) = self.slant_fit.slope(SLANT_PROBE_LINES) else { return };
        // A walk the pulse's own scatter could have made is noise, not drift:
        // on a weak signal the fit used to name a slant out of it and lean a
        // page that was straight.
        if slope.abs() < SLANT_SIGNIFICANCE * self.slant_fit.slope_error() {
            return;
        }
        // Pulse movement in samples per line, as a fraction of the nominal
        // line: a clock that is too long walks the pulse backwards, and that is
        // exactly the amount the trim must come down by.
        let nominal = self.rate * self.lpm.line_secs();
        let ppm = slope / nominal * 1e6;
        // Small corrections need more evidence than large ones: over twenty
        // lines the fit's own noise is several parts per million, over the
        // whole signal it is a couple. Acting on the noise is worse than
        // leaving the picture as it is.
        let floor = if self.slant_fit.n >= f64::from(SLANT_FINE_LINES) {
            SLANT_MIN_PPM
        } else {
            SLANT_COARSE_MIN_PPM
        };
        // The window since the last correction has now been measured against
        // the clock that correction set. A good correction leaves no walk; one
        // that leaves a walk the other way, at least half as large, was made
        // from a mis-measure and has made the page worse: undo it and leave the
        // clock alone for the rest of the page.
        if let Some((prev, applied, prev_last)) = self.slant_pending {
            if ppm * applied < 0.0 && ppm.abs() >= 0.5 * applied.abs() && ppm.abs() >= floor {
                // **One window against a correction is not enough to undo it.**
                // A gain that moves during the phasing signal bends a single
                // twelve-line window the other way (bench, 2026-11-12: +32 ppm,
                // right, then -54 ppm, and the right one was thrown away).
                // The window is left out and the next one decides.
                self.slant_contra += 1;
                self.slant_fit = SlantFit::default();
                self.phasing_lines = 0;
                if self.slant_contra >= 2 {
                    self.slant_pending = None;
                    self.slant_contra = 0;
                    self.slant_ppm = prev;
                    self.last_slant = prev_last;
                    self.slant_locked = true;
                }
                return;
            }
            self.slant_pending = None;
            self.slant_contra = 0;
        }
        if (floor..=SLANT_MAX_PPM).contains(&ppm.abs()) {
            self.slant_pending = Some((self.slant_ppm, ppm, self.last_slant));
            // Clamped as well as filtered: a handful of small mis-measures in a
            // row must not walk the clock away from the signal either.
            self.last_slant = (ppm, self.slant_fit.n as u32);
            self.slant_ppm = (self.slant_ppm + ppm).clamp(-SLANT_MAX_PPM, SLANT_MAX_PPM);
            // The clock has just moved, so every walk measured so far was
            // measured against the old one: the next window starts here.
            self.slant_fit = SlantFit::default();
            self.phasing_lines = 0;
        } else if ppm.abs() > SLANT_MAX_PPM {
            // Not drift: re-take the baseline rather than keep measuring the
            // same mistake against a stale one.
            self.slant_fit = SlantFit::default();
            self.phasing_lines = 0;
        }
    }

    /// Correct the slant from the picture itself, a stretch of lines at a time.
    ///
    /// The phasing signal is a second opinion about the line clock and not
    /// always a good one: a receiver whose gain moves during the thirty seconds
    /// of it (an AGC, an operator on the gain control) moves the pulse for a
    /// reason that is not the clock, and the page that follows is leaned the
    /// wrong way by the very correction meant to straighten it. The picture
    /// says what is left: a frame's edge or a meridian that walks sideways from
    /// one line to the next. Each stretch of [`LEAN_BLOCK`] lines or more is
    /// measured by [`image_lean`], and what is left of the walk is taken off the
    /// trim once two stretches agree.
    fn image_slant(&mut self, gray: &[u8]) {
        if self.lean_off || !self.auto_slant {
            return;
        }
        // A measurement that ran on its own thread, taken in when it is done.
        if let Some(job) = &self.lean_job {
            match job.try_recv() {
                Ok(measured) => {
                    self.lean_job = None;
                    self.take_lean(measured);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.lean_job = None,
            }
        }
        self.lean_rows.push(gray.to_vec());
        // A stretch is measured from LEAN_BLOCK lines on, every LEAN_STEP_LINES
        // more, and grows while the picture has nothing vertical to go by (a
        // title, a white margin) up to LEAN_MAX_LINES.
        let n = self.lean_rows.len();
        if n < LEAN_BLOCK || (n - LEAN_BLOCK) % LEAN_STEP_LINES != 0 {
            return;
        }
        if self.lean_job.is_some() {
            // The last one has not finished: the next step will do.
            return;
        }
        if self.lean_background {
            let rows = self.lean_rows.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new().name("wefax-lean".into()).spawn(move || {
                let _ = tx.send(image_lean(&rows));
            });
            if spawned.is_ok() {
                self.lean_job = Some(rx);
                return;
            }
        }
        let measured = image_lean(&self.lean_rows);
        self.take_lean(measured);
    }

    /// What one stretch's measured walk comes to: see [`WefaxRx::image_slant`].
    fn take_lean(&mut self, measured: Option<(f64, f64)>) {
        let n = self.lean_rows.len();
        let sharp = measured.is_some_and(|(_, sharpness)| sharpness >= LEAN_CONFIDENCE);
        if !sharp {
            if n >= LEAN_MAX_LINES {
                self.lean_rows.clear();
            }
            return;
        }
        let Some((ppm, _)) = measured else { return };
        self.lean_rows.clear();
        // One stretch is a chart's texture as much as its clock. A walk that is
        // the clock is there in stretch after stretch, on the same side.
        self.lean_hist.push(ppm);
        if self.lean_hist.len() < LEAN_WINDOWS {
            return;
        }
        let n = self.lean_hist.len() as f64;
        let mean = self.lean_hist.iter().sum::<f64>() / n;
        let spread = (self.lean_hist.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        self.lean_hist.remove(0);
        if mean.abs() < LEAN_MIN_PPM || spread > LEAN_STEADY * mean.abs() {
            return;
        }
        // The clock moves, so what was measured against the old one is stale.
        self.lean_hist.clear();
        self.slant_ppm = (self.slant_ppm + LEAN_GAIN * mean).clamp(-SLANT_MAX_PPM, SLANT_MAX_PPM);
        // The pulse-based check compares windows of phasing lines against the
        // clock set before; this correction is not the one it is watching.
        self.slant_pending = None;
    }

    fn note_phasing_line(&mut self, line: &[f32]) -> bool {
        // The strict test: mid-picture, a line that is merely "mostly black
        // with a white patch" is a chart's own graphic, not a phasing pulse.
        let Some(skew) = phasing_skew_strict(line) else {
            // **One missed pulse does not undo the ones that agreed.** On a
            // weak chart roughly a quarter of the phasing lines fail the pulse
            // test — the pulse is there, the noise just won that line — and a
            // hard reset meant the run restarted before it ever reached
            // [`REPHASE_LINES`]. Measured on a real chart (2026-11-01): thirty
            // of forty phasing lines read, in runs of three or four, and the
            // page never cut. Costing one instead of all makes the run survive
            // the dropouts while a chart's stray bright patch still cannot
            // build eight.
            self.rephase_lines = self.rephase_lines.saturating_sub(1);
            return false;
        };
        let (skew, n) = (skew as f64, line.len() as f64);
        // Circular: a pulse sitting on the line boundary reads as ≈0 on one
        // line and ≈n on the next, and that is the same place.
        let d = (skew - self.rephase_skew).abs();
        let moved = d.min(n - d) > n * self.rephase_tol();
        self.rephase_lines =
            if self.rephase_lines > 0 && moved { 1 } else { self.rephase_lines + 1 };
        self.rephase_skew = skew;
        self.rephase_lines >= REPHASE_LINES
    }

    /// Track a white carrier after the chart. Returns true once an automatic
    /// page has had nothing but white for [`BLANK_END_S`]. Only white *after*
    /// content counts, so a chart's own white margin never ends it, and a fade
    /// shorter than [`BLANK_GAP_LINES`] does not restart the count.
    fn note_blank_line(&mut self, line: &[f32]) -> bool {
        let n = line.len().max(1) as f32;
        let mean = line.iter().sum::<f32>() / n;
        // A carrier left on after the chart is blank wherever it sits: above
        // white it reads as white, but one that has drifted (or a station on a
        // slightly different frequency) is steady at some other brightness.
        let std = (line.iter().map(|l| (l - mean) * (l - mean)).sum::<f32>() / n).sqrt();
        // **White paper is not blank if it has ink on it.** A chart's footer is
        // very nearly white — mean well over BLANK_MEAN — with only the ruled
        // border of a scale box drawn across it, and the mean alone read that
        // as the end of the page: the chart was closed a minute early, mid-box
        // (bench, 2026-11-03, GYA 4608 kHz). A white carrier has no ink in it
        // at all, which is exactly the difference.
        let ink = line.iter().filter(|l| **l < INK_LEVEL).count();
        let blank = ink < INK_SAMPLES
            && (mean >= BLANK_MEAN || (std <= FLAT_STD && mean >= FLAT_MEAN));
        if blank {
            self.blank_gap = 0;
            self.blank_run += 1;
        } else {
            self.had_content = true;
            self.blank_gap += 1;
            if self.blank_gap > BLANK_GAP_LINES {
                self.blank_run = 0;
            }
        }
        let blank_s = f64::from(self.blank_run) * self.samples_per_line() / self.rate;
        self.had_content && blank_s >= BLANK_END_S
    }

    /// Receiving: accumulate the line, watch for the stop tone — and for the
    /// next transmission arriving on top of this one.
    fn step_receiving(&mut self, level: f32, step: Complex32, out: &mut Vec<WefaxEvent>) {
        if self.px_buf.is_empty() {
            // The sample just counted occupies [sample_idx - 1, sample_idx).
            self.px_start = self.sample_idx as f64 - 1.0;
        }
        self.px_buf.push(step);
        self.line_buf.push(level);
        self.hz_buf.push(self.inst_hz as f32);

        if self.auto_stop {
            let window = (self.rate * TONE_WINDOW_S) as u32;
            if let Some(rate) = self.stop_det.push(level, window) {
                let alt_hz = rate / TONE_WINDOW_S / 2.0;
                // Same two conditions as a start signal: it has to be the
                // subcarrier swinging, not a plain audio tone or a steady
                // carrier that the discriminator reads as noise.
                // Silence is not a carrier either: with no energy in the
                // window the share test would say "in band" about nothing.
                let in_band =
                    self.band_raw > 0.0 && self.band_sub >= self.band_raw * self.subcarrier_share();
                // Taken before the band counters are cleared for the next
                // window: this is the power the carrier rule is measured on.
                let power = self.band_sub / (self.rate * TONE_WINDOW_S) as f32;
                self.band_raw = 0.0;
                self.band_sub = 0.0;
                // **The carrier went away.** A white line is not this: a
                // chart's footer is white with the carrier still there, which
                // is why that rule needs no ink (#426) — but noise has ink, so
                // a page that runs into the gap between transmissions passes
                // the blank test. The subcarrier's own share of the band is the
                // honest test, and a minute and a half of its absence is a gap
                // between transmissions, not a fade.
                // **The carrier went away**, and the measure is the
                // subcarrier band's **power** against what it has been for the
                // last twenty seconds — not its share of the audio. `in_band`
                // above is the share, and the bench showed a real gap reading
                // "in band" in every one of 1687 windows: a receiver's audio is
                // already band-limited, so noise fills the share. A station
                // that goes off drops tens of dB in that band, and that shows.
                if self.carrier_ref <= 0.0 {
                    self.carrier_ref = power;
                }
                self.carrier_ratio =
                    if self.carrier_ref > 0.0 { power / self.carrier_ref } else { 1.0 };
                if self.carrier_ratio < CARRIER_DROP {
                    self.carrier_gone_s += TONE_WINDOW_S;
                    if self.carrier_gone_s >= CARRIER_GONE_S {
                        self.finish(false, out);
                        return;
                    }
                } else {
                    self.carrier_gone_s = 0.0;
                    // Follow the level — but only from a window that is not far
                    // below it, or a gap would drag the reference down to the
                    // noise it is supposed to be measured against.
                    let k = (TONE_WINDOW_S as f32 / CARRIER_REF_S).clamp(0.0, 1.0);
                    self.carrier_ref += k * (power - self.carrier_ref);
                }
                let stop_share = self.level_spec.share(self.stop_det.window(), STOP_TONE_HZ);
                // The swing test keeps a carrier wobbling about mid-grey from
                // passing for the tone. A tone this clean at 450 Hz is not that,
                // and a weak or filtered one (a signal 2026-11-12 whose stop tone
                // swung the full range in three windows out of eight) is still
                // the tone.
                if in_band
                    && (self.stop_det.swing || stop_share >= STOP_SHARE_SURE)
                    && (alt_hz - STOP_TONE_HZ).abs() <= STOP_TONE_HZ * self.tone_tol()
                    && stop_share >= self.periodic_share()
                {
                    self.stop_run += 1;
                    self.stop_miss = 0;
                    if self.stop_run >= STOP_RUN {
                        // The stop tone is already in the last line or two;
                        // they are dropped rather than left as a barcode at the
                        // bottom of the chart.
                        self.line_no = self.line_no.saturating_sub(2);
                        self.finish(true, out);
                        return;
                    }
                } else if self.stop_run > 0 {
                    self.stop_miss += 1;
                    if self.stop_miss > STOP_MISS_TOL {
                        self.stop_run = 0;
                        self.stop_miss = 0;
                    }
                }
            }
        }

        let spl = self.samples_per_line();
        let end = self.line_start + spl;
        if (self.sample_idx as f64) < end {
            return;
        }

        // One line's worth of brightness is in hand.
        let line: Vec<f32> = std::mem::take(&mut self.line_buf);
        let line_hz: Vec<f32> = std::mem::take(&mut self.hz_buf);
        self.note_tuning(&line_hz);
        let from = self.line_start;
        self.line_start = end;
        // Earlier lines are drawn and gone: keep this one and what follows.
        self.trim_steps(from);

        match self.phase {
            Phase::Phasing => {
                self.phase_samples += line.len() as u64;
                self.probe_slant(&line);
                if let Some((mean, pulse)) = self.phasing_stack.push(&line, self.lock_lines as usize) {
                    // The pulse sits at the *end* of a phasing line, so the
                    // picture starts just after it.
                    self.calibrate(&line, &line_hz);
                    self.line_start += f64::from(mean);
                    // The line clock has just been re-aligned to the pulse, so
                    // the pulse position's baseline moved: the auto-slant's fit
                    // has to restart from here.
                    self.slant_fit = SlantFit::default();
                    self.phasing_lines = 0;
                    self.phasing_stack.reset();
                    self.phase = Phase::Image;
                    // The phasing signal runs for half a minute and the lock
                    // takes a few lines of it: the rest are still phasing, not
                    // chart, and are dropped as they pass (see `lead_phasing`).
                    self.lead_phasing = Some(self.aligned_pulse(pulse));
                    self.lead_skipped = 0;
                    self.lead_keep = LEAD_PHASING_KEEP;
                    if self.start_pending {
                        self.start_pending = false;
                        out.push(WefaxEvent::Start { ioc: self.ioc, lpm: self.lpm, auto: true });
                    }
                    out.push(WefaxEvent::Phased { skew: mean });
                }
                if self.phase == Phase::Phasing
                    && self.auto_begun
                    && self.phase_samples as f64 > AUTO_PHASE_TIMEOUT_S * self.rate
                {
                    self.finish(false, out);
                    return;
                }
                if self.phase == Phase::Phasing
                    && self.phase_samples as f64 > PHASE_TIMEOUT_S * self.rate
                {
                    // No usable pulse: start anyway rather than sit on a chart
                    // that is already halfway through.
                    self.phase = Phase::Image;
                    out.push(WefaxEvent::Phased { skew: 0 });
                }
            }
            Phase::Image => {
                // The phasing signal continues past the lock; the auto-slant
                // keeps measuring its drift on the lines that still carry the
                // pulse (and a chart line simply fails the strict pulse test).
                self.probe_slant(&line);
                // The other half of the same rule: the start signal is five
                // seconds long and a fade can swallow it whole, while the
                // phasing signal that follows it runs for thirty. A run of
                // phasing lines in the middle of a picture is the next
                // transmission, whether its start tone arrived or not — and
                // "in the middle of a picture" is load-bearing, because a page
                // opens with its *own* phasing signal: see
                // [`WefaxRx::note_phasing_line`].
                // Counting the phasing lines dropped from the head as well as
                // the ones drawn: it is time since the lock that is asked.
                let lines_since_lock =
                    u32::from(self.line_no) + self.lead_skipped.saturating_sub(self.lead_keep);
                let since_lock_s = f64::from(lines_since_lock) * self.samples_per_line() / self.rate;
                // The run is maintained whether or not the guard has expired —
                // `note_phasing_line` is what counts the lines, so skipping the
                // call would leave the count at zero when the guard lifts.
                if self.auto_start
                    && self.note_phasing_line(&line)
                    && since_lock_s >= PHASING_SIGNAL_S
                {
                    // The new transmission is announced once its own phasing
                    // locks, not here: a transmission also ends with a few
                    // seconds of the same black-and-pulse lines, and that is
                    // the end of this page, not the start of another.
                    self.finish(false, out);
                    self.begin_pending(self.ioc);
                    return;
                }
                // A page begun by its phasing signal alone is still in the
                // rest of it: those lines are the signal, not the chart.
                if let Some(pulse) = self.lead_phasing {
                    // Judged against where the lock found the pulse, not by the
                    // line's own shape: the end of a phasing signal is where it
                    // fades into the chart, and a line there is too scattered
                    // to show a pulse of its own.
                    let room =
                        (PHASING_SIGNAL_S * 1.5 * self.rate / self.samples_per_line()) as u32;
                    // The first line after a lock carries the samples between
                    // the cut and the picture's start in front of it: judge the
                    // line proper, the last `spl` samples.
                    let proper = &line[line.len().saturating_sub(self.samples_per_line().round() as usize)..];
                    let mean = proper.iter().sum::<f32>() / proper.len().max(1) as f32;
                    if self.lead_skipped < room && mean <= 0.4 && pulse.stands_out_by(proper, 0.12) {
                        self.lead_skipped += 1;
                        if self.lead_skipped > self.lead_keep {
                            return;
                        }
                        // Drawn: a thin strip of it stays as the page's top edge.
                        let gray = self.picture(from);
                        out.push(WefaxEvent::Line { y: self.line_no, gray });
                        self.line_no = self.line_no.saturating_add(1);
                        return;
                    }
                    self.lead_phasing = None;
                }
                if self.auto_begun && self.note_blank_line(&line) {
                    let gray = self.picture(from);
                    out.push(WefaxEvent::Line { y: self.line_no, gray });
                    self.line_no = self.line_no.saturating_add(1);
                    self.finish(false, out);
                    return;
                }
                let gray = self.picture(from);
                self.image_slant(&gray);
                out.push(WefaxEvent::Line { y: self.line_no, gray });
                self.line_no = self.line_no.saturating_add(1);
                if self.line_no >= MAX_LINES {
                    self.finish(false, out);
                }
            }
            Phase::Idle => {}
        }
    }
}

/// Map a subcarrier frequency to brightness, 0.0 at `black` … 1.0 at `white`.
fn brightness_between(hz: f64, black: f64, white: f64) -> f32 {
    (((hz - black) / (white - black)) as f32).clamp(0.0, 1.0)
}

/// Half the picture band-pass's width, Hz. The detection filter is ±1100 Hz and
/// is kept as it is. A wider one for the picture sharpens an edge (1.8 pixels
/// wide at ±1100, 1.6 at ±1300, 1.4 at ±1400 on the test chart) and lets in
/// noise at the same rate: on a real recording the isolated pixels per line
/// went 1.5, 2.1, 2.7, 3.2 over the same widths. ±1200 is the middle.
const PIC_HALF_BW_HZ: f64 = 1200.0;

/// The detection brightness is smoothed by a one-pole filter that lags the raw
/// discriminator by a sample; the picture is cut on the raw one, so its line
/// starts this much earlier than the line clock found on the smoothed one.
const PIC_DELAY_SAMPLES: f64 = 1.0;

/// A phase step longer than this many times the recent mean is a crash, not
/// signal, and is left out of the pixel it lands in.
const PIC_CRASH: f32 = 3.0;

/// Share of its expected signal a pixel needs to be drawn from its own steps.
const PIC_MIN_WEIGHT: f32 = 0.34;

/// Coherence a pixel needs, and how far its stand-in may be, pixels.
const PIC_COHERENCE: f32 = 0.25;
const PIC_FILL_PX: usize = 24;
/// Gaps this wide (the distance from the nearest good pixel on one side plus
/// the other, in pixels) are filled from the line above alone.
const PIC_FILL_NEAR: usize = 6;

/// A line with fewer than this share of pixels drawn from their own signal is
/// a lost line, and the line above is repeated for it, at most
/// [`PIC_LOST_RUN`] times in a row.
const PIC_LOST_SHARE: f32 = 0.15;
const PIC_LOST_RUN: u32 = 4;

/// Sum the phase steps across each of `width` pixels of a line that begins
/// `origin` steps into `px` and is `spl` long. The ends of a pixel count in
/// part, by how much of the step they cover, so no pixel is a step wider than
/// its neighbour. Returns each pixel's sum and the sum of its steps' lengths.
fn picture_line(px: &[Complex32], origin: f64, spl: f64, width: usize) -> Vec<(Complex32, f32)> {
    let mut out = vec![(Complex32::default(), 0.0f32); width];
    if px.is_empty() || width == 0 {
        return out;
    }
    let step = spl / width as f64;
    let last = px.len() as f64;
    for (x, o) in out.iter_mut().enumerate() {
        let a = (origin + x as f64 * step).clamp(0.0, last);
        let b = (origin + (x + 1) as f64 * step).clamp(0.0, last);
        if b <= a {
            continue;
        }
        let (ia, ib) = (a.floor() as usize, (b.ceil() as usize).min(px.len()));
        let (mut s, mut m) = (Complex32::default(), 0.0f32);
        for (i, d) in px.iter().enumerate().take(ib).skip(ia) {
            let w = ((i as f64 + 1.0).min(b) - (i as f64).max(a)) as f32;
            s += *d * w;
            m += d.norm() * w;
        }
        *o = (s, m);
    }
    out
}

/// How far a stretch of picture leans, in parts per million of a line, and how
/// sharply the picture says so.
///
/// **The lean that makes the picture's columns sharpest.** A page cut on a wrong
/// clock moves *everything* sideways by the same amount per line, so a frame's
/// edge or a column of figures is smeared over as many pixels as the stretch
/// has lines times the error; cut on the right clock it falls on one column.
/// Each candidate lean shifts the lines back by what it says and the squares of
/// the column sums are added up: the right one is the sum that peaks. A
/// coastline, a meridian or a line of text moves with its own slope whatever
/// the candidate, so it adds the same to all of them and does not pull the
/// answer, which a correlation of one line against the next did (bench,
/// 2026-11-12: -111, -62, -110, +203, +248 ppm from windows of a page whose
/// clock was a few tens off).
///
/// The second number is the best score over the median one: near 1 the
/// picture has nothing vertical to go by.
fn image_lean_once(rows: &[Vec<u8>]) -> Option<(f64, f64)> {
    lean_search(rows, LEAN_MAX_PPM, LEAN_STEP_PPM)
}

/// [`image_lean_once`] over `±max_ppm` in steps of `step_ppm`, then a fifth of a
/// step around the coarse peak.
fn lean_search(rows: &[Vec<u8>], max_ppm: f64, step_ppm: f64) -> Option<(f64, f64)> {
    let width = rows.first()?.len();
    if width == 0 || rows.len() < 8 {
        return None;
    }
    let ink: Vec<Vec<f32>> = rows
        .iter()
        .map(|r| {
            let mean = r.iter().map(|&g| 255.0 - f32::from(g)).sum::<f32>() / width as f32;
            r.iter().map(|&g| 255.0 - f32::from(g) - mean).collect()
        })
        .collect();
    let mid = (rows.len() - 1) as f64 / 2.0;
    let score = |ppm: f64| -> f64 {
        let mut column = vec![0.0f32; width];
        for (y, row) in ink.iter().enumerate() {
            // Where this line's content has got to, in pixels, against the middle one.
            let shift = ppm * 1e-6 * width as f64 * (y as f64 - mid);
            let base = shift.floor();
            let frac = (shift - base) as f32;
            let base = base as isize;
            for (x, c) in column.iter_mut().enumerate() {
                let a = (x as isize + base).rem_euclid(width as isize) as usize;
                let b = (a + 1) % width;
                *c += row[a] * (1.0 - frac) + row[b] * frac;
            }
        }
        column.iter().map(|&c| f64::from(c) * f64::from(c)).sum()
    };
    let steps = (2.0 * max_ppm / step_ppm).round() as usize;
    let grid: Vec<(f64, f64)> = (0..=steps)
        .map(|k| {
            let ppm = -max_ppm + k as f64 * step_ppm;
            (ppm, score(ppm))
        })
        .collect();
    let (best, _) = grid.iter().enumerate().max_by(|a, b| a.1.1.total_cmp(&b.1.1))?;
    // The edge of the grid is not a peak.
    if best == 0 || best + 1 == grid.len() {
        return None;
    }
    // Finer, around the coarse peak.
    let centre = grid[best].0;
    let fine: Vec<(f64, f64)> = (-5..=5)
        .map(|k| {
            let ppm = centre + f64::from(k) * step_ppm / 5.0;
            (ppm, score(ppm))
        })
        .collect();
    let top = fine.iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    let mut all: Vec<f64> = grid.iter().map(|g| g.1).collect();
    all.sort_by(f64::total_cmp);
    let median = all[all.len() / 2];
    (median > 0.0 && top.1 > 0.0).then_some((top.0, top.1 / median))
}

/// [`image_lean_once`] over a stretch, **believed only if each half of it says
/// the same**. A clock leans every line of the page alike, so both halves read
/// it; a chart's long straight meridian or the edge of a title is a peak of its
/// own that one half may find and the other does not (bench, 2026-11-12: -122
/// and -186 ppm from stretches of a page whose clock was 27 off, sharp enough
/// to pass on their own).
fn image_lean(rows: &[Vec<u8>]) -> Option<(f64, f64)> {
    let (ppm, sharpness) = image_lean_once(rows)?;
    let (a, b) = rows.split_at(rows.len() / 2);
    for half in [a, b] {
        let (h, hs) = image_lean_once(half)?;
        if hs < LEAN_HALF_CONFIDENCE || (h - ppm).abs() > LEAN_HALF_PPM {
            return None;
        }
    }
    Some((ppm, sharpness))
}

/// Rows in one stretch of [`straighten_page`], at least; a page shorter than
/// this is left alone.
const PAGE_SEGMENT_ROWS: usize = 500;
/// The shortest page [`straighten_page`] looks at.
pub const STRAIGHTEN_MIN_ROWS: usize = PAGE_SEGMENT_ROWS;
/// The leans a finished page is searched over, parts per million, and the step
/// of the coarse search (a fifth of it is the fine one).
const PAGE_MAX_PPM: f64 = 60.0;
const PAGE_STEP_PPM: f64 = 4.0;
/// How sharply the picture must say it: [`lean_search`]'s best score over its
/// median. Measured on four real pages, a whole page with a frame reads 1.4 to
/// 2.4 and a stretch that is nothing but a white margin or a chart's sea 1.0.
const PAGE_SHARP: f64 = 1.15;
const PAGE_SHARP_WHOLE: f64 = 1.3;
/// How far a stretch's lean may be from the whole page's and still be its own
/// (parts per million): a clock that moved during the page is a few parts per
/// million, a chart's meridian that tricked one stretch is tens.
const PAGE_STRETCH_PPM: f64 = 25.0;
/// Pages that would move less than this, in pixels, are left as they are.
const PAGE_MIN_MOVE_PX: f64 = 3.0;

/// What [`straighten_page`] did to a page.
#[derive(Debug, Clone, PartialEq)]
pub struct Straightened {
    /// The whole page's lean, parts per million of a line.
    pub lean_ppm: f64,
    /// The most any row was moved, in pixels.
    pub max_move_px: f64,
    /// The lean taken out of each stretch of the page, top to bottom.
    pub stretch_ppm: Vec<f64>,
}

/// Straighten a finished page from what the picture says of its own clock.
///
/// A line clock is fixed by the phasing signal and by what the receiver says of
/// its own sample rate, and neither is good to better than a few parts per
/// million; a page is 1300 lines of 1809 pixels, so five parts per million is a
/// frame edge that walks twelve pixels, and the clock that moved during the page
/// bends it. The finished page is the best witness there is: [`lean_search`]
/// finds the shear that makes its columns sharpest, over the whole page and
/// over stretches of it, and each row is moved back by what the walk up to it
/// came to, to a fraction of a pixel, so a frame edge comes out a straight line.
///
/// A stretch with nothing vertical in it (a title, a white margin) says nothing
/// and takes the page's lean; one that disagrees with the page by more than
/// [`PAGE_STRETCH_PPM`] is a chart's meridian and takes it too. A page that says
/// nothing at all is left alone.
pub fn straighten_page(image: &mut [u8], width: usize, height: usize) -> Option<Straightened> {
    if width < 64 || height < PAGE_SEGMENT_ROWS || image.len() < width * height {
        return None;
    }
    let rows: Vec<Vec<u8>> = image.chunks(width).take(height).map(<[u8]>::to_vec).collect();
    let (whole, sharp) = lean_search(&rows, PAGE_MAX_PPM, PAGE_STEP_PPM)?;
    if sharp < PAGE_SHARP_WHOLE {
        return None;
    }
    let stretches = (height / PAGE_SEGMENT_ROWS).clamp(1, 8);
    let measured: Vec<Option<f64>> = (0..stretches)
        .map(|k| {
            let part = &rows[k * height / stretches..(k + 1) * height / stretches];
            lean_search(part, PAGE_MAX_PPM, PAGE_STEP_PPM)
                .filter(|&(ppm, sharp)| {
                    sharp >= PAGE_SHARP && (ppm - whole).abs() <= PAGE_STRETCH_PPM
                })
                .map(|(ppm, _)| ppm)
        })
        .collect();
    // A stretch that said nothing is joined to the ones that did, so the page
    // keeps bending the way its neighbours say instead of snapping to the mean.
    let known: Vec<(usize, f64)> =
        measured.iter().enumerate().filter_map(|(i, m)| m.map(|v| (i, v))).collect();
    let lean: Vec<f64> = (0..stretches)
        .map(|i| match (known.iter().rev().find(|k| k.0 <= i), known.iter().find(|k| k.0 >= i)) {
            (Some(&(a, va)), Some(&(b, vb))) if a != b => {
                va + (vb - va) * (i - a) as f64 / (b - a) as f64
            }
            (Some(&(_, v)), _) | (_, Some(&(_, v))) => v,
            _ => whole,
        })
        .collect();
    // Each stretch's lean holds at its middle row and is joined to its
    // neighbours' linearly, so the page bends smoothly where the clock moved.
    let at = |y: f64| -> f64 {
        let pos = (y / height as f64 * stretches as f64 - 0.5).clamp(0.0, (stretches - 1) as f64);
        let (i, f) = (pos.floor() as usize, pos.fract());
        let j = (i + 1).min(stretches - 1);
        lean[i] * (1.0 - f) + lean[j] * f
    };
    // The walk up to row `y`, in pixels: the sum of the lean of the rows above.
    let per_ppm = width as f64 * 1e-6;
    let mut walk = Vec::with_capacity(height);
    let mut sum = 0.0;
    for y in 0..height {
        walk.push(sum);
        sum += at(y as f64 + 0.5) * per_ppm;
    }
    // Centred, so the page keeps its middle where it was.
    let mean = walk.iter().sum::<f64>() / height as f64;
    let max_move = walk.iter().map(|w| (w - mean).abs()).fold(0.0, f64::max);
    if max_move < PAGE_MIN_MOVE_PX {
        return None;
    }
    let mut row = vec![0u8; width];
    for (y, line) in image.chunks_mut(width).take(height).enumerate() {
        let d = walk[y] - mean;
        let base = d.floor();
        let f = (d - base) as f32;
        let base = base as isize;
        for (x, o) in row.iter_mut().enumerate() {
            let i = (x as isize + base).rem_euclid(width as isize) as usize;
            let j = (i + 1) % width;
            *o = (f32::from(line[i]) * (1.0 - f) + f32::from(line[j]) * f).round() as u8;
        }
        line.copy_from_slice(&row);
    }
    Some(Straightened { lean_ppm: whole, max_move_px: max_move, stretch_ppm: lean })
}

/// Where the phasing pulse ends, in samples from the start of `line`.
///
/// A phasing line is black apart from a white pulse occupying about 5 % of it.
/// Correlating against that shape — rather than hunting for an edge — is what
/// makes this survive a noisy line: a single spike cannot move a correlation
/// taken over a twentieth of the line, and the returned offset is the amount
/// the line clock has to be advanced to put the pulse at the end where it
/// belongs.
///
/// `None` when the line does not look like a phasing line at all, which is how
/// a picture that is already under way falls through to the timeout.
#[cfg(test)]
fn phasing_skew(line: &[f32]) -> Option<u32> {
    phasing_skew_with(line, false)
}

/// The same, with the extra evidence a *mid-picture* rephase has to demand.
///
/// A chart's own graphic can be mostly black with one bright patch, and a patch
/// that does not move is in the same place line after line — which is the whole
/// of what the lenient test looks for, and enough to cut a picture in two
/// (issue #496). So the strict test also requires the line to be almost black
/// and the patch to be nearly white. A chart's bright area is usually wide —
/// a cloud mass, a filled border, a heavy graphic — and that pushes the line's
/// mean up and fails the test, while a real phasing pulse is a narrow 5 % strip
/// of full white on black.
///
/// The pulse's *position* is deliberately not tested. Mid-picture the line
/// buffer is cut on the old transmission's clock, so the next transmission's
/// phasing pulse lands at an arbitrary offset (issue #276); requiring it at the
/// end of the buffer rejected real rephases. Shape here, and the place-to-place
/// consistency of `note_phasing_line`, are what tell phasing from picture.
///
/// Where this stops: a chart feature that is *itself* a narrow near-white strip
/// on an almost-black line is a phasing pulse as far as any of these tests can
/// tell — same width, same brightness, same place line after line — and it will
/// still cut a page. Nothing here can separate the two, and neither can a
/// longer run. What is ruled out is the broad bright area a dark chart actually
/// tends to carry, which is the shape issue #496 was reported on (DWD line-art
/// charts, mostly white, never come near the mean threshold at all).
///
/// Used only for the mid-picture rephase. Phasing proper uses the lenient test,
/// because there the pulse is expected and a noisy line should still align.
fn phasing_skew_strict(line: &[f32]) -> Option<u32> {
    phasing_skew_with(line, true)
}

fn phasing_skew_with(line: &[f32], strict: bool) -> Option<u32> {
    let pulse = pulse_region(line, strict)?;
    Some(pulse.picture_start(line.len()))
}

/// Where a phasing line's white pulse is: the samples (circular, so one that
/// straddles the line boundary is found whole) from its rising edge, `start`,
/// for `width` samples.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PulseRegion {
    start: usize,
    width: usize,
}

impl PulseRegion {
    /// Where the picture begins, in samples into a line of `n`.
    ///
    /// The picture starts in the middle of the pulse.
    ///
    /// The pulse is five per cent of the line and the picture's own white
    /// margin is the other half of it: the test pattern of 2026-10-19, a
    /// reference decoded square by other programs, came out with its left ruler
    /// cut off and wrapped to the right edge, 2.5 % of a line, when the picture
    /// was started on the pulse's falling edge, and 5 % too far the other way
    /// when started on its rising edge. A station whose pulse is wider (one real
    /// recording's is a quarter of the line) is placed by its middle as well.
    fn picture_start(&self, n: usize) -> u32 {
        ((self.start + self.width / 2 + (self.width % 2)) % n.max(1)) as u32
    }

    /// Whether `line` is brighter inside the pulse than outside it by a clear
    /// margin — whether this one line carries the pulse at all.
    fn stands_out(&self, line: &[f32]) -> bool {
        self.stands_out_by(line, 0.25)
    }

    /// The same, for a margin of the caller's choosing.
    fn stands_out_by(&self, line: &[f32], margin: f32) -> bool {
        let n = line.len();
        if n == 0 || self.width == 0 || self.width >= n {
            return false;
        }
        let inside: f32 = (0..self.width).map(|i| line[(self.start + i) % n]).sum();
        let total: f32 = line.iter().sum();
        let (inside, outside) =
            (inside / self.width as f32, (total - inside) / (n - self.width) as f32);
        inside - outside >= margin
    }
}

/// Find the white pulse of a phasing line, or `None` if it is not one.
fn pulse_region(line: &[f32], strict: bool) -> Option<PulseRegion> {
    let n = line.len();
    let pulse = (n as f64 * 0.05).round() as usize;
    if n < 32 || pulse == 0 {
        return None;
    }
    // A phasing line is mostly black with one narrow bright pulse. "Mostly
    // black" was an absolute mean before — 0.20 strict, 0.35 loose — and on a
    // real, weak signal that never holds: noise lifts every black sample, so a
    // measured phasing line sat at a mean of 0.19 in its best moments and 0.28
    // to 0.50 for the rest, while its pulse still reached 1.00. Every line
    // failed, the eight-in-a-row that announce the next transmission never
    // accumulated, and the phasing was drawn into the picture as a black band
    // instead of cutting the page (issue #239: charts glued together, pages
    // running to the length limit).
    //
    // The floor is judged on the **median** — what most of the line is — and
    // the pulse has to stand clear of it by `PHASING_CONTRAST`. That is the
    // property a phasing line actually has, and it survives a noisy signal
    // because noise moves the median and the pulse together.
    let mut sorted: Vec<f32> = line.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    let max_body = if strict { 0.55 } else { 0.75 };
    if median > max_body {
        return None;
    }
    // Running sum over a pulse-wide window, wrapping, so a pulse split across
    // the line boundary is still found in one piece.
    let mut best = (f32::MIN, 0usize);
    let mut acc: f32 = line[..pulse].iter().sum();
    for start in 0..n {
        if acc > best.0 {
            best = (acc, start);
        }
        acc -= line[start];
        acc += line[(start + pulse) % n];
    }
    // The window has to actually be white, or there was no pulse to find —
    // and on the strict path it has to stand clear of the line's own body.
    let min_pulse = if strict { 0.75 } else { 0.55 };
    let window = best.0 / pulse as f32;
    if window <= min_pulse {
        return None;
    }
    if strict && window < median + PHASING_CONTRAST {
        return None;
    }
    // Grow the window to the pulse: out to where the line goes black on either
    // side. A station's pulse can be several times wider than the standard's
    // five per cent, and the window then sits anywhere inside it.
    let quiet = (n / 600).max(2);
    let reach = n * 3 / 10;
    let edge = |from: usize, forward: bool| -> Option<usize> {
        let mut run = 0usize;
        for off in 0..reach {
            let i = if forward { (from + off) % n } else { (from + n - off % n) % n };
            if line[i] < 0.5 {
                run += 1;
                if run >= quiet {
                    return Some(off + 1 - run);
                }
            } else {
                run = 0;
            }
        }
        None
    };
    // Forward from the window's end, backward from just before its start.
    let after = edge((best.1 + pulse) % n, true)?;
    let before = edge((best.1 + n - 1) % n, false)?;
    let width = pulse + after + before;
    if width > reach {
        // White for most of the line is a bright picture, not a pulse.
        return None;
    }
    if strict && width > pulse * 2 {
        // A phasing pulse is five per cent of the line, give or take the
        // edges. A chart's own dark bar carries a wider bright patch (its
        // border or lettering) that the checks above can still read as a
        // pulse, and it is not one (#444).
        return None;
    }
    if strict {
        // A phasing line is black outside its one pulse. A chart's thick black
        // bar with white lettering carries several bright patches, and the run
        // of "same pulse, same place, line after line" cannot reject it — the
        // graphic is fixed in the chart. The bright area *outside* the pulse
        // can: a phasing line keeps it near zero, the bar does not (#444).
        let start = (best.1 + n - before) % n;
        let bright_outside = line
            .iter()
            .enumerate()
            .filter(|(i, v)| {
                let off = (*i + n - start) % n;
                **v > 0.5 && off >= width
            })
            .count();
        if bright_outside as f32 / n as f32 > REPHASE_BRIGHT_OUTSIDE {
            return None;
        }
        return Some(PulseRegion { start, width });
    }
    Some(PulseRegion { start: (best.1 + n - before) % n, width })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 12_000.0;

    /// Synthesise a WEFAX subcarrier from `levels` (0.0 black … 1.0 white), one
    /// entry per sample.
    ///
    /// Built from the *standard's* description — 1500 Hz black, 2300 Hz white,
    /// phase-continuous FM — and not from anything the decoder does, so a
    /// decoder that agreed only with itself would fail these.
    fn modulate(levels: &[f32]) -> Vec<f32> {
        let mut ph = 0.0f64;
        levels
            .iter()
            .map(|&l| {
                let hz = BLACK_HZ + (WHITE_HZ - BLACK_HZ) * l as f64;
                ph += TAU * hz / RATE;
                if ph > TAU {
                    ph -= TAU;
                }
                (ph.sin() * 0.5) as f32
            })
            .collect()
    }

    /// [`modulate`] with the whole subcarrier moved `offset_hz` up: a receiver
    /// tuned that far below the signal.
    fn modulate_off(levels: &[f32], offset_hz: f64) -> Vec<f32> {
        let mut ph = 0.0f64;
        levels
            .iter()
            .map(|&l| {
                let hz = BLACK_HZ + offset_hz + (WHITE_HZ - BLACK_HZ) * l as f64;
                ph = (ph + TAU * hz / RATE) % TAU;
                (ph.sin() * 0.5) as f32
            })
            .collect()
    }

    /// `lines` lines of a ramp black → white, on a receiver `offset_hz` off.
    fn ramp_off(lpm: Lpm, lines: usize, offset_hz: f64) -> Vec<f32> {
        let per = (RATE * lpm.line_secs()) as usize;
        let levels: Vec<f32> =
            (0..lines).flat_map(|_| (0..per).map(move |i| i as f32 / per as f32)).collect();
        modulate_off(&levels, offset_hz)
    }

    /// `secs` of the subcarrier alternating black/white at `alt_hz` — the shape
    /// of the start and stop signals.
    fn tone(alt_hz: f64, secs: f64) -> Vec<f32> {
        let n = (RATE * secs) as usize;
        let levels: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / RATE;
                if (t * alt_hz).fract() < 0.5 {
                    1.0
                } else {
                    0.0
                }
            })
            .collect();
        modulate(&levels)
    }

    /// `count` phasing lines: black, with a white pulse of 5 % centred on the
    /// line boundary — the picture begins in the middle of the pulse.
    fn phasing(lpm: Lpm, count: usize) -> Vec<f32> {
        let per = (RATE * lpm.line_secs()) as usize;
        let pulse = (per as f64 * 0.05) as usize;
        let mut levels = Vec::new();
        for _ in 0..count {
            levels.extend(std::iter::repeat_n(1.0f32, pulse / 2));
            levels.extend(std::iter::repeat_n(0.0f32, per - pulse));
            levels.extend(std::iter::repeat_n(1.0f32, pulse - pulse / 2));
        }
        modulate(&levels)
    }

    /// Add white noise, of `level` against the subcarrier's own 0.5 amplitude.
    ///
    /// Deterministic, so a failing case is the same case next time. Noise is
    /// what a weak chart on a real band actually has, and it is the one thing a
    /// synthetic signal has to carry to be worth testing against.
    fn noise(audio: &mut [f32], level: f32) {
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        for v in audio.iter_mut() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            *v += (((s >> 40) as f32) / 16_777_216.0 - 0.5) * level;
        }
    }

    /// Phasing lines whose black is lifted to `body` — what a weak off-air
    /// signal does, because the noise floor raises every black sample. The pulse
    /// is still full white.
    fn phasing_lifted(lpm: Lpm, count: usize, body: f32) -> Vec<f32> {
        let per = (RATE * lpm.line_secs()) as usize;
        let pulse = (per as f64 * 0.05) as usize;
        let mut levels = Vec::new();
        for _ in 0..count {
            levels.extend(std::iter::repeat_n(body, per - pulse));
            levels.extend(std::iter::repeat_n(1.0f32, pulse));
        }
        modulate(&levels)
    }

    /// The same, with the pulse at `frac` of the line.
    ///
    /// Mid-picture the line buffer is cut on the *old* transmission's clock, so
    /// the next transmission's phasing pulse lands at an arbitrary offset
    /// (issue #276) — never conveniently at the end.
    fn phasing_at(lpm: Lpm, count: usize, frac: f64) -> Vec<f32> {
        let per = (RATE * lpm.line_secs()) as usize;
        let pulse = (per as f64 * 0.05) as usize;
        let at = (per as f64 * frac) as usize;
        let mut levels = Vec::new();
        for _ in 0..count {
            let mut line = vec![0.0f32; per];
            for v in line.iter_mut().skip(at).take(pulse) {
                *v = 1.0;
            }
            levels.extend(line);
        }
        modulate(&levels)
    }

    /// Phasing lines cut on a clock that is `slant_ppm` per-mille long: the
    /// pulse sits at the end of each of *these* lines, so against the nominal
    /// line clock it walks right by that fraction of a line per line.
    fn phasing_slanted(lpm: Lpm, count: usize, slant_ppm: f64) -> Vec<f32> {
        let per = RATE * lpm.line_secs();
        let slanted = per * (1.0 + slant_ppm * 1e-6);
        let pulse = (slanted * 0.05) as usize;
        // The fractional part of the line length **accumulates**: a clock that
        // is twenty-one parts per million long is 0.126 samples longer per line,
        // and rounding each line on its own would make that zero. A sound card's
        // crystal does not round.
        let mut levels = Vec::new();
        let mut at = 0.0f64;
        let mut cut = 0usize;
        for _ in 0..count {
            at += slanted;
            let end = at.round() as usize;
            levels.extend(std::iter::repeat_n(0.0f32, end - cut - pulse));
            levels.extend(std::iter::repeat_n(1.0f32, pulse));
            cut = end;
        }
        modulate(&levels)
    }

    /// A picture whose every line is a horizontal ramp black → white, which is
    /// the pattern that makes a misalignment obvious: a correctly phased decode
    /// is monotonic across the row and a wrapped one has a cliff in it.
    fn ramp_picture(lpm: Lpm, lines: usize) -> Vec<f32> {
        let per = (RATE * lpm.line_secs()) as usize;
        let mut levels = Vec::new();
        for _ in 0..lines {
            for i in 0..per {
                levels.push(i as f32 / per as f32);
            }
        }
        modulate(&levels)
    }

    /// A dark chart: every line mostly black with one broad bright band at
    /// `x_frac` of the line — the night side of an infrared satellite image, or
    /// any chart with one heavy graphic on an otherwise black field.
    ///
    /// This is the shape that used to cut pictures up: it passes the lenient
    /// "mostly black with a bright patch" test for a phasing line, and a band
    /// fixed in place does so line after line. It is not a phasing pulse,
    /// though: the band is far wider than the 5 % a pulse occupies, so it lifts
    /// the line's mean above the strict threshold and is turned away.
    fn dark_chart(lpm: Lpm, lines: usize, x_frac: f64) -> Vec<f32> {
        let per = (RATE * lpm.line_secs()) as usize;
        let band = (per as f64 * 0.25) as usize;
        let x = (per as f64 * x_frac) as usize;
        let mut levels = Vec::new();
        for _ in 0..lines {
            let mut line = vec![0.0f32; per];
            for v in line.iter_mut().skip(x).take(band) {
                *v = 1.0;
            }
            levels.extend(line);
        }
        modulate(&levels)
    }

    /// Drive `rx` from idle into `Phase::Image`, so a test can feed it picture.
    fn into_image(rx: &mut WefaxRx, lpm: Lpm) -> Vec<WefaxEvent> {
        let mut out = Vec::new();
        rx.start_manual(&mut out);
        let ev = drain(rx, &phasing(lpm, 8));
        assert!(rx.receiving() && !rx.phasing(), "did not reach the picture: {ev:?}");
        out
    }

    fn drain(rx: &mut WefaxRx, audio: &[f32]) -> Vec<WefaxEvent> {
        let mut out = Vec::new();
        // In blocks, the way the engine feeds it, so nothing may depend on
        // seeing a whole transmission in one call.
        for chunk in audio.chunks(1024) {
            rx.process(chunk, &mut out);
        }
        out
    }

    /// A picture whose every line is black but for a white bar at 40 % of the
    /// line, cut on a clock `clock_ppm` long — what a stream whose real sample
    /// rate is off by that much hands a receiver that believes it is nominal.
    fn bar_picture_on_clock(lpm: Lpm, lines: usize, clock_ppm: f64) -> Vec<f32> {
        bar_picture_bars(lpm, lines, clock_ppm, &[(0.4, 0.5)])
    }

    /// The same with several bars, each between its two fractions of the line:
    /// a chart's frame and meridians. Several of them, so the picture is not
    /// mistaken for a phasing line, and thin, so the correlation has a peak.
    fn bar_picture_bars(lpm: Lpm, lines: usize, clock_ppm: f64, bars: &[(f64, f64)]) -> Vec<f32> {
        let slanted = RATE * lpm.line_secs() * (1.0 + clock_ppm * 1e-6);
        let mut levels = Vec::new();
        let (mut at, mut cut) = (0.0f64, 0usize);
        for _ in 0..lines {
            at += slanted;
            let end = at.round() as usize;
            let len = end - cut;
            let spans: Vec<_> = bars
                .iter()
                .map(|b| ((len as f64 * b.0) as usize)..((len as f64 * b.1) as usize))
                .collect();
            levels.extend(
                (0..len).map(|i| if spans.iter().any(|r| r.contains(&i)) { 1.0f32 } else { 0.0 }),
            );
            cut = end;
        }
        modulate(&levels)
    }

    /// Where the white bar sits in each decoded row, in pixels (centroid).
    fn bar_positions(events: &[WefaxEvent]) -> Vec<f64> {
        events
            .iter()
            .filter_map(|e| match e {
                WefaxEvent::Line { gray, .. } => {
                    let (sum, wsum) = gray.iter().enumerate().fold((0.0, 0.0), |(s, w), (x, &g)| {
                        let g = g as f64;
                        (s + g * x as f64, w + g)
                    });
                    (wsum > 0.0).then(|| sum / wsum)
                }
                _ => None,
            })
            .collect()
    }

    /// A stream whose sample rate runs slow (an RSP1 at its highest gain was
    /// measured 220 ppm slow) leans the page: every line is a little shorter in
    /// samples than the receiver believes, so the picture walks along the row.
    /// Told the measured error, the receiver cuts its lines to the real clock
    /// and the picture stands square.
    #[test]
    fn a_measured_clock_error_squares_a_page_cut_on_a_slow_clock() {
        let clock = -600.0f64;
        let lines = 60;
        let audio = bar_picture_on_clock(Lpm::L120, lines, clock);

        let walk = |told: f64| {
            let mut rx = WefaxRx::new(RATE);
            let mut ev = into_image(&mut rx, Lpm::L120);
            rx.set_clock_ppm(told);
            // This is about the clock the receiver is told, not about the
            // auto-slant, which would otherwise start trimming it.
            rx.slant_locked = true;
            ev.extend(drain(&mut rx, &audio));
            let pos = bar_positions(&ev);
            assert!(pos.len() > 40, "only {} lines", pos.len());
            let settled = &pos[3..];
            settled.last().unwrap() - settled.first().unwrap()
        };

        let width = Ioc::I576.width() as f64;
        let untold = walk(0.0);
        assert!(
            untold.abs() > width * 0.02,
            "the slow clock should lean the page by itself, walked {untold:.1} px"
        );
        let told = walk(clock);
        assert!(
            told.abs() < width * 0.004,
            "with the error known the page must stand square, walked {told:.1} px of {width}"
        );
    }

    /// A stream running at its nominal rate is not touched: telling the receiver
    /// zero changes nothing, so the correction costs a healthy radio nothing.
    #[test]
    fn a_zero_clock_error_leaves_the_decode_as_it_was() {
        let audio = bar_picture_on_clock(Lpm::L120, 20, 0.0);
        let run = |told: Option<f64>| {
            let mut rx = WefaxRx::new(RATE);
            let mut ev = into_image(&mut rx, Lpm::L120);
            if let Some(ppm) = told {
                rx.set_clock_ppm(ppm);
            }
            ev.extend(drain(&mut rx, &audio));
            bar_positions(&ev)
        };
        assert_eq!(run(None), run(Some(0.0)));
    }

    /// The auto-slant measures the pulse's walk over the phasing signal and
    /// trims the line clock to match, so a chart comes out square without the
    /// operator reaching for the slant setting.
    #[test]
    fn the_auto_slant_trims_the_line_clock_to_the_signal() {
        // A clock 1000 ppm long walks the pulse right by a per-mille of a line
        // each line; the receiver must stretch its own clock to follow.
        for slant in [300.0f64, 1000.0, -400.0] {
            let mut rx = WefaxRx::new(RATE);
            rx.start_manual(&mut Vec::new());
            let _ = drain(&mut rx, &phasing_slanted(Lpm::L120, 40, slant));
            assert!(
                (rx.slant_ppm() as f64 - slant).abs() < 80.0,
                "slant {} ppm was left at {}",
                slant,
                rx.slant_ppm()
            );
            // ...and the page can say what the trim was fitted over, which is
            // what a report of a leaning chart needs to be diagnosed from.
            let (ppm, lines) = rx.slant_correction();
            assert!((ppm as f64 - slant).abs() < 80.0, "reported {ppm} for {slant}");
            assert!(lines > 0, "the correction must say how many lines it used");
        }
    }

    /// With the auto-slant off the line clock stays where the operator put it,
    /// from the phasing signal and from the picture alike.
    #[test]
    fn with_the_auto_slant_off_the_trim_is_left_alone() {
        let mut rx = WefaxRx::new(RATE);
        rx.set_auto_slant(false);
        assert!(!rx.auto_slant());
        rx.set_slant_ppm(12.0);
        rx.start_manual(&mut Vec::new());
        let _ = drain(&mut rx, &phasing_slanted(Lpm::L120, 40, 700.0));
        assert_eq!(rx.slant_ppm(), 12.0, "the phasing signal moved the trim");
    }

    /// **The auto-slant follows a pulse that walks across the line boundary,
    /// and does it from a short phasing signal.** A pulse that sits just before
    /// the cut reads as `n - 3`, `n - 5`... and, a line later, as `2`: a straight
    /// walk the fit took for a jump of a whole line, so it never measured a
    /// thing (a test pattern whose phasing signal was fifteen lines long, 2026-11-12).
    #[test]
    fn the_auto_slant_follows_a_pulse_across_the_line_boundary() {
        let n = RATE * Lpm::L120.line_secs();
        for slant in [-500.0f64, 400.0] {
            let mut rx = WefaxRx::new(RATE);
            rx.start_manual(&mut Vec::new());
            // Cut the stream so that the first pulse ends twenty samples short
            // of (or past) a line boundary, on the side the walk is coming
            // from: the walk carries it across the boundary half way through.
            let per = n * (1.0 + slant * 1e-6);
            let skip = (per + slant.signum() * 20.0).round() as usize;
            let audio: Vec<f32> = phasing_slanted(Lpm::L120, 21, slant).into_iter().skip(skip).collect();
            let _ = drain(&mut rx, &audio);
            assert!(
                (rx.slant_ppm() as f64 - slant).abs() < 120.0,
                "slant {slant} ppm across the boundary was left at {}",
                rx.slant_ppm()
            );
        }
    }

    /// The positions a pulse reads as while it walks across the line boundary
    /// are one straight line to the fit, and a chart line that happened to pass
    /// the pulse test is not part of it.
    #[test]
    fn the_fit_unwraps_positions_and_ignores_a_jump() {
        let n = 6000.0;
        let max_step = 14.0;
        let mut fit = SlantFit::default();
        let walk = [4.0, 1.0, 5998.0, 5995.0, 5992.0];
        let out: Vec<f64> = walk.iter().filter_map(|&p| fit.unwrap(p, n, max_step)).collect();
        assert_eq!(out, vec![4.0, 1.0, -2.0, -5.0, -8.0]);
        for (i, p) in out.iter().enumerate() {
            fit.add(i as f64, *p);
        }
        assert!((fit.slope(5).unwrap() + 3.0).abs() < 1e-9);
        // A position a whole chart away from the walk is left out, and the walk
        // goes on from where it was.
        assert_eq!(fit.unwrap(3000.0, n, max_step), None);
        assert_eq!(fit.unwrap(5989.0, n, max_step), Some(-11.0));
    }

    /// **A page starts from the operator's trim, not from the last page's
    /// correction**: a wrong measurement on a weak phasing signal leaned every
    /// page after it, while the pages that were re-measured came out straight.
    #[test]
    fn a_new_page_starts_from_the_operators_trim() {
        let mut rx = WefaxRx::new(RATE);
        rx.set_slant_ppm(100.0);
        rx.start_manual(&mut Vec::new());
        let _ = drain(&mut rx, &phasing_slanted(Lpm::L120, 40, 700.0));
        assert!(rx.slant_ppm() > 500.0, "the page must have been corrected first");
        rx.start_manual(&mut Vec::new());
        assert_eq!(rx.slant_ppm(), 100.0);
        assert!(!rx.slant_reverted());
    }

    /// **A correction that makes the walk worse is undone.** Twenty lines walk
    /// one way and the next twenty, measured against the clock the first
    /// correction set, walk back: the first reading was a mis-measure.
    #[test]
    fn a_correction_that_made_the_walk_worse_is_reverted() {
        let mut rx = WefaxRx::new(RATE);
        rx.start_manual(&mut Vec::new());
        // A correction of +600 ppm has just been made (as if from a window the
        // noise bent), and the signal's own clock is 1000 ppm the other way.
        rx.slant_ppm = 600.0;
        rx.slant_pending = Some((0.0, 600.0, (0.0, 0)));
        let audio = phasing_slanted(Lpm::L120, 36, -1000.0);
        let _ = drain(&mut rx, &audio);
        assert!(rx.slant_reverted(), "the bad correction was kept: {}", rx.slant_ppm());
        assert_eq!(rx.slant_ppm(), 0.0);
        assert_eq!(rx.slant_correction().1, 0);
    }

    /// **One window against a correction does not undo it** (bench,
    /// 2026-11-12, DWD 3855 on an RSP1: +32 ppm from the first twelve lines,
    /// right, then -54 ppm from the next twelve while the gain moved, and the
    /// right correction was thrown away and the page leaned).
    #[test]
    fn one_window_against_a_correction_does_not_undo_it() {
        let mut rx = WefaxRx::new(RATE);
        rx.start_manual(&mut Vec::new());
        rx.slant_ppm = 600.0;
        rx.slant_pending = Some((0.0, 600.0, (0.0, 0)));
        // Twenty lines: one window of twelve, walking the other way.
        let _ = drain(&mut rx, &phasing_slanted(Lpm::L120, 20, -1000.0));
        assert!(!rx.slant_reverted(), "a single window undid the correction");
        assert_eq!(rx.slant_ppm(), 600.0);
    }

    const FRAME: [(f64, f64); 3] = [(0.20, 0.22), (0.45, 0.47), (0.70, 0.72)];

    /// **A walk of fifty parts per million is corrected from the picture.**
    /// The page of that bench recording walked its frame 68 px over 760 lines
    /// (49 ppm), eight windows in a row on the same side, and the picture
    /// correction ignored all of them because it only acted above 80 ppm.
    #[test]
    fn a_small_steady_walk_is_corrected_from_the_picture() {
        let audio = bar_picture_bars(Lpm::L120, 420, 50.0, &FRAME);
        let mut rx = WefaxRx::new(RATE);
        let mut ev = into_image(&mut rx, Lpm::L120);
        ev.extend(drain(&mut rx, &audio));
        let pos = bar_positions(&ev);
        assert!(pos.len() > 380, "only {} lines", pos.len());
        let tail = &pos[pos.len() - 100..];
        let walk = tail.last().unwrap() - tail.first().unwrap();
        assert!(walk.abs() < 3.0, "the bar still walks {walk:.1} px over 100 lines");
    }

    /// The same measurement made on its own thread, so the audio is not held
    /// for it, lands on the same trim as the one made in line.
    #[test]
    fn a_lean_measured_on_a_thread_is_applied_like_one_measured_in_line() {
        let audio = bar_picture_bars(Lpm::L120, 420, 50.0, &FRAME);
        let run = |background: bool| {
            let mut rx = WefaxRx::new(RATE);
            rx.set_background_lean(background);
            let mut ev = into_image(&mut rx, Lpm::L120);
            for block in audio.chunks(500) {
                rx.process(block, &mut ev);
                rx.wait_for_lean();
            }
            (rx.slant_ppm(), bar_positions(&ev))
        };
        let (inline, _) = run(false);
        let (threaded, pos) = run(true);
        assert!(inline.abs() > 20.0, "the fixture has nothing to correct ({inline:.1} ppm)");
        assert!((inline - threaded).abs() < 3.0, "in line {inline:.1} ppm, on a thread {threaded:.1}");
        let tail = &pos[pos.len() - 100..];
        let walk = tail.last().unwrap() - tail.first().unwrap();
        assert!(walk.abs() < 3.0, "the bar still walks {walk:.1} px over 100 lines");
    }

    /// A window that names more than a clock can be off by is a header, a
    /// slanted meridian or a coastline, not the clock: the first windows of
    /// that page read -280, -424 and -476 ppm and led the trim 240 ppm the
    /// wrong way.
    #[test]
    fn a_walk_larger_than_any_clock_is_left_alone() {
        let audio = bar_picture_bars(Lpm::L120, 300, 450.0, &FRAME);
        let mut rx = WefaxRx::new(RATE);
        let mut ev = into_image(&mut rx, Lpm::L120);
        ev.extend(drain(&mut rx, &audio));
        assert_eq!(rx.slant_ppm(), 0.0, "the picture correction followed a walk of 450 ppm");
    }

    /// **The tuning readout is the page's centre, not the pixel's frequency**
    /// (bench, 2026-11-12: the readout swung with the picture, +691 on white,
    /// -54 on grey, and the dial that followed it walked 335 Hz and broke the
    /// page). A receiver 150 Hz off reads +150 whatever pixel is being drawn.
    #[test]
    fn the_tuning_centre_is_the_pages_and_not_the_pixels() {
        let mut rx = WefaxRx::new(RATE);
        rx.start_manual(&mut Vec::new());
        let audio = ramp_off(Lpm::L120, 80, 150.0);
        let mut seen = Vec::new();
        let mut out = Vec::new();
        for (i, block) in audio.chunks(500).enumerate() {
            rx.process(block, &mut out);
            if i > audio.len() / 500 / 2 {
                seen.push(rx.subcarrier_hz() - 1900.0);
            }
        }
        let (lo, hi) = seen.iter().fold((f64::MAX, f64::MIN), |(l, h), &v| (l.min(v), h.max(v)));
        assert!(lo > 110.0 && hi < 190.0, "the centre wandered over {lo:.0}..{hi:.0} Hz, not +150");
    }

    /// A carrier standing on the channel between charts is not a tuning error:
    /// with no page there is nothing to centre.
    #[test]
    fn a_carrier_between_charts_does_not_read_as_a_tuning_error() {
        let mut rx = WefaxRx::new(RATE);
        let carrier: Vec<f32> = (0..(RATE * 6.0) as usize)
            .map(|i| (TAU * 2320.0 * i as f64 / RATE).sin() as f32 * 0.5)
            .collect();
        let _ = drain(&mut rx, &carrier);
        assert_eq!(rx.subcarrier_hz(), 1900.0, "the carrier was read as the subcarrier");
    }

    /// A stretch of white paper moves nothing: its middle is white, not the
    /// subcarrier's.
    #[test]
    fn a_stretch_of_paper_does_not_move_the_tuning_centre() {
        let mut rx = WefaxRx::new(RATE);
        rx.start_manual(&mut Vec::new());
        let _ = drain(&mut rx, &ramp_off(Lpm::L120, 60, 0.0));
        let per = (RATE * Lpm::L120.line_secs()) as usize;
        let _ = drain(&mut rx, &modulate(&vec![1.0f32; per * 60]));
        let off = rx.subcarrier_hz() - 1900.0;
        assert!(off.abs() < 40.0, "sixty lines of paper moved the centre to {off:+.0} Hz");
    }

    /// **The phasing signal is believed over all its lines, not over the first
    /// dozen.** The dozen read +32 ppm on the page of 2026-11-12 and the whole
    /// signal (54 lines) +6: the page's clock was 6 off, and the dozen's trim
    /// leaned its first 880 lines by 26 ppm. A short fit has to name a large
    /// slant to be acted on; the whole signal may name a small one.
    #[test]
    fn a_short_stretch_of_phasing_does_not_set_a_small_trim() {
        let mut rx = WefaxRx::new(RATE);
        rx.start_manual(&mut Vec::new());
        // Forty lines: not enough for the fine floor, and 40 ppm is under the
        // coarse one.
        let _ = drain(&mut rx, &phasing_slanted(Lpm::L120, 40, 40.0));
        assert_eq!(rx.slant_ppm(), 0.0, "a short fit set the trim to {:+.1}", rx.slant_ppm());
        let mut rx = WefaxRx::new(RATE);
        rx.start_manual(&mut Vec::new());
        let _ = drain(&mut rx, &phasing_slanted(Lpm::L120, 60, 40.0));
        assert!(
            (rx.slant_ppm() - 40.0).abs() < 10.0,
            "the whole signal should name 40 ppm, set {:+.1}",
            rx.slant_ppm()
        );
    }

    /// The operator can undo the auto-slant by hand, and it stays off for the
    /// page; the next page measures again.
    #[test]
    fn the_operator_can_revert_the_auto_slant() {
        let mut rx = WefaxRx::new(RATE);
        rx.set_slant_ppm(50.0);
        rx.start_manual(&mut Vec::new());
        let _ = drain(&mut rx, &phasing_slanted(Lpm::L120, 32, 800.0));
        assert!(rx.slant_ppm() > 500.0);
        rx.revert_slant();
        assert_eq!(rx.slant_ppm(), 50.0);
        let _ = drain(&mut rx, &phasing_slanted(Lpm::L120, 30, 800.0));
        assert_eq!(rx.slant_ppm(), 50.0, "the auto-slant must stay off after a revert");
        rx.start_manual(&mut Vec::new());
        let _ = drain(&mut rx, &phasing_slanted(Lpm::L120, 40, 800.0));
        assert!(rx.slant_ppm() > 500.0, "the next page must measure again");
    }

    /// A walk the pulse's scatter could have made is not a slant.
    #[test]
    fn a_noisy_phasing_walk_is_not_believed() {
        let mut fit = SlantFit::default();
        for i in 1..=20 {
            // Positions that jump about with no trend, then a tiny drift on top.
            let p = 3000.0 + if i % 2 == 0 { 40.0 } else { -40.0 } + f64::from(i) * 0.5;
            fit.add(f64::from(i), p);
        }
        let slope = fit.slope(20).unwrap();
        assert!(slope.abs() < SLANT_SIGNIFICANCE * fit.slope_error());
        let mut clean = SlantFit::default();
        for i in 1..=20 {
            clean.add(f64::from(i), 3000.0 + f64::from(i) * 2.0);
        }
        assert!(clean.slope(20).unwrap().abs() > SLANT_SIGNIFICANCE * clean.slope_error());
    }

    /// **A line the receiver lost is not drawn as a black stripe.**
    #[test]
    fn a_lost_line_repeats_the_line_above_for_a_while() {
        let mut rx = WefaxRx::new(RATE);
        rx.pic_mag = 1.0;
        let n = 64usize;
        let good: Vec<(Complex32, f32)> = (0..n)
            .map(|_| {
                let m = rx.samples_per_line() as f32 / n as f32;
                (Complex32::new(m, 0.0), m)
            })
            .collect();
        let lost: Vec<(Complex32, f32)> = (0..n).map(|_| (Complex32::new(0.0, 0.0), 0.0)).collect();
        let (g, share) = rx.shade(&good);
        assert!(share > 0.9);
        let (_, lost_share) = rx.shade(&lost);
        assert!(lost_share < PIC_LOST_SHARE);
        let first = rx.conceal(g.clone(), share);
        assert_eq!(first, g);
        let bad = vec![0u8; n];
        for _ in 0..PIC_LOST_RUN {
            assert_eq!(rx.conceal(bad.clone(), lost_share), g, "the line above must stand in");
        }
        assert_eq!(rx.conceal(bad.clone(), lost_share), bad, "a long fade is shown as it is");
        let _ = rx.conceal(g.clone(), share);
        assert_eq!(rx.conceal(bad.clone(), lost_share), g, "a good line starts the count again");
    }

    /// **A slant too small for a short measurement is still corrected.**
    ///
    /// The DWD chart of 2026-11-03 14:07 came out with its frame border walking
    /// 51 px to the right over 1354 lines: twenty-one parts per million. That is
    /// under the twenty-five a twenty-line measurement can name, so nothing was
    /// applied and the page leaned. The phasing signal is sixty lines long, and
    /// a fit over the whole of it names a slant this small.
    #[test]
    fn a_small_slant_is_corrected_over_the_whole_phasing_signal() {
        let mut rx = WefaxRx::new(RATE);
        rx.start_manual(&mut Vec::new());
        let _ = drain(&mut rx, &phasing_slanted(Lpm::L120, 60, 21.0));
        assert!(
            (rx.slant_ppm() as f64 - 21.0).abs() < 6.0,
            "21 ppm was left at {}",
            rx.slant_ppm()
        );
    }

    /// **A noisy phasing signal still cuts the page.**
    ///
    /// The strict pulse test used to require the line's *mean* to be under 0.20
    /// — "mostly black". On a real chart the noise floor lifts every black
    /// sample and that never holds: measured on one off-air capture the phasing
    /// lines sat at 0.19 in their best moments and 0.28–0.50 otherwise while the
    /// pulse still reached 1.00. Every line failed, the eight-in-a-row never
    /// accumulated, and the next transmission was drawn into the picture as a
    /// black band instead of starting a new page (issue #239).
    ///
    /// Both defects are in this test: the absolute floor, and a hard reset on a
    /// single missed line. The phasing here carries noise on its black, which is
    /// what an off-air signal has.
    #[test]
    fn a_noisy_phasing_signal_still_cuts_the_page() {
        let mut audio = phasing(Lpm::L120, 30);
        audio.extend(ramp_picture(Lpm::L120, 90));
        // The phasing's black sits at 0.35 rather than 0: that is what noise
        // does to it off air, and it is what the old "mean under 0.20" test
        // could not survive.
        audio.extend(phasing_lifted(Lpm::L120, 30, 0.35));
        audio.extend(ramp_picture(Lpm::L120, 40));

        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(Lpm::L120);
        rx.set_auto_start(true);
        let ev = drain(&mut rx, &audio);

        let starts = ev.iter().filter(|e| matches!(e, WefaxEvent::Start { .. })).count();
        let complete = ev
            .iter()
            .filter(|e| matches!(e, WefaxEvent::Complete { by_tone: false, .. }))
            .count();
        assert!(
            complete >= 1,
            "the noisy phasing never ended the page: {starts} start(s), no complete"
        );
        assert!(starts >= 2, "the next chart did not begin a page: {starts} start(s)");
    }

    /// Noise on the phasing signal must leave the clock where it was.
    ///
    /// The probe reads the *position* of the white pulse, and noise moves the
    /// position it reads. Believed in full, a noisy phasing signal measured
    /// **47 500 ppm** — a clock 4.75 % short — and a chart that should have been
    /// merely marred came out as a page of horizontal bands, which reads as "the
    /// decoder is broken" (issue #239). Nothing in a sound card or a station's
    /// clock is that far out, so a measurement that large is not a clock error
    /// and must be dropped rather than applied.
    #[test]
    fn noise_on_the_phasing_signal_does_not_move_the_line_clock() {
        for level in [0.1f32, 0.3, 0.8] {
            let mut rx = WefaxRx::new(RATE);
            rx.set_lpm(Lpm::L120);
            rx.set_auto_start(true);
            let mut audio = phasing(Lpm::L120, 60);
            noise(&mut audio, level);
            let _ = drain(&mut rx, &audio);
            assert!(
                rx.slant_ppm().abs() <= SLANT_MAX_PPM as f32,
                "noise at {level} moved the clock to {} ppm",
                rx.slant_ppm()
            );
            // And in practice a noisy signal gets nowhere near the bound.
            assert!(
                rx.slant_ppm().abs() < SLANT_MAX_PPM as f32 / 4.0,
                "noise at {level} moved the clock to {} ppm",
                rx.slant_ppm()
            );
        }
    }

    #[test]
    fn the_index_of_cooperation_sets_the_line_length() {
        // IOC × π, which is the definition.
        assert_eq!(Ioc::I576.width(), 1809);
        assert_eq!(Ioc::I288.width(), 904);
        assert_eq!(Ioc::I576.start_tone_hz(), 300.0);
        assert_eq!(Ioc::I288.start_tone_hz(), 675.0);
        // 120 LPM is half a second a line.
        assert_eq!(Lpm::L120.line_secs(), 0.5);
        assert_eq!(Lpm::L60.line_secs(), 1.0);
        assert!((Lpm::L240.line_secs() - 0.25).abs() < 1e-12);
    }

    #[test]
    fn black_and_white_land_where_the_standard_puts_them() {
        let brightness = |hz| brightness_between(hz, BLACK_HZ, WHITE_HZ);
        assert_eq!(brightness(BLACK_HZ), 0.0);
        assert_eq!(brightness(WHITE_HZ), 1.0);
        assert!((brightness(CENTRE_HZ) - 0.5).abs() < 1e-6);
        // A badly tuned receiver is clamped rather than wrapped.
        assert_eq!(brightness(1000.0), 0.0);
        assert_eq!(brightness(3000.0), 1.0);
    }

    /// The 300 Hz start signal has to arm the receiver, and the 675 Hz one has
    /// to arm it at the other index of cooperation. The page is announced when
    /// the phasing that follows the tone locks, not on the tone alone.
    #[test]
    fn a_start_tone_begins_a_picture_at_the_index_it_announces() {
        for ioc in Ioc::ALL {
            let mut rx = WefaxRx::new(RATE);
            let mut audio = tone(ioc.start_tone_hz(), 5.0);
            audio.extend(phasing(Lpm::L120, 10));
            let ev = drain(&mut rx, &audio);
            let started = ev.iter().find_map(|e| match e {
                WefaxEvent::Start { ioc, auto, .. } => Some((*ioc, *auto)),
                _ => None,
            });
            assert_eq!(started, Some((ioc, true)), "{ioc:?} start tone was not recognised");
            assert!(rx.receiving());
            assert_eq!(rx.ioc(), ioc);
        }
    }

    /// ...and ordinary picture must not.
    #[test]
    fn a_picture_is_not_mistaken_for_a_start_tone() {
        let mut rx = WefaxRx::new(RATE);
        let ev = drain(&mut rx, &ramp_picture(Lpm::L120, 20));
        assert!(!ev.iter().any(|e| matches!(e, WefaxEvent::Start { .. })), "{ev:?}");
        assert!(!rx.receiving());
        // Nor may silence.
        let mut rx = WefaxRx::new(RATE);
        let ev = drain(&mut rx, &vec![0.0f32; (RATE * 6.0) as usize]);
        assert!(!ev.iter().any(|e| matches!(e, WefaxEvent::Start { .. })), "{ev:?}");
    }

    #[test]
    fn a_stop_tone_ends_the_picture() {
        let mut rx = WefaxRx::new(RATE);
        let mut out = Vec::new();
        rx.start_manual(&mut out);
        assert!(rx.receiving());
        let ev = drain(&mut rx, &tone(STOP_TONE_HZ, 5.0));
        let done = ev.iter().find_map(|e| match e {
            WefaxEvent::Complete { by_tone, .. } => Some(*by_tone),
            _ => None,
        });
        assert_eq!(done, Some(true), "the stop tone did not end the picture: {ev:?}");
        assert!(!rx.receiving());

        // With auto-stop off it keeps going, which is what an operator who is
        // capturing a continuous transmission wants.
        let mut rx = WefaxRx::new(RATE);
        rx.set_auto_stop(false);
        let mut out = Vec::new();
        rx.start_manual(&mut out);
        let ev = drain(&mut rx, &tone(STOP_TONE_HZ, 5.0));
        assert!(!ev.iter().any(|e| matches!(e, WefaxEvent::Complete { .. })));
        assert!(rx.receiving());
    }

    /// The whole point of the phasing signal: the decoded rows have to come out
    /// aligned to the line the transmitter sent, not to whenever we tuned in.
    #[test]
    fn the_phasing_pulse_aligns_the_picture() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut out = Vec::new();
        rx.start_manual(&mut out);

        // Start mid-line, so an unphased decode would be visibly wrapped.
        let per = (RATE * lpm.line_secs()) as usize;
        let mut audio = modulate(&vec![0.0f32; per / 3]);
        audio.extend(phasing(lpm, 8));
        audio.extend(ramp_picture(lpm, 6));
        let ev = drain(&mut rx, &audio);

        assert!(
            ev.iter().any(|e| matches!(e, WefaxEvent::Phased { .. })),
            "never phased: {:?}",
            ev.iter().take(4).collect::<Vec<_>>()
        );
        let lines: Vec<&Vec<u8>> = ev
            .iter()
            .filter_map(|e| match e {
                WefaxEvent::Line { gray, .. } => Some(gray),
                _ => None,
            })
            .collect();
        assert!(lines.len() >= 3, "only {} lines", lines.len());

        // A correctly phased ramp rises monotonically across the row. Check the
        // middle of a settled line, away from the ends where the discriminator's
        // filter delay and the pulse itself blur things.
        let row = lines[lines.len() - 2];
        let w = row.len();
        assert_eq!(w, Ioc::I576.width() as usize);
        let (a, b) = (row[w / 4] as i32, row[3 * w / 4] as i32);
        assert!(b - a > 80, "the row is not a ramp: {a} → {b}");
        // ...and has no cliff in the middle, which is what a wrapped line is.
        let biggest_step = row[w / 8..7 * w / 8]
            .windows(2)
            .map(|p| (p[1] as i32 - p[0] as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(biggest_step < 60, "the row wraps: biggest step {biggest_step}");
    }

    /// Issue #276: the next chart begins and the stop tone never arrives.
    ///
    /// A receiver that just keeps appending draws the new transmission onto the
    /// old page, on the old line phase — the picture is shifted and no amount
    /// of waiting straightens it. The phasing signal at the head of the new
    /// transmission is thirty seconds long and unmistakable, so it is enough on
    /// its own: the page ends there and the next one is phased properly.
    #[test]
    fn a_new_transmission_ends_the_page_and_re_phases_without_a_stop_tone() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        // Auto-stop on, and no stop tone anywhere in what follows: this is the
        // fade that takes the tone, not an operator who switched it off.
        let mut out = Vec::new();
        rx.start_manual(&mut out);
        let mut audio = phasing(lpm, 8);
        // A page long enough for the rule to be allowed to fire at all: the
        // first thirty seconds of a page are its own phasing signal, so the
        // fixture's first page has to run past that — sixty lines at 120 LPM,
        // where a real chart runs for minutes.
        audio.extend(ramp_picture(lpm, 60));
        // The next transmission, arriving with nothing but its phasing signal.
        // A real one runs about thirty seconds — sixty lines at 120 LPM — and
        // the rephase spends eight of them recognising it before the new page
        // starts phasing afresh, so the fixture has to leave enough to phase
        // the second page from.
        audio.extend(phasing(lpm, 16));
        audio.extend(ramp_picture(lpm, 6));
        let ev = drain(&mut rx, &audio);

        let complete = ev.iter().filter(|e| matches!(e, WefaxEvent::Complete { .. })).count();
        assert_eq!(complete, 1, "the first page should have been finished: {ev:?}");
        let starts = ev.iter().filter(|e| matches!(e, WefaxEvent::Start { .. })).count();
        assert_eq!(starts, 1, "and a second page started (the manual one is not an event)");
        let phased = ev.iter().filter(|e| matches!(e, WefaxEvent::Phased { .. })).count();
        assert_eq!(phased, 2, "both pages have to be phased: {ev:?}");

        // The second page's rows are numbered from the top again, and its ramp
        // comes out unwrapped — which is the whole complaint.
        let after: Vec<&Vec<u8>> = ev
            .iter()
            .skip_while(|e| !matches!(e, WefaxEvent::Complete { .. }))
            .filter_map(|e| match e {
                WefaxEvent::Line { gray, .. } => Some(gray),
                _ => None,
            })
            .collect();
        assert!(after.len() >= 3, "only {} lines on the second page", after.len());
        let row = after[after.len() - 2];
        let w = row.len();
        let (a, b) = (row[w / 4] as i32, row[3 * w / 4] as i32);
        assert!(b - a > 80, "the second page is not a ramp: {a} → {b}");
        let biggest_step = row[w / 8..7 * w / 8]
            .windows(2)
            .map(|p| (p[1] as i32 - p[0] as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(biggest_step < 60, "the second page wraps: biggest step {biggest_step}");
    }

    /// The same #276 recovery, when the new transmission's phasing pulse does
    /// *not* line up with the old line clock.
    ///
    /// Mid-picture the buffer is cut on the old transmission's clock, so the
    /// new pulse lands at an arbitrary offset — never conveniently at the end.
    /// A test that only ever places it at the end passes on a decoder that
    /// happens to require that, and misses the real case (caught in review of
    /// #500).
    #[test]
    fn a_new_transmission_is_found_at_an_arbitrary_line_offset() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut out = Vec::new();
        rx.start_manual(&mut out);
        let mut audio = phasing(lpm, 8);
        // Past the page's own phasing signal, as above: the rule cannot fire
        // before then.
        audio.extend(ramp_picture(lpm, 60));
        // The next transmission, its pulse a third of the way into the line as
        // the old clock slices it.
        audio.extend(phasing_at(lpm, 16, 0.3));
        audio.extend(ramp_picture(lpm, 6));
        let ev = drain(&mut rx, &audio);
        assert!(
            ev.iter().any(|e| matches!(e, WefaxEvent::Complete { by_tone: false, .. })),
            "the new transmission was missed: {ev:?}"
        );
    }

    /// **A page's own phasing signal is not the next transmission's.**
    ///
    /// The lock is taken on the first phasing pulse and the standard signal runs
    /// for thirty seconds after it, so every page opens with black lines and a
    /// white pulse — the very shape the rephase rule looks for. Ungated, it cut
    /// the page it had just started: on an off-air recording (`Sigidwiki`'s
    /// *WFax_IntroNORTHWOOD*, a start tone followed by its phasing signal) one
    /// transmission came out as **four** pages of eight to eleven lines, cut
    /// every seven seconds. The rule is for a run *in the middle of a picture*,
    /// so it has to wait for chart content — and once there is chart on the
    /// page, the next transmission is still caught, which is the other half.
    #[test]
    fn the_pages_own_phasing_signal_does_not_cut_the_page() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut out = into_image(&mut rx, lpm);
        // The rest of this transmission's phasing signal, and then its chart.
        let mut audio = phasing(lpm, 40);
        audio.extend(ramp_picture(lpm, 6));
        out.extend(drain(&mut rx, &audio));
        assert!(
            !out.iter().any(|e| matches!(e, WefaxEvent::Complete { .. })),
            "the page's own phasing signal ended it: {out:?}"
        );
        assert!(rx.receiving(), "the page is still being drawn");

        // With chart on the page, a run of phasing lines is the next page again.
        let mut next = phasing(lpm, 16);
        next.extend(ramp_picture(lpm, 6));
        out.extend(drain(&mut rx, &next));
        assert!(
            out.iter().any(|e| matches!(e, WefaxEvent::Complete { by_tone: false, .. })),
            "a new transmission after chart content was missed: {out:?}"
        );
    }

    /// The same, the ordinary way round: the new transmission's *start* signal
    /// arrives while the old page is still being drawn, and its phasing signal
    /// follows. The page ends on the phasing, not on the start tone.
    #[test]
    fn a_start_tone_then_phasing_during_a_picture_starts_the_next_one() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut out = Vec::new();
        rx.start_manual(&mut out);
        let mut audio = phasing(lpm, 8);
        // Past the page's own phasing signal: a page opens with it and the
        // rephase rule may not fire inside it, so the fixture's first page has
        // to run beyond those thirty seconds (sixty lines at 120 LPM).
        audio.extend(ramp_picture(lpm, 60));
        audio.extend(tone(Ioc::I576.start_tone_hz(), 5.0));
        audio.extend(phasing(lpm, 16));
        let ev = drain(&mut rx, &audio);
        assert!(
            ev.iter().any(|e| matches!(e, WefaxEvent::Complete { by_tone: false, .. })),
            "the page should have ended on the next transmission: {ev:?}"
        );
        assert!(rx.receiving(), "and the next one should be under way");
    }

    /// A white carrier of `secs`, with `dips` of black (a fade) of 1.5 s every
    /// 10 s when asked.
    fn white_carrier(secs: f64, fade: bool) -> Vec<f32> {
        let n = (RATE * secs) as usize;
        let levels: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / RATE;
                if fade && t % 10.0 > 8.5 {
                    0.0
                } else {
                    1.0
                }
            })
            .collect();
        modulate(&levels)
    }

    /// An automatic page: start tone, phasing, then `chart` lines of picture.
    fn auto_page(rx: &mut WefaxRx, lpm: Lpm, chart: usize) -> Vec<WefaxEvent> {
        let mut audio = tone(Ioc::I576.start_tone_hz(), 5.0);
        audio.extend(phasing(lpm, 40));
        audio.extend(ramp_picture(lpm, chart));
        drain(rx, &audio)
    }

    /// **The phasing signal does not fill the top of the picture.**
    ///
    /// A start tone, then half a minute of phasing, then the chart. The lock
    /// takes a few lines of the phasing; the rest of it used to be drawn as
    /// fifty-odd black rows at the top of the DWD page of 2026-11-11, where
    /// JVComm32 shows a thin strip.
    #[test]
    fn the_phasing_signal_leaves_a_thin_strip_at_the_top_of_the_page() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut audio = tone(Ioc::I576.start_tone_hz(), 5.0);
        audio.extend(phasing(lpm, 60));
        audio.extend(ramp_picture(lpm, 40));
        let ev = drain(&mut rx, &audio);
        let rows: Vec<&Vec<u8>> = ev
            .iter()
            .filter_map(|e| match e {
                WefaxEvent::Line { gray, .. } => Some(gray),
                _ => None,
            })
            .collect();
        let dark = rows
            .iter()
            .take_while(|r| r.iter().map(|&v| v as u32).sum::<u32>() / (r.len() as u32) < 60)
            .count();
        assert!(
            dark <= LEAD_PHASING_KEEP as usize + 1,
            "{dark} dark rows at the top, of {} lines: the phasing signal is in the picture",
            rows.len()
        );
        assert!(rows.len() >= 36, "the chart itself lost lines: only {} drawn", rows.len());
    }

    fn completes(ev: &[WefaxEvent]) -> usize {
        ev.iter().filter(|e| matches!(e, WefaxEvent::Complete { .. })).count()
    }

    /// **A page ends when the carrier goes away.**
    ///
    /// DWD on 3855 is off the air for minutes between transmissions, and the
    /// schedule chart of 2026-11-03 14:25 UTC ran through that gap: 2286 lines
    /// for a twelve-minute chart, the last six and a half minutes of it noise,
    /// ending only when the next transmission's tone arrived. Neither the stop
    /// tone nor the rephase signal can end such a page — neither is being sent —
    /// and the blank-carrier rule cannot either, because noise has ink in it.
    #[test]
    fn a_page_ends_when_the_carrier_goes_away() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut ev = auto_page(&mut rx, lpm, 70);
        assert!(rx.receiving(), "the page should be under way: {ev:?}");

        // The transmitter goes off: noise, and no 1900 Hz subcarrier in it.
        let mut gone = vec![0.0f32; (RATE * 100.0) as usize];
        noise(&mut gone, 0.5);
        ev.extend(drain(&mut rx, &gone));

        let done = ev.iter().filter(|e| matches!(e, WefaxEvent::Complete { .. })).count();
        assert!(
            done >= 1,
            "the page ran on through a hundred seconds of no carrier ({done} complete)"
        );
    }

    /// A near-white stretch **with a ruled border across it** is content, not
    /// the end of the page.
    ///
    /// This is the bench case of 2026-11-03: GYA on 4608 kHz ended its chart a
    /// minute early, cutting the wind-scale box in half, because the footer is
    /// very nearly white and its mean alone read as a white carrier. The
    /// difference is that the footer has ink in it — the box's own vertical
    /// rules, one in every line of the block — and a carrier has none.
    fn white_carrier_with_a_rule(secs: f64, samples_per_line: f64) -> Vec<f32> {
        let n = (RATE * secs) as usize;
        let step = samples_per_line.round().max(1.0) as usize;
        let levels: Vec<f32> = (0..n)
            .map(|i| if i % step < 30 { 0.0 } else { 1.0 })
            .collect();
        modulate(&levels)
    }

    #[test]
    fn a_nearly_white_footer_with_a_ruled_border_does_not_end_the_page() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut ev = auto_page(&mut rx, lpm, 20);
        assert!(rx.receiving(), "the page should be under way: {ev:?}");
        let spl = rx.samples_per_line();
        ev.extend(drain(&mut rx, &white_carrier_with_a_rule(150.0, spl)));
        assert_eq!(
            completes(&ev),
            0,
            "a ruled footer is ink, not a white carrier: {ev:?}"
        );
        assert!(rx.receiving(), "the page is still open");

        // ...and the carrier that really follows it still closes it.
        ev.extend(drain(&mut rx, &white_carrier(130.0, false)));
        assert_eq!(completes(&ev), 1, "a clean white carrier ends the page: {ev:?}");
    }

    /// The station keeps a white carrier up after the chart and the stop tone
    /// was missed: the page ends, and the next start tone begins another.
    #[test]
    fn a_white_carrier_after_the_chart_ends_an_automatic_page() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut ev = auto_page(&mut rx, lpm, 20);
        assert!(rx.receiving(), "the page should be under way: {ev:?}");
        ev.extend(drain(&mut rx, &white_carrier(130.0, false)));
        assert_eq!(completes(&ev), 1, "the white carrier should end the page: {ev:?}");
        assert!(!rx.receiving());
        let next = auto_page(&mut rx, lpm, 6);
        assert!(rx.receiving(), "a new start tone should begin a page: {next:?}");
    }

    /// Fades in the carrier do not restart the count.
    #[test]
    fn a_fading_white_carrier_still_ends_the_page() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut ev = auto_page(&mut rx, lpm, 20);
        ev.extend(drain(&mut rx, &white_carrier(150.0, true)));
        assert_eq!(completes(&ev), 1, "the fading carrier should end the page: {ev:?}");
    }

    /// A white stretch shorter than the limit is chart, not an ending.
    #[test]
    fn a_white_margin_shorter_than_the_limit_does_not_cut_the_page() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut ev = auto_page(&mut rx, lpm, 20);
        ev.extend(drain(&mut rx, &white_carrier(40.0, false)));
        ev.extend(drain(&mut rx, &ramp_picture(lpm, 10)));
        assert_eq!(completes(&ev), 0, "a short white margin ended the page: {ev:?}");
        assert!(rx.receiving());
    }

    /// The operator's own START is theirs to stop: no white-carrier ending.
    #[test]
    fn a_manual_page_is_not_ended_by_a_white_carrier() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut ev = into_image(&mut rx, lpm);
        ev.extend(drain(&mut rx, &ramp_picture(lpm, 20)));
        ev.extend(drain(&mut rx, &white_carrier(90.0, false)));
        assert_eq!(completes(&ev), 0, "a manual page was ended: {ev:?}");
    }

    /// A stop tone with a hole in it still ends the page.
    #[test]
    fn a_stop_tone_with_a_fade_in_it_still_ends_the_page() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut ev = into_image(&mut rx, lpm);
        ev.extend(drain(&mut rx, &ramp_picture(lpm, 6)));
        let mut audio = tone(STOP_TONE_HZ, 3.0);
        audio.extend(white_carrier(0.5, false));
        audio.extend(tone(STOP_TONE_HZ, 3.0));
        ev.extend(drain(&mut rx, &audio));
        assert!(
            ev.iter().any(|e| matches!(e, WefaxEvent::Complete { by_tone: true, .. })),
            "the stop tone with a fade in it was missed: {ev:?}"
        );
    }

    /// A stop tone that is weak or filtered, swinging only between 0.3 and 0.7
    /// of the range and lasting four seconds, still ends the page: a signal of
    /// 2026-11-12 sent one, and the page ran on past it for minutes.
    #[test]
    fn a_weak_four_second_stop_tone_still_ends_the_page() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut ev = into_image(&mut rx, lpm);
        ev.extend(drain(&mut rx, &ramp_picture(lpm, 6)));
        let n = (RATE * 4.0) as usize;
        let levels: Vec<f32> = (0..n)
            .map(|i| if (i as f64 / RATE * STOP_TONE_HZ).fract() < 0.5 { 0.7 } else { 0.3 })
            .collect();
        let mut audio = modulate(&levels);
        audio.extend(white_carrier(2.0, false));
        ev.extend(drain(&mut rx, &audio));
        assert!(
            ev.iter().any(|e| matches!(e, WefaxEvent::Complete { by_tone: true, .. })),
            "the weak stop tone was missed: {ev:?}"
        );
    }

    /// A start tone mid-picture with no phasing behind it is the chart's own
    /// detail, not a new transmission: the page goes on.
    #[test]
    fn a_start_tone_with_no_phasing_behind_it_does_not_cut_the_page() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut out = into_image(&mut rx, lpm);
        let mut audio = ramp_picture(lpm, 2);
        audio.extend(tone(Ioc::I576.start_tone_hz(), 5.0));
        audio.extend(ramp_picture(lpm, 2));
        out.extend(drain(&mut rx, &audio));
        assert!(
            !out.iter().any(|e| matches!(e, WefaxEvent::Complete { .. })),
            "a start-tone-like pattern ended the page: {out:?}"
        );
        assert!(rx.receiving());
    }

    /// A periodic pattern in the chart (dotted graticule lines) alternates at
    /// the start tone's rate for as long as it is on the page. It must not
    /// restart the picture over and over.
    #[test]
    fn a_chart_with_periodic_detail_is_not_restarted() {
        let lpm = Lpm::L120;
        let per = (RATE * lpm.line_secs()) as usize;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut out = into_image(&mut rx, lpm);
        let mut levels = Vec::new();
        for _ in 0..40 {
            for i in 0..per {
                let px = i * 1809 / per;
                levels.push(if (px / 6) % 2 == 0 { 0.0 } else { 1.0 });
            }
        }
        out.extend(drain(&mut rx, &modulate(&levels)));
        let lines = out.iter().filter(|e| matches!(e, WefaxEvent::Line { .. })).count();
        assert!(lines >= 30, "only {lines} lines came out");
        assert!(
            !out.iter().any(|e| matches!(e, WefaxEvent::Complete { .. })),
            "periodic detail restarted the picture: {out:?}"
        );
    }

    /// An automatic start that is never followed by phasing is noise or a
    /// chart's detail. It must not be saved as a picture — and must not be
    /// announced either, or the picture on screen is wiped for nothing.
    #[test]
    fn an_auto_start_with_no_phasing_yields_no_picture() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut audio = tone(Ioc::I576.start_tone_hz(), 5.0);
        audio.extend(ramp_picture(lpm, 60));
        let ev = drain(&mut rx, &audio);
        assert!(
            !ev.iter().any(|e| matches!(e, WefaxEvent::Phased { .. })),
            "a ramp was phased: {ev:?}"
        );
        assert!(
            !ev.iter().any(|e| matches!(e, WefaxEvent::Start { .. } | WefaxEvent::Complete { .. })),
            "the unphased start was announced: {ev:?}"
        );
        assert!(!rx.receiving());
    }

    /// ...but only when the operator asked for automatic starts. Someone
    /// capturing a continuous transmission with AUTO START off gets one page,
    /// however many charts go by.
    #[test]
    fn a_manual_capture_is_never_cut_in_two() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        rx.set_auto_start(false);
        let mut out = Vec::new();
        rx.start_manual(&mut out);
        let mut audio = phasing(lpm, 8);
        audio.extend(ramp_picture(lpm, 4));
        audio.extend(phasing(lpm, 10));
        let ev = drain(&mut rx, &audio);
        assert!(!ev.iter().any(|e| matches!(e, WefaxEvent::Complete { .. })), "{ev:?}");
        assert!(rx.receiving());
    }

    /// Issue #496: a chart's own high-contrast detail must not be read as the
    /// next transmission's phasing signal.
    ///
    /// `dark_chart` is mostly black with one broad bright band per line, in a
    /// fixed place — a satellite image's night side, or a chart with a heavy
    /// graphic. It passes the lenient shape test for a phasing line and, being
    /// fixed, does so line after line, so the old four-line run cut the picture
    /// into fragments. The band is far wider than the 5 % a phasing pulse
    /// occupies, which is what the strict shape test sees. The pulse's position
    /// is not what tells them apart: a real rephase's pulse is at an arbitrary
    /// offset anyway (issue #276).
    #[test]
    fn a_dark_chart_with_a_bright_feature_is_not_a_new_transmission() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut out = into_image(&mut rx, lpm);
        // Six seconds of it, which is what a chart produces.
        out.extend(drain(&mut rx, &dark_chart(lpm, 12, 0.5)));
        assert!(
            !out.iter().any(|e| matches!(e, WefaxEvent::Complete { .. })),
            "a chart's own graphic ended the page: {out:?}"
        );
        assert!(rx.receiving());
    }

    /// Issue #496: a brief burst at the stop tone's rate is a noise burst or a
    /// fade, not five seconds of stop tone. It must not end a healthy chart.
    #[test]
    fn a_brief_stop_tone_burst_does_not_end_the_picture() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut out = into_image(&mut rx, lpm);
        let mut audio = ramp_picture(lpm, 2);
        audio.extend(tone(STOP_TONE_HZ, 1.5));
        audio.extend(ramp_picture(lpm, 2));
        out.extend(drain(&mut rx, &audio));
        assert!(
            !out.iter().any(|e| matches!(e, WefaxEvent::Complete { .. })),
            "a 1.5 s burst ended the page: {out:?}"
        );
        assert!(rx.receiving());
    }

    /// ...and the same for a brief burst at the start tone's rate mid-picture.
    #[test]
    fn a_brief_start_tone_burst_does_not_cut_the_page() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut out = into_image(&mut rx, lpm);
        let mut audio = ramp_picture(lpm, 2);
        audio.extend(tone(Ioc::I576.start_tone_hz(), 1.5));
        audio.extend(ramp_picture(lpm, 2));
        out.extend(drain(&mut rx, &audio));
        assert!(
            !out.iter().any(|e| matches!(e, WefaxEvent::Complete { .. })),
            "a 1.5 s start burst cut the page: {out:?}"
        );
        assert!(rx.receiving());
    }

    /// A line has to come out at the width the index of cooperation demands,
    /// whatever the line rate — that is the only thing fixing the aspect ratio.
    #[test]
    fn every_line_rate_produces_a_full_width_line() {
        for lpm in Lpm::ALL {
            for ioc in Ioc::ALL {
                let mut rx = WefaxRx::new(RATE);
                rx.set_lpm(lpm);
                rx.set_ioc(ioc);
                let mut out = Vec::new();
                rx.start_manual(&mut out);
                let mut audio = phasing(lpm, 8);
                audio.extend(ramp_picture(lpm, 4));
                let ev = drain(&mut rx, &audio);
                let line = ev.iter().find_map(|e| match e {
                    WefaxEvent::Line { gray, .. } => Some(gray),
                    _ => None,
                });
                let line = line.unwrap_or_else(|| panic!("{lpm:?} {ioc:?} produced no line"));
                assert_eq!(line.len(), ioc.width() as usize, "{lpm:?} {ioc:?}");
            }
        }
    }

    /// Rows must be numbered from zero and consecutively — a gap would be an
    /// image with a band of nothing across it.
    #[test]
    fn rows_are_numbered_consecutively_from_the_top() {
        let lpm = Lpm::L240; // fastest, so the test stays short
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut out = Vec::new();
        rx.start_manual(&mut out);
        let mut audio = phasing(lpm, 8);
        audio.extend(ramp_picture(lpm, 12));
        let ev = drain(&mut rx, &audio);
        let ys: Vec<u16> = ev
            .iter()
            .filter_map(|e| match e {
                WefaxEvent::Line { y, .. } => Some(*y),
                _ => None,
            })
            .collect();
        assert!(ys.len() >= 8, "only {} lines", ys.len());
        assert_eq!(ys[0], 0);
        for (i, y) in ys.iter().enumerate() {
            assert_eq!(*y as usize, i, "row numbering jumped at {i}");
        }
        assert_eq!(rx.lines() as usize, ys.len());
    }

    /// The slant trim has to change the line length, which is the whole of what
    /// it does: a sound card a hundred parts per million fast walks the image
    /// sideways over a quarter-hour chart.
    #[test]
    fn the_slant_trim_stretches_the_line_clock() {
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(Lpm::L120);
        let nominal = rx.samples_per_line();
        assert!((nominal - 6000.0).abs() < 1e-6, "{nominal}");
        rx.set_slant_ppm(1000.0);
        assert!((rx.samples_per_line() - 6006.0).abs() < 1e-6);
        rx.set_slant_ppm(-1000.0);
        assert!((rx.samples_per_line() - 5994.0).abs() < 1e-6);
        assert_eq!(rx.slant_ppm(), -1000.0);
    }

    /// Nudging the alignment moves the picture by whole pixels, so the operator
    /// can straighten a chart whose phasing was missed.
    #[test]
    fn a_phase_nudge_moves_the_line_by_pixels() {
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(Lpm::L120);
        let before = rx.line_start;
        rx.nudge_phase(100);
        let spp = rx.samples_per_line() / Ioc::I576.width() as f64;
        assert!(((before - rx.line_start) - 100.0 * spp).abs() < 1e-6);
        rx.nudge_phase(-100);
        assert!((rx.line_start - before).abs() < 1e-6);
    }

    /// Settings may not change under a picture in progress: a chart whose top
    /// half is 1809 pixels wide and whose bottom half is 904 is not an image.
    #[test]
    fn the_line_geometry_is_frozen_while_receiving() {
        let mut rx = WefaxRx::new(RATE);
        let mut out = Vec::new();
        rx.start_manual(&mut out);
        rx.set_ioc(Ioc::I288);
        rx.set_lpm(Lpm::L60);
        assert_eq!(rx.ioc(), Ioc::I576);
        assert_eq!(rx.lpm(), Lpm::L120);
        // Once it has finished they take effect.
        rx.stop_manual(&mut out);
        rx.set_ioc(Ioc::I288);
        assert_eq!(rx.ioc(), Ioc::I288);
        // Stopping twice is harmless and emits one completion.
        let before = out.len();
        rx.stop_manual(&mut out);
        assert_eq!(out.len(), before);
    }

    /// Picture must not be read as a phasing line, or a chart tuned into
    /// halfway would keep re-aligning to its own content.
    #[test]
    fn only_a_real_phasing_line_yields_a_skew() {
        let n = 6000;
        // Black with a white pulse at the end: a phasing line.
        let mut l = vec![0.0f32; n];
        let pulse = n / 20;
        for v in l.iter_mut().skip(n - pulse) {
            *v = 1.0;
        }
        let skew = phasing_skew(&l).expect("a phasing line must yield a skew");
        // The picture starts in the middle of the pulse.
        assert!((skew as usize).abs_diff(n - pulse / 2) <= 2, "skew {skew} of {n}");

        // The same pulse a third of the way in.
        let mut l = vec![0.0f32; n];
        for v in l.iter_mut().skip(n / 3).take(pulse) {
            *v = 1.0;
        }
        assert_eq!(phasing_skew(&l), Some((n / 3 + pulse / 2) as u32));

        // A ramp is picture, not phasing.
        let ramp: Vec<f32> = (0..n).map(|i| i as f32 / n as f32).collect();
        assert_eq!(phasing_skew(&ramp), None);
        // All white is not phasing either.
        assert_eq!(phasing_skew(&vec![1.0; n]), None);
        // All black has no pulse to find.
        assert_eq!(phasing_skew(&vec![0.0; n]), None);
        // Degenerate input is refused rather than panicking.
        assert_eq!(phasing_skew(&[]), None);
        assert_eq!(phasing_skew(&[0.0; 8]), None);
    }

    /// A mid-picture line that is a chart's own thick black bar with white
    /// lettering must not be read as the next transmission's phasing: the run
    /// of "same pulse, same place, line after line" fits it exactly, so the
    /// bright area outside the pulse is what has to tell them apart (#444).
    #[test]
    fn a_chart_bar_is_not_read_as_mid_picture_phasing() {
        let n = 6000;
        let pulse = n / 20;
        // A real phasing line: black with one narrow white pulse.
        let mut phasing = vec![0.0f32; n];
        for v in phasing.iter_mut().skip(n / 3).take(pulse) {
            *v = 1.0;
        }
        assert!(
            phasing_skew_strict(&phasing).is_some(),
            "a real phasing line must still pass the strict test"
        );

        // The water-temperature chart's black bar: several white patches (the
        // longitude labels) spread across an almost-black line.
        let mut bar = vec![0.0f32; n];
        for k in 0..5 {
            let at = n / 10 + k * n / 6;
            for v in bar.iter_mut().skip(at).take(pulse) {
                *v = 1.0;
            }
        }
        assert_eq!(
            phasing_skew_strict(&bar),
            None,
            "a chart's dark bar with white lettering must not cut the page"
        );
    }

    #[test]
    fn a_line_is_summed_down_to_the_requested_width_with_partial_ends() {
        let one = Complex32::new(1.0, 0.0);
        let px = vec![one; 6];
        // Two steps per pixel: each pixel's sum is two steps long.
        let s = picture_line(&px, 0.0, 6.0, 3);
        assert!(s.iter().all(|(c, m)| (c.re - 2.0).abs() < 1e-5 && (m - 2.0).abs() < 1e-5));
        // A pixel boundary in the middle of a step splits the step's weight:
        // 2.5 steps per pixel from half a step in.
        let s = picture_line(&px, 0.5, 5.0, 2);
        assert!((s[0].1 - 2.5).abs() < 1e-5 && (s[1].1 - 2.5).abs() < 1e-5, "{s:?}");
        // Before the buffer and after it there is nothing, not a panic.
        let s = picture_line(&px, -3.0, 6.0, 3);
        assert!(s[0].1 == 0.0 && s[2].1 > 0.0);
        assert_eq!(picture_line(&[], 0.0, 6.0, 3).len(), 3);
    }
    /// **The real thing: an off-air recording decodes a picture.**
    ///
    /// Point `SDROXIDE_WEFAX_SAMPLE` at a mono **12 kHz** WAV — convert a
    /// capture with `ffmpeg -i capture.mp3 -ac 1 -ar 12000 fax.wav` (the
    /// engine's digi tap rate, which is what the decoder is built for) — and
    /// set the geometry if it is not the usual one:
    /// `SDROXIDE_WEFAX_LPM` and `SDROXIDE_WEFAX_IOC`.
    ///
    /// Off-air material cannot live in the tree, so the test is `#[ignore]`d and
    /// skips when the variable is unset. Run it with
    /// `cargo test -p sdroxide-dsp --release -- --ignored --nocapture`.
    ///
    /// **What it asks of the recording**, and this is the part worth knowing
    /// before pointing it at a file: a picture can only be phased from a
    /// *phasing pulse*, which a weather service sends in the thirty seconds
    /// before its chart. A recording that starts in the middle of a chart has
    /// no pulse in it and therefore no line start — the decoder is right to
    /// decode nothing from it, and this test would fail on a perfectly good
    /// file. So record from before the start tone, or take the transmission
    /// whole: `Sigidwiki`'s *WFax_IntroNORTHWOOD* sample is such a recording —
    /// a start tone, thirty seconds of phasing and six seconds of chart — and
    /// it is what this test was checked against (start found, phased, 8–11
    /// lines at 120 LPM, which is what six seconds of a chart is).
    #[test]
    #[ignore]
    fn an_off_air_recording_decodes() {
        let Ok(path) = std::env::var("SDROXIDE_WEFAX_SAMPLE") else {
            eprintln!("SDROXIDE_WEFAX_SAMPLE unset; skipping");
            return;
        };
        let mut reader = hound::WavReader::open(&path).expect("open the sample WAV");
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, RATE as u32, "the sample must be at 12 000 Hz");
        assert_eq!(spec.channels, 1, "the sample must be mono");
        let audio: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>(),
            hound::SampleFormat::Int => {
                let peak = (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.map(|v| v as f32 / peak))
                    .collect::<Result<_, _>>()
            }
        }
        .expect("read the sample");

        let lpm = match std::env::var("SDROXIDE_WEFAX_LPM").ok().as_deref() {
            Some("60") => Lpm::L60,
            Some("90") => Lpm::L90,
            Some("180") => Lpm::L180,
            Some("240") => Lpm::L240,
            _ => Lpm::L120,
        };
        let ioc = match std::env::var("SDROXIDE_WEFAX_IOC").ok().as_deref() {
            Some("288") => Ioc::I288,
            _ => Ioc::I576,
        };

        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        rx.set_ioc(ioc);
        rx.set_auto_start(true);
        let mut events = Vec::new();
        let mut lines = 0usize;
        let mut pages: Vec<u16> = Vec::new();
        let mut phased = false;
        let mut started = false;
        for block in audio.chunks(4096) {
            events.clear();
            rx.process(block, &mut events);
            for e in &events {
                match e {
                    WefaxEvent::Start { .. } => started = true,
                    WefaxEvent::Phased { skew } => {
                        phased = true;
                        println!("phased {skew} samples into the line");
                    }
                    WefaxEvent::Line { .. } => lines += 1,
                    WefaxEvent::Complete { lines, by_tone } => {
                        pages.push(*lines);
                        println!("complete: {lines} lines, by_tone={by_tone}")
                    }
                }
            }
        }
        // The page structure is printed rather than asserted: a recording of one
        // transmission that ends mid-chart leaves its page open, one that carries
        // a stop tone closes it, and one of several transmissions makes several
        // pages — all of them correct. What the *decoder* must not do is cut a
        // page inside its own phasing signal, and that is pinned by
        // `the_pages_own_phasing_signal_does_not_cut_the_page`.
        println!(
            "{path}: started={started} phased={phased} lines={lines} pages={pages:?} open={}",
            lines - pages.iter().map(|n| *n as usize).sum::<usize>()
        );
        assert!(started, "no start tone found in the recording");
        assert!(phased, "the start tone was found but no phasing pulse — was the file cut short?");
        assert!(
            lines >= 4,
            "phased but produced {lines} lines: a picture that stops immediately is the fault \
             this test exists to catch"
        );
    }

    // ---- START while a page is in the picture ----

    /// Events with the picture lines collapsed, so a failure reads as a story.
    fn story(ev: &[WefaxEvent]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut run = 0;
        for e in ev {
            if let WefaxEvent::Line { .. } = e {
                run += 1;
                continue;
            }
            if run > 0 {
                out.push(format!("{run} lines"));
                run = 0;
            }
            out.push(match e {
                WefaxEvent::Start { auto, .. } => format!("Start(auto={auto})"),
                WefaxEvent::Phased { .. } => "Phased".into(),
                WefaxEvent::Complete { lines, by_tone } => {
                    format!("Complete({lines}, by_tone={by_tone})")
                }
                WefaxEvent::Line { .. } => unreachable!(),
            });
        }
        if run > 0 {
            out.push(format!("{run} lines"));
        }
        out
    }

    fn starts(ev: &[WefaxEvent]) -> usize {
        ev.iter().filter(|e| matches!(e, WefaxEvent::Start { .. })).count()
    }

    /// START while a page is in the picture closes it and begins another.
    #[test]
    fn start_during_a_picture_begins_a_new_page() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut ev = into_image(&mut rx, lpm);
        ev.extend(drain(&mut rx, &ramp_picture(lpm, 10)));
        rx.start_manual(&mut ev);
        let s = story(&ev);
        assert_eq!(completes(&ev), 1, "START left the old page open: {s:?}");
        // The first START, from `into_image`, and the new one.
        assert_eq!(starts(&ev), 2, "START did not begin a page: {s:?}");
        assert!(rx.phasing(), "not phasing a new page: {s:?}");
    }

    /// Control: START from idle, after a finished page, does begin one.
    #[test]
    fn start_after_a_finished_page_begins_one() {
        let lpm = Lpm::L120;
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(lpm);
        let mut ev = into_image(&mut rx, lpm);
        ev.extend(drain(&mut rx, &ramp_picture(lpm, 6)));
        rx.stop_manual(&mut ev);
        assert!(!rx.receiving());
        rx.start_manual(&mut ev);
        assert!(rx.receiving(), "START from idle did nothing: {:?}", story(&ev));
    }

    /// Auto is the decoder that shipped: exactly `1.0`, and every threshold the
    /// constant beside it. `+` widens, `−` narrows, and the ends are the ends of
    /// the slider.
    #[test]
    fn the_reference_sensitivity_is_the_decoder_as_it_was() {
        let mut rx = WefaxRx::new(RATE);
        assert_eq!(rx.sensitivity(), 0, "a new decoder did not start at the reference");
        assert_eq!(rx.sens_k(), 1.0, "the reference multiplier is not exactly one");
        assert_eq!(rx.tone_tol(), TONE_TOL, "the reference crossing tolerance moved");
        assert_eq!(rx.rephase_tol(), REPHASE_TOL, "the reference phasing tolerance moved");

        rx.set_sensitivity(5);
        assert!(rx.tone_tol() > TONE_TOL, "+ did not widen the crossing tolerance");
        assert!(rx.rephase_tol() > REPHASE_TOL, "+ did not widen the phasing tolerance");
        let wide = cal_range(BLACK_RANGE_HZ, rx.sens_k());

        rx.set_sensitivity(-5);
        assert!(rx.tone_tol() < TONE_TOL, "− did not narrow the crossing tolerance");
        assert!(rx.rephase_tol() < REPHASE_TOL, "− did not narrow the phasing tolerance");
        let narrow = cal_range(BLACK_RANGE_HZ, rx.sens_k());
        assert!(wide.1 - wide.0 > BLACK_RANGE_HZ.1 - BLACK_RANGE_HZ.0, "{wide:?}");
        assert!(narrow.1 - narrow.0 < BLACK_RANGE_HZ.1 - BLACK_RANGE_HZ.0, "{narrow:?}");

        // Off the end of the slider is off the end, not wherever the value landed.
        rx.set_sensitivity(9);
        assert_eq!(rx.sensitivity(), 5);
        rx.set_sensitivity(-9);
        assert_eq!(rx.sensitivity(), -5);
    }

    /// The three thresholds that are *shares* stay shares at every setting.
    ///
    /// They are scaled as `f · (2 − k)` and not `f / k`: at `−5` dividing asks
    /// for 1.5 of a window's energy in one line, which no signal can hold, so the
    /// detector would hear no start or stop tone at all — not a strict decoder, a
    /// deaf one. `2 − k` keeps every fraction inside `(0, 1)` and is monotonic.
    #[test]
    fn a_fraction_threshold_stays_inside_the_share_it_measures() {
        let mut rx = WefaxRx::new(RATE);
        for s in -5..=5i8 {
            rx.set_sensitivity(s);
            for f in [rx.audio_tone.purity_threshold(), rx.subcarrier_share(), rx.periodic_share()]
            {
                assert!(f > 0.0 && f < 1.0, "at {s:+}: {f:.2} is not a share of anything");
            }
        }

        rx.set_sensitivity(0);
        assert_eq!(rx.audio_tone.purity_threshold(), AUDIO_TONE_PURITY);
        assert_eq!(rx.subcarrier_share(), SUBCARRIER_SHARE);
        assert_eq!(rx.periodic_share(), PERIODIC_SHARE);

        // `+` is the more sensitive direction, so the share it must reach falls.
        rx.set_sensitivity(-5);
        let strict = rx.audio_tone.purity_threshold();
        rx.set_sensitivity(5);
        let loose = rx.audio_tone.purity_threshold();
        assert!(loose < AUDIO_TONE_PURITY && AUDIO_TONE_PURITY < strict, "{loose} < {strict}");
    }

    // ----- Sharpness of a strong picture -------------------------------------

    /// One line of the sharpness chart, as pixel values 0.0 / 1.0, 1809 of them:
    /// white, 3-pixel stripes at 330..670, a black block at 700..899, a white
    /// field at 900..1499, a black block at 1500..1699, white to the end.
    fn sharp_pixel(x: usize) -> f32 {
        match x {
            330..670 => {
                if ((x - 330) / 3) % 2 == 0 {
                    0.0
                } else {
                    1.0
                }
            }
            700..900 | 1500..1700 => 0.0,
            _ => 1.0,
        }
    }

    /// `lines` lines of that chart at 120 LPM, modulated.
    fn sharp_chart(lines: usize) -> Vec<f32> {
        let per = (RATE * Lpm::L120.line_secs()) as usize;
        let w = Ioc::I576.width() as usize;
        let mut levels = Vec::with_capacity(per * lines);
        for _ in 0..lines {
            for i in 0..per {
                levels.push(sharp_pixel(i * w / per));
            }
        }
        modulate(&levels)
    }

    /// What the receiver draws of the chart, averaged over the lines.
    #[derive(Debug)]
    struct Sharpness {
        /// Pixels in the middle of the edge at x = 700 (values 25..230).
        front: f32,
        /// Stripe contrast: brightest minus darkest in each 6-pixel period, 0..255.
        stripes: f32,
        /// Pixels in the flats on the wrong side of 127, per line.
        stray: f32,
        /// Pixels in the flats more than 40 from the right value, per line.
        off: f32,
    }

    fn sharpness(audio: &[f32]) -> Sharpness {
        sharpness_with(audio, |_| {})
    }

    fn sharpness_with(audio: &[f32], setup: impl Fn(&mut WefaxRx)) -> Sharpness {
        let mut rx = WefaxRx::new(RATE);
        setup(&mut rx);
        into_image(&mut rx, Lpm::L120);
        let ev = drain(&mut rx, audio);
        let rows: Vec<&Vec<u8>> = ev
            .iter()
            .filter_map(|e| match e {
                WefaxEvent::Line { gray, .. } => Some(gray),
                _ => None,
            })
            .collect();
        assert!(rows.len() >= 12, "only {} lines", rows.len());
        // The first and last lines carry the junction with the phasing signal.
        let rows = &rows[3..rows.len() - 1];
        let n = rows.len() as f32;
        let (mut front, mut stripes, mut stray, mut off) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for r in rows {
            // Where the edge really is, so a pixel or two of line phase does not
            // change what is measured.
            let d = (690..712).find(|&x| r[x] < 127 && r[x + 1] >= 127).unwrap_or(700) as i32 - 700;
            let at = |x: i32| r[(x + d) as usize];
            front += (694..706).filter(|&x| (25..=230).contains(&at(x))).count() as f32;
            let mut c = 0.0f32;
            let mut k = 0.0f32;
            for p in (336..660).step_by(6) {
                let seg = (p..p + 6).map(|x| at(x) as f32);
                let hi = seg.clone().fold(0.0f32, f32::max);
                let lo = seg.fold(255.0f32, f32::min);
                c += hi - lo;
                k += 1.0;
            }
            stripes += c / k;
            for (a, b, want) in [(712, 888, 0u8), (912, 1488, 255), (1512, 1688, 0)] {
                for x in a..b {
                    let v = at(x);
                    if (v >= 127) != (want == 255) {
                        stray += 1.0;
                    }
                    if (i32::from(v) - i32::from(want)).abs() > 40 {
                        off += 1.0;
                    }
                }
            }
        }
        Sharpness { front: front / n, stripes: stripes / n, stray: stray / n, off: off / n }
    }

    /// Bursts of impulse noise: 40 samples every 2900, at amplitude 6.
    fn crashes(audio: &mut [f32]) {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        for (i, v) in audio.iter_mut().enumerate() {
            if i % 2900 < 40 {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                *v += (((s >> 40) as f32) / 16_777_216.0 - 0.5) * 12.0;
            }
        }
    }

    /// Measurement only: the chart with several settings, printed.
    #[test]
    #[ignore]
    fn sharpness_sweep() {
        let clean = sharp_chart(40);
        let mut noisy = sharp_chart(40);
        noise(&mut noisy, 0.3);
        crashes(&mut noisy);
        for (bw, curve, fill) in [
            (1100.0, false, false),
            (1100.0, false, true),
            (1100.0, true, true),
            (1300.0, true, true),
            (1400.0, true, true),
            (1500.0, true, true),
        ] {
            {
                let set = |rx: &mut WefaxRx| {
                    rx.set_pic_band(bw);
                    rx.set_contrast(curve);
                    rx.set_fill(fill);
                };
                eprintln!("bw {bw} curve {curve} fill {fill}");
                eprintln!("  clean: {:?}", sharpness_with(&clean, set));
                eprintln!("  noisy: {:?}", sharpness_with(&noisy, set));
            }
        }
    }

    /// A strong, clean chart comes out with a sharp edge and full stripes.
    #[test]
    fn a_clean_strong_chart_is_sharp() {
        let m = sharpness(&sharp_chart(40));
        eprintln!("clean: {m:?}");
        // Measured: front 2.0, stripes 255 (before the weighted detector: 2.0, 243).
        assert!(m.front <= 2.5, "{m:?}");
        assert!(m.stripes >= 250.0, "{m:?}");
        assert!(m.stray < 0.5, "{m:?}");
    }

    /// The pieces that are only for the picture each earn their place on a
    /// noisy chart: filling a pixel the crash blanker emptied removes strays,
    /// and the contrast curve narrows the edge and pulls grey back to the rails.
    #[test]
    fn the_fill_and_the_contrast_curve_each_help_a_noisy_chart() {
        let mut audio = sharp_chart(40);
        noise(&mut audio, 0.3);
        crashes(&mut audio);
        let plain = sharpness_with(&audio, |rx| {
            rx.set_fill(false);
            rx.set_contrast(false);
        });
        let fill = sharpness_with(&audio, |rx| rx.set_contrast(false));
        let both = sharpness(&audio);
        eprintln!("plain {plain:?}\nfill  {fill:?}\nboth  {both:?}");
        assert!(fill.stray < plain.stray - 1.0, "{plain:?} {fill:?}");
        assert!(both.front < fill.front - 0.3, "{fill:?} {both:?}");
        assert!(both.off < fill.off * 0.8, "{fill:?} {both:?}");
    }

    /// A strong chart under noise and impulse crashes has no stray pixels in
    /// its flat areas.
    #[test]
    fn a_strong_chart_with_noise_and_crashes_has_no_stray_pixels() {
        let mut audio = sharp_chart(40);
        noise(&mut audio, 0.3);
        crashes(&mut audio);
        let m = sharpness(&audio);
        eprintln!("noisy: {m:?}");
        // Measured with the 40-sample crashes every 2900 samples: stray 8.8, off
        // 16 per line (before this change: 8.3 and 18; the receiver's own
        // crashes bury a pixel, and the guard is that no more get through).
        assert!(m.stray < 11.0, "{m:?}");
        assert!(m.off < 25.0, "{m:?}");
        assert!(m.front <= 2.5, "{m:?}");
        assert!(m.stripes >= 240.0, "{m:?}");
    }
}

/// Start and stop signals that arrive as plain audio tones.
///
/// The standard describes them as the subcarrier alternating black/white at 300
/// and 450 Hz, but a transmission can also carry them as a literal 300 / 450 Hz
/// tone in the audio, which the subcarrier discriminator cannot see at all.
#[cfg(test)]
mod audio_tone_tests {
    use super::*;

    const RATE: f64 = 12_000.0;
    const LPM: Lpm = Lpm::L120;

    fn sine(hz: f64, secs: f64) -> Vec<f32> {
        (0..(RATE * secs) as usize)
            .map(|i| (0.5 * (TAU * hz * i as f64 / RATE).sin()) as f32)
            .collect()
    }

    fn fm(levels: &[f32]) -> Vec<f32> {
        let mut ph = 0.0f64;
        levels
            .iter()
            .map(|&l| {
                ph += TAU * (BLACK_HZ + (WHITE_HZ - BLACK_HZ) * l as f64) / RATE;
                (ph.sin() * 0.5) as f32
            })
            .collect()
    }

    fn per_line() -> usize {
        (RATE * LPM.line_secs()) as usize
    }

    /// Black lines, each with a 5 % white pulse centred on the line boundary.
    fn phasing(lines: usize) -> Vec<f32> {
        let per = per_line();
        let pulse = per / 20;
        let mut v = Vec::new();
        for _ in 0..lines {
            v.extend(std::iter::repeat_n(1.0f32, pulse / 2));
            v.extend(std::iter::repeat_n(0.0f32, per - pulse));
            v.extend(std::iter::repeat_n(1.0f32, pulse - pulse / 2));
        }
        fm(&v)
    }

    fn picture(lines: usize) -> Vec<f32> {
        let per = per_line();
        let v: Vec<f32> =
            (0..lines).flat_map(|_| (0..per).map(move |i| i as f32 / per as f32)).collect();
        fm(&v)
    }

    fn black(secs: f64) -> Vec<f32> {
        fm(&vec![0.0; (RATE * secs) as usize])
    }

    /// Start tone, phasing, a picture, stop tone, black carrier: the shape of a
    /// reference recording, with the tones as audio.
    fn page(start_hz: f64, chart: usize) -> Vec<f32> {
        let mut a = sine(start_hz, 5.0);
        a.extend(phasing(20));
        a.extend(picture(chart));
        a.extend(sine(450.0, 5.0));
        a.extend(black(3.0));
        a
    }

    fn feed(rx: &mut WefaxRx, audio: &[f32]) -> Vec<WefaxEvent> {
        let mut out = Vec::new();
        for c in audio.chunks(1024) {
            rx.process(c, &mut out);
        }
        out
    }

    fn rx() -> WefaxRx {
        let mut rx = WefaxRx::new(RATE);
        rx.set_lpm(LPM);
        rx
    }

    fn starts(ev: &[WefaxEvent]) -> usize {
        ev.iter().filter(|e| matches!(e, WefaxEvent::Start { .. })).count()
    }

    fn completes(ev: &[WefaxEvent]) -> Vec<(u16, bool)> {
        ev.iter()
            .filter_map(|e| match e {
                WefaxEvent::Complete { lines, by_tone } => Some((*lines, *by_tone)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_300_hz_audio_tone_starts_a_page_and_a_450_hz_one_ends_it() {
        let mut rx = rx();
        let ev = feed(&mut rx, &page(300.0, 80));
        assert_eq!(starts(&ev), 1, "{ev:?}");
        let c = completes(&ev);
        assert_eq!(c.len(), 1, "{ev:?}");
        assert!(c[0].1 && c[0].0 >= 70, "the page did not end on its stop tone: {c:?}");
        assert!(!rx.receiving(), "the decoder did not return to detection");
    }

    #[test]
    fn a_second_page_follows_straight_after_the_first() {
        let mut rx = rx();
        let mut a = page(300.0, 60);
        a.extend(page(300.0, 70));
        let ev = feed(&mut rx, &a);
        assert_eq!(starts(&ev), 2, "{ev:?}");
        assert_eq!(completes(&ev).len(), 2, "{ev:?}");
    }

    #[test]
    fn the_start_tone_may_be_off_frequency() {
        for hz in [250.0, 280.0, 330.0, 355.0] {
            let mut rx = rx();
            let ev = feed(&mut rx, &page(hz, 50));
            assert_eq!(starts(&ev), 1, "{hz} Hz: {ev:?}");
        }
    }

    #[test]
    fn a_bare_carrier_does_not_start_a_page() {
        // A carrier at the start-tone pitch with nothing behind it: before the
        // signal, or after it.
        let mut rx = rx();
        let mut a = sine(300.0, 8.0);
        a.extend(black(40.0));
        let ev = feed(&mut rx, &a);
        assert!(ev.is_empty(), "a bare carrier produced {ev:?}");
        assert!(!rx.receiving());

        // The same after a finished page.
        let mut rx = rx_after_page();
        let mut a = sine(300.0, 8.0);
        a.extend(black(40.0));
        let ev = feed(&mut rx, &a);
        assert!(ev.is_empty(), "a carrier after the page produced {ev:?}");
    }

    fn rx_after_page() -> WefaxRx {
        let mut rx = rx();
        feed(&mut rx, &page(300.0, 60));
        rx
    }

    #[test]
    fn tones_that_are_not_the_start_tone_start_nothing() {
        for hz in [450.0, 150.0, 600.0, 1900.0, 2300.0] {
            let mut rx = rx();
            let ev = feed(&mut rx, &sine(hz, 8.0));
            assert!(ev.is_empty(), "{hz} Hz produced {ev:?}");
        }
    }

    #[test]
    fn a_tone_buried_in_noise_does_not_start_a_page() {
        let mut rx = rx();
        let mut seed = 0x1234_5678u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let a: Vec<f32> = sine(300.0, 8.0).iter().map(|s| 0.1 * s + 0.6 * noise()).collect();
        let ev = feed(&mut rx, &a);
        assert!(ev.is_empty(), "{ev:?}");
    }

    /// The last verdict the decoder's own tone detector reaches on a second of
    /// `hz` at `sensitivity`, asked through `set_sensitivity` so the test drives
    /// the same path the slider does.
    fn verdict(hz: f64, sensitivity: i8) -> AudioTone {
        let mut rx = rx();
        rx.set_sensitivity(sensitivity);
        let mut last = AudioTone::Other;
        for i in 0..RATE as usize {
            let a = (0.5 * (TAU * hz * i as f64 / RATE).sin()) as f32;
            if let Some(v) = rx.audio_tone.push(a) {
                last = v;
            }
        }
        last
    }

    /// `+` listens to a fainter, mistuned tone and `−` will not.
    ///
    /// 210 Hz is the one tone inside the detector's band (`200..520`) that is
    /// further than the reference 75 Hz from both the 300 Hz start pitch and the
    /// 450 Hz stop pitch — 225 would be inside the start window and 375 inside
    /// both — so it is the single tone whose verdict the setting decides.
    #[test]
    fn the_sensitivity_setting_widens_the_audio_tone_window() {
        assert_eq!(verdict(210.0, 0), AudioTone::Other, "210 Hz was a tone at the reference");
        assert_eq!(
            verdict(210.0, 5),
            AudioTone::Start,
            "+5 did not reach a tone 90 Hz off the start pitch"
        );
        assert_eq!(verdict(210.0, -5), AudioTone::Other, "−5 took a tone 90 Hz off");
    }
}

/// A carrier on the channel, a signal that has drifted, noise that looks like a
/// stop tone: what a real recording does to the receiver and a clean synthetic
/// one does not. The recorded versions are in `tests/wefax_real_recording.rs`.
#[cfg(test)]
mod carrier_tests {
    use super::*;

    const RATE: f64 = 12_000.0;

    fn fm(levels: &[f32], black: f64, white: f64) -> Vec<f32> {
        let mut ph = 0.0f64;
        levels
            .iter()
            .map(|&l| {
                ph += TAU * (black + (white - black) * l as f64) / RATE;
                if ph > TAU {
                    ph -= TAU;
                }
                (ph.sin() * 0.5) as f32
            })
            .collect()
    }

    fn carrier(hz: f64, secs: f64) -> Vec<f32> {
        (0..(RATE * secs) as usize)
            .map(|i| (TAU * hz * i as f64 / RATE).sin() as f32 * 0.5)
            .collect()
    }

    fn per() -> usize {
        (RATE * Lpm::L120.line_secs()) as usize
    }

    /// `count` phasing lines with a pulse `width` of a line wide starting at
    /// `at` of it, scattered like a weak signal's: inside the pulse each sample
    /// is white only `density` of the time, from a fixed pseudo-random stream.
    fn phasing(count: usize, at: f64, width: f64, density: f32) -> Vec<f32> {
        let n = per();
        let (a, w) = ((n as f64 * at) as usize, (n as f64 * width) as usize);
        let mut seed = 0x2545_f491u32;
        let mut levels = Vec::new();
        for _ in 0..count {
            for i in 0..n {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let r = (seed >> 8) as f32 / (1u32 << 24) as f32;
                // Circular, so a pulse centred on the line boundary wraps.
                let inside = (i + n - a) % n < w;
                levels.push(if inside && r < density { 1.0 } else { 0.0 });
            }
        }
        levels
    }

    fn ramp(lines: usize) -> Vec<f32> {
        let n = per();
        (0..lines * n).map(|i| (i % n) as f32 / n as f32).collect()
    }

    fn feed(rx: &mut WefaxRx, audio: &[f32]) -> Vec<WefaxEvent> {
        let mut out = Vec::new();
        for chunk in audio.chunks(1024) {
            rx.process(chunk, &mut out);
        }
        out
    }

    fn lines(ev: &[WefaxEvent]) -> Vec<(u16, Vec<u8>)> {
        ev.iter()
            .filter_map(|e| match e {
                WefaxEvent::Line { y, gray } => Some((*y, gray.clone())),
                _ => None,
            })
            .collect()
    }

    /// Whatever its frequency — above white, on it, in the middle of the scale
    /// or at black — a steady carrier is not a transmission starting.
    #[test]
    fn a_carrier_alone_starts_nothing() {
        for hz in [2328.0, 2300.0, 2100.0, 1900.0, 1500.0] {
            let mut rx = WefaxRx::new(RATE);
            let ev = feed(&mut rx, &carrier(hz, 60.0));
            assert!(ev.is_empty(), "a carrier at {hz} Hz started something: {ev:?}");
        }
    }

    /// The start tone can be lost under a carrier or a fade; the phasing signal
    /// behind it is what is left, and a page begins from that alone — with the
    /// rest of the signal left out of the picture.
    #[test]
    fn the_phasing_signal_alone_begins_a_page() {
        let mut rx = WefaxRx::new(RATE);
        let mut audio = fm(&phasing(12, 0.975, 0.05, 1.0), BLACK_HZ, WHITE_HZ);
        audio.extend(fm(&ramp(20), BLACK_HZ, WHITE_HZ));
        let ev = feed(&mut rx, &audio);
        assert!(ev.iter().any(|e| matches!(e, WefaxEvent::Start { auto: true, .. })), "{ev:?}");
        let l = lines(&ev);
        assert!(l.len() >= 15, "only {} lines", l.len());
        // The first row is the ramp, not a black phasing line.
        let mean = l[0].1.iter().map(|&g| f64::from(g)).sum::<f64>() / l[0].1.len() as f64 / 255.0;
        assert!(mean > 0.3, "the page opens on the phasing signal: {mean:.2}");
    }

    /// The rest of a phasing signal is told from the chart by where its pulse
    /// sits in the lines cut after the lock, and the lock finds it wherever it
    /// fell in the line it locked on — here 40 % of the way along, not at the
    /// end. The dropped lines must be judged against the pulse as the aligned
    /// lines show it, or none of them is taken for phasing and all fifty are
    /// drawn as a black band (the DWD page of 2026-10-31).
    #[test]
    fn the_phasing_signal_is_dropped_whatever_part_of_the_line_its_pulse_fell_in() {
        let mut rx = WefaxRx::new(RATE);
        let mut audio = fm(&phasing(60, 0.40, 0.05, 1.0), BLACK_HZ, WHITE_HZ);
        audio.extend(fm(&ramp(20), BLACK_HZ, WHITE_HZ));
        let ev = feed(&mut rx, &audio);
        let l = lines(&ev);
        assert!(l.len() >= 15, "only {} lines", l.len());
        let mean = l[0].1.iter().map(|&g| f64::from(g)).sum::<f64>() / l[0].1.len() as f64 / 255.0;
        assert!(mean > 0.3, "the page opens on the phasing signal: {mean:.2}");
    }

    /// 400 lines (480-879) of the DWD 3855 page of 2026-11-12, cut on a trim of
    /// +32 ppm, one bit a pixel (1 is ink). Its frame and the box at the foot
    /// walk left by 27 ppm of the line: the clock wants a trim of about +5.
    fn real_lean_rows() -> Vec<Vec<u8>> {
        let path = format!("{}/tests/testdata/wefax/lean_rows.bits", env!("CARGO_MANIFEST_DIR"));
        let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let width = usize::from(data[0]) | usize::from(data[1]) << 8;
        let rows = usize::from(data[2]) | usize::from(data[3]) << 8;
        let bit = |i: usize| (data[4 + i / 8] >> (7 - i % 8)) & 1;
        (0..rows)
            .map(|y| (0..width).map(|x| if bit(y * width + x) == 1 { 0u8 } else { 255 }).collect())
            .collect()
    }

    /// **The lean of a real page is read to a few parts per million.** The
    /// correlation of one line with the one eight on, window by window, read
    /// -60, -23, -144, +53, +124, -48 ppm over the same kind of page; the
    /// columns' sharpness over a stretch of 400 lines reads -27.
    #[test]
    fn the_lean_of_a_real_page_is_read_from_its_columns() {
        let (ppm, sharpness) = image_lean(&real_lean_rows()).expect("no lean read");
        assert!((ppm + 27.0).abs() <= 10.0, "read {ppm:+.0} ppm, the page leans -27");
        assert!(sharpness >= LEAN_CONFIDENCE, "sharpness {sharpness:.2}");
    }

    /// **A stretch whose halves disagree is not believed.** The first 160 of
    /// those lines, taken alone, read -186 ppm sharply enough to pass, and the
    /// page leans -27: a meridian's own slope, not the clock. Its two halves
    /// do not say the same, and nothing is read.
    #[test]
    fn a_stretch_whose_halves_disagree_reads_nothing() {
        assert_eq!(image_lean(&real_lean_rows()[..160]), None);
    }

    /// A straight page is left as it is: the bars do not move, whatever else
    /// crosses them.
    #[test]
    fn a_straight_page_is_read_as_straight() {
        let width = Ioc::I576.width() as usize;
        let rows: Vec<Vec<u8>> = (0..240)
            .map(|y| {
                let mut row = vec![255u8; width];
                for x in [120, 700, 1500] {
                    row[x..x + 4].fill(0);
                }
                // A meridian: a line walking 0.25 px a line (138 ppm) of its own.
                let m = (900.0 + 0.25 * y as f64) as usize;
                row[m..m + 3].fill(0);
                row
            })
            .collect();
        let (ppm, _) = image_lean(&rows).expect("no lean read");
        assert!(ppm.abs() <= 10.0, "a straight page with a meridian read {ppm:+.0} ppm");
    }

    /// A page whose frame walks sideways line after line is straightened by
    /// the picture itself, a window at a time; one whose walk is only texture,
    /// first one way and then the other, is left alone.
    #[test]
    fn the_picture_straightens_a_slant_that_stays_and_ignores_one_that_does_not() {
        let width = Ioc::I576.width() as usize;
        // Two black bars, the position of each moved by `walk` pixels a line.
        let rows = |walk: &dyn Fn(usize) -> f64, count: usize| -> Vec<Vec<u8>> {
            (0..count)
                .map(|y| {
                    let mut row = vec![255u8; width];
                    for base in [300.0, 1100.0] {
                        let x = (base + walk(y)).round() as usize;
                        row[x..x + 4].fill(0);
                    }
                    row
                })
                .collect()
        };
        // 0.4 px a line to the right is 221 ppm of 1809 px: lines cut short.
        let mut rx = WefaxRx::new(RATE);
        for row in rows(&|y| 0.4 * y as f64, 400) {
            rx.image_slant(&row);
        }
        assert!(
            rx.slant_ppm() > 150.0 && rx.slant_ppm() < 700.0,
            "the walk was not taken off, or too much of it: {}",
            rx.slant_ppm()
        );
        // The same walk, sign flipped every window: the chart's texture.
        let mut rx = WefaxRx::new(RATE);
        for row in rows(&|y| 0.4 * (y % 56) as f64 * if (y / 56) % 2 == 0 { 1.0 } else { -1.0 }, 400) {
            rx.image_slant(&row);
        }
        assert!(rx.slant_ppm().abs() < 1.0, "a walk that changes sign was followed: {}", rx.slant_ppm());
        // The operator's revert keeps it off.
        let mut rx = WefaxRx::new(RATE);
        rx.revert_slant();
        for row in rows(&|y| 0.4 * y as f64, 400) {
            rx.image_slant(&row);
        }
        assert_eq!(rx.slant_ppm(), 0.0);
    }

    /// A stretch of lost pixels beside the black edge of a frame is not drawn as
    /// a black dash running out of that edge: the line above says what is there.
    #[test]
    fn lost_pixels_beside_a_black_edge_are_not_drawn_as_a_dash() {
        let mut rx = WefaxRx::new(RATE);
        rx.pic_mag = 1.0;
        let width = rx.ioc.width() as usize;
        let weight = rx.samples_per_line() as f32 / width as f32;
        let step = |hz: f64, m: f32| {
            Complex32::from_polar(m, ((hz - CENTRE_HZ) * TAU / RATE) as f32)
        };
        let mut sums = vec![(step(WHITE_HZ, weight), weight); width];
        sums[30] = (step(BLACK_HZ, weight), weight);
        for s in &mut sums[31..46] {
            *s = (Complex32::default(), 0.0);
        }
        rx.last_line = vec![255; width];
        let (gray, _) = rx.shade(&sums);
        assert!(gray[30] < 40, "the edge itself stays black: {}", gray[30]);
        for (x, &g) in gray.iter().enumerate().take(46).skip(31) {
            assert!(g > 200, "pixel {x} is {g}: the edge was drawn out into the gap");
        }
    }

    /// A burst of noise that wipes out a stretch of a line must not wipe out
    /// the vertical stroke it crosses: the two good sides are white, the line
    /// above has the stroke, and the stroke stays. (Drawn from the sides alone
    /// it was a white streak through every stroke the burst crossed.) A gap of a
    /// few pixels is still drawn from its sides.
    #[test]
    fn a_wide_burst_does_not_cut_the_strokes_it_crosses() {
        let mut rx = WefaxRx::new(RATE);
        rx.pic_mag = 1.0;
        let width = rx.ioc.width() as usize;
        let weight = rx.samples_per_line() as f32 / width as f32;
        let white =
            (Complex32::from_polar(weight, ((WHITE_HZ - CENTRE_HZ) * TAU / RATE) as f32), weight);
        let mut sums = vec![white; width];
        for s in &mut sums[95..110] {
            *s = (Complex32::default(), 0.0);
        }
        // One lost pixel beside the stroke of the next gap, for the short case.
        sums[300] = (Complex32::default(), 0.0);
        rx.last_line = vec![255; width];
        for x in 100..103 {
            rx.last_line[x] = 0;
        }
        rx.last_line[300] = 0;
        let (gray, _) = rx.shade(&sums);
        for (x, &g) in gray.iter().enumerate().take(103).skip(100) {
            assert!(g < 60, "pixel {x} is {g}: the stroke was cut by the burst");
        }
        assert!(gray[96] > 200 && gray[108] > 200, "the rest of the gap stays white");
        assert!(gray[300] > 200, "a single lost pixel is drawn from its sides: {}", gray[300]);
    }

    /// A line cut from a lock begins at the middle of the pulse, so the pulse of
    /// the next phasing line is centred on that line's end: its region is
    /// [`PulseRegion::picture_start`] of itself, one line on.
    #[test]
    fn the_pulse_of_a_cut_line_is_centred_on_the_line_boundary() {
        let rx = WefaxRx::new(RATE);
        let n = (rx.samples_per_line().round() as usize).max(1);
        for width in [20usize, 21, 600, 601] {
            let region = rx.aligned_pulse(PulseRegion { start: 1000, width });
            assert_eq!(region.width, width);
            assert_eq!(region.picture_start(n) as usize, 0, "width {width}: {region:?}");
        }
    }

    /// A weak signal's pulse is a scatter of blips a fifth of a line wide, not a
    /// clean five per cent. It still locks, and the picture is placed by the
    /// pulse's middle.
    #[test]
    fn a_wide_noisy_pulse_is_found_and_the_picture_placed_by_its_middle() {
        let mut rx = WefaxRx::new(RATE);
        let audio = fm(&phasing(14, 0.10, 0.25, 0.6), BLACK_HZ, WHITE_HZ);
        let ev = feed(&mut rx, &audio);
        let skew = ev
            .iter()
            .find_map(|e| match e {
                WefaxEvent::Phased { skew } => Some(*skew),
                _ => None,
            })
            .unwrap_or_else(|| panic!("a wide noisy pulse was not found: {ev:?}"));
        // The pulse runs from 0.10 to 0.35 of the line: its middle is 0.225 and
        // the picture starts there.
        let want = 0.225 * per() as f64;
        assert!((f64::from(skew) - want).abs() < 0.03 * per() as f64, "skew {skew}, wanted {want}");
    }

    /// A transmission that has drifted — a receiver a hundred hertz off tune, or
    /// the path's Doppler — puts black and white somewhere other than 1500 and
    /// 2300 Hz. The phasing signal says where, and the chart is read against it.
    #[test]
    fn black_and_white_follow_a_subcarrier_that_has_moved() {
        for shift in [-100.0, 100.0] {
            let (black, white) = (BLACK_HZ + shift, WHITE_HZ + shift);
            let mut rx = WefaxRx::new(RATE);
            let mut audio = fm(&phasing(10, 0.975, 0.05, 1.0), black, white);
            audio.extend(fm(&ramp(12), black, white));
            let l = lines(&feed(&mut rx, &audio));
            assert!(l.len() >= 6, "shift {shift}: only {} lines", l.len());
            let row = &l[l.len() / 2].1;
            let (lo, hi) = (row[10], row[row.len() - 10]);
            assert!(lo < 12 && hi > 243, "shift {shift}: a black-to-white ramp read {lo}..{hi}");
        }
    }

    /// Discriminator noise on a weak chart crosses mid-grey as often as a stop
    /// tone does, and in a real recording ended the page that way. A stop tone
    /// is *periodic*; noise at its rate is not.
    #[test]
    fn noise_at_the_stop_tones_rate_is_not_a_stop_tone() {
        let mut rx = WefaxRx::new(RATE);
        let mut out = Vec::new();
        rx.start_manual(&mut out);
        let mut audio = fm(&phasing(8, 0.975, 0.05, 1.0), BLACK_HZ, WHITE_HZ);
        audio.extend(fm(&ramp(20), BLACK_HZ, WHITE_HZ));
        // Ten seconds of random black and white, a symbol every seven samples
        // (1714 a second): a crossing every other symbol, which is the stop
        // tone's own count of nine hundred a second give or take.
        let mut seed = 7u32;
        let symbol = 7;
        let mut noise = Vec::new();
        while noise.len() < (RATE * 10.0) as usize {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let level = if seed >> 31 == 0 { 0.0 } else { 1.0 };
            noise.extend(std::iter::repeat_n(level, symbol));
        }
        audio.extend(fm(&noise, BLACK_HZ, WHITE_HZ));
        let ev = feed(&mut rx, &audio);
        assert!(
            !ev.iter().any(|e| matches!(e, WefaxEvent::Complete { .. })),
            "noise was taken for a stop tone"
        );
    }

    /// A carrier left on after the chart is blank wherever it sits. Only one
    /// above white read as white, so one that had drifted down into the scale
    /// kept the page open for good.
    #[test]
    fn a_carrier_below_white_still_ends_an_automatic_page() {
        let mut rx = WefaxRx::new(RATE);
        let mut audio = fm(&phasing(12, 0.975, 0.05, 1.0), BLACK_HZ, WHITE_HZ);
        audio.extend(fm(&ramp(30), BLACK_HZ, WHITE_HZ));
        audio.extend(carrier(2100.0, 150.0));
        let ev = feed(&mut rx, &audio);
        assert!(
            ev.iter().any(|e| matches!(e, WefaxEvent::Complete { by_tone: false, .. })),
            "a steady carrier at 2100 Hz did not end the page"
        );
    }

    /// The few seconds of phasing-like lines a chart ends on are not the start
    /// of another transmission, and nor is anything right after a page.
    #[test]
    fn nothing_begins_a_page_straight_after_one_ends() {
        let mut rx = WefaxRx::new(RATE);
        let mut audio = fm(&phasing(12, 0.975, 0.05, 1.0), BLACK_HZ, WHITE_HZ);
        audio.extend(fm(&ramp(20), BLACK_HZ, WHITE_HZ));
        let first = feed(&mut rx, &audio);
        // Stop it by hand, then phasing at once.
        let mut out = Vec::new();
        rx.stop_manual(&mut out);
        assert!(out.iter().any(|e| matches!(e, WefaxEvent::Complete { .. })), "{first:?}");
        let ev = feed(&mut rx, &fm(&phasing(8, 0.975, 0.05, 1.0), BLACK_HZ, WHITE_HZ));
        assert!(
            !ev.iter().any(|e| matches!(e, WefaxEvent::Start { .. })),
            "phasing straight after a page began another: {ev:?}"
        );
    }

/// A thick black bar whose bright patch is narrow but wider than a real
/// phasing pulse must not be read as phasing, even though it carries only
/// one bright region (so `bright_outside` does not catch it). The pulse
/// width cap is what closes this (#444).
#[test]
fn a_thick_black_bar_with_a_narrow_border_is_not_phasing() {
    let n = 6000;
    let pulse = n / 20; // 5 %, the standard phasing pulse width
    // A real phasing line still passes.
    let mut phasing = vec![0.0f32; n];
    for v in phasing.iter_mut().skip(n - pulse) {
        *v = 1.0;
    }
    assert!(
        phasing_skew_strict(&phasing).is_some(),
        "a real phasing line must still pass the strict test"
    );

    // A thick black bar with a single bright border ~15 % wide: one bright
    // region, so bright_outside is zero, but the patch is three times the
    // standard pulse width.
    let border = pulse * 3;
    let mut bar = vec![0.0f32; n];
    for v in bar.iter_mut().skip(n - border) {
        *v = 1.0;
    }
    assert_eq!(
        phasing_skew_strict(&bar),
        None,
        "a thick black bar with a narrow border must not be read as phasing"
    );
}


    /// A page of vertical rules, a frame and some text-like blocks, cut on a
    /// clock `lean_ppm` out: every row's content has walked `lean_ppm * width`
    /// pixels more than the row above, with a pixel of wobble on each.
    fn leaning_page(width: usize, height: usize, lean_ppm: f64) -> Vec<u8> {
        let mut img = vec![255u8; width * height];
        let mut seed = 11u32;
        for y in 0..height {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let wobble = f64::from(seed >> 30) / 3.0 - 0.5;
            let d = lean_ppm * 1e-6 * width as f64 * y as f64 + wobble;
            for x in 0..width {
                let u = (x as f64 - d).rem_euclid(width as f64);
                let frame = (u - 200.0).abs() < 4.0 || (u - 1600.0).abs() < 4.0;
                let rule = [520.0, 900.0, 1250.0].iter().any(|r| (u - r).abs() < 2.0);
                let text = y % 90 < 14 && (u as usize / 7) % 3 == 0 && u > 300.0 && u < 1500.0;
                if frame || rule || text {
                    img[y * width + x] = 0;
                }
            }
        }
        img
    }

    /// Column of the left frame, averaged over `rows` rows from `y`.
    fn frame_at(img: &[u8], width: usize, y: usize, rows: usize) -> f64 {
        let (mut sum, mut n) = (0.0, 0.0);
        for r in y..y + rows {
            for x in 100..300 {
                let w = f64::from(255 - img[r * width + x]);
                sum += w * x as f64;
                n += w;
            }
        }
        sum / n
    }

    #[test]
    fn a_leaning_page_comes_out_with_its_frame_straight() {
        for lean in [9.0, -7.0, 25.0] {
            let (w, h) = (1809, 1300);
            let mut img = leaning_page(w, h, lean);
            let before = frame_at(&img, w, 1100, 50) - frame_at(&img, w, 100, 50);
            let done = straighten_page(&mut img, w, h)
                .unwrap_or_else(|| panic!("a page leaning {lean} ppm was left alone"));
            assert!((done.lean_ppm - lean).abs() < 2.0, "lean {lean} read as {}", done.lean_ppm);
            let after = frame_at(&img, w, 1100, 50) - frame_at(&img, w, 100, 50);
            assert!(before.abs() > 10.0, "the fixture does not lean ({before:.1} px)");
            assert!(after.abs() < 2.0, "lean {lean}: the frame still walks {after:.1} px");
        }
    }

    #[test]
    fn a_straight_page_and_a_blank_one_are_left_alone() {
        let (w, h) = (1809, 1300);
        let mut img = leaning_page(w, h, 0.0);
        let before = img.clone();
        let moved = straighten_page(&mut img, w, h);
        assert!(moved.is_none(), "a straight page was moved: {moved:?}");
        assert_eq!(img, before);
        let mut blank = vec![255u8; w * h];
        assert_eq!(straighten_page(&mut blank, w, h), None);
        // And a page too short to say anything.
        let mut short = leaning_page(w, 300, 20.0);
        assert_eq!(straighten_page(&mut short, w, 300), None);
    }
}
