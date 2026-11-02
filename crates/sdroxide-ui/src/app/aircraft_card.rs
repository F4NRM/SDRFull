//! What the AIRCRAFT window says about one aircraft.
//!
//! The registry ([`super::aircraft_registry`]) holds the merged cards; this is
//! the wording and the ordering, kept apart from the drawing so it can be
//! tested. Nothing here reads the network: a field nobody has heard is shown as
//! "—" with the reason, so an empty line is never a silent one.

use crate::i18n::tr;
use super::aircraft_registry::{AircraftCard, AircraftRegistry, FallbackKey, Source, Stamped};

/// Which card is meant: an ICAO address, or an HFDL aircraft that never gave one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum CardKey {
    Icao(u32),
    Fallback(FallbackKey),
}

/// One line of the list on the left.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::app) struct Row {
    pub key: CardKey,
    pub title: String,
    pub detail: String,
    pub age_s: i64,
}

/// One line of the detail on the right.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::app) struct Line {
    pub label: &'static str,
    pub value: String,
    pub note: String,
    /// Whether the radio has heard it, as opposed to the "—" of a gap.
    pub present: bool,
}

/// The six hex digits of an ICAO address, as printed on a registration database.
pub(in crate::app) fn icao_hex(icao: u32) -> String {
    format!("{:06X}", icao & 0xff_ffff)
}

pub(in crate::app) fn source_label(s: Source) -> &'static str {
    match s {
        Source::Adsb => "ADS-B",
        Source::Vdl2 => "VDL2",
        Source::Hfdl => "HFDL",
        Source::AcarsAudio => "ACARS",
    }
}

/// A line's label as drawn. The label itself stays the English key the rows
/// are matched by; only what is shown is translated.
pub(in crate::app) fn field_label(label: &str) -> String {
    match label {
        "Flight" => tr!("aircraft-label-flight"),
        "Registration" => tr!("aircraft-label-registration"),
        "Position" => tr!("aircraft-label-position"),
        "Altitude" => tr!("aircraft-label-altitude"),
        "Speed" => tr!("aircraft-label-speed"),
        "Track" => tr!("aircraft-label-track"),
        "Last message" => tr!("aircraft-label-last-message"),
        "Type" => tr!("aircraft-label-type"),
        "Operator" => tr!("aircraft-label-operator"),
        "Origin" => tr!("aircraft-label-origin"),
        "Destination" => tr!("aircraft-label-destination"),
        "Online" => tr!("aircraft-label-online"),
        "Range" => tr!("aircraft-label-range"),
        "Vertical" => tr!("aircraft-label-vertical"),
        "Emergency" => tr!("aircraft-label-emergency"),
        "History" => tr!("aircraft-label-history"),
        "First heard" => tr!("aircraft-label-first-heard"),
        "Last heard" => tr!("aircraft-label-last-heard"),
        "Track points" => tr!("aircraft-label-track-points"),
        "Highest" => tr!("aircraft-label-highest"),
        "Callsigns" => tr!("aircraft-label-callsigns"),
        "Registrations" => tr!("aircraft-label-registrations"),
        other => other.to_string(),
    }
}

pub(in crate::app) fn fmt_position(lat: f64, lon: f64) -> String {
    let ns = if lat < 0.0 { 'S' } else { 'N' };
    let ew = if lon < 0.0 { 'W' } else { 'E' };
    format!("{:.4}° {ns}, {:.4}° {ew}", lat.abs(), lon.abs())
}

/// The heading of a card: the flight, else the registration, else the address.
pub(in crate::app) fn title(key: &CardKey, card: &AircraftCard) -> String {
    if let Some(f) = &card.flight {
        return f.value.clone();
    }
    if let Some(r) = &card.registration {
        return r.value.clone();
    }
    match key {
        CardKey::Icao(icao) => format!("ICAO {}", icao_hex(*icao)),
        CardKey::Fallback(FallbackKey::AircraftId(id)) => tr!("aircraft-hfdl-aircraft", id = id),
        CardKey::Fallback(FallbackKey::Flight(f)) => f.clone(),
    }
}

/// The grey line under a title: the address, and the registration when the
/// title is not already it.
pub(in crate::app) fn detail(key: &CardKey, card: &AircraftCard) -> String {
    match key {
        CardKey::Icao(icao) => {
            let hex = icao_hex(*icao);
            match (&card.flight, &card.registration) {
                (Some(_), Some(r)) => format!("{hex} · {}", r.value),
                _ => hex,
            }
        }
        CardKey::Fallback(_) => tr!("aircraft-no-icao"),
    }
}

