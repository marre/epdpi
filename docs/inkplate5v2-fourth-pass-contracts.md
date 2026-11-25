# Fourth review: interface contracts

**Hardware:** Inkplate 5 Gen 2 / ED052TC4  
**Review date:** 2026-11-24  
**Branch:** `integration/inkplate-5v2-bringup`

> **Scope:** Source review and host simulation only. No physical board testing, electrical measurements, DMA transfer on the board, or high-voltage activation.

## At a glance

This review changed the angle of investigation. The first three passes concentrated on I2S and PMIC reference matching and individual failure paths.

The fourth pass instead asked: **What does each layer promise its caller, and does it keep that promise when given an unsupported mode, an incomplete frame, or an error?**

Two correctness defects were found and fixed.

| Finding | Previous behavior | Resolution |
| --- | --- | --- |
| Unsupported ED052TC4 refresh modes | Could report success after an idle-only frame and incorrectly record the requested image | Reject the mode before power-on |
| Incorrect gate-row count | Could accept an incomplete frame or transmit beyond the 720 physical rows | Enforce exactly 720 completed rows per frame |

The high-voltage interlock remains in place.

## Contracts checked

A correctly implemented display must satisfy these invariants:

1. **Actual refresh:** A successful refresh executes a supported drawing waveform, not just an idle frame.
2. **No side effects on invalid requests:** Unsupported modes are rejected before changing panel power, hardware signals, or the previous-image record.
3. **Complete frame:** ED052TC4 needs exactly **720 completed scanlines** per frame. Too few or too many is an error.
4. **Accurate image record:** An unsuccessful refresh must not record the requested framebuffer as successfully displayed.
5. **HV safety:** `Inkplate5v2Bus::set_power(true)` remains locked until recovery and physical hardware validation are complete.

---

## Finding 1 — Unsupported refresh modes appeared to succeed

**Priority:** P1 · Correctness

### What was wrong

The ED052TC4 has:

- A native **nine-phase grayscale waveform**.
- A **48-frame conditioning sequence**.
- **No** M5GFX quality, text, fast, or fastest waveform tables.

Previously, the generic refresh engine returned zero drawing passes for a missing M5 waveform. It then:

1. Sent the panel's final idle frame.
2. Returned success.
3. Copied the requested framebuffer into `prev_buffer`.

**Result:** Software reported an image as displayed even though no drawing waveform had run.

There was a second unsupported assumption: the generic four-pass `FastMono` waveform had been available for ED052TC4, although Soldered's own binary update uses a different waveform. Its suitability for this panel has not been established.

### What changed

**Waveform eligibility**

- `RefreshMode::supported_for::<PANEL>()` checks that the required waveform tables exist and have valid shapes.
- `Clear` must include a real, non-neutral cleaning step.
- `EpdPanel::SUPPORTS_FAST_MONO` defaults to `false`.
- ED047TC1 explicitly opts into its tested `FastMono` implementation; ED052TC4 does not.

**Refresh API**

All four entry points now reject unsupported modes *before* `set_power(true)`:

- `refresh()`
- `refresh_rect()`
- `refresh_dirty()`
- `clear()`

They return:

```rust
Result<(), RefreshError<BUS::Error>>
```

The error distinguishes:

- `RefreshError::UnsupportedMode(mode)` — invalid for this panel.
- `RefreshError::Bus(error)` — underlying bus or power failure.

This is an **intentional public API change** on the unreleased integration branch. Callers may need to update error matches and `?` conversions. There is no silent fallback to another refresh mode.

### Regression coverage

The ED052TC4 tests attempt every unsupported M5 mode and `FastMono` through full-screen, rectangular, and dirty-refresh APIs.

They verify that rejection causes:

- No power or bus activity.
- No mutation of `prev_buffer`.
- No loss of the dirty region.

Both blocking and async configurations are covered.

---

## Finding 2 — The bus did not enforce frame length

**Priority:** P1 · Scan sequencing

### What was wrong

Previously, the board bus could accept either of these sequences as a successful frame:

- `start_frame()` followed immediately by `end_frame()`, with **zero rows**.
- More than 720 `write_scanline()` / `next_row()` operations.

That violated the ED052TC4's physical requirement of **720 gate rows per frame**.

### What changed

