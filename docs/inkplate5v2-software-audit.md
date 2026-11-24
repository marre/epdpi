# ED052TC4 / Inkplate 5V2: completed software audit

_2026-11-24. Integration branch: `integration/inkplate-5v2-bringup`._

## Scope and verdict

This audit checks **software-level** panel data, classic ESP32 I2S1 configuration, ED052TC4 waveform, power-control state machine and the staged test firmware. It does **not** establish electrical timing or analog high-voltage correctness. No physical device has been tested.

**Decision:** Keep `epdpi` and the existing `esp-hal` I2S1 transport. The source review found testable issues that we fixed, but does not prove that the remaining HAL/vendor differences cause a panel failure. Do not rewrite the entire transport without a concrete failure or validated limit violation.

## Evidence and comparison

| Item | Soldered Inkplate 5V2 | Rust implementation | Status |
| --- | --- | --- | --- |
| Chip and transport | Classic ESP32, I2S1, 8-bit parallel DMA | Classic ESP32 `esp-hal::i2s::parallel::I2sParallel` | Software verified |
| Source clock | GPIO0, I2S1 clock path | GPIO0, I2S1 HAL clock output | Pin mapping verified; physical clock edge not measured |
| Data pins D0…D7 | GPIO4, 5, 18, 19, 23, 25, 26, 27 | Same mapping in the board firmware | Verified from sources |
| Other direct pins | LE GPIO2, CKV GPIO32, SPH GPIO33 | Same mapping | Verified from sources |
| SPV | PCAL6416A Port 0 bit 2, via I²C | Shared I²C owner in the power controller | Software verified |
| Row payload | 320 pixel bytes + 16 trailing source clocks | `336`-byte DMA row and identical padding | Host-tested |
| Eight gray levels | 3-bit physical shades, nine passes | Adjacent Gray4 values map to eight shades | Host-tested |
| Full conditioning | 48 full-panel scans + nine gray + one idle scan | Same sequence in `Quality16` | Host-tested |
| Physical source byte order | Vendor direct I2S FIFO/clock configuration | HAL 4-byte swizzle + vendor row reversal | **Raw DMA equivalence tested; wire order unverified** |
| `tx_right_first` | 0 | 1 (in `esp-hal` 1.1) | Difference documented; no speculative register patch |
| FIFO and stopping | `tx_fifo_mod=1`, `tx_data_num=1`, `tx_stop_en=0` | HAL `tx_fifo_mod=1`, `tx_data_num=32`, `tx_stop_en=1` | Internal difference; effect requires testing |
| DMA start/finish | Descriptor armed before SPH; TX starts immediately; SPH rises at EOF | Row packed before SPH, HAL still prepares descriptor after SPH and returns resources before raising SPH | **Unmeasured risk; not proven equivalent** |
| Row latch (XLE) | Direct LE high/low GPIO pulse | Explicit 1 µs waits around and during LE pulse | Conservative software minima; actual edges unmeasured |
| Panel startup | TPS65186, PGOOD, VCOM/OE after rails | Requires PMIC all-rails status **and** separate physical PCAL P0.7 PGOOD | Host-tested fault gate |
| Normal shutdown | Power off without permanent driver fault | Normal shutdown now leaves bus reusable; errors latch fault and attempt shutdown | Host-tested in blocking/async |
| Reset/recovery | Known Soldered firmware | First-stage read-only diagnostic, dual full-flash backup instructions | Firmware builds; **physical recovery unverified** |

## Source-level I2S review

The board firmware configures the I2S1 transmitter at `Rate::from_mhz(16)`, matching the nominal Soldered BCK clock rate. Both drivers select eight-bit LCD mode, a BCK divider of two and FIFO mode one. The vendor's direct register setup and `esp-hal`'s `setup()` are **not** instruction- or register-identical. The HAL internally connects its I2S1 `WS` signal to the supplied clock pin, whereas Soldered connects `I2S1O_BCK_OUT`. The actual physical relationship between sample data, GPIO0 clock and SPH cannot be proven from the application-level bytes.