fn row(key: CardKey, card: &AircraftCard, now: i64) -> Row {
    Row {
        title: title(&key, card),
        detail: detail(&key, card),
        age_s: (now - card.last_seen).max(0),
        key,
    }
}

/// Every card, the most recently heard first. A tie goes to the lower address,
/// the ICAO cards before the ones without, so a redraw never shuffles the list.
pub(in crate::app) fn rows(reg: &AircraftRegistry, now: i64) -> Vec<Row> {
    let mut v: Vec<(i64, Row)> = reg
        .entries()
        .map(|(icao, c)| (c.last_seen, row(CardKey::Icao(icao), c, now)))
        .chain(
            reg.fallback_entries()
                .map(|(k, c)| (c.last_seen, row(CardKey::Fallback(k.clone()), c, now))),
        )
        .collect();
    v.sort_by(|(at_a, a), (at_b, b)| {
        at_b.cmp(at_a).then_with(|| sort_key(&a.key).cmp(&sort_key(&b.key)))
    });
    v.into_iter().map(|(_, r)| r).collect()
}

/// A total order over keys: addresses first, then the fallback cards by name.
fn sort_key(k: &CardKey) -> (u8, u32, String) {
    match k {
        CardKey::Icao(i) => (0, *i, String::new()),
        CardKey::Fallback(FallbackKey::AircraftId(id)) => (1, *id, String::new()),
        CardKey::Fallback(FallbackKey::Flight(f)) => (2, 0, f.clone()),
    }
}

pub(in crate::app) fn lookup<'a>(
    reg: &'a AircraftRegistry,
    key: &CardKey,
) -> Option<&'a AircraftCard> {
    match key {
        CardKey::Icao(icao) => reg.card(*icao),
        CardKey::Fallback(k) => reg.fallback_card(k),
    }
}

/// `"ADS-B, 12s ago"`.
fn provenance<T>(s: &Stamped<T>, now: i64) -> String {
    tr!(
        "aircraft-provenance",
        source = source_label(s.source),
        age = super::util::fmt_age(now - s.at)
    )
}

fn line<T>(
    label: &'static str,
    field: &Option<Stamped<T>>,
    now: i64,
    show: impl Fn(&T) -> String,
    missing: &str,
) -> Line {
    match field {
        Some(s) => Line { label, value: show(&s.value), note: provenance(s, now), present: true },
        None => Line { label, value: "—".to_string(), note: missing.to_string(), present: false },
    }
}

