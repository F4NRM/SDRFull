use serde::{Deserialize, Serialize};

/// Fraction of converter samples at full scale above which the front end is
/// called overloaded — see [`Meters::adc_clip`].
///
/// Not zero, because "at full scale" is measured with a shade of margin: the
/// backends disagree on what the top code converts to (0.9922 on a packed 8-bit
/// front end, 0.99688 on an RTL-SDR, 0.99997 on a 16-bit one), so the test has
/// to sit under all of them and a signal that legitimately fills the converter
/// then grazes it on the odd sample. One in two hundred is well clear of that
/// and far below what any genuinely clipped signal produces — the mildest
/// clipped case measured for issue #173 was already at 43 %.
///
/// Lives here rather than beside the meter that fills it because the UI asks
/// this question too, and the UI builds for wasm32 where the DSP crate does not
/// follow.
pub const OVERLOAD_FRACTION: f32 = 0.005;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TxMeters {
    /// Forward power in watts, if the device exposes a sensor for it.
    pub fwd_w: Option<f32>,
    pub swr: Option<f32>,
    /// ALC as `0.0..=1.0`, or `None` when the rig has not reported one.
    ///
    /// [`Option`] rather than the engine's own drive level, because the two are
    /// different claims and only the rig's is the answer to "am I overdriving
    /// it". A rig that never answers an ALC read must read as **not reported**,
    /// not as a confident `0%`: the two look identical on the meter otherwise,
    /// and the operator cannot tell a transmitter that is genuinely barely
    /// moving from one whose meter sdroxide is not reading at all (issue #600,
    /// three Icom LAN users). When nothing reported one this falls back to the
    /// engine's own drive level, which is what that field used to be.
    pub alc: Option<f32>,
    /// The rig's own power-output meter as a `0.0..=1.0` fraction of full
    /// scale, if it has one. Deliberately not watts: see [`TxTelemetry::po`].
    pub po: Option<f32>,
    /// Reflected power in watts, if the device measures it: see
    /// [`TxTelemetry::rfl_w`]. Appended last, as the wire requires.
    #[serde(default)]
    pub rfl_w: Option<f32>,
    /// The drive actually in force on this transmission, as `0.0..=100.0` per
    /// cent of full drive: the operator's Drive (or TUNE) setting after the
    /// band calibration and every ceiling, which is exactly what the engine
    /// hands the modulator or the rig. Filled by the engine on every
    /// transmission, whether or not the radio can measure anything, so a radio
    /// with no power sensor still has something true to show. Appended last, as
    /// the wire requires.
    #[serde(default)]
    pub set_pct: Option<f32>,
    /// That drive turned into an estimate of watts, when the operator has stated
    /// what the station makes on this band at full drive
    /// ([`RadioConfig::tx_band_power_w`](crate::RadioConfig::tx_band_power_w)).
    /// An estimate and nothing more: a rig that commands its own power
    /// (CAT, LAN, TCI) takes drive as a fraction of its rated power, so the
    /// estimate goes as drive; an I/Q radio scales an amplitude, so it goes as
    /// drive squared. `None` where no power is stated for the band. Appended
    /// last, as the wire requires.
    #[serde(default)]
    pub set_w: Option<f32>,
}

/// What a transmission reads as power, from the best source available.
///
/// Measured beats estimated, and the order is the point: a figure a device
/// measured is a claim about the antenna, a drive setting is only a claim about
/// what was asked of the transmitter, and the meter must not present the second
/// as the first. See [`TxMeters::power`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TxPower {
    /// Watts a device measured (`fwd_w`).
    Measured(f32),
    /// The rig's own power meter, `0.0..=1.0` of its scale. Not watts.
    Needle(f32),
    /// Watts estimated from the drive and the band power the operator stated.
    EstimatedW(f32),
    /// The drive in force, per cent of full. All that is known on a band whose
    /// power was never stated.
    EstimatedPct(f32),
    /// Nothing at all: no measurement and no drive figure.
    Unknown,
}

