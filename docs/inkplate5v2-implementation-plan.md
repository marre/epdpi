# ED052TC4 / Inkplate 5 Gen 2 — implementation plan

_Last revised: 2026-11-24. Working branch: `integration/inkplate-5v2-bringup`._

## Implementation progress (2026-11-24)

- **Software milestone reached:** [`firmware/inkplate5v2-bringup`](../firmware/inkplate5v2-bringup/README.md) builds a complete classic ESP32 binary linked to the actual `epdpi-esp` board stack. The default mode performs read-only PCAL inspections. An explicitly compiled `low-voltage-prep` mode initializes the direct control pins, I2S1 DMA routing and the PMIC sequencer while leaving panel high voltage locked.
- **CI verified:** [HV-locked firmware build](https://github.com/marre/epdpi/actions/runs/45447718321) passed both build modes and the independent first-stage diagnostic build.
- **Not yet performed:** Flash backup/recovery on the owner's device, first diagnostic execution, on-device low-voltage preparation, electrical verification or visible display updates. `set_power(true)` still returns `HardwareNotValidated` and neither new firmware mode sends a scanline.
- **Next parallel work:** Finish the targeted I2S/waveform protocol audit in Phase 3 while the hardware owner completes Phase 1's recovery prerequisites. The first high-voltage test is a later, separately reviewed milestone.

## Secondary review: additional gates (2026-11-24)

The [independent second-pass review](inkplate5v2-second-pass-review.md) found additional software defects and remaining hazards. This section supersedes any implied statement that cross-compilation alone makes the hardware ready for a powered refresh.

- **Implemented:** read-only low-voltage preflight **before** direct GPIO/I2S setup, stricter PGOOD/pin-state checks, separate physical PGOOD discharge verification, conservative bounded DMA **completion**, generic best-effort shutdown after failed power-on, and preservation of unknown ED052TC4 factory VCOM.
- **Still P0 before unlocking high voltage:** the HAL's `send()` start path includes **unbounded internal waits**, and Soldered-equivalent high-Z/clock quiescence is not implemented on the actual GPIO/I2S bus.
- **Still unverified:** actual GPIO0 pixel clock versus vendor BCK routing, source-latch window, physical rail behavior and ED052TC4 image quality. The existing firmware is intentionally non-driving.
- The physical backup, ROM recovery and first-stage diagnostic remain prerequisites. No on-device observations were obtained during this review.

## Goal and constraints

Build a reliable Rust driver for the **Soldered Inkplate 5 Gen 2** (ED052TC4, 1280 × 720 landscape, classic ESP32 WROVER-E), using the existing `epdpi` integration branch.

**First usable milestone:** a full-screen black/white image, all **eight physical gray levels**, correct pixel geometry, repeatable refreshes, and predictable startup/shutdown. Partial refresh and performance optimization come later.

The hardware owner **does not have an oscilloscope, logic analyzer, or voltmeter**. Plan for USB/serial diagnostics, host tests and visual checks. Such checks cannot establish analog rail voltages, signal edges or all datasheet timing limits; **a successful display test is functional evidence, not proof of electrical safety**. Explicitly record this limitation.

## Architecture and decisions

| Layer | Responsibility |
| --- | --- |
| `epdpi` | Generic framebuffer, panel geometry, waveform/refresh engine; no ESP32 dependency |
| ED052TC4 panel implementation | Panel-specific 8-gray, nine-phase waveform and cleaning behavior |
| `epdpi-esp` | Existing classic ESP32 I2S1 + DMA and board integration |
| Inkplate 5V2 board layer | Direct GPIO, PCAL6416A expander and TPS65186 power sequencing |
| Diagnostic/example firmware | Stepwise bring-up via USB serial; explicit display test only after gates |

- **Keep `epdpi` and the existing `esp-hal` I2S1 implementation.** The remaining delay from SPH assertion to HAL DMA start/completion differs from Soldered; this is **not an established fault** and a custom register-level DMA rewrite is **not on the critical path**.
- **Meet the panel's protocol needs, not Soldered's software instruction-for-instruction timing.** Keep known vendor ordering and nominal waveform as the conservative starting point.
- The `tx_right_first` difference, data-bit order, sample edge, clock configuration and 336-sample row layout **do** require a software audit. Existing host byte-equivalence tests do *not* prove the emitted physical order.
- Use the current **4-bpp logical framebuffer** and 8 physical shades; preserve the vendor's existing full-clear, conditioning and nine-phase grayscale sequence until full refresh has been tested.
- Preserve factory VCOM; never write calibration EEPROM as part of bring-up.
- High-voltage enable currently returns `HardwareNotValidated`. Keep this lock intact until the stated recovery, source review, power-control review and diagnostic gates are complete; then add a **separately opt-in, tightly bounded experimental display-test path**, not an unconditional production unlock.
- Any inconclusive PGOOD, shutdown or communication state is **faulted**, not assumed powered off. Do not automatically retry after a fault.

## Existing work to reuse

From [integration PR #4](https://github.com/marre/epdpi/pull/4):

- ED052TC4 panel/waveform implementation ([PR #1](https://github.com/marre/epdpi/pull/1)).
- Classic ESP32 I2S1 DMA transmitter and packing ([PR #2](https://github.com/marre/epdpi/pull/2)).
- `epdpi-esp/src/inkplate5v2_power.rs`: PCAL6416A/TPS65186 sequencing and host failure tests.
- `epdpi-esp/src/inkplate5v2_timing.rs`: host-tested CKV/SPV/SPH/LE control sequences.
- `epdpi-esp/src/inkplate5v2_bus.rs`: composed board bus and deliberate HV safety lock.
- `epdpi-esp/tests/inkplate5v2_pipeline_tests.rs`: vendor-derived row packing and waveform integration tests.
- `firmware/inkplate5v2-diagnostic/`: independent read-only diagnostic firmware and CI.
- [Backup/recovery instructions](inkplate5v2-recovery.md) and [current bring-up notes](inkplate5v2-bringup.md).

These are **software milestones only**; neither the board bus nor its supply rails have been validated on real hardware.

## Phase 1 — Backup, recovery and read-only diagnostics (P0)

**Tasks**

1. Identify the actual board revision, ESP32 chip, flash size and security restrictions.
2. Demonstrate repeatable ROM download-mode entry via USB, without modifying GPIO0 boot jumpers.
3. Read the complete original flash twice and confirm identical sizes and SHA-256 values. Store both copies **offline**, not in GitHub.
4. Verify the intended flash image offsets and partition layout before writing new firmware.
5. Run the read-only diagnostic, verifying UART, I2C at PCAL6416A address 0x20, Port 0/1 states and physical P0.7 PWR_GOOD reading. Do not wake the PMIC or enable rails in this phase.
6. Save observed register snapshots and identify unexpected board differences.

**Gate:** Recovery and two matching backups are verified; diagnostics run on the physical board. If ROM recovery, board identification or backups are uncertain, **stop before flashing experimental firmware**.

## Phase 2 — Complete native board integration and safe low-voltage setup (P0)

**Tasks**

1. Add minimal classic-ESP32 firmware composing real I2C (GPIO21/22), the I2S1 transmitter, direct LE/CKV/SPH pins and the existing board controller.
2. Audit the full pin map against Soldered's Inkplate 5V2 code and published V1.1.0 hardware design, particularly GPIO0 boot/clock gating and the shared expander I2C owner.
3. Implement explicit **read-only**, **prepare-low-voltage**, and **test-refresh** operations; no automatic power-up in constructors, startup or diagnostics.
4. Make low-voltage initialization configure verified idle levels, clock gate and PMIC *register sequencing* while rails remain off. Log readbacks and retain unrelated expander pin states.
5. Read factory VCOM and PMIC status without changing calibration; match the vendor's enable/PGOOD and shutdown ordering.
6. Complete I2S clock quiescence and appropriate GPIO output/high-impedance shutdown handling; latch errors and avoid automatic retries.
7. Add host tests for unexpected existing pin state, clock-gate readback failures, I2C errors, timeouts, invalid state transitions and shutdown cleanup.
8. Build the minimal firmware for `xtensa-esp32-none-elf` in CI, with the active display test disabled by default.

**Gate:** Software tests and ESP32 builds pass; on-device low-voltage setup reports expected register states, without energizing panel supplies.

## Phase 3 — Audit the existing transport and waveform, without rewriting DMA (P0)

**Tasks**

1. Independently compare panel pixel packing and row reversal with Soldered's `display3b()`; preserve coverage for nonuniform rows and all eight shades over all nine grayscale passes.
2. Inspect `esp-hal` I2S1 configuration versus Soldered: `tx_right_first`, sample edge, clock divider, FIFO packing and GPIO matrix outputs. Address **demonstrable** configuration errors, not merely different implementation choices.
3. Assert **320 data bytes + 16 trailing clocks** (336 samples per row), buffer lifetime, proper DMA-capable memory and bounded completion handling.
4. Test CKV/SPV/SPH/LE sequence order and the existing blocking microsecond waits. Account for extra I2C time on SPV; avoid claiming precise electrical timing from host event logs.
5. Keep row packing/preparation before SPH as already implemented. Document the remaining HAL descriptor-start and EOF/SPH differences as **unverified but not blockers by themselves**.
6. Add deterministic image patterns: white, black, checkerboard, corner/border markers, 8-gray bars and gradient.

**Gate:** No known byte-packing, pin-assignment, state-machine, waveform or power-control defect remains in the reviewed source and host tests. If a specific datasheet constraint is demonstrably violated, fix that constraint.

## Phase 4 — Explicitly gated, controlled first refresh (P0)

**Tasks**

1. Independently review Soldered-equivalent startup/shutdown, source bus state, bounded timeouts and VCOM readback. Document remaining uncertainty because physical rail/timing measurements are unavailable.
2. Add a **separate opt-in experimental refresh command/build setting**. Keep the normal diagnostic and default firmware HV-disabled. Require an explicit operator action to attempt each test.
3. On each attempt: confirm preconditions; initialize safe pins; configure and request the vendor power sequence; read PMIC status and PGOOD; only then enable panel output.
4. Execute **one** complete vendor-derived black/white refresh with an obvious border/corner test pattern. Always request shutdown afterward, including error paths. A shutdown call is best effort, not proof of physical rail discharge.
5. Log stage, first failure, DMA status, timeout and PMIC/expander readbacks to serial. No unattended retry, refresh loop, VCOM write or waveform tuning.
6. Inspect the result visually; stop and restore the reference firmware if behavior is unexpected.

**Gate:** An explicit one-shot black/white update renders correctly and status readbacks are consistent across startup and shutdown. Mark the result as **visually validated, electrically unmeasured**.

**Risk disclosure:** Even matching the reference sequence and receiving PGOOD does not guarantee that every high-voltage rail or transient is within specification. The operator must knowingly accept that residual risk before the first HV test. If preserving the only panel without such risk is mandatory, obtain instrumented validation instead.

## Phase 5 — Eight-gray full refresh (P0)

**Tasks**

1. Keep Soldered's full conditioning/clear sequence and nine grayscale passes unchanged initially.
2. Render solid white/black, eight shade bars, a ramp, checkerboard, fine lines and corner markers.
3. Check visual ordering, inverted shades, missing rows/columns, residual image and ghosting after contrasting images.
4. Verify that a new refresh starts from a known power and framebuffer state and ends in shutdown.
5. Compare outcomes against the reference firmware when possible; retain example photos and serial logs outside the repository if they contain private board information.

**Gate:** All eight physical levels are distinguishable; full refresh is repeatable and free from obvious addressing defects.

## Phase 6 — Reliability and regression (P1)

**Tasks**

- Run a repeatable series of alternating patterns and power cycles, initially supervised.
- Test 100 full refreshes **only after** earlier supervised trials are stable and the board is known to shut down predictably.
- Record refresh time and error counters over USB, as well as visible ghosting/degradation.
- Add fault-injection tests for DMA timeout, I2C failures, PGOOD errors, invalid transitions and interrupted setup/cleanup.
- Run blocking and async host checks and classic ESP32 cross-compilation in CI.
- Document unverified electrical properties separately from demonstrated functional reliability.

**Gate:** No reproducible image corruption or software-state failure across the regression campaign.

## Phase 7 — Partial refresh and optimization (P2)

After stable full grayscale support:

- Implement vendor-compatible black/white partial refresh first.
- Evaluate differential old/new framebuffers and EPDiy/FastEPD approaches where appropriate.
- Measure changes in refresh time, memory pressure and visible ghosting before adopting them.
- Introduce temperature-based waveform selection only when ED052TC4 data supports it.
- Consider register-level or custom DMA **only** if evidence links a persistent defect or hard performance requirement to the existing HAL path.

**Gate:** A measurable improvement without image-quality or reliability regression.

## Work items and dependencies

| Work item | Deliverable | Depends on |
| --- | --- | --- |
| A (P0) | Recoverable device and read-only diagnostic evidence | Existing recovery docs |
| B (P0) | Real board initialization and low-voltage firmware | A for on-device testing |
| C (P0) | Existing I2S/packing/waveform audit and regression tests | Existing integration code; can run alongside A/B |
| D (P0) | Explicit opt-in, one-shot full-refresh firmware | A + B + C; power/safety review |
| E (P0) | Eight-gray full-refresh verification | D |
| F (P1) | Reliability campaign and documented limitations | E |
| G (P2) | Partial refresh and measured optimization | F |

**Current software milestone:** The HV-locked firmware and source-level I2S/packing/waveform audit are implemented, with host regression tests for repeated shutdown, physical PGOOD agreement and source-latch timing. The next gate is physical backup/recovery and read-only diagnostics; only after those should the first controlled refresh path be enabled. Do **not** rewrite DMA without evidence.

## When to revisit SPH/DMA timing

Only elevate the remaining `esp-hal` descriptor-start/EOF overhead if there is a specific observed failure: missing first columns, unstable pixel alignment, repeatable scan corruption, failed source-clock behavior, or a confirmed datasheet timing violation. First isolate packing/configuration/clock problems. Then consider a small register-level optimization rather than replacing the whole architecture.

## Upstream and merging

Keep this branch as the integrated hardware-validation workspace ([PR #4](https://github.com/marre/epdpi/pull/4)); it is **not** a single upstreamable merge. Port generic ED052TC4/waveform improvements into PR #1 and reusable classic ESP32 transport improvements into PR #2 once validated. Keep board-specific power and GPIO support separate.

## References

- [Completed source-level software audit and remaining physical uncertainties](inkplate5v2-software-audit.md)

- [ED052TC4 datasheet and redistribution notice](inkplate5v2-datasheet.md)

- [Deep Research: ED052TC4 specifications, timing and library alternatives](inkplate5v2-research.md) — archived research; current decisions are recorded in this plan.


- [Detailed current implementation and vendor timing differences](inkplate5v2-bringup.md)
- [Flash backup and recovery procedure](inkplate5v2-recovery.md)
- [Soldered Inkplate Arduino library](https://github.com/SolderedElectronics/Inkplate-Arduino-library)
- [Inkplate 5 Gen 2 hardware design](https://github.com/SolderedElectronics/Soldered-Inkplate-5-Gen2-hardware-design)
- [ED052TC4 panel datasheet](https://www.laskakit.cz/user/related_files/ed052tc4_final_ver_2.pdf)
- [EPDiy](https://github.com/vroland/epdiy)
- [FastEPD](https://github.com/bitbank2/FastEPD)
