# Inkplate 5 Gen 2: PMIC research and upstream failure history

Reviewed 2026-11-26. Target: ED052TC4, TPS65186, PCAL6416A and classic ESP32.
This is the supporting research for the [implementation plan](inkplate5v2-implementation-plan.md)
and [power controller](../epdpi-esp/src/inkplate5v2_power.rs).
See [PMIC hardening](inkplate5v2-pmic-hardening.md) for implementation details and
[physical bring-up](inkplate5v2-hardware-validation.md) for observations from our board.

## Summary and evidence quality

- The Soldered Arduino library has repeatedly changed power sequencing to fix
  real display failures. Its E4/1B order is intentional, although the rationale
  for the specific shutdown order on ED052TC4 is not documented.
- Independent projects use different sequences. A TPS65185 Carta 1300 failure
  was resolved by a different startup order and delay, with I2C captures from
  a working commercial controller. This is strong evidence that sequencing is
  panel-dependent; it is not an ED052TC4 prescription.
- Soldered reproduced a persistent PG=0xA0 partial-power failure on the exact
  Inkplate 5 Gen 2 board. Other users reported similar failures on Inkplate 10
  V2. There is no established software-only recovery or root-cause fix.
- The original vendor supply-first startup was changed in the shared PMIC
  refactor. A later experimental Inkplate 10 patch restores that ordering.
  Backfeeding is plausible and independently reported on a different EPD board.
- Our provisional baseline is the historically corrected vendor E4/1B order,
  factory delays and calibration, logic supply first, verified PG before
  VCOM/OE, bounded waits, fault latching and verified shutdown. The extra
  settling dwell is a conservative software precaution, not proof of electrical
  compliance.
- None of the source comparisons or our standby tests measures actual rail
  decay, VDD ramp, VCOM settling or the panel's discharge curve. High voltage
  remains locked. No instruments are available for physical waveform measurement.

Evidence categories used below: source inspection establishes what code
requests; a commit message records the developer's stated motivation; a
hardware report establishes what its author observed; a datasheet establishes
specified requirements. None alone proves that another board is safe.

## Hardware and register model

Primary references:

