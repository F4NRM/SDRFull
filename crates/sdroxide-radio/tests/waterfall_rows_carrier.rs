//! A carrier in the band must be visible in the waterfall rows, not only in
//! the trace (issue #559).
//!
//! The trace is the running average; the rows are a per-bin peak held between
//! two rows. One report had a carrier on the trace and a waterfall of flat
//! horizontal bands. This drives the engine at the reporter's 960 kS/s with
//! noise and one tone, once as complex IQ and once as a real signal (what an
//! RTL-SDR in direct sampling hands over), and asks every row for the tone.

use std::time::{Duration, Instant};

use sdroxide_radio::{Complex32, EngineConfig, EngineHandles, IqSource, Result, start_engine};
use sdroxide_types::{Command, DeviceCaps, SpectrumConfig, SpectrumFrame};

const RATE: f64 = 960_000.0;
const CENTER: f64 = 15_000_000.0;
/// Tone offset from the centre, inside the span.
const TONE_HZ: f64 = 100_000.0;

struct Band {
    center_hz: f64,
    seed: u32,
    phase: f64,
    real: bool,
}

impl Band {
    fn noise(&mut self) -> f32 {
        self.seed = self.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.seed >> 8) as f32 / (1 << 23) as f32 - 1.0
    }
}

impl IqSource for Band {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        self.center_hz
    }
    fn set_center_hz(&mut self, hz: f64) -> Result<()> {
        self.center_hz = hz;
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        std::thread::sleep(Duration::from_millis(4));
        let n = buf.len().min(8192);
        let step = std::f64::consts::TAU * TONE_HZ / RATE;
        for s in buf[..n].iter_mut() {
            self.phase = (self.phase + step) % std::f64::consts::TAU;
            let (sin, cos) = self.phase.sin_cos();
            let (nr, ni) = (self.noise() * 0.3, self.noise() * 0.3);
            *s = if self.real {
                // Real signal on the Q branch, nothing on I.
                Complex32::new(0.0, nr + 0.1 * cos as f32)
            } else {
                Complex32::new(nr + 0.1 * cos as f32, ni + 0.1 * sin as f32)
            };
        }
        Ok(n)
    }
    fn describe(&self) -> String {
        "noise and a tone".into()
    }
}

fn caps() -> DeviceCaps {
    DeviceCaps {
        driver: "test".into(),
        label: "test".into(),
        rx_channels: 1,
        sample_rates: vec![RATE],
        freq_ranges_rx: vec![(500_000.0, 30_000_000.0)],
        ..DeviceCaps::default()
    }
}

fn cfg() -> Command {
    Command::SetSpectrumCfg(SpectrumConfig {
        fft_size: 4096,
        display_bins: 2048,
        rows_per_sec: 10,
        db_floor: -140.0,
        db_ceil: 0.0,
        viewport: Some((CENTER - RATE / 2.0, CENTER + RATE / 2.0)),
        fps: 30,
        avg_tc: 0.0,
    })
}

/// How far the tone stands above the row's median, in `u8` steps, taking the
/// best of a few columns either side of where the tone should be.
fn lift(f: &SpectrumFrame, row: &[u8]) -> i32 {
    let cols = row.len();
    let lo = f.center_hz - f.span_hz / 2.0;
    let at = ((CENTER + TONE_HZ - lo) / f.span_hz * cols as f64) as isize;
    let mut sorted = row.to_vec();
    sorted.sort_unstable();
    let median = i32::from(sorted[cols / 2]);
    let peak = (at - 4..=at + 4)
        .filter(|&i| i >= 0 && (i as usize) < cols)
        .map(|i| i32::from(row[i as usize]))
        .max()
        .unwrap_or(0);
    peak - median
}

fn run(real: bool) -> (Vec<i32>, Vec<i32>) {
    let mut h: EngineHandles = start_engine(
        Box::new(Band { center_hz: CENTER, seed: 4242, phase: 0.0, real }),
        caps(),
        EngineConfig { remember_session: false, ..Default::default() },
    );
    let thread = h.thread.take();
    h.cmd_tx.send(cfg()).unwrap();
    let (mut trace, mut rows) = (Vec::new(), Vec::new());
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs_f64(4.0) {
        if h.spectrum_out.update() && start.elapsed() > Duration::from_secs_f64(1.5) {
            let f = h.spectrum_out.output_buffer();
            let cols = f.bins.len();
            if cols > 0 {
                trace.push(lift(f, &f.bins));
                for row in f.rows.chunks_exact(cols) {
                    rows.push(lift(f, row));
                }
            }
        }
        while h.event_rx.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
    (trace, rows)
}

fn check(real: bool) {
    let (trace, rows) = run(real);
    assert!(!trace.is_empty() && !rows.is_empty(), "no picture came out (rows {})", rows.len());
    // 20 dB at 255 / 140 dB per step is about 36 steps; ask for far less.
    let want = 20;
    let weak_trace = trace.iter().filter(|&&l| l < want).count();
    let weak_rows = rows.iter().filter(|&&l| l < want).count();
    eprintln!(
        "real={real}: trace lifts {:?}..{:?}, {} rows, row lifts {:?}..{:?}",
        trace.iter().min(),
        trace.iter().max(),
        rows.len(),
        rows.iter().min(),
        rows.iter().max()
    );
    assert_eq!(weak_trace, 0, "the trace lost the carrier: {trace:?}");
    assert_eq!(weak_rows, 0, "{weak_rows} of {} rows lost the carrier: {rows:?}", rows.len());
}

#[test]
fn a_complex_carrier_shows_in_every_waterfall_row() {
    check(false);
}

#[test]
fn a_real_carrier_shows_in_every_waterfall_row() {
    check(true);
}
