# Inkplate 5 Gen 2 integrated bring-up

This is the working integration branch for Soldered's Inkplate 5 Gen 2
(1280 x 720 ED052TC4, classic ESP32 WROVER-E). It temporarily combines
two independent, upstream-oriented changes:

- [PR #1](https://github.com/marre/epdpi/pull/1): ED052TC4 eight-gray
  waveform, panel-specific full-clear sequence, framework integration.
- [PR #2](https://github.com/marre/epdpi/pull/2): 8-bit classic ESP32 I2S1
  DMA transmitter and four-byte packing/byte-order tests.

The host integration test in `epdpi-esp/tests/inkplate5v2_pipeline_tests.rs`
exercises the grayscale waveform through I2S1 word rearrangement and back
to the simulated on-wire data. It does not prove physical pin order, bus
timing or safe panel power.

## Hardware dependencies to implement here

1. PCAL6416A GPIO expander (controls SPV, OE, GMOD, WAKEUP, PWRUP and
   VCOM). Use a proven I2C implementation with precise error handling.
2. TPS65186 PMIC: preserve manufacturer register sequencing,
   VCOM calibration, rail readiness checks and safe power down.
3. A full `Inkplate5v2Bus: EpdParallelBus` composing the reusable ESP32
   I2S1 transport with direct GPIO and expander pins. Connect the
   board-specific gate/source start/end timing to scanline boundaries.
4. Board example using PSRAM-backed framebuffers. The framework uses
   4 bits per pixel, so it needs two 460,800-byte buffers and a 336-byte
   line buffer (320 data bytes plus 16 trailing clocks).
5. Physical tests: power up without display scan, verify PGOOD,
   check clock and data pin order using a logic analyzer, then white/black
   full refresh, eight-level gray ramp, partial updates, ghosting and
   repeated power cycles.

**Safety**: Do not run a display update on hardware until the TPS65186
power-up and PGOOD paths are verified. Wrong rail sequencing can
damage the panel. Do not alter factory VCOM calibration casually.

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
