# ED052TC4 / Inkplate 5 Gen 2 — third independent source review

_2026-11-24. Scope: `integration/inkplate-5v2-bringup`. This is an additional adversarial pass, not a substitute for recovery testing or instrumented electrical verification._

## Audit method

Rather than reviewing only the last patch, this pass traced a whole refresh through the generic `EpdDisplay`, the `Inkplate5v2Bus`, the PCAL6416A/TPS65186 power state machine and classic ESP32 I2S1 DMA. It compared the ED052TC4 source and gate sequencing against Soldered's Arduino implementation and the actual configuration of `esp-hal` 1.1's parallel I2S driver. It challenged assumptions baked into the fake I²C/PGOOD devices with new fault scenarios.

## Confirmed findings and changes

### 1. P0 — cached `On` state could conceal loss of panel power between passes

Before this pass, `Inkplate5v2Bus::require_ready()` only checked `PowerState::On`. The controller could report `On` after the original startup even if a rail or output-enable signal went away before the next frame. The next `start_frame()` would continue controlling the panel.

**Implemented:** Before **every** new frame, `Inkplate5v2Power::verify_live_rails()` checks the TPS65186 PGOOD register, the separate PCAL P0.7 physical PGOOD level, and the expected PCAL output levels and directions for OE, VCOM, PWRUP, GMOD and SPV. Any disagreement latches `Faulted`; the bus requests best-effort shutdown and does not enter the vertical scan.

**Tests:** Mock PMIC rail dropout after one completed pass, independent physical PGOOD dropout, an unexpectedly deasserted OE control and an I²C PGOOD-read failure. These prove fail-closed *software behavior*, not that an energized panel can always be discharged by software.

### 2. P1 — I²C failure in the initial clock-gate check did not latch a fault

The `enable()` function checked the expander clock-gate configuration using `inspect_expander().await?` *before* assigning `Faulted`. If that read failed, the driver stayed `Idle` and allowed repeated attempts even though the observed board state was uncertain.

**Implemented:** An I²C fault in this pre-enable check now latches `Faulted`. The ordinary `ClockNotEnabled` result (a valid read that reports a disabled gate) remains recoverable while HV is off.

**Test:** Inject a failure on the expander read; verify state stays faulted, no PMIC ENABLE write occurred, and further `enable()` attempts are rejected.

### 3. P1 — safe PCAL initialization accepted an ACK without final register readback

`initialize()` programmed the PCAL output latches and directions low, but could mark the power controller `Idle` without checking whether the expander had actually latched those writes. An acknowledged-but-ignored write could therefore incorrectly certify panel-control GPIO as safe outputs.

**Implemented:** At the end of PMIC sequencer initialization, read back the expander and require every panel-control output latch low, every corresponding direction configured to output, the physical PGOOD input configured as input, and PGOOD low. Otherwise remain `Faulted`.

**Test:** Simulate a successful I²C ACK of the PCAL direction write that does *not* change the register. The controller must return `LowVoltageReadbackMismatch` rather than `Idle`.

### 4. P1 — fault injection previously covered only selected power-up writes

Existing I²C fault tests exercised one sequencer-register failure and one PGOOD-read failure. That left a blind spot at other points of the PMIC/expander enable sequence.

**Implemented:** A parameterized host test injects one failure at **each of the eight writes** in the power-up path, including the last output-enable write. It requires permanent fault latching, a shutdown attempt, OE/VCOM/PWRUP inactive and PMIC ENABLE cleared. This test runs in both blocking and async variants.

## Cross-checks that did not justify changes

- Soldered's ED052TC4 panel dimensions, nine grayscale phases, conditioning sequence and scanline byte count still match the `epdpi` implementation's intended behavior and existing host tests.
- The classic ESP32 eight data output pins selected by `esp-hal::TxEightBits` map to `I2S1O_DATA_0..7` in argument order; this matches Soldered's pin assignments.
- Soldered's `I2S1O_BCK_OUT` GPIO0 clock is **not** the same matrix output selected by `esp-hal` (`I2S1O_WS`). The actual physical timing/polarity relationship is unmeasured; this is **not closed** by data-packing tests.
- The already implemented DMA **completion** timeout is not an end-to-end timeout: `esp-hal` still contains unbounded waits inside its `send()` start procedure. The pass did not replace HAL, consistent with the decision not to rewrite DMA without proven need.

## Remaining P0s before any high-voltage unlock

1. **Bound or independently recover from HAL transmit-start hangs.** At present an I2S start stall can prevent software shutdown.
2. **Complete a board-specific clock/data/control high-Z lifecycle.** Soldered's `pinsZstate()` releases GPIO after power-down; this branch still owns GPIO and I2S in output mode.
3. **Review the first physical test safety case:** flash backup/recovery verified, no PMIC/VCOM EEPROM programming, explicit opt-in single attempt, tested shutdown behavior. Serial readbacks do not measure voltage or electrical timing.

## Other limitations not addressed by this pass

- A sudden rail drop **during** the current row cannot be caught by a check performed before the frame. Additional health polling during an active scan would perturb timing, and an independent hardware fail-safe may be necessary for a stronger guarantee.
- Physical PGOOD low and PMIC PGOOD zero do not rule out partially energized or unsafe analog rails.
- An I²C failure after asserting a low-voltage control output can leave unknown hardware state; the driver faults and requires manual recovery rather than claiming physical quiescence.
- The branch does not yet contain a hardware-validated PSRAM framebuffer and first-image refresh firmware.
- Mocked I²C state transitions and passed cross-target builds do **not** prove transistor-level signal ordering, rail voltages, or robustness to sudden reset.
- No real Inkplate 5V2 or its vendor-firmware backup was exercised during this audit.

## Testing strategy going forward

Keep expanding the fault matrix by failure site and observed effect, not by increasing test counts alone. Before any powered refresh, test a board recovery drill, GPIO/I2S cleanup, DMA start timeout/recovery, power-on and power-off readbacks, and one-shot visibly inspected vendor waveform behavior. CI must verify blocking and async host tests plus classic ESP32 firmware compilation. Hardware observations should be recorded separately from simulation evidence.

## Reference files

- [First source audit](inkplate5v2-software-audit.md)
- [Second independent review](inkplate5v2-second-pass-review.md)
- [Implementation plan](inkplate5v2-implementation-plan.md)
- [Soldered Inkplate 5V2 driver](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/boards/Inkplate5V2/Inkplate5V2Driver.cpp)
- [Soldered I2S1 implementation](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/system/UtilI2S/UtilI2S.cpp)
- [Soldered TPS65186 implementation](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/features/TPS65186/TPS65186.cpp)
- [esp-hal 1.1 parallel I2S source](https://github.com/esp-rs/esp-hal/blob/esp-hal-v1.1.0/esp-hal/src/i2s/parallel.rs)
- [ED052TC4 datasheet reference](inkplate5v2-datasheet.md)
