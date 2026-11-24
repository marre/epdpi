# ED052TC4 / Inkplate 5 Gen 2 — independent second-pass software review

_Reviewed 2026-11-24 on `integration/inkplate-5v2-bringup`. Status: source fixes implemented; see CI and known limitations below. This does not represent physical validation._

## Purpose

The previous review found issues late in the process. This second pass therefore reviewed the **complete path again**, including ordinary and failed startup, GPIO/I2S ownership, the PCAL6416A/TPS65186 state machine, shutdown, framebuffer/VCOM configuration, and what the automated tests do **not** prove. The goal is to identify assumptions masked by mocks, not merely repeat the prior test suite.

The board owner has **no voltage or waveform measurement equipment**. Keep the HV interlock; distinguish logic-level observations and software tests from electrical safety.

## Confirmed defects and changes

| Priority | Finding | Mitigation in the branch | Verification |
| --- | --- | --- | --- |
| P0 | Opt-in low-voltage firmware configured direct panel outputs and I2S before deciding whether the panel might still be powered | Added read-only `preflight_low_voltage()` in both firmware **before pin routing** and board bus **before idle GPIO writes**, with fail-closed P0.7 PGOOD and output-direction checks | Host tests inject physical PGOOD high and assert **zero** I²C writes, GPIO writes, and DMA transmissions |
| P0 | Preflight originally only checked OE, PWRUP and VCOM, allowing interrupted WAKEUP/GMOD/SPV output states | Reject *any* actively asserted PCAL Port 0 panel-control output, even while PGOOD is low | Host fault tests cover WAKEUP, GMOD and SPV independently |
| P0 | Shutdown treated `TPS65186::PGOOD == 0` as definitive discharge even if the separate physical PGOOD input remained high | After PMIC reaches zero, also read PCAL P0.7 and require it low before declaring Idle; discrepancy times out and faults, PMIC shutdown still attempted | Host test forces P0.7 high after PMIC reads zero |
| P1 | The DMA completion wait in `esp-hal` was unbounded | Poll HAL transfer `is_done()` with a conservative 25 ms completion deadline; timeout drops/stops the transfer, permanently invalidates transmitter and latches the board fault | Classic ESP32 cross-build; the actual timeout requires hardware fault injection |
| P1 | Generic panel driver returned immediately when `set_power(true)` failed, even though rails might be partially on | On any power-on error, attempt `set_power(false)` before returning the original error | New blocking/async test bus rejects power-on; asserts power-off attempt and no frame data |
| P1 | ED052TC4 `VCOM_MV=None` silently became a fabricated `-2000` mV target in `EpdDisplayBuilder` | Preserve `Option<i16>` instead of manufacturing a VCOM; `vcom_mv()` now returns `None` for unconfigured factory-calibrated panels | New ED052TC4 regression; existing ED047TC1 tests updated for `Some(...)` |
| P1 | Startup could reject a single transient mismatch where PMIC PGOOD is ready before PCAL input catches up | Wait for both indicators until the bounded startup deadline; do not enable VCOM/OE on mismatch | Fake low-PGOOD input and timeout tests |
| P2 | Failure on a preflight read could leave the bus reporting a clean shutdown when it had been faulted before initialization | Preserve the bus fault on `set_power(false)` when the power controller never left Uninitialized | Host regression for rejected preflight |

The changes to `vcom_mv()` are a **public API signature change** in this integration branch; callers should handle `Option<i16>`, especially if configuring a PMIC. No code in this branch writes the ED052TC4's factory VCOM calibration.

## Issues **not** resolved and why

### 1. P0 before HV unlock: HAL's internal start waits are unbounded

A 25 ms completion deadline does **not** constrain `I2sParallel::send()` itself: the HAL's `tx_start()` waits for FIFO state and transmitter activity *before* returning a transfer handle. A stuck peripheral during those waits could still prevent the driver from reaching its best-effort shutdown.

