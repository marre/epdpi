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

## Repeated low-voltage test, 2026-11-25

Maintainer reported a complete 100-cycle run using the explicit
`low-voltage-cycles` firmware. No reboot between cycles, no failures,
and no requests to enable HV or send display data. The fresh PMIC
standby snapshot was identical across all 100 cycles, including the
previously recorded VCOM and sequencer settings.

| Register | Prepared | Released |
| --- | --- | --- |
| PCAL INPUT0 | 0x00 | 0x00 |
| PCAL OUTPUT0 | 0xC0 | 0xC0 |
| PCAL DIR0 | 0xC0 | 0xC7 |
| PCAL OUTPUT1 | 0xFF | 0xFE |
| PCAL DIR1 | 0xFE | 0xFE |
| PWR_GOOD | LOW | LOW |
| Clock gate | ON | OFF |

| Software timing | Prepare | Shutdown |
| --- | --- | --- |
| First cycle | 34688 us | 15403 us |
| All cycles, minimum | 30478 us | 15136 us |
| All cycles, maximum | 34688 us | 15403 us |
| Cycles 3 through 100, range | 30478-30483 us | 15136-15141 us |

```text
PASS cycle 100/100: prep=30479 us shutdown=15137 us
PASS: 100/100 low-voltage cycles; PMIC configuration unchanged; HV never enabled.
Preparation min/max: 30478/34688 us; shutdown min/max: 15136/15403 us
Panel GPIO shutdown complete; no further cycles.
PCAL INPUT0=0x00 OUTPUT0=0xC0 DIR0=0xC7
PCAL OUTPUT1=0xFE DIR1=0xFE; PWR_GOOD=LOW CLOCK_GATE=OFF
```

The final snapshot was stable across repeated serial reports. The test
exercised initialization, repeated rearm, fresh standby reads, clock gate
configuration and release. Its software timings do not measure energized
rail ramps or discharge, and readback does not establish electrical high-Z.

## Next gate

The repeated low-voltage acceptance test passed on this unit. Before
considering a separately controlled HV diagnostic, review the I2C
transaction timeout, elapsed-time polling deadlines and fault containment
including MCU reset. The HAL I2S start stall is a blocker for scanning
but no DMA operation is needed for a PMIC-only test. No HV activation
is authorized by this record; the interlock stays in place.

Electrical high-Z, actual HV ramp/discharge, I2S timing, HAL start stalls,
I2C failure containment and reset behavior remain unverified.
