# Inkplate 5 Gen 2 integrated bring-up

This is the working integration branch for Soldered's Inkplate 5 Gen 2
(1280 x 720 ED052TC4, classic ESP32 WROVER-E). It temporarily combines
two independent, upstream-oriented changes:

- [PR #1](https://github.com/marre/epdpi/pull/1): ED052TC4 eight-gray
  waveform, panel-specific full-clear sequence, framework integration.
- [PR #2](https://github.com/marre/epdpi/pull/2): 8-bit classic ESP32 I2S1
  DMA transmitter and four-byte packing/byte-order tests.

The host integration test in `epdpi-esp/tests/inkplate5v2_pipeline_tests.rs`
compares the **actual vendor DMA pixel layout** with an independently
transcribed Soldered `display3b()` lookup-and-reversal algorithm for nine
passes and a nonuniform row using every one of the eight grayscale shades.
The `pack_inkplate5v2_scanline` helper reverses logical pixel order and
performs the I2S1 word swizzle without changing the generic I2S packer.
The hardware transmitter exposes `send_inkplate5v2_scanline` for a future
board-specific bus.

These comparisons prove software-equivalent DMA bytes, **not** physical
clock and data ordering. The manufacturer's direct register driver uses
`tx_right_first=0`, whereas esp-hal 1.1 configures `tx_right_first=1`.
This discrepancy requires measuring the clock and data signals before
any high-voltage display refresh.

## Hardware dependencies to implement here

1. PCAL6416A GPIO expander and TPS65186 PMIC control: **software
   controller implemented** in `epdpi-esp/src/inkplate5v2_power.rs`.
   The controller follows Soldered's register ordering, gates OE/VCOM on
   PGOOD, has bounded waits and a fault latch. Host tests cover failure paths.
   **Not yet wired to real ESP32 GPIO or tested on physical rails.**
2. The power controller now exposes `inspect_expander()` for read-only
   PCAL6416A Port 0/1 output and direction registers. This includes Port 1
   bit 0 (`GPIO0_ENABLE`). It does not drive the pins or wake the PMIC.
   Initialization checks PGOOD before modifying the sequencer and shutdown
   follows the manufacturer's WAKEUP-before-ENABLE=0 order. The clock-enable
   signal itself is not yet driven.
   Verify the PMIC and factory VCOM behavior on a physical device with
   non-driving diagnostics. No VCOM EEPROM writes are used.
3. The manufacturer-matched CKV/SPV/SPH/LE transitions are implemented as
   **host-tested signal-sequence primitives** in
   `epdpi-esp/src/inkplate5v2_timing.rs`. They do not access real GPIO,
   activate rails, or guarantee sub-microsecond physical timing. The later
   board bus must fail safe on signal/DMA errors.
4. A **safety-locked** `Inkplate5v2Bus: EpdParallelBus` is now in
   `epdpi-esp/src/inkplate5v2_bus.rs`. It owns the classic ESP32 I2S1
   transmitter, direct LE GPIO2 / CKV GPIO32 / SPH GPIO33, and the shared
   expander/PMIC controller. It uses the vendor's frame and row timing
   helpers and board-specific DMA byte layout. On a GPIO, I2C, or DMA error,
   it latches an error and requests best-effort power-down.

   The constructor and `inspect()` are non-driving. The explicit
   `prepare_low_voltage()` method changes GPIO states, configures the
   vendor's GPIO0 clock enable (PCAL Port 1 bit 0), programs PMIC sequencing
   while PGOOD reports zero, and does **not** enable panel bias rails.
   This is not read-only and must not be used as the first hardware test
   on an unknown board state.

   **Critically, `EpdParallelBus::set_power(true)` is locked and always
   returns `HardwareNotValidated`**. No display pass can be started through
   the bus. The implementation is deliberately not an approved firmware
   image: it still requires verified electrical timing, factory VCOM
   readback and a safe recovery procedure. No option currently bypasses
   the hardware lock.
5. Board example using PSRAM-backed framebuffers. The framework uses
   4 bits per pixel, so it needs two 460,800-byte buffers and a 336-byte
   line buffer (320 data bytes plus 16 trailing clocks).
6. Physical tests: power up without display scan, verify PGOOD,
   check clock and data pin order using a logic analyzer, then white/black
   full refresh, eight-level gray ramp, partial updates, ghosting and
   repeated power cycles.

**Safety**: This new power controller is still untested on hardware. Its
`enable()` method must only be called after the *direct ESP32 control pins*
(CKV, SPH, LE, data and clock) are configured to their manufacturer-required
idle levels. There is no integration with the display bus yet. Before enabling
rails on the only device, verify physical wiring, power-good and a recovery
procedure. If shutdown fails, the controller latches Faulted and refuses to
restart until reset and reinitialization.

**Safety**: Do not run a display update on hardware until the TPS65186
power-up and PGOOD paths are verified. Wrong rail sequencing can
damage the panel. Do not alter factory VCOM calibration casually.

## Bring-up stages and current gate

1. **Read-only:** inspect the expander state without changing any registers;
   compare Port 0 control pins, Port 1 GPIO0 clock-enable and the actual
   board's current state against the manufacturer.
2. **Low voltage:** only after verifying the board is idle, run explicit
   preparation while recording the I2C transactions. This configures GPIO0
   clock gating but not high-voltage rails.
3. **Signal analysis:** verify the real I2S1 clock/data and CKV/SPV/SPH/LE
   edges against Soldered with a logic analyzer. The vendor drives
   `tx_right_first=0` whereas esp-hal 1.1 uses `1`; the host-equivalent
   DMA byte stream alone is insufficient proof.
4. **High voltage:** not implemented as an exposed operation yet. Do not
   unlock until signal order, PMIC sequencing, VCOM calibration, failure
   recovery and timeouts are independently reviewed.

Never flash this branch as an active Inkplate refresh firmware. It is
currently a host-validated, cross-compiled library workspace.

## Branch and upstream strategy

This branch is an integration workspace and is **not intended to merge
into fork main**. The existing focused PRs remain open. Once hardware
is validated, port verified improvements into the appropriate topic PRs,
extract reusable board support if warranted, and submit separately to
melastmohican/epdpi. Keep the generic core independent of ESP32 and
Inkplate-specific hardware.

## Sources

- Soldered Inkplate Arduino library, `src/boards/Inkplate5V2` and
  `src/system/UtilI2S`: https://github.com/SolderedElectronics/Inkplate-Arduino-library
- esp-hal 1.1 classic ESP32 parallel I2S:
  https://docs.espressif.com/projects/rust/esp-hal/1.1.0/esp32/esp_hal/i2s/parallel/index.html
