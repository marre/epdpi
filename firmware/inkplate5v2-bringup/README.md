# Inkplate 5 Gen 2 — Rust board bring-up firmware

Experimental classic ESP32 WROVER-E (Inkplate 5V2 / ED052TC4) firmware linking
`epdpi-esp`'s real Inkplate board/power implementation.

> Before flashing, follow [the flash backup and ROM recovery procedure](../../docs/inkplate5v2-recovery.md)
> on your actual board first. A successful build does not prove hardware safety.

## Modes

| Mode | Build command | Board behavior |
| --- | --- | --- |
| Default | `cargo build --release --target xtensa-esp32-none-elf -Zbuild-std=core,alloc` | Reads only PCAL6416A registers over I2C, prints snapshots over UART |
| `low-voltage-prep` | Same command with `--features low-voltage-prep` | Performs one preparation/shutdown cycle, including read-only PMIC standby inspection, with HV disabled |
| `low-voltage-cycles` | Same command with `--features low-voltage-cycles` | Runs 100 preparation/shutdown cycles on one bus instance, then stays in the released state |
| `pmic-standby-check` | Same command with `--features pmic-standby-check` | Tests volatile PMIC sequencing writes while awake with all rails disabled; no display GPIO or DMA |

This is intentionally a separate workspace from the independent
`firmware/inkplate5v2-diagnostic`. The first-stage diagnostic does not
link any display library and should remain the safest first firmware test.

In all these firmware modes, **panel HV is never requested**:
`Inkplate5v2Bus::set_power(true)` still rejects requests with
`HardwareNotValidated`. I2S DMA is constructed only in the opt-in mode;
it sends no display rows. The opt-in builds **do** modify hardware pins and wake the PMIC, but do not
write its register configuration in the low-voltage modes. The PMIC standby
check explicitly writes only volatile sequencer registers. The logic-only
check additionally enables and disables the logic supply (ENABLE=20/00). These are not
substitutes for read-only diagnostics.

The UART is the board's CH340C serial bridge. A 5-second snapshot shows
PCAL input/output levels and configurations, including P0.7 PGOOD and
P1.0 GPIO0 clock-enable state. The logic readback is **not** a voltage
measurement.

The opt-in build performs a **read-only preflight before configuring any
display control outputs or I2S pins**. It rejects asserted physical PGOOD,
an incorrectly configured PGOOD input, or an actively driven PCAL power/control
output (including WAKEUP, GMOD and SPV). Its board library repeats that check
before changing direct GPIO. A failed setup halts rather than retrying. On a dirty boot with a known
configured expander, the opt-in modes attempt shutdown-only containment
before halting. The default image remains strictly read-only.

**Unresolved before activating display high voltage:** the HAL's I2S
`send()` start code still has unbounded waits, I2C can stall, and arbitrary
cancellation/reset cannot guarantee cleanup. GPIO high-Z shutdown is
implemented but its electrical behavior remains unmeasured. See the
[PMIC hardening notes](../../docs/inkplate5v2-pmic-hardening.md).

## Repeated low-voltage test

Only after the read-only and single-cycle tests succeed, build and flash
with the verified backup/recovery procedure (adjust the serial port):

```sh
cargo +esp build --release --features low-voltage-cycles \
  --target xtensa-esp32-none-elf -Zbuild-std=core,alloc
espflash flash --chip esp32 --port /dev/ttyUSB0 --monitor \
  target/xtensa-esp32-none-elf/release/inkplate5v2-bringup
```

The firmware reads the PMIC afresh on each wake, compares all standby
registers against the first cycle, verifies prepared and released PCAL
states including unrelated latch/direction preservation, and logs each
cycle plus minimum/maximum durations. It waits 100 ms between cycles,
then stays released and prints snapshots every five seconds. Any error
stops the test; do not repeatedly reset on failure. Capture the entire
serial log. Expected final line begins `PASS: 100/100 low-voltage cycles`.
No HV or DMA row transfers are requested.

## PMIC-only standby check

After the 100-cycle test passes, this independent opt-in mode verifies
the same sequence register writes used by future power-up, without
writing ENABLE or asserting PWRUP/VCOM/OE. It never configures direct
display pins, the clock gate stays off and no I2S is instantiated.
The factory snapshot is read again after sleep and must match the
initial snapshot exactly. Use the same flash procedure as above, replacing
the feature with `pmic-standby-check`. Expected output includes
`Volatile sequencer verified: UP=E4/55 DOWN=1B/E0` on the tested unit,
then the original factory `UP=E4/55 DOWN=1E/E0` and a PASS line.
Do not enable or attempt a powered refresh based on this test.

## PMIC logic supply check

After the successful standby diagnostic, select `pmic-logic-check`
instead of `pmic-standby-check`. It performs one cycle per boot with
ENABLE=20, verifies PG=00 and inactive PWRUP/VCOM/OE/GMOD/SPV, holds
the logic-only state for 10 ms, then uses the normal verified shutdown
and factory reload. No direct GPIO or I2S is configured; the clock gate
remains off. Diagnostics are printed only after shutdown on success.
Expected captured logic-ready INPUT0=08, OUTPUT0=C8, DIR0=C0, gate off.
Final snapshots should match the earlier released state. Any failure halts;
do not repeatedly reset on error. This verifies digital readback only.

## Design notes

- Chip: classic ESP32, not ESP32-S3.
- I2C0: SDA GPIO21, SCL GPIO22.
- Direct panel controls: LE GPIO2 (low), CKV GPIO32 (low),
  SPH GPIO33 (low until logic supply is enabled in a future powered mode).
- I2S1 source clock: GPIO0; eight data pins in board order:
  GPIO4, 5, 18, 19, 23, 25, 26, 27.
- DMA capacity: 336 bytes per row (320 source data + 16 padding clocks).
- Existing `esp-hal` I2S driver is retained; no DMA rewrite.
- Initial read-only, one-cycle, 100/100 repeated low-voltage and PMIC standby sequencer tests passed on the maintainer's physical board (reported 2026-11-25). No electrical measurements or HV tests have been performed. See the [hardware log](../../docs/inkplate5v2-hardware-validation.md).

For the stages after low-voltage integration see the
[implementation plan](../../docs/inkplate5v2-implementation-plan.md).
