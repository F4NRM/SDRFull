//! Hardware-free server for tools/web-startup/smoke.mjs. Uses the real HTTP,
//! proxy-prefix, embedded/static asset and WebSocket handlers. No radio engine.
use sdroxide_server::{RadioParams, ServerParams, serve};
use sdroxide_types::SpectrumFrame;

#[tokio::main]
async fn main() {
    assert!(std::env::var_os("SDROXIDE_CONFIG_DIR").is_some(), "use a scratch SDROXIDE_CONFIG_DIR");
    let args: Vec<_> = std::env::args().collect();
    let port: u16 = args.get(1).expect("port").parse().unwrap();
    let web_root = args.get(2).filter(|s| s.as_str() != "--embedded").map(std::path::PathBuf::from);
    let frame = SpectrumFrame {
        seq: 0,
        center_hz: 14_200_000.0,
        span_hz: 1_536_000.0,
        db_floor: -120.0,
        db_ceil: 0.0,
        bins: Vec::new(),
        rows: Vec::new(),
        rows_clocked: false,
    };
    let (_spec_in, spectrum_out) = triple_buffer::triple_buffer(&frame);
    let (_wide_in, wide_spectrum_out) = triple_buffer::triple_buffer(&frame);
    let (cmd_tx, _cmd_rx) = crossbeam_channel::unbounded();
    let (_event_tx, event_rx) = crossbeam_channel::unbounded();
    let (_audio_tx, audio_rx) = rtrb::RingBuffer::<f32>::new(96_000);
    let (mic_tx, _mic_rx) = rtrb::RingBuffer::<f32>::new(48_000);
    serve(ServerParams {
        radios: vec![RadioParams {
            id: 0,
            name: "Startup test".into(),
            cmd_tx,
            event_rx,
            spectrum_out,
            wide_spectrum_out,
            audio_rx,
            mic_tx,
        }],
        bind: "127.0.0.1".into(),
        port,
        web_root,
        access: None,
        probe: None,
        add_radio: None,
        remove_radio: None,
        rename_radio: None,
        radio_power: None,
        aircraft_online: Some(false),
    })
    .await
    .unwrap();
}
