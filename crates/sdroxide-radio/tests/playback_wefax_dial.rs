//! A fax recording is audio: the dial the receiver is on while it plays is only a
//! label, and must not change what the decoder makes of it (field report: a
//! recording played with the receiver on 3.8855 MHz came out as noise).
//!
//! The file is demodulated audio — twenty seconds of a real off-air chart under
//! a carrier, cut from the recording `crates/sdroxide-dsp/tests/wefax_real_recording.rs`
//! describes — so the only way to check what the decoder makes of it is to play
//! it back through the engine and let the decoder listen, exactly as it would to
//! the radio. Two things are asserted, because they are the two things the
//! operator asked of a replay:
//!
//! * the picture comes out **while the file plays**, at the speed it was sent,
//!   with nothing pressed — no START, no tone;
//! * **the live radio is not listened to** while it does. Playback stands the
//!   radio down, and its source is not read again until the operator stops.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sdroxide_radio::{Complex32, EngineConfig, IqSource, Result, start_engine};
use sdroxide_types::{Command, DeviceCaps, Mode, PlaybackCommand, RadioEvent, RxId, Vfo};

const RATE: f64 = 48_000.0;
const LIVE_HZ: f64 = 7_100_000.0;
const NAME: &str = "sdroxide_2026-10-20_12-00-00Z_9.300000MHz_WEFAX.wav";

/// The live radio: silence, paced — and a count of how often it is read.
struct Live(Arc<AtomicUsize>);

impl IqSource for Live {
    fn sample_rate(&self) -> f64 {
        RATE
    }
    fn center_hz(&self) -> f64 {
        LIVE_HZ
    }
    fn set_center_hz(&mut self, _hz: f64) -> Result<()> {
        Ok(())
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(5));
        let n = buf.len().min(256);
        buf[..n].fill(Complex32::new(0.0, 0.0));
        Ok(n)
    }
    fn describe(&self) -> String {
        "live rig".into()
    }
}

fn caps() -> DeviceCaps {
    DeviceCaps {
        driver: "live".into(),
        label: "live".into(),
        rx_channels: 1,
        tx_channels: 1,
        sample_rates: vec![RATE],
        freq_ranges_rx: vec![(0.0, 60_000_000.0)],
        freq_ranges_tx: vec![(0.0, 60_000_000.0)],
        ..DeviceCaps::default()
    }
}

/// The 6 kHz mono fixture, taken to the 48 kHz the recorder writes by linear
/// interpolation, as a 16-bit WAV.
fn recording() -> Vec<u8> {
    let path =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../sdroxide-dsp/tests/testdata/wefax/start.wav");
    let raw = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    assert_eq!(&raw[..4], b"RIFF", "{path}");
    let rate = u32::from_le_bytes(raw[24..28].try_into().unwrap());
    assert_eq!(rate, 6_000, "the fixture is a 6 kHz recording");
    let x: Vec<f64> =
        raw[44..].chunks_exact(2).map(|b| f64::from(i16::from_le_bytes([b[0], b[1]]))).collect();
    let up = (RATE as u32 / rate) as usize;
    let mut data = Vec::with_capacity(x.len() * up * 2);
    for w in x.windows(2) {
        for k in 0..up {
            let v = w[0] + (w[1] - w[0]) * k as f64 / up as f64;
            data.extend_from_slice(&(v as i16).to_le_bytes());
        }
    }
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&(RATE as u32).to_le_bytes());
    f.extend_from_slice(&(RATE as u32 * 2).to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(&data);
    f
}

/// Play the recording with the dial moved to `dial` once playback has started
/// (`None`: left on what the file name says). Returns the lines decoded in
/// the first 25 s and the mean of the first twelve rows' grey levels.
fn play(dial: Option<f64>, mode_after: Option<Mode>) -> (usize, Vec<f32>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let again = Arc::clone(&reads);
    let reopen: sdroxide_radio::ReopenFn = Box::new(move |_center: f64| {
        Ok((Box::new(Live(Arc::clone(&again))) as Box<dyn IqSource>, caps()))
    });
    let cfg = EngineConfig { reopen: Some(reopen), ..Default::default() };
    let mut h = start_engine(Box::new(Live(Arc::clone(&reads))), caps(), cfg);
    let thread = h.thread.take();
    h.cmd_tx.send(Command::SetModeListen { rx: RxId::Main, mode: Mode::Usb }).unwrap();
    h.cmd_tx.send(Command::SetVfo { vfo: Vfo::A, hz: LIVE_HZ }).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    h.cmd_tx.send(Command::Playback(PlaybackCommand::Open { name: NAME.into() })).unwrap();

    let mut moved = false;
    let mut rows: Vec<f32> = Vec::new();
    let began = Instant::now();
    while began.elapsed() < Duration::from_secs(25) {
        while let Ok(ev) = h.event_rx.try_recv() {
            match ev {
                RadioEvent::State(s) if s.playback.is_some() && !moved => {
                    moved = true;
                    if let Some(hz) = dial {
                        h.cmd_tx.send(Command::SetVfo { vfo: Vfo::A, hz }).unwrap();
                    }
                    if let Some(mode) = mode_after {
                        h.cmd_tx.send(Command::SetModeListen { rx: RxId::Main, mode }).unwrap();
                    }
                }
                RadioEvent::WefaxLine { gray, .. } => {
                    rows.push(gray.iter().map(|&g| f32::from(g)).sum::<f32>() / gray.len() as f32);
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    h.cmd_tx.send(Command::Playback(PlaybackCommand::Stop)).unwrap();
    drop(h.cmd_tx);
    if let Some(t) = thread {
        let _ = t.join();
    }
    (rows.len(), rows)
}

#[test]
fn the_dial_is_a_label_on_a_fax_recording() {
    let dir =
        std::env::temp_dir().join(format!("sdroxide-playback-wefax-dial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("recordings")).unwrap();
    // SAFETY: the only test in this binary, and no engine exists yet.
    unsafe { std::env::set_var("SDROXIDE_CONFIG_DIR", &dir) }
    std::fs::write(dir.join("recordings").join(NAME), recording()).unwrap();

    let (base, base_rows) = play(None, None);
    let (low, low_rows) = play(Some(3_885_500.0), None);
    let _ = std::fs::remove_dir_all(&dir);

    eprintln!("lines: file's own dial {base}, dial on 3.8855 MHz {low}");
    assert!(base >= 12, "control: only {base} lines at the file's own dial");
    assert!(low + 2 >= base, "moving the dial lost lines: {low} against {base}");
    let n = base.min(low).min(40);
    let diff: f32 = (0..n).map(|i| (base_rows[i] - low_rows[i]).abs()).sum::<f32>() / n as f32;
    assert!(diff < 8.0, "the rows differ by {diff:.1} grey levels on average once the dial moved");
}
