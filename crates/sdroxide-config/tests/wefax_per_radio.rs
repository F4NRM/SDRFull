//! Each radio's weather-fax trim and auto-slant switch are its own.
//!
//! One test per file on purpose: `SDROXIDE_CONFIG_DIR` is process-global, and
//! setting it from a `#[test]` that shares a binary with others would race them.

use sdroxide_config::Store;

/// A sound card on one radio and an IQ stream on another are a few hundred
/// parts per million apart, so one shared trim straightened one and leaned the
/// other. A radio's trim now follows only its own saves, while the operator's
/// shared settings still reach every radio.
#[test]
fn one_radios_slant_trim_does_not_move_another() {
    let root = std::env::temp_dir().join(format!("sdroxide-wefax-radio-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("scratch dir");
    // SAFETY: this is the only test in this binary; nothing races the setter.
    unsafe { std::env::set_var("SDROXIDE_CONFIG_DIR", &root) };

    let (first, second) = (Store::radio(0), Store::radio(1));

    // An installation from before the split: the trim is in the shared file,
    // and radio 0 keeps it. A radio added since starts untrimmed, auto on.
    let mut legacy = sdroxide_config::load_digi_config();
    legacy.wefax_slant_ppm = 40.0;
    sdroxide_config::save_digi_config(&legacy).unwrap();
    assert_eq!(first.load_digi_config().wefax_slant_ppm, 40.0);
    assert_eq!(second.load_digi_config().wefax_slant_ppm, 0.0);
    assert!(second.load_digi_config().wefax_auto_slant);

    // Radio 0 is trimmed by hand, auto-slant off, and the callsign changes.
    let mut cfg = first.load_digi_config();
    cfg.wefax_slant_ppm = -120.0;
    cfg.wefax_auto_slant = false;
    cfg.my_call = "N0CALL".into();
    first.save_digi_config(&cfg).unwrap();

    // Radio 1 hears the callsign and neither the trim nor the switch.
    let seen = second.load_digi_config();
    assert_eq!(seen.my_call, "N0CALL");
    assert_eq!(seen.wefax_slant_ppm, 0.0);
    assert!(seen.wefax_auto_slant);

    // Radio 1 saves a trim of its own: radio 0 keeps its, including through the
    // shared file an older build reads.
    let mut cfg = second.load_digi_config();
    cfg.wefax_slant_ppm = 55.0;
    second.save_digi_config(&cfg).unwrap();
    assert_eq!(second.load_digi_config().wefax_slant_ppm, 55.0);
    assert_eq!(first.load_digi_config().wefax_slant_ppm, -120.0);
    assert!(!first.load_digi_config().wefax_auto_slant);
    assert_eq!(sdroxide_config::load_digi_config().wefax_slant_ppm, -120.0);

    let _ = std::fs::remove_dir_all(&root);
}