impl TxMeters {
    /// The two estimate fields, `(set_pct, set_w)`, for a drive `level` in
    /// `0.0..=1.0` — the level the engine hands the hardware, calibration and
    /// ceilings already applied.
    ///
    /// `band_w` is what the operator stated for the band at full drive.
    /// `drive_is_power` is the same question [`BandDriveTrim::factor_for`]
    /// asks: a control that is a fraction of the rig's rated power goes as the
    /// level, one that scales the modulated I/Q amplitude goes as its square.
    /// A NaN or out-of-range level is held to the range rather than shown.
    ///
    /// [`BandDriveTrim::factor_for`]: crate::BandDriveTrim::factor_for
    pub fn estimate(level: f32, band_w: Option<f32>, drive_is_power: bool) -> (f32, Option<f32>) {
        let level = if level.is_finite() { level.clamp(0.0, 1.0) } else { 0.0 };
        let share = if drive_is_power { level } else { level * level };
        (level * 100.0, band_w.filter(|w| w.is_finite() && *w > 0.0).map(|w| w * share))
    }

    /// The power of this transmission, best source first: measured watts, the
    /// rig's own power needle, estimated watts, estimated per cent.
    pub fn power(&self) -> TxPower {
        let ok = |v: f32| v.is_finite() && v >= 0.0;
        if let Some(w) = self.fwd_w.filter(|v| ok(*v)) {
            TxPower::Measured(w)
        } else if let Some(p) = self.po.filter(|v| ok(*v)) {
            TxPower::Needle(p.min(1.0))
        } else if let Some(w) = self.set_w.filter(|v| ok(*v)) {
            TxPower::EstimatedW(w)
        } else if let Some(p) = self.set_pct.filter(|v| ok(*v)) {
            TxPower::EstimatedPct(p.min(100.0))
        } else {
            TxPower::Unknown
        }
    }
}

/// TX-side telemetry a rig reports out-of-band (CAT / TCI): forward power,
/// SWR, the rig's own ALC and its power-output meter. Distinct from
/// [`TxMeters`], which also carries the engine's own ALC — this is only what
/// the *device* measures, merged into `TxMeters` by the engine while
/// transmitting.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct TxTelemetry {
    /// Forward power in watts, if the device exposes a sensor for it.
    pub fwd_w: Option<f32>,
    /// SWR as a ratio (e.g. `1.4` = 1.4:1), if the device measures it.
    pub swr: Option<f32>,
    /// ALC as `0.0..=1.0` of the rig's own meter, if it reports one.
    ///
    /// This is the rig saying how hard its automatic level control is working,
    /// which is the number that says whether the audio being fed to it is too
    /// hot. Nothing on this side can compute it: [`TxMeters::alc`] is what
    /// SDRoxide SENDS, and this is what the rig does about it. On a CAT rig the
    /// two are different measurements and only this one is the operator's
    /// answer to "am I overdriving it".
    pub alc: Option<f32>,
    /// The rig's power-output meter as a `0.0..=1.0` fraction of full scale.
    ///
    /// Kept separate from `fwd_w`, and deliberately not converted into it,
    /// because the two are different claims. `fwd_w` is watts a device has
    /// actually measured; this is a needle position. An Icom answers its PO
    /// meter as a raw `0..255` with published breakpoints for the *scale* but
    /// no calibrated wattage behind them, so turning it into watts would
    /// invent a precision the rig never offered — the same reasoning the ALC
    /// reading is reported as a percentage rather than in dB.
    ///
    /// A device that genuinely measures forward power still fills `fwd_w`, and
    /// the two can be present together on a rig that reports both.
    pub po: Option<f32>,
    /// Reflected power in watts, if the device measures it. The ELAD FDM-DUO
    /// does (`RP`, in milliwatts); most rigs give only the SWR. Appended last,
    /// as the wire requires.
    #[serde(default)]
    pub rfl_w: Option<f32>,
}

