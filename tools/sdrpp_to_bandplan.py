#!/usr/bin/env python3
"""Convert an SDR++ bandplan JSON file to sdroxide's bandplan.json format.

SDR++ bandplans are a flat list of bands with name/type/start/end (Hz).
sdroxide bandplans are region-based (R1/R2/R3) with amateur Band entries
and sub-segments (Cw/Phone/Digi/All) with optional labels.

Usage:
    python3 sdrpp_to_bandplan.py input.json output.json

The converter maps SDR++ band types to segment kinds. Amateur bands are
identified by frequency overlap with known amateur allocations. All three
regions get the same plan (SDR++ has no region concept).
"""

import json
import sys
import argparse

TYPE_TO_KIND = {
    "amateur": "All",
    "broadcast": "Phone",
    "aviation": "Phone",
    "marine": "Phone",
    "military": "Phone",
    "satellite": "Digi",
    "other": "All",
    "utility": "All",
    "cellular": "All",
    "ism": "All",
}

# Amateur band frequency ranges (Hz) -> sdroxide Band enum
AMATEUR_BANDS = [
    (135700, 137800, None),  # 2200m (not in our enum, skip)
    (472000, 479000, None),  # 630m (not in our enum)
    (1810000, 2000000, "M160"),
    (3500000, 3800000, "M80"),
    (5351500, 5366500, "M60"),
    (7000000, 7200000, "M40"),
    (10100000, 10150000, "M30"),
    (14000000, 14350000, "M20"),
    (18068000, 18168000, "M17"),
    (21000000, 21450000, "M15"),
    (24890000, 24990000, "M12"),
    (28000000, 29700000, "M10"),
    (50000000, 52000000, "M6"),
    (70000000, 70500000, "M4"),
    (144000000, 146000000, "M2"),
    (430000000, 440000000, "M70"),
    (1240000000, 1300000000, "Cm23"),
    (2320000000, 2450000000, "Cm13"),
    (3400000000, 3475000000, "Cm9"),
    (5650000000, 5850000000, "Cm6"),
    (10000000000, 10500000000, "Cm3"),
    (220000000, 225000000, "M125"),
    (902000000, 928000000, "Cm33"),
]

# Listening bands by frequency
LISTENING_BANDS = [
    (148500, 283500, "Lw"),
    (526500, 1606500, "Mw"),
    (530000, 1700000, "Mw"),
    (2300000, 26100000, None),  # SW - too broad, skip as band
    (87500000, 108000000, "Fm"),
]

README = [
    "Converted from SDR++ bandplan.",
    "Edit, save, then press RELOAD BAND PLAN on the General tab.",
]


def hz_to_mhz(hz):
    return round(hz / 1_000_000.0, 4)


def identify_band(start, end):
    """Identify sdroxide Band from frequency range."""
    for lo, hi, band in AMATEUR_BANDS:
        if abs(start - lo) < 1000 and abs(end - hi) < 1000:
            return band
    for lo, hi, band in LISTENING_BANDS:
        if band and abs(start - lo) < 1000 and abs(end - hi) < 1000:
            return band
    return None


def convert(sdrpp):
    bands = sdrpp.get("bands", [])

    amateur_bands = {}
    segments = []

    for b in bands:
        name = b.get("name", "")
        btype = b.get("type", "other")
        start = b.get("start", 0)
        end = b.get("end", 0)
        if start >= end:
            continue

        band_enum = identify_band(start, end)

        if band_enum is not None:
            amateur_bands[band_enum] = (start, end)

        kind = TYPE_TO_KIND.get(btype, "All")
        segments.append({
            "lo_mhz": hz_to_mhz(start),
            "hi_mhz": hz_to_mhz(end),
            "kind": kind,
            "label": name,
        })

    # Add missing amateur/listening bands from IARU so they appear in the
    # band menu (bands without segments won't show on the waterfall)
    IARU_EDGES = {
        "M160": (1.81, 2.0), "M80": (3.5, 3.8), "M60": (5.3515, 5.3665),
        "M40": (7.0, 7.2), "M30": (10.1, 10.15), "M20": (14.0, 14.35),
        "M17": (18.068, 18.168), "M15": (21.0, 21.45), "M12": (24.89, 24.99),
        "M10": (28.0, 29.7), "M11": (26.965, 27.86), "M6": (50.0, 52.0), "M4": (70.0, 70.5),
        "M2": (144.0, 146.0), "M70": (430.0, 440.0), "Cm23": (1240.0, 1300.0),
        "Cm13": (2300.0, 2450.0), "Cm9": (3400.0, 3475.0), "Cm6": (5650.0, 5850.0),
        "Cm3": (10489.5, 10490.0), "M125": (220.0, 225.0), "Cm33": (902.0, 928.0),
        "Lw": (0.1485, 0.2835), "Mw": (0.5265, 1.6065), "Fm": (87.5, 108.0),
    }
    for band_name, (lo, hi) in IARU_EDGES.items():
        if band_name not in amateur_bands:
            amateur_bands[band_name] = (int(lo * 1e6), int(hi * 1e6))

    band_list = []
    for band_name, (start, end) in amateur_bands.items():
        band_list.append({
            "band": band_name,
            "lo_mhz": hz_to_mhz(start),
            "hi_mhz": hz_to_mhz(end),
        })

    segments.sort(key=lambda s: s["lo_mhz"])
    deduped = []
    for seg in segments:
        if deduped and seg["lo_mhz"] < deduped[-1]["hi_mhz"]:
            if seg["hi_mhz"] - seg["lo_mhz"] > deduped[-1]["hi_mhz"] - deduped[-1]["lo_mhz"]:
                deduped[-1] = seg
        else:
            deduped.append(seg)
    segments = deduped

    plan = {
        "readme": README,
        "complete": True,
        "region1": {
            "bands": [dict(b) for b in band_list],
            "segments": [dict(s) for s in segments],
            "psk_windows": [],
            "rtty_windows": [],
        },
        "region2": {
            "bands": [dict(b) for b in band_list],
            "segments": [dict(s) for s in segments],
            "psk_windows": [],
            "rtty_windows": [],
        },
        "region3": {
            "bands": [dict(b) for b in band_list],
            "segments": [dict(s) for s in segments],
            "psk_windows": [],
            "rtty_windows": [],
        },
    }

    return plan


def main():
    parser = argparse.ArgumentParser(description="Convert SDR++ bandplan to sdroxide format")
    parser.add_argument("input", help="SDR++ bandplan JSON file")
    parser.add_argument("output", help="Output sdroxide bandplan JSON file")
    args = parser.parse_args()

    with open(args.input) as f:
        sdrpp = json.load(f)

    plan = convert(sdrpp)

    with open(args.output, 'w') as f:
        json.dump(plan, f, indent=2)
        f.write('\n')

    print("Converted %d SDR++ bands to %d sdroxide bands and %d segments." % (
        len(sdrpp.get('bands', [])),
        len(plan['region1']['bands']),
        len(plan['region1']['segments'])))


if __name__ == "__main__":
    main()