**Required before powered continuous operation:** bound/abort the start sequence, or provide a separate independently acting hardware watchdog/fail-safe and prove its shutdown/recovery behavior. Do not mislabel the completion timeout as a full DMA transaction timeout. A narrow low-level adjustment may be needed; no complete DMA rewrite has been justified.

### 2. P0 before HV unlock: high-Z and clock quiescence are incomplete

Soldered's `einkOff()/pinsZstate()` returns direct GPIO and pixel/clock pins to inputs after shutting down. Current generic `OutputPin` ownership and `esp-hal` I2S setup do **not** implement this entire board-specific high-Z lifecycle. The clock-enable expander bit is not an electrical proof of stopped source clock.

**Required before approved refresh:** define an explicit suspend/shutdown plan for clock/data/control outputs, including non-error and timeout paths. Ensure any changes allow the next refresh or require a deliberate reboot.

### 3. P1: the real wire signals remain unknown

The vendor uses `I2S1O_BCK_OUT`; the HAL's parallel driver maps its clock parameter to `I2S1O_WS`. Both are associated with I2S LCD operation, but the exact sample-edge relationship on **this physical board** is not demonstrated. `tx_right_first`, FIFO settings, descriptor-start latency and SPH-to-clock relationship also differ from Soldered. Host byte tests do not resolve these issues. Compare visible results against the vendor reference when the board can be used; do not assert electrical equivalence.

### 4. P1: low-voltage failure can leave intermediate register state

If an I²C write fails after it was accepted electrically, the software may not know whether WAKEUP or the GPIO0 gate was changed. The low-voltage firmware latches failure and halts, rather than repeating potentially unsafe writes. A future powered refresh will require a documented abort/recovery path and explicit safe-state verification.

### 5. P1: no actual framebuffer/PSRAM-backed refresh application yet

ED052TC4's 1280 × 720 framebuffer is **460,800 bytes** at 4 bpp. Storing both current and previous images needs at least **921,600 bytes**, excluding scanlines, DMA-capable internal RAM, stacks and other data. The present read-only/low-voltage firmware does not allocate and validate its required PSRAM or produce a visible image.

### 6. Unmeasurable here: physical rails and signal integrity

PMIC registers and PCAL logic readbacks cannot verify voltages, rise/fall edges, VCOM analog level, the complete discharge time, or the absence of transient clock pulses. A successful on-device display test will establish **functional** behavior, not full datasheet compliance. Treat it as explicitly accepted residual risk if proceeding without instruments.

### 7. Recovery gate has not been executed

No full-flash backup, ROM boot recovery drill or on-device read-only diagnostic result has been supplied. That prevents any defensible claim that experimental flashing has been proven recoverable.

## What is safe to do next

1. Keep all high-voltage entrypoints locked (`HardwareNotValidated`). The current opt-in mode is **low-voltage only**.
2. Finish the CI matrix for blocking/async tests, classic ESP32, ESP32-S3, RP2040 and the independent diagnostic firmware; do not equate a successful cross-build to real device validation.
3. Complete the physical backup/recovery procedure and gather read-only serial diagnostics.
4. Reassess the specific **HAL start wait** and **high-Z shutdown** blockers *before* adding an explicitly opt-in powered first-image build.
5. Only then perform the supervised one-shot vendor-waveform test, with no automatic retries or VCOM EEPROM writes.

## Source material

- [First-pass audit](inkplate5v2-software-audit.md)
- [Current implementation plan](inkplate5v2-implementation-plan.md)
- [Soldered board driver](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/boards/Inkplate5V2/Inkplate5V2Driver.cpp)
- [Soldered classic ESP32 I2S implementation](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/system/UtilI2S/UtilI2S.cpp)
- [esp-hal 1.1 parallel I2S implementation](https://github.com/esp-rs/esp-hal/blob/esp-hal-v1.1.0/esp-hal/src/i2s/parallel.rs)
- [ED052TC4 datasheet reference / redistribution notice](inkplate5v2-datasheet.md)
- [Flash backup and recovery](inkplate5v2-recovery.md)

_The findings and mitigation claims above are source-based. The actual board remains untested._