/// What the adaptive-predistortion loop is doing, for the radios that run one
/// (an HPSDR board correcting itself from its own receiver; a LimeSDR with its
/// second chain given over to the job).
///
/// Reported in receive as well as transmit, because the question an operator
/// asks about PureSignal is mostly asked *between* overs: did it find the
/// feedback, and how much is it correcting. A loop that never locks is the
/// ordinary failure — a coupler not wired, an attenuator too deep, the T/R
/// switch taking the sample away for the length of the over — and until this
/// reached the screen the only place it was ever said was the log, which is
/// why a working installation and a dead one looked exactly alike
/// (issue #441).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PsMeter {
    /// The loop has found the transmission in the feedback and is correcting.
    pub locked: bool,
    /// How much compression the table is taking out, in dB. Zero on an
    /// unlocked loop — and zero on a locked one that has found a transmitter
    /// with nothing to correct, which is a perfectly good answer.
    pub correction_db: f32,
    /// How well the feedback matched the transmission, `0.0..=1.0`. This is
    /// the number that says *why* an unlocked loop is unlocked: near zero is
    /// "nothing came back", partway up is "something came back and it is not
    /// what we sent".
    pub score: f32,
    /// The operator has told the loop to stop adapting and keep what it has.
    pub frozen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Meters {
    /// Signal level in the RX passband, dBm (after `cal_offset_db`).
    pub s_dbm: f32,
    /// ADC headroom indicator: the highest either converter axis reached over
    /// the last meter window, in dBFS. `f32::NEG_INFINITY` before anything has
    /// been measured.
    pub adc_peak_dbfs: f32,
    /// The radio's own temperature in degrees Celsius, where it measures one.
    ///
    /// The board's sensor, not a figure derived on this side, and reported in
    /// receive as well as transmit: what a small PA does is heat up over an
    /// afternoon and cool slowly afterwards, and both halves of that are what
    /// the operator is watching (issue #333). `None` on the great majority of
    /// radios, which have no such sensor — see `IqSource::pa_temp_c`.
    #[serde(default)]
    pub pa_temp_c: Option<f32>,
    /// Fraction of converter samples at full scale over the same window,
    /// `0.0..=1.0`. Above [`OVERLOAD_FRACTION`] — ask [`Meters::adc_overloaded`] —
    /// the front end is
    /// running into its rails and everything downstream is reading a distorted
    /// signal — including the `s_dbm` beside this, which understates a clipped
    /// carrier.
    ///
    /// Beside the peak rather than instead of it because neither answers on its
    /// own: the peak cannot distinguish a signal that fills the converter from
    /// one twice too big for it, and this saturates as soon as a
    /// constant-envelope signal passes √2 of full scale. Together they say both
    /// *whether* and roughly *how far*.
    pub adc_clip: f32,
    /// The *radio's own* converter-overflow flag, where it reports one.
    ///
    /// A different claim from `adc_clip` beside it, and neither replaces the
    /// other. `adc_clip` is measured here, from the samples that arrived; this
    /// is the front end reporting on the converter those samples came out of.
    /// On a direct-sampling radio the two can disagree completely and the
    /// radio's is the one that is right: a Hermes-Lite 2 puts the whole of
    /// 0–38 MHz onto one 12-bit ADC and then hands over 48 kHz of it, so a
    /// broadcaster three bands away can drive the converter into its rails
    /// while every sample that reaches us sits at a tenth of full scale
    /// (issue #362).
    ///
    /// `None` on the great majority of radios, which have no such flag —
    /// see `IqSource::adc_overload`.
    #[serde(default)]
    pub adc_overload: Option<bool>,
    /// Present while transmitting.
    pub tx: Option<TxMeters>,
    /// A WFM stereo pilot is locked on the main receiver. Drives the `ST`
    /// indicator; always `false` in every other mode.
    pub stereo: bool,
    /// The CTCSS tone or DCS code being received on the main receiver. Drives
    /// the sub-audible readout; always `None` outside NFM, and `None` in NFM
    /// until a tone has been present long enough to be sure of.
    pub tone: Option<crate::SubTone>,
    /// The level in the receive passband in **dBFS**, uncalibrated and
    /// ungained — exactly the figure the software squelch compares its
    /// threshold against.
    ///
    /// Not the same number as `s_dbm` beside it, and that is the point.
    /// `s_dbm` is what the *operator* is shown: the front end's own gain
    /// subtracted, `cal_offset_db` added, and on a rig that reports its own
    /// meter it is the rig's reading rather than a measurement made here at
    /// all. None of that scale reaches the squelch, so on such a radio there
    /// was nothing on screen to set the threshold against and the rail had to
    /// be hunted across blind (issue #394).
    ///
    /// `f32::NEG_INFINITY` where there is no chain to measure — a demod-audio
    /// front end, which has no software squelch either.
    #[serde(default = "minus_infinity")]
    pub passband_dbfs: f32,
    /// The adaptive-predistortion loop's state, on a radio running one and
    /// `None` on every other — see [`PsMeter`].
    #[serde(default)]
    pub puresignal: Option<PsMeter>,
}

fn minus_infinity() -> f32 {
    f32::NEG_INFINITY
}

impl Meters {
    /// The front end is running into its rails, so nothing downstream — this
    /// struct's own `s_dbm` included — is reading an undistorted signal.
    pub fn adc_overloaded(&self) -> bool {
        // The radio's own flag wins where there is one: it is watching the
        // converter, and this side is only watching what came out of it.
        self.adc_overload.unwrap_or(false) || self.adc_clip > OVERLOAD_FRACTION
    }

