//! A committed probe, not scratch: connect to SondeHub's live feed the way the
//! station does and print what arrives.
//!
//! The feed's MQTT-over-websocket path is the one thing in `sonde_live.rs` that
//! no unit test can settle — the packets are checked offline, but "does the far
//! end answer our CONNECT and send `batch`" is a question for their server. This
//! runs the same code the station runs, for a few seconds, and says what it
//! heard.
//!
//! ```text
//! cargo run --release -p sdroxide-server --example sonde_live_probe
//! ```

use std::time::{Duration, Instant};

fn main() -> Result<(), String> {
    // What the station logs, so a refused connection says why.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let live = std::sync::Arc::new(sdroxide_server::sonde_live_probe::Live::default());
        live.touch();
        live.start();
        let started = Instant::now();
        // One batch a second or so on the real feed; ten seconds is plenty.
        while started.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let heard = sdroxide_server::sonde_live_probe::held(&live);
            println!("{:>2}s  sondes held: {heard}", started.elapsed().as_secs());
            if heard > 0 {
                for (name, lat, lon) in
                    [("52N 4E (NL)", 52.0, 4.0), ("45N 8E (IT)", 45.0, 8.0), ("40N 75W (US)", 40.0, -75.0)]
                {
                    let near = live.answer(lat, lon, Instant::now());
                    let count = near
                        .map(|b| b.matches("\"serial\"").count())
                        .unwrap_or(0);
                    println!("      near {name}: {count} sonde(s)");
                }
            }
        }
        Ok(())
    })
}
