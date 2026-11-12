# Sample-rate test (IQ / sound-card rate against true time)

Use it when a decoder that cuts lines or symbols to a transmitter's clock
(WEFAX above all) drifts, and you need to know whether the radio's sample clock
moves. The engine already measures it: the WEFAX slant correction follows the
same figure.

## What is measured

Every block of samples is counted against the instant it arrived and a
least-squares line is fitted through them (`IqRateMeter` in
`crates/sdroxide-radio/src/engine.rs`). The result is the stream's real rate
against true time, in ppm of its nominal rate (negative: the stream runs slow).
A radio fed from a sound card (CAT rig, a transceiver's audio next to a
panadapter receiver) is measured on its audio stream with the same code. A
recording being played back is not measured.

The clock is `Instant`, which the time service disciplines, so the figure is
against *true* time, which is what a decoder needs. An undisciplined clock
(`CLOCK_MONOTONIC_RAW`) would add the PC's own oscillator error (about
+30 ppm on the test machine); that variant is commit `2515c9a7` on branch
`diag/iq-rate` and must not be merged, because it would change the live
correction.

## Switch it on

Nothing is printed by default. Start the program with:

```bash
RUST_LOG=info,sdroxide_radio::iq_rate=debug,sdroxide_sdrplay::gain_change=info sdroxide --server
```

On a systemd service put the same text in `Environment=RUST_LOG=...`.

Every 10 s (after a first 30 s of data) a line appears:

```
IQ rate 2499449.97 Hz, -220.1 ppm against the PC clock (+0.4 since the last report, fit over 60 s)
```

The SDRplay handle also logs each gain change at INFO, so a step can be tied to
the figure:

```
LNA state -> 1 (tuner 0)
AGC mode code -> 0 (tuner 0)
```

## Method for a gain test

1. Idle receiver, no gain touched, five minutes: the figure should settle after
   a minute and wander by a few ppm between reports.
2. Move one setting at a time (LNA state, then IF gain), waiting two minutes
   after each, and note the time of each step (the journal is in local time,
   the file names of saved pictures in UTC).
3. A step of more than about 30 ppm between two reports that lines up with a
   gain change is the radio, not the decoder.

## What was found

On the RSP1 (2.5 Msps, 2026-11-12), LNA state 0 gave about -220 ppm against
the PC clock; states 1 to 3 gave 0 to +30 ppm. That is the WEFAX slant seen
when the gain was moved (100 ppm is 0.18 px per line on a 1809 px line).

Other front ends have not been measured: run the test on them before assuming
they are steady.