    /// These meters as the operator is shown them on a radio whose S-meter
    /// is calibrated by `cal_db` ([`RadioConfig::smeter_cal_db`](crate::RadioConfig::smeter_cal_db)).
    ///
    /// Only [`Self::s_dbm`] moves — the figure the needle, the readouts, a
    /// report's pre-filled level and the spoken meter are all read from.
    /// [`Self::passband_dbfs`], the squelch's own measure, is left exactly as
    /// it was, so a calibration can never open or close a squelch or start a
    /// recording.
    pub fn with_smeter_cal(mut self, cal_db: f32) -> Self {
        if self.s_dbm.is_finite() {
            self.s_dbm += cal_db;
        }
        self
    }

    /// S-units for display: S9 = -73 dBm, 6 dB per unit below, dB-over-9 above.
    pub fn s_units(&self) -> (u8, f32) {
        let over = self.s_dbm + 73.0;
        if over >= 0.0 {
            (9, over)
        } else {
            let units = 9.0 + over / 6.0;
            (units.max(0.0) as u8, 0.0)
        }
    }
}

#[cfg(test)]
mod tx_power_tests {
    use super::*;

    fn silent() -> TxMeters {
        TxMeters {
            fwd_w: None,
            swr: None,
            alc: None,
            po: None,
            rfl_w: None,
            set_pct: None,
            set_w: None,
        }
    }

    /// Best source first: measured watts, the rig's needle, estimated watts,
    /// estimated per cent, nothing.
    #[test]
    fn the_power_comes_from_the_best_source_there_is() {
        let all = TxMeters {
            fwd_w: Some(5.0),
            po: Some(0.5),
            set_w: Some(20.0),
            set_pct: Some(40.0),
            ..silent()
        };
        assert_eq!(all.power(), TxPower::Measured(5.0));
        assert_eq!(TxMeters { fwd_w: None, ..all }.power(), TxPower::Needle(0.5));
        assert_eq!(TxMeters { fwd_w: None, po: None, ..all }.power(), TxPower::EstimatedW(20.0));
        assert_eq!(
            TxMeters { fwd_w: None, po: None, set_w: None, ..all }.power(),
            TxPower::EstimatedPct(40.0)
        );
        assert_eq!(silent().power(), TxPower::Unknown);
        // A NaN from somewhere is not a reading.
        assert_eq!(TxMeters { fwd_w: Some(f32::NAN), ..silent() }.power(), TxPower::Unknown);
    }

    /// An I/Q radio scales an amplitude, so power goes as drive squared; a rig
    /// that takes drive as a fraction of its rated power goes as drive.
    #[test]
    fn the_estimate_follows_how_the_drive_control_works() {
        let (pct, w) = TxMeters::estimate(0.5, Some(100.0), false);
        assert_eq!(pct, 50.0);
        assert_eq!(w, Some(25.0), "half the amplitude is a quarter of the power");
        let (pct, w) = TxMeters::estimate(0.5, Some(100.0), true);
        assert_eq!(pct, 50.0);
        assert_eq!(w, Some(50.0), "half the rated power setting is half the watts");
        assert_eq!(TxMeters::estimate(0.5, None, false), (50.0, None), "no band power: no watts");
        assert_eq!(TxMeters::estimate(0.5, Some(0.0), false).1, None);
        assert_eq!(TxMeters::estimate(2.0, Some(10.0), true), (100.0, Some(10.0)), "held to full");
        assert_eq!(TxMeters::estimate(f32::NAN, Some(10.0), true), (0.0, Some(0.0)));
    }

    /// The new fields travel: appended last, and a frame without them (an
    /// older `radio.json`-style JSON) loads with both empty.
    #[test]
    fn the_estimate_fields_round_trip_and_default_to_none() {
        let tx = TxMeters { set_pct: Some(33.0), set_w: Some(7.5), ..silent() };
        let json = serde_json::to_string(&tx).unwrap();
        assert_eq!(serde_json::from_str::<TxMeters>(&json).unwrap(), tx);
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let o = v.as_object_mut().unwrap();
        o.remove("set_pct");
        o.remove("set_w");
        let old: TxMeters = serde_json::from_value(v).unwrap();
        assert_eq!((old.set_pct, old.set_w), (None, None));
    }
}