/// The fixed set of lines for a card, a missing field included.
pub(in crate::app) fn lines(card: &AircraftCard, now: i64) -> Vec<Line> {
    vec![
        line("Flight", &card.flight, now, |v| v.clone(), &tr!("aircraft-missing-flight")),
        line(
            "Registration",
            &card.registration,
            now,
            |v| v.clone(),
            &tr!("aircraft-missing-registration"),
        ),
        line(
            "Position",
            &card.position,
            now,
            |&(lat, lon)| fmt_position(lat, lon),
            &tr!("aircraft-missing-position"),
        ),
        line(
            "Altitude",
            &card.altitude_ft,
            now,
            |v| sdroxide_types::AirUnits::current().altitude(*v),
            &tr!("aircraft-missing-adsb"),
        ),
        line(
            "Speed",
            &card.ground_speed_kt,
            now,
            |v| sdroxide_types::AirUnits::current().speed(*v),
            &tr!("aircraft-missing-adsb"),
        ),
        line(
            "Track",
            &card.track_deg,
            now,
            |v| format!("{v:.0}°"),
            &tr!("aircraft-missing-adsb"),
        ),
        line(
            "Last message",
            &card.last_message,
            now,
            |v| v.clone(),
            &tr!("aircraft-missing-message"),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdroxide_types::{AdsbAircraft, HfdlDecode, HfdlFix};

    fn adsb(icao: u32, call: &str, at: i64) -> AdsbAircraft {
        let mut a = AdsbAircraft::new(icao, at);
        a.callsign = call.to_string();
        a.lat = Some(52.37);
        a.lon = Some(-4.9);
        a.altitude_ft = Some(35_000);
        a.ground_speed_kt = Some(430.0);
        a.track_deg = Some(90.0);
        a.last_at = at;
        a.last_pos_at = at;
        a
    }

    fn hfdl(id: Option<u32>, flight: Option<&str>, at: i64) -> HfdlDecode {
        HfdlDecode {
            unix: at,
            kind: "performance-data".to_string(),
            gs: None,
            freq_khz: 8_912,
            snr_db: None,
            freq_skew_hz: None,
            fec_corrected: None,
            details: String::new(),
            position: Some(HfdlFix {
                lat: 10.0,
                lon: -30.0,
                aircraft_id: id,
                icao: None,
                flight: flight.map(str::to_string),
            }),
        }
    }

    fn line<'a>(ls: &'a [Line], label: &str) -> &'a Line {
        ls.iter().find(|l| l.label == label).unwrap_or_else(|| panic!("no {label} line"))
    }

    #[test]
    fn icao_is_six_upper_case_hex_digits() {
        assert_eq!(icao_hex(0x4CA1FA), "4CA1FA");
        assert_eq!(icao_hex(0x0000AB), "0000AB");
        // Only 24 bits are an address.
        assert_eq!(icao_hex(0xFF_4CA1FA), "4CA1FA");
    }

    #[test]
    fn every_source_has_its_own_name() {
        assert_eq!(source_label(Source::Adsb), "ADS-B");
        assert_eq!(source_label(Source::Vdl2), "VDL2");
        assert_eq!(source_label(Source::Hfdl), "HFDL");
        assert_eq!(source_label(Source::AcarsAudio), "ACARS");
    }

    #[test]
    fn position_carries_its_hemispheres() {
        assert_eq!(fmt_position(52.37, -4.9), "52.3700° N, 4.9000° W");
        assert_eq!(fmt_position(-33.5, 151.25), "33.5000° S, 151.2500° E");
    }

    #[test]
    fn the_list_is_newest_first() {
        let mut r = AircraftRegistry::default();
        r.ingest_adsb(&adsb(0x4CA1FA, "KLM123", 1_000));
        r.ingest_adsb(&adsb(0x3C6444, "DLH9", 1_050));
        r.ingest_adsb(&adsb(0x484000, "TRA55", 1_020));
        let rows = rows(&r, 1_060);
        let order: Vec<_> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(order, ["DLH9", "TRA55", "KLM123"]);
    }

    #[test]
    fn equal_age_goes_by_address_whatever_the_table_order() {
        // A HashMap hands its entries back in an order of its own; with twenty
        // aircraft heard in the same second, only a real tie-break gives the same
        // list twice.
        let mut r = AircraftRegistry::default();
        for i in 0..20u32 {
            r.ingest_adsb(&adsb(0x400000 + i * 0x101, "X", 1_000));
        }
        let keys: Vec<_> = rows(&r, 1_010).into_iter().map(|r| r.key).collect();
        let mut sorted = keys.clone();
        sorted.sort_by_key(sort_key);
        assert_eq!(keys, sorted);
        assert_eq!(keys[0], CardKey::Icao(0x400000));
    }

    #[test]
    fn a_row_reports_its_age_and_never_a_negative_one() {
        let mut r = AircraftRegistry::default();
        r.ingest_adsb(&adsb(0x4CA1FA, "KLM123", 1_000));
        assert_eq!(rows(&r, 1_012)[0].age_s, 12);
        assert_eq!(rows(&r, 990)[0].age_s, 0, "a clock behind the data is not an age of -10");
    }

    #[test]
    fn the_title_falls_back_from_flight_to_registration_to_address() {
        let key = CardKey::Icao(0x4CA1FA);
        let mut c = AircraftCard::default();
        assert_eq!(title(&key, &c), "ICAO 4CA1FA");
        c.registration = Some(Stamped { value: "PH-BXA".into(), source: Source::Vdl2, at: 1 });
        assert_eq!(title(&key, &c), "PH-BXA");
        c.flight = Some(Stamped { value: "KLM123".into(), source: Source::Adsb, at: 1 });
        assert_eq!(title(&key, &c), "KLM123");
    }

    #[test]
    fn the_detail_names_the_address_and_the_registration() {
        let key = CardKey::Icao(0x4CA1FA);
        let mut c = AircraftCard::default();
        assert_eq!(detail(&key, &c), "4CA1FA");
        c.registration = Some(Stamped { value: "PH-BXA".into(), source: Source::Vdl2, at: 1 });
        // No flight yet: the registration is the title, so it is not said twice.
        assert_eq!(detail(&key, &c), "4CA1FA");
        c.flight = Some(Stamped { value: "KLM123".into(), source: Source::Adsb, at: 1 });
        assert_eq!(detail(&key, &c), "4CA1FA · PH-BXA");
    }

    #[test]
    fn an_hfdl_aircraft_without_an_address_is_listed_and_says_so() {
        let mut r = AircraftRegistry::default();
        r.ingest_hfdl(&hfdl(Some(7), Some("BAW7"), 1_000));
        let rows = rows(&r, 1_005);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, CardKey::Fallback(FallbackKey::AircraftId(7)));
        assert_eq!(rows[0].title, "BAW7");
        assert!(rows[0].detail.contains("no ICAO address"), "{}", rows[0].detail);
    }

    #[test]
    fn a_fallback_card_is_never_listed_among_the_icao_ones() {
        let mut r = AircraftRegistry::default();
        r.ingest_adsb(&adsb(0x4CA1FA, "KLM123", 1_000));
        r.ingest_hfdl(&hfdl(Some(7), Some("KLM123"), 1_000));
        // Same flight number, but HFDL gave no address: two cards, not one.
        assert_eq!(rows(&r, 1_000).len(), 2);
    }

    #[test]
    fn lookup_finds_a_card_and_loses_one_that_expired() {
        let mut r = AircraftRegistry::default();
        r.ingest_adsb(&adsb(0x4CA1FA, "KLM123", 1_000));
        r.ingest_hfdl(&hfdl(Some(7), None, 1_000));
        assert!(lookup(&r, &CardKey::Icao(0x4CA1FA)).is_some());
        assert!(lookup(&r, &CardKey::Fallback(FallbackKey::AircraftId(7))).is_some());
        r.expire(1_000 + 601);
        assert!(lookup(&r, &CardKey::Icao(0x4CA1FA)).is_none());
        assert!(lookup(&r, &CardKey::Fallback(FallbackKey::AircraftId(7))).is_none());
    }

    #[test]
    fn a_heard_field_shows_its_value_its_source_and_its_age() {
        let mut r = AircraftRegistry::default();
        r.ingest_adsb(&adsb(0x4CA1FA, "KLM123", 1_000));
        let ls = lines(r.card(0x4CA1FA).unwrap(), 1_012);
        let alt = line(&ls, "Altitude");
        assert_eq!(alt.value, "35000 ft");
        assert_eq!(alt.note, "ADS-B, 12s ago");
        assert_eq!(line(&ls, "Speed").value, "430 kt");
        assert_eq!(line(&ls, "Track").value, "90°");
        assert_eq!(line(&ls, "Position").value, "52.3700° N, 4.9000° W");
        assert_eq!(line(&ls, "Flight").value, "KLM123");
    }

    #[test]
    fn a_missing_field_is_a_dash_with_the_reason() {
        let mut r = AircraftRegistry::default();
        r.ingest_adsb(&adsb(0x4CA1FA, "KLM123", 1_000));
        let ls = lines(r.card(0x4CA1FA).unwrap(), 1_000);
        let reg = line(&ls, "Registration");
        assert_eq!(reg.value, "—");
        assert!(reg.note.contains("VDL2"), "{}", reg.note);
        let msg = line(&ls, "Last message");
        assert_eq!(msg.value, "—");
        assert!(!msg.note.is_empty());
    }

    #[test]
    fn every_line_is_always_present() {
        let ls = lines(&AircraftCard::default(), 1_000);
        let labels: Vec<_> = ls.iter().map(|l| l.label).collect();
        assert_eq!(
            labels,
            ["Flight", "Registration", "Position", "Altitude", "Speed", "Track", "Last message"]
        );
        assert!(ls.iter().all(|l| l.value == "—" && !l.note.is_empty()));
    }

    #[test]
    fn each_field_names_the_source_that_actually_set_it() {
        let mut r = AircraftRegistry::default();
        r.ingest_hfdl(&hfdl(Some(7), Some("BAW7"), 1_000));
        let key = CardKey::Fallback(FallbackKey::AircraftId(7));
        let ls = lines(lookup(&r, &key).unwrap(), 1_001);
        assert_eq!(line(&ls, "Position").note, "HFDL, 1s ago");
        assert_eq!(line(&ls, "Altitude").value, "—");
    }
}