- [TI TPS65186, SLVSB04A](https://www.ti.com/lit/ds/symlink/tps65186.pdf),
  especially Fig. 2, operating states, ENABLE, VCOM, PG and sequencer registers.
- [TI TPS65185](https://www.ti.com/lit/ds/symlink/tps65185.pdf) for comparison;
  do not assume identical silicon, defaults or wiring.
- [ED052TC4 Rev 2.0](https://www.laskakit.cz/user/related_files/ed052tc4_final_ver_2.pdf),
  sections 6 and 7, especially power-on and power-off timing.
- [Inkplate 5 Gen 2 V1.1.0 power schematic](https://github.com/SolderedElectronics/Soldered-Inkplate-5-Gen2-hardware-design/blob/main/CAD/V1.1.0/POWER.kicad_sch),
  [panel schematic](https://github.com/SolderedElectronics/Soldered-Inkplate-5-Gen2-hardware-design/blob/main/CAD/V1.1.0/E-paper.kicad_sch)
  and [MCU schematic](https://github.com/SolderedElectronics/Soldered-Inkplate-5-Gen2-hardware-design/blob/main/CAD/V1.1.0/MCU.kicad_sch).

The PMIC is at I2C address 0x48 on our board; the PCAL6416A is at 0x20.
Other boards using TPS65185 frequently use 0x68 and different expanders.
WAKEUP permits register access after at least 1.8 ms. PWRUP requests
sequenced HV activation, and VCOM_CTRL controls the common-electrode bias.
The logic-supply switch is controlled separately. ENABLE=0x20 requests the
logic supply without explicitly enabling individual HV rails. ENABLE=0x3F
requests all five rail/bias enable bits plus the logic supply; do not treat
these writes as interchangeable. PG=0xFA is the expected healthy aggregate
rail status; PG=0x00 is the off status while awake. Physical PGOOD is a
second observation of the same PMIC, not an independent electrical sensor.
A sleeping PMIC cannot be queried reliably. An I2C error or 0xFF from an
unchecked read is not a valid off-state observation.

| Register | Address | Purpose |
| --- | --- | --- |
| ENABLE | 0x01 | Power state and individual supply enables |
| VADJ | 0x02 | Source supply voltage setting |
| VCOM1/2 | 0x03/0x04 | Bias setting and acquisition/programming controls |
| INT1/2 | 0x07/0x08 | Read-to-clear interrupt flags |
| UPSEQ0/1 | 0x09/0x0A | Startup assignment/delay |
| DWNSEQ0/1 | 0x0B/0x0C | Shutdown assignment/delay |
| PG | 0x0F | Rail status |
| REVID | 0x10 | Device revision |

Our unit returned REVID=66, VADJ=03, VCOM1/2=FC/00 (configured -2520 mV),
factory UP=E4/55 and DOWN=1E/E0. In a standby-only test we programmed
E4/55 and 1B/E0; after sleep the original factory pair returned. This
demonstrates volatile programming and factory reload on this unit. Do not
write calibration EEPROM, assume these values on other units, or mistake
configured voltage for measured voltage.

### Shutdown sequence options and the unresolved conflict

Decoded command order from TPS65186 strobe assignments:

| Value | Direction | Ordered rails |
| --- | --- | --- |
| E4 | Up | VNEG, VEE (VGL), VPOS, VDDH (VGH) |
| E1 | Up | VEE, VNEG, VPOS, VDDH |
| 1B | Down | VDDH, VPOS, VEE, VNEG |
| 1E | Down | VDDH, VPOS, VNEG, VEE |

The ED052TC4 specification requires VGL to remain negative while the source
supplies decay toward ground and describes a minimum 500 ms VGL decay interval
to a specified threshold. Factory 1E appears more consistent with the
specified order than vendor 1B when comparing only disable requests. However,
capacitors, clamps, discharge circuits and loads determine physical decay;
command order is not a voltage trace. Vendor 1B has a documented history as
part of a deliberate sequence fix. Neither configuration has comparative
analog measurements on Inkplate 5 Gen 2 in the material reviewed.

Options for our implementation:

| Option | Evidence | Trade-off / decision |
| --- | --- | --- |
| E4/1B with factory delays | Vendor 2024 corrections, original 5V2 implementation, our standby readback | Provisional baseline; no proof of optimal decay or lockup prevention |
| E4/1E with factory delays | Observed factory setup and panel specification | Plausible alternative; no comparative energized evidence; not selected for first baseline |
| E1 with different startup delays | FastEPD 5V2; independently observed Carta 1300 requirement on a different panel | Do not import without ED052TC4 evidence |
| Explicit short delay bytes (00/00) | Vendor and FastEPD initialization writes | Discarded by sleep; preserve actual factory delay bytes on every startup instead |
| Keep logic supply on through settling | Vendor 6 Plus historical fix; panel timing requirement | Retain 500 ms after digital off confirmation and recheck before removing logic; does not change physical decay slope |
| Keep PMIC awake between refreshes | FastEPD 5V2 power-off path; commercial designs | Possible future operating strategy, not a demonstrated cure; idle draw and failure behavior require evaluation |
| Keep HV on for successive refreshes | Some libraries support leave-on modes | Reduces power cycles but increases energized dwell and fault exposure; no assumption of safety or prevention |

Do not expose a user-facing arbitrary sequence toggle as a substitute for
board validation. Do not use unverified alternative register values to
try to recover a wedged PMIC.

## Soldered Arduino library: history

Source: [Inkplate-Arduino-library](https://github.com/SolderedElectronics/Inkplate-Arduino-library),
[src/features/TPS65186/TPS65186.cpp](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/features/TPS65186/TPS65186.cpp)
and [Inkplate5V2Driver.cpp](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/boards/Inkplate5V2/Inkplate5V2Driver.cpp).

| Date | Commit | Change and stated reason |
| --- | --- | --- |
| 2020-09-20 | [e16f608](https://github.com/SolderedElectronics/Inkplate-Arduino-library/commit/e16f608d31d80358fa47e9cc7645cd3c24633300) | Fix random vertical black lines on some panels. Added 6 ms after VCOM low and 50 ms before high-Z. Author noted incomplete power sequencing documentation. |
| 2020-10-07 | [fc268c7](https://github.com/SolderedElectronics/Inkplate-Arduino-library/commit/fc268c7655a56c8049ae2622e66bc8695a573b29) | Fix stuck refresh and partial update bugs; poll PG with 250 ms deadline. |
| 2021-11-03 | [5ef9e62](https://github.com/SolderedElectronics/Inkplate-Arduino-library/commit/5ef9e6204e1c6d6f4bf41f22532be875f5c80480) | Fix vertical lines on 6 Plus after power-down and leave-on bug. Retain 3.3 V during a 100 ms settling wait. Different panel/board. |
| 2021-11-25 | [87ee17e](https://github.com/SolderedElectronics/Inkplate-Arduino-library/commit/87ee17e90428f4da9bccd05b453118130d853d8f) | Override UPSEQ0 to E1 because VEE and VNEG order was considered swapped. |
| 2024-08-06 | [64fa39c](https://github.com/SolderedElectronics/Inkplate-Arduino-library/commit/64fa39c7788b7d1bbdcdb0c49cecd9b66a7bee33) | Change startup to E4 and explicitly set shutdown to 1B. Commit identifies a sequencing fix, but no affected-panel list or oscilloscope results. |
| 2024-08-10 | [c9492d9](https://github.com/SolderedElectronics/Inkplate-Arduino-library/commit/c9492d9ccd21b2b6b4419b4160b513bdd9815323) | Change ENABLE 3F to 20, arrange logic supply before panel GPIO highs, defer VCOM until PG, call shutdown on ramp timeout. Retain E4/1B. |
| 2025-10-23 | [38eee97](https://github.com/SolderedElectronics/Inkplate-Arduino-library/commit/38eee97ae6ccd69577ab217976f80c278558ba26) | Original 5V2 driver uses the supply-first order inherited from 2024. |
| 2026-04-20 | [f7c9e27](https://github.com/SolderedElectronics/Inkplate-Arduino-library/commit/f7c9e27497010cd4f287f7e64fc45c069cfeb0d2) | Shared TPS65186 class. 5V2 prepares idle GPIO levels before calling PMIC powerUp; shared class retains E4/1B. |

The original 5V2 sequence was wake, wait 5 ms, request logic supply, program
sequencer, configure idle signals, assert PWRUP, wait PG, enable VCOM and OE.
The refactored driver raises SPH/GMOD/SPV before the PMIC power-up call.
Inkplate 6 Plus still has supply-first code. See
[Inkplate6PLUSDriver.cpp](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/boards/Inkplate6PLUS/Inkplate6PLUSDriver.cpp).
This ordering difference is a plausible backfeeding risk, not a proven cause
of the lockup. The first published 5V2 lockup report predates the April
refactor; the refactor cannot explain all historical failures by itself.

The current common power-up routine uses 5 ms wake delay and a 500 ms PG
timeout. Power-down lowers VCOM and PWRUP, polls for PG=0 for up to 250 ms
and enters sleep. Review actual return values: the vendor path does not
provide the same fault-latched guarantee as our planned controller, and a
post-WAKEUP-low I2C write cannot reliably clear the logic supply.

### Issue #297: persistent partial power

[Open upstream issue](https://github.com/SolderedElectronics/Inkplate-Arduino-library/issues/297).
Reported 2026-03-15 on Inkplate 5V2: screen stops updating after 1–2 hours
while CPU and application remain functional; PG stuck at A0 instead of FA
(active) or 00 (awake/off). Reporter suspected an I2C error in shutdown
and initially reported soft recovery. This was a hypothesis, not a confirmed
cause.

- [2026-04-15 vendor reproduction](https://github.com/SolderedElectronics/Inkplate-Arduino-library/issues/297#issuecomment-4250592188):
  Soldered reproduced on Inkplate 5V2; power cycling worked, suggested
  software recovery did not.
- [2026-06-08 Inkplate 10 V2 report](https://github.com/SolderedElectronics/Inkplate-Arduino-library/issues/297#issuecomment-4647504557):
  REVID=66, PG=A0, ENABLE=20, INT1/2=00; I2C remained responsive and
  repeated ESP32 resets did not clear the state. A 3 s rail clean-down
  mitigation did not prevent a later recurrence. Removing PMIC VIN was
  required on that unit.
- [2026-11-23 Inkplate 10 V2 report](https://github.com/SolderedElectronics/Inkplate-Arduino-library/issues/297#issuecomment-7088533882):
  USB-powered unit with scheduled restart also wedged (PG=A0, ENABLE=20,
  INT1/2=00, P0=18). Reporter observed different recurrence intervals with
  different refresh policies, but too few events to establish causation.
  Experimental vendor patch reverses startup ordering to logic supply first.
  The patch had been running since November 22, too briefly to assess its
  effectiveness. Only removing VIN recovered the reported faults.

Raw PG bytes are retained here because issue participants describe individual
rail bits inconsistently. Decode using the applicable TI register map, not
the issue prose. A0 alone identifies the observed pattern, not the cause,
actual energized voltages or a particular damaged component. Zero interrupt
flags do not rule out a persistent failure; they may also have been cleared
by a previous read.

Implications: check both PG observations and ENABLE, preserve first failure
evidence before sleep (including expander inputs, output latches, directions
and read-to-clear interrupt flags), report failure to the application, avoid
automatic startup retry, and never equate MCU reset with removal of PMIC VIN.
The board reset net may reset the PCAL and ESP32 together on external reset,
but it does not remove PMIC input power. Internal reset behavior and
physical containment remain unverified.

## Soldered ESPHome: two distinct failure modes

[Custom ESPHome component](https://github.com/SolderedElectronics/Soldered-Inkplate-ESPHome/blob/main/components/inkplate/inkplate.cpp),
[upstream refactor PR #17179](https://github.com/esphome/esphome/pull/17179),
[5V2 support thread](https://community.soldered.com/t/inkplate-5v2-partial-refresh-fails-i2s-dma-timeout-line-not-sent/202).

The 2026-08-28 report describes DMA timeouts after 1–2 hours with partial
updates. On September 1, replacing a 32-bit unsigned deadline derived
from esp_timer_get_time() with a signed 64-bit deadline extended operation
past eight hours. The 32-bit microsecond wrap is about 71.6 minutes, which
is consistent with the reported initial failure. The reporter later observed
PG=A0 after roughly 12 hours, a distinct failure even after the timer fix.
Soldered identified the second problem as a known board-related PMIC issue
also affecting Inkplate 10, recommending a board power cycle. This does not
prove full refresh avoids the fault or that the PMIC fault is caused by DMA.

The inspected nonblocking ESPHome controller programs E4/1B and ENABLE=20.
Its PGOOD timeout branch advances to the VCOM/OE state anyway. Do not copy
that behavior. Use bounded monotonic 64-bit elapsed deadlines and abort
scan activation on power errors. Keep DMA startup/transfer failures distinct
from PMIC failures in error reporting.

## FastEPD: exact board, different power choices

[bitbank2/FastEPD](https://github.com/bitbank2/FastEPD),
[FastEPD.inl](https://github.com/bitbank2/FastEPD/blob/main/src/FastEPD.inl).
Inspect Inkplate5V2EinkPower, Inkplate10EinkPower, bbepTPS65186Init and
Inkplate5V2IOInit rather than assuming all boards share the same path.

| Parameter | Soldered 5V2 | FastEPD 5V2 | FastEPD Inkplate 10 |
| --- | --- | --- | --- |
| Wake delay | 5 ms | 5 ms | 5 ms |
| UPSEQ0 at power-up | E4 | E1 | E4 |
| DWNSEQ0 at power-up | 1B | No rewrite | 1B |
| Initial ENABLE | 20 | 3F | 20 |
| VCOM | After PG | Before PG polling | Before PG polling |
| PG timeout | 500 ms | 250 ms | 250 ms |
| OE | After PG | After PG | After PG |
| Shutdown | Poll PG, enter sleep | Lower signals and PWRUP, no PG wait or explicit sleep in power-off function | Similar |

FastEPD initialization writes the sequence pair 1B/00 and 1B/00, then
lowers WAKEUP. These writes should not be described as a guaranteed
effective shutdown sequence on later wakes, because sleep reloads defaults.
Also inspect deinitialization separately; the power-off function does not
represent all possible application lifecycle behavior.

FastEPD provides valuable exact-board evidence but weaker sequencing and
shutdown safeguards in these paths. Its code is not proof of analog
compliance or long-term reliability.

Recent related history:

- [570f867, 2026-03-25](https://github.com/bitbank2/FastEPD/commit/570f8671ef4942db688b22e69bc86cc2f86091f0):
  added Inkplate 10 support.
- [bd2d9c6, 2026-11-22](https://github.com/bitbank2/FastEPD/commit/bd2d9c637e00d40bb2683dfd302a0660af4896b8):
  added timeout and cleanup paths for EPDiy V7 power-good waits and
  bounded row-DMA waits, with refresh error propagation.
- [da4fa86, 2026-11-22](https://github.com/bitbank2/FastEPD/commit/da4fa863b7fd2abe082765b49715f9150b000a9a):
  changed the power-good timeout error classification.

These changes independently support treating power and DMA failure as
recoverable application-visible errors rather than waiting indefinitely.
They do not establish a 5V2-specific fix.

## EPDiy: TPS65185, ED052TC4 adapter and Carta 1300

[vroland/epdiy](https://github.com/vroland/epdiy),
[V6](https://github.com/vroland/epdiy/blob/main/src/board/epd_board_v6.c),
[V7](https://github.com/vroland/epdiy/blob/main/src/board/epd_board_v7.c),
[PMIC helper](https://github.com/vroland/epdiy/blob/main/src/board/tps65185.c).
The standard power paths rely largely on factory sequencing; they assert
WAKEUP/PWRUP/VCOM before enabling additional rails through ENABLE=3F.
Some paths contain unbounded physical PGOOD waits or only log a failure.
V6/V7 power-off requests sleep after a short delay, with separate deinit
waits for interrupts and pins. These are not direct templates for our
TPS65186 board.

| Issue | Evidence and implication |
| --- | --- |
| [#370 ED052TC4 support](https://github.com/vroland/epdiy/issues/370) | Exact panel (720x1280) worked using an adapter and later V7 Raw hardware. Feb 11, 2025 report says PWRGOOD did not go high despite visible operation. Different board, no reason to bypass our checks. |
| [V7 Raw discussion #398](https://github.com/vroland/epdiy/discussions/398) | Additional context on the raw board and unresolved PGOOD behavior. |
| [#136 LilyGo T5 current leakage](https://github.com/vroland/epdiy/issues/136) | Reporter measured about 115 mA powered, 235 mA after panel off on battery; suspected backfeeding through display inputs. Different LT1945 circuit. Supports low/high-Z GPIO discipline, not a proven explanation for TPS65186 lockup. |
| [#92 LilyGo T5 fading](https://github.com/vroland/epdiy/issues/92) | White areas darkened after shutdown on battery but not USB; suspected incomplete power-down. Hypothesis, not confirmed root cause. |
| [#393 VCOM kickback / damaged display](https://github.com/vroland/epdiy/issues/393) | Experiments show kickback readings depend on temperature and panel state; Feb 20, 2025 comment warns that leaving PWRUP high on a shorted panel may overheat the PMIC. Do not import calibration writes or assume kickback measures supply rails. |
| [#451 panel-specific startup](https://github.com/vroland/epdiy/issues/451) | ED103MC2 startup failed with VNEG undervoltage until custom UPSEQ was applied after WAKEUP and before PWRUP. |

### Carta 1300 case study

[FastEPD #20](https://github.com/bitbank2/FastEPD/issues/20) and
[EPDiy #451](https://github.com/vroland/epdiy/issues/451) record the
same investigation by an EPDiy contributor. A working Waveshare IT8951
controller was instrumented to capture its I2C setup, identifying UPSEQ0=E1
and UPSEQ1=AA written 3–4 ms after WAKEUP, before power activation.
The contributor reported successful startup after applying those settings.
This was an ED103MC2 Carta 1300 panel, not ED052TC4. The two issues should
not be counted as two independent reproductions.

[Commit 1edbd7a, 2026-01-02](https://github.com/vroland/epdiy/commit/1edbd7a995f5a008e1dd091b3d4da4acd2689e76)
introduces the panel-specific helper and display flag for V6/V7. It retains
the existing behavior for other panels. This supports explicit board/panel
profiles rather than a universal sequencing constant.

## Linux and commercial ONYX firmware

The upstream [Linux TPS65185 regulator driver](https://github.com/torvalds/linux/blob/master/drivers/regulator/tps65185.c)
separates the logic regulator, the combined HV regulator and VCOM into
separate operations. It waits for a PGOOD interrupt and checks the pin;
a timeout returns an error. The PMIC can be driven using a PWRUP GPIO or
register-controlled active/standby transitions depending on board wiring.

[Commit 44f7b87, 2026-08-17](https://kernel.googlesource.com/pub/scm/linux/kernel/git/netdev/net-next.git/+/44f7b876b7c6c7b3a219b7a810d3d9548df21540)
fixes premature I2C access after WAKEUP by waiting 1.8–3 ms.
The fix was reported on ONYX BOOX C67ML hardware. It independently
supports our 5 ms wake guard. The Linux driver is not a complete display
controller and does not validate Inkplate discharge timing.

The reverse-engineered [ONYX RC2 EBC driver notes](https://github.com/hsw/onyx-rc2-ebc/blob/main/driver/README.md)
and [hardware notes](https://github.com/hsw/onyx-rc2-hardware) describe a
commercial TPS65185 setup with UPSEQ0/1=39/11, ENABLE=20 followed by a
2 ms wait then active request, and roughly 600 ms of PG polling per attempt.
Its standby policy retains logic power; board power and GPIO arrangements
differ from ours. The analysis distinguishes evidence confidence; this
is reverse engineering, not an E Ink electrical specification. Do not copy
the retry policy or swallowed failure behavior.

## epdpi decisions and remaining gates

The [implementation](https://github.com/marre/epdpi/commit/0247ed35d74955365148415be769e9ea71ad318e)
and [follow-up](https://github.com/marre/epdpi/commit/175da48d6bb091ccd80f2866960828c7353be1a7)
reflect the research in the current branch.

| Decision | Reason / status |
| --- | --- |
| E4/1B with factory delay bytes | Provisional vendor baseline, tested in standby; analog discharge unresolved |
| 5 ms wake guard | TI minimum, upstream Linux fix and vendor practice |
| Separate LogicReady phase | Supply-first, keep direct controls low until logic enable readback and settling; active path exercised only in host tests |
| Preserve factory VCOM/VADJ | Board-specific calibration; no EEPROM writes |
| No VCOM/OE on bad or late PG | Prevent use of partial rails; check actual PCAL levels, PG and ENABLE |
| Wait 500 ms before clearing logic after confirmed off | Conservative additional dwell; does not set or measure VGL decay |
| Bounded I2C and elapsed-time polling | HAL 20 ms transaction timeout plus possible bus recovery time; 64-bit monotonic timing |
| Capture terminal A0 PG evidence | Persist the first PG/ENABLE/PCAL snapshot and interrupt flags in RAM; missing reads unknown |
| Faulted state, no auto retry | Wedge modeled as persisting across sleep; no claim software restores a physical wedge |
| High-Z release | Avoid backfeeding; preserve low control output containment if shutdown uncertain |
| HV interlock | Production set_power(true) still rejects activation |

Suggested subsequent work, in order:

1. Validate the new LogicReady boundary with a deliberately limited logic-only
   diagnostic, no PWRUP/VCOM/OE, no DMA. Verify both the logic-only and off
   state by readback, and ensure cleanup on all error paths.
2. Review upstream issue #297 and the experimental patch after meaningful
   long-duration results appear. Seek vendor clarification on E4/1B versus
   E4/1E and ED052TC4-specific discharge requirements. Do not label
   logic-supply-first a confirmed fix based on short tests.
3. Decide whether an energized diagnostic is justified given the unresolved
   analog requirements and lack of instrumentation. Serial status alone
   does not satisfy those requirements. Keep HV gated until reviewed.
4. Before scanning, address the HAL's unbounded synchronous DMA startup
   waits, ensure all row/frame failure paths contain power, and review
   cancellation/reset behavior. No HAL fork is assumed.
5. If energized operation is eventually authorized, collect versioned,
   phase-specific fault evidence and distinguish PMIC power cycles, full/
   partial updates, uptime and temperature in long-duration validation.
   Compare only one experimental variable at a time. Do not treat a successful
   short test as proof that a rare long-term wedge is resolved.

This research is a snapshot of public sources as of 2026-11-26. It does not
establish a unique correct sequencer for ED052TC4, a causal explanation
for the partial-power lockup, or a guarantee of electrical safety.
