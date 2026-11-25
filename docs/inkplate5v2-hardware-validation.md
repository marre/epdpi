# Inkplate 5 Gen 2 physical bring-up record

## Maintainer-reported results, 2026-11-25

One classic ESP32 Inkplate 5 Gen 2 unit. Recovery (flash and restore) was
reported successful before the diagnostic tests. These are serial log
observations, not voltage or waveform measurements. No high-voltage
activation was requested by either diagnostic.

### Independent read-only diagnostic

Three consecutive snapshots were identical:

```text
INPUT0=00 INPUT1=04
OUTPUT0=FF OUTPUT1=FF
CONFIG0=FF CONFIG1=FF
Physical PWR_GOOD=LOW
```

All expander pins were inputs and the physical PMIC control pins read low.
No configuration writes were made by this diagnostic.

### Single-cycle low-voltage preparation and shutdown

```text
Read-only power-entry preflight passed.
Low-voltage preparation succeeded; panel HV remains LOCKED.
TPS65186 standby: REVID=0x66 ENABLE=0x00 PG=0x00 VADJ=0x03
Factory VCOM=-2520 mV (configured, not measured); VCOM1=0xFC VCOM2=0x00
Factory sequencer UP=E4/55 DOWN=1E/E0
Panel GPIO shutdown complete; HV never enabled.
PCAL INPUT0=0x00 OUTPUT0=0xC0 DIR0=0xC7
PCAL OUTPUT1=0xFE DIR1=0xFE; PWR_GOOD=LOW CLOCK_GATE=OFF
```

Final snapshots were stable. The final expander direction/latch values
match the expected shutdown configuration. The factory VCOM and sequencer
bytes are retained as reference observations for this unit, not global
constants for other panels. The single-cycle firmware did not write PMIC
calibration or sequencer registers.

## Next gate

Run the explicit `low-voltage-cycles` firmware on the same unit. Expect
100/100 passes, unchanged standby PMIC configuration on each fresh read,
and a stable released state after the final cycle. Record the complete
serial log. No result for this test has been reported yet.

Electrical high-Z, actual HV ramp/discharge, I2S timing, HAL start stalls,
I2C failure containment and reset behavior remain unverified. Keep the HV
interlock in place.
