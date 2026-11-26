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

## PMIC standby sequencer test, 2026-11-25

Maintainer reported a successful `pmic-standby-check` run with the
20 ms HAL software transaction timeout configured. The observed diagnostic
bootloader was ESP-IDF v6.1-829-g9a97f6c54ec, ESP32 revision v3.1,
4 MB flash, DIO at 40 MHz. The experimental factory app booted at
0x10000 (length 0x3f0000). This identifies the current experimental
image layout, not the original vendor firmware's partition table.

```text
Mode: PMIC STANDBY SEQUENCER CHECK; HV LOCKED
Volatile sequencer verified: UP=E4/55 DOWN=1B/E0
TPS65186 standby: REVID=0x66 ENABLE=0x00 PG=0x00 VADJ=0x03
Factory VCOM=-2520 mV (configured, not measured); VCOM1=0xFC VCOM2=0x00
Factory sequencer UP=E4/55 DOWN=1E/E0
PASS: factory standby configuration restored; no HV, VCOM or OE request.
PCAL INPUT0=0x00 OUTPUT0=0xC0 DIR0=0xC7
PCAL OUTPUT1=0xFE DIR1=0xFE; PWR_GOOD=LOW CLOCK_GATE=OFF
```

Four final snapshots agreed. The volatile write/readback and subsequent
factory reload are now verified on this unit. The factory delay and VCOM
bytes match the previous tests. No powered rail operation or scanline was
requested. Both [source CI](https://github.com/marre/epdpi/actions/runs/45690571489)
and [firmware CI](https://github.com/marre/epdpi/actions/runs/45690571532)
passed for the tested branch revision c45bbece4c4be7fae44edbcb3088dd2e5ef1c095.

## Next gate

All planned standby-only physical checks have passed on this unit.
Review the ED052TC4 electrical power sequencing constraints before
implementing a separately gated powered PMIC diagnostic. A powered test
without scanning must still respect all panel rail, idle-signal and
VCOM requirements; OE low alone does not establish that it is safe.
The diagnostic must have explicit one-shot authorization, bounded
execution, fault cleanup and no automatic restart or DMA activity.

Physical I2C fault/reset containment, electrical high-Z, actual HV
ramp/discharge, and panel timing remain unverified. The HAL I2S start
stall remains a blocker for scanning. No HV activation is authorized by
this record; the existing interlock remains in place.

Follow-up source review selected the panel-specific E4/1E sequence instead of the vendor E4/1B override, and added a logic-supply settling dwell. The earlier E4/1B diagnostic result is retained as historical evidence, not the current expected programmed order. No energized operation has been tested. See the PMIC hardening notes.

2026-11-26 historical review restores the previously tested vendor E4/1B
baseline provisionally and corrects direct/expander signal ordering before
logic supply. Low-voltage SPH is now low. New physical tests are pending;
the earlier 100-cycle result applies to the previous SPH-high preparation.
See PMIC hardening notes for issue #297 and outstanding analogue constraints.

Next physical gate: `pmic-logic-check` (not yet tested). One cycle per boot,
clock gate off, no direct GPIO/I2S, ENABLE=20 only, PG=00, shutdown and
factory snapshot verification. The public HV interlock remains unchanged.
