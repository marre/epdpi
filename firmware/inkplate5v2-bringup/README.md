# Inkplate 5 Gen 2 — Rust board bring-up firmware

Experimental classic ESP32 WROVER-E (Inkplate 5V2 / ED052TC4) firmware linking
`epdpi-esp`'s real Inkplate board/power implementation.

> **Do not flash yet.** Follow [the flash backup and ROM recovery procedure](../../docs/inkplate5v2-recovery.md)
> on your actual board first. A successful build does not prove hardware safety.

## Modes

| Mode | Build command | Board behavior |
| --- | --- | --- |
| Default | `cargo build --release --target xtensa-esp32-none-elf -Zbuild-std=core,alloc` | Reads only PCAL6416A registers over I2C, prints snapshots over UART |
| `low-voltage-prep` | Same command with `--features low-voltage-prep` | Initializes direct GPIO, I2S1 clock/data routing and the PMIC sequencer with panel high-voltage disabled |

This is intentionally a separate workspace from the independent
`firmware/inkplate5v2-diagnostic`. The first-stage diagnostic does not
link any display library and should remain the safest first firmware test.

In either mode, there is **no source-code path that turns on panel HV**:
`Inkplate5v2Bus::set_power(true)` still rejects requests with
`HardwareNotValidated`. I2S DMA is constructed only in the opt-in mode;
it sends no display rows. The opt-in build **does** modify hardware pins and
PMIC configuration, so it is not a substitute for read-only diagnostics.

The UART is the board's CH340C serial bridge. A 5-second snapshot shows
PCAL input/output levels and configurations, including P0.7 PGOOD and
P1.0 GPIO0 clock-enable state. The logic readback is **not** a voltage
measurement.

The opt-in build performs a **read-only preflight before configuring any
display control outputs or I2S pins**. It rejects asserted physical PGOOD,
an incorrectly configured PGOOD input, or an actively driven PCAL power/control
output (including WAKEUP, GMOD and SPV). Its board library repeats that check
before changing direct GPIO. A failed setup halts rather than retrying.

**Unresolved before activating display high voltage:** the HAL's I2S
`send()` start code still has unbounded waits (only transfer completion is
now time-limited), and source/clock/data high-Z shutdown is not fully
implemented. These are documented in the
[second-pass review](../../docs/inkplate5v2-second-pass-review.md).

## Design notes

- Chip: classic ESP32, not ESP32-S3.
- I2C0: SDA GPIO21, SCL GPIO22.
- Direct panel controls: LE GPIO2 (low), CKV GPIO32 (low),
  SPH GPIO33 (high).
- I2S1 source clock: GPIO0; eight data pins in board order:
  GPIO4, 5, 18, 19, 23, 25, 26, 27.
- DMA capacity: 336 bytes per row (320 source data + 16 padding clocks).
- Existing `esp-hal` I2S driver is retained; no DMA rewrite.
- **No physical boot or electrical validation has been performed.**

For the stages after low-voltage integration see the
[implementation plan](../../docs/inkplate5v2-implementation-plan.md).