The host integration tests independently transcribe the Soldered `display3b()` byte/pixel reversal and verify all nine gray waveform passes, including nonuniform rows. New tests verify all four source drive codes, edge bytes, padded samples and no overwrite beyond the DMA row. **These are software-format tests, not physical wire captures.**

The research identifies an upper bound on the XSTL/SPH relationship to XCL (approximately 240 source clocks, about 15 µs at 16 MHz). Since `esp-hal::I2sParallel::send()` sets up DMA *after* SPH is asserted, this is an outstanding physical timing uncertainty. It is not meaningful to mark it tested merely because a compilation or host test passed. If a specific image error implicates first/last source pixels or source clock alignment, investigate this difference first.

The HAL also has internal waits without a user-supplied deadline during start; avoid claiming that the existing blocking DMA path has a comprehensive timeout. A later verified hardware driver may need a more deterministic start/EOF contract if testing exposes a problem.

## Bugs and defensive improvements addressed

1. **Clean shutdown was treated like a fault:** `Inkplate5v2Bus::set_power(false)` previously called `emergency_off()` and permanently latched `faulted`. Normal shutdown now preserves reusable state; genuine errors still latch and run best-effort cleanup. Regression tests exercise two full software power cycles while leaving public HV power-on locked.
2. **LE pulse lacked explicit minimum duration:** there are now conservative blocking one-microsecond gaps before, during and after XLE/LE assertion, with recorded-order tests. The source-clock and GPIO pulse widths are not electrically measured.
3. **Only one PGOOD indication was enforced:** after PMIC status reports all rails good, the power controller now also requires physical P0.7 PGOOD high before enabling VCOM or output enable. A simulated disagreement must fault and shut down without enabling panel outputs.
4. **More complete scanline coverage:** dedicated boundary and padding tests exercise drive codes 0…3 and verify the HAL byte permutation and panel-specific pixel reversal independently.

## Tests and limits

The standard workspace CI includes:

- Host tests (blocking and async) for panel waveforms, GPIO ordering, PMIC state transitions, and DMA data packing.
- `clippy`, docs and host checks, plus classic ESP32 cross-compilation.
- Separate Xtensa firmware builds for read-only and opt-in low-voltage board preparation.

**The hardware test gate remains unchanged:** `Inkplate5v2Bus::set_power(true)` rejects requests with `HardwareNotValidated` in all firmware builds in this branch. There is no ready-to-flash high-voltage or visible-refresh binary. Flash backup and ROM recovery must precede the first hardware experiment. The owner does not have measurement instruments, so subsequent visual/serial validation will remain electrically incomplete.

## What remains outside software verification

- Actual XCL clock polarity/frequency and pixel order on the board, including `tx_right_first` and WS versus BCK routing.
- Source-start (SPH/XSTL) latency relative to the first data clock and source-latch pulse width.
- TPS65186 output voltage levels, VCOM voltage, rail rise/fall/discharge timing and unintended source clocks at shutdown.
- The first physical image and repeated gray/ghosting measurements.

Do not silently equate correct CI output with safe electrical operation. The [implementation plan](inkplate5v2-implementation-plan.md) describes the staged on-device validation and later explicitly gated refresh work.

## Primary source material

- [Soldered Inkplate Arduino driver](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/boards/Inkplate5V2/Inkplate5V2Driver.cpp)
- [Soldered parallel I2S](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/system/UtilI2S/UtilI2S.cpp)
- [esp-hal 1.1 parallel I2S](https://github.com/esp-rs/esp-hal/blob/esp-hal-v1.1.0/esp-hal/src/i2s/parallel.rs)
- [Soldered Inkplate 5 Gen 2 hardware design](https://github.com/SolderedElectronics/Soldered-Inkplate-5-Gen2-hardware-design)
- [ED052TC4 panel specification reference](inkplate5v2-datasheet.md)
- [Archived deep research](inkplate5v2-research.md)