The bus now tracks frame progress using:

- `INKPLATE5V2_ROWS = 720`
- `completed_rows`
- `FrameRowCount { expected, actual }`

The rules are:

1. `start_frame()` resets the count to zero.
2. Each successfully completed `next_row()` advances the count.
3. A 721st scanline is rejected **before** preparing or starting DMA.
4. `end_frame()` succeeds only after exactly 720 rows.
5. Invalid counts latch a bus fault and request best-effort shutdown.
6. Normal and emergency shutdown clear the logical row counter.

The public high-voltage interlock is unchanged.

### Regression coverage

Tests cover three situations:

| Case | Expected result |
| --- | --- |
| End a frame after zero rows | Reject and fault |
| Attempt a 721st row | Reject before DMA |
| Complete two consecutive 720-row frames | Succeed and reset the count for the next frame |

---

## Independent hardware cross-checks

### Power-good pin mapping

The official Soldered KiCad design connects the TPS65186 `PWR_GOOD` signal to **PCAL6416A P0_7**.

Relevant source files:

- `CAD/V1.1.0/MCU.kicad_sch` — PCAL expander and `PWR_GOOD` label.
- `CAD/V1.1.0/POWER.kicad_sch` — source of the `PWR_GOOD` net.

This supports the **logical pin mapping** used by the driver. It does **not** prove the actual pin voltage or independence under an electrical fault.

### I2S clock mapping

The manufacturer uses `I2S1O_BCK_OUT`, while the generic esp-hal parallel driver maps `I2S1O_WS`.

Host-side byte-packing tests cannot establish whether the two produce equivalent physical clock behavior.

**Consequence:** The no-high-voltage decision remains in effect.

---

## Remaining risks

The fixes above close specific contract violations. They do **not** close the following risks.

### 1. Previous-image validity after interruption

An interrupted powered refresh may have already changed some pixels, even though the operation returns an error and leaves `prev_buffer` unchanged.

This means:

- The recorded image may no longer match the glass.
- Retrying a differential M5/`FastMono` update can produce an incorrect image.
- Retrying safely may require a complete clear and quality refresh.

**Recommended design change:** Track explicitly whether the previous-image record is trustworthy, including across driver reconstruction and reboot. Do not infer validity from buffer contents alone.

### 2. Undetected rail loss within a frame

Power-good is checked before each frame, **not before every row**.

A rail failure during a frame can therefore occur before the software notices it.

### 3. Interrupted cleanup

Dropping an async future or resetting the MCU can interrupt the shutdown path. The API cannot guarantee that `set_power(false)` runs.

A first high-voltage experiment needs a deliberate watchdog and recovery strategy.

### 4. Incomplete DMA and GPIO safety work

Two previously identified blockers remain:

- HAL's DMA **start** path contains unbounded waits.
- Display control, clock, and data pins still lack complete high-impedance teardown.

Do **not** unlock high voltage until these are resolved or a verified hardware safety mechanism provides equivalent protection.

### 5. Hardware verification is still missing

The branch does not yet have a hardware-tested PSRAM framebuffer / first-image firmware or a verified physical recovery procedure.

---

## Validation and release gates

A green CI run on an **older commit** is not evidence that newer API or frame-count changes passed. Review conclusions must refer to the latest source revision.

This pass:

- Did **not** program TPS65186 VCOM EEPROM registers.
- Did **not** send DMA data to a physical display.
- Did **not** enable the panel's high-voltage rails.

[PR #4](https://github.com/marre/epdpi/pull/4) remains a **draft**.

## References

**Earlier work**

- [Third review](inkplate5v2-third-pass-review.md)
- [Second review](inkplate5v2-second-pass-review.md)
- [Implementation plan](inkplate5v2-implementation-plan.md)

**Manufacturer and HAL sources**

- [Soldered Inkplate 5V2 panel driver](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/boards/Inkplate5V2/Inkplate5V2Driver.cpp)
- [Soldered Inkplate 5 Gen 2 hardware design](https://github.com/SolderedElectronics/Soldered-Inkplate-5-Gen2-hardware-design)
- [esp-hal classic ESP32 I2S parallel source](https://github.com/esp-rs/esp-hal/blob/esp-hal-v1.1.0/esp-hal/src/i2s/parallel.rs)
