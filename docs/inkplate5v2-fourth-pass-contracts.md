# Inkplate 5 Gen 2 / ED052TC4 — fourth independent review: interface contracts

_2026-11-24. Branch `integration/inkplate-5v2-bringup`. A source and simulation review, **not** an electrical or on-device test._

## Different angle

The first three passes focused on I2S/PMIC source matching and individual failure paths. This pass asks what each layer **promises to its caller**, and whether those promises hold for every combination of panel, refresh mode, incomplete frame and error.

The key invariants:

1. A successful refresh must execute an actual supported drive waveform, not just an idle frame.
2. An unsupported request must be rejected **before** touching panel power, hardware signals or the saved previous-image record.
3. A successful ED052TC4 frame must contain exactly 720 completed scanlines; an incomplete or oversized frame is not a successful gate scan.
4. A failed refresh must not falsely commit the new framebuffer as the observed glass state.
5. The public `Inkplate5v2Bus::set_power(true)` interlock stays locked until hardware recovery and verification.

## Confirmed defect 1: successful no-op for unsupported refresh modes

**Severity: correctness / P1.**

ED052TC4 provides its native 9-phase grayscale LUT and 48 conditioning frames, but has **no** M5GFX quality, text, fast or fastest LUTs. Previously, `WaveformEngine::passes::<Ed052tc4>()` returned zero for those modes. `EpdDisplay::run_refresh()` then sent only the panel's post-refresh idle pass, returned success and copied the new framebuffer into `prev_buffer`. The caller believed a new image had been displayed, even though no drawing waveform had run.

There was also no evidence that the generic 4-pass FastMono pulse is valid for this ED052TC4 glass: Soldered's own binary update has a different waveform. Treating this as universally supported was another unsupported assumption.

### Fix

- `RefreshMode::supported_for::<PANEL>()` now checks that each requested mode has the required nonempty, correctly shaped LUT; `Clear` requires an actual non-neutral cleaning step.
- `EpdPanel::SUPPORTS_FAST_MONO` defaults **false**. ED047TC1 explicitly opts in to its tested mode; ED052TC4 does not.
- `EpdDisplay::{refresh, refresh_rect, refresh_dirty, clear}` reject unsupported modes *before* calling `set_power(true)`.
- Their return type is now `Result<(), RefreshError<BUS::Error>>`, with explicit `UnsupportedMode(RefreshMode)` and `Bus(E)` variants. This is an **intentional public API signature change**, appropriate for this unreleased integration branch. Callers must adjust pattern matches and `?` conversions; errors are never silently downgraded to another refresh mode.
- Tests on ED052TC4 attempt all M5 modes and FastMono through full, rectangular and dirty-refresh entrypoints. Assert no power or bus events, no previous-image change and no dirty-region loss; execute on blocking **and** async builds.

## Confirmed defect 2: the physical gate-row count was unvalidated

**Severity: sequencing / P1.**

The board bus previously allowed `start_frame()`, then `end_frame()` after zero rows, or an arbitrary number of `write_scanline()` / `next_row()` calls, while still reporting a successful frame. This permitted the generic bus trait to violate the physical ED052TC4 requirement that every frame advances 720 gate lines.

### Fix

- Added `INKPLATE5V2_ROWS=720`, `completed_rows` tracking and a dedicated `FrameRowCount { expected, actual }` error.
- A frame starts at row zero. `next_row()` counts each successfully latched scanline, and an extra `write_scanline()` is rejected **before** preparing or starting DMA.
- `end_frame()` succeeds only when exactly 720 rows have completed. An incorrect count triggers fault latching and best-effort shutdown.
- Normal and emergency shutdown reset the logical counter.
- Tests exercise a zero-row frame, a 721st attempted row, and two consecutive correctly sized frames. The public high-voltage interlock is unchanged.

## Independent cross-check

The official Soldered Inkplate 5 Gen 2 KiCad sources place the TPS65186 `PWR_GOOD` net on PCAL6416A P0_7: the expander symbol and `PWR_GOOD` global label are in `CAD/V1.1.0/MCU.kicad_sch`, with the source originating from `CAD/V1.1.0/POWER.kicad_sch`. That supports the existing software's *logical pin mapping*, but does not measure the actual board level or prove electrical independence under failure.

The manufacturer clock mapping (`I2S1O_BCK_OUT`) still differs from the generic esp-hal parallel driver's mapped `I2S1O_WS`. A host-side byte-packing test cannot verify this clock relationship, so the prior no-HV decision remains.

## Residual risks highlighted by the contract review

- **After an interrupted powered refresh, `prev_buffer` may no longer represent the glass.** The driver avoids committing it on a returned error, but it cannot undo changes already made to the glass. Retrying differential M5/FastMono without resynchronizing by a complete clear/quality refresh can produce incorrect results. A robust future design should explicitly represent record validity across driver reconstruction/reboot, not infer it from raw memory. The new unsupported-mode checks avoid a specific false success but do not solve all partial-update recovery.
- **Power-state checking is per frame, not per row.** A rail loss mid-frame is still not guaranteed detectable before further row clocks.
- **Cancellation/reset can interrupt cleanup.** The async contract does not guarantee `set_power(false)` executes if a future is dropped; the physical high-voltage test needs a watchdog/recovery strategy.
- **HAL DMA start has unbounded waits; GPIO/data/clock high-Z teardown remains incomplete.** Do not unlock HV until these are addressed or explicitly bounded by a verified hardware safety mechanism.
- The current branch still lacks a hardware-tested PSRAM framebuffer / first-image firmware and verified physical recovery procedure.

## Status and next gates

Review and test conclusions must be sourced to the **latest source commit**. A green CI result on an older commit does not establish correctness after these API and row-cardinality changes.

This pass changes no VCOM EEPROM registers, does not run a DMA transfer on the physical board, and does not enable the panel's high-voltage supplies. [PR #4](https://github.com/marre/epdpi/pull/4) remains a draft.

## References

- [Third review](inkplate5v2-third-pass-review.md)
- [Second review](inkplate5v2-second-pass-review.md)
- [Implementation plan](inkplate5v2-implementation-plan.md)
- [Soldered panel driver](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/boards/Inkplate5V2/Inkplate5V2Driver.cpp)
- [Official board hardware](https://github.com/SolderedElectronics/Soldered-Inkplate-5-Gen2-hardware-design)
- [esp-hal classic ESP32 I2S source](https://github.com/esp-rs/esp-hal/blob/esp-hal-v1.1.0/esp-hal/src/i2s/parallel.rs)
