# Inkplate 5 Gen 2: refresh, grayscale and optical recovery research

Reviewed 2026-11-26. Target: E Ink ED052TC4, 1280 x 720, on the Soldered
Inkplate 5 Gen 2 (classic ESP32, I2S parallel DMA, TPS65186).

This report complements the [timing and driver architecture research](inkplate5v2-research.md),
[PMIC research](inkplate5v2-pmic-research.md),
[implementation plan](inkplate5v2-implementation-plan.md) and
[physical validation record](inkplate5v2-hardware-validation.md).
It records source findings and recommendations, not authorization to energize
the panel. The existing bring-up interlocks and electrical validation gates
remain in effect.

## Summary

- Retain epdpi as the Rust foundation. Use the Soldered Arduino implementation
  as the baseline for ED052TC4 waveform recipes and ESP-IDF-InkPlate as an
  independent native ESP32 implementation reference.
- The panel datasheet specifies 2–16 gray levels depending on controller and
  waveform. Soldered provides eight levels, using a nine-phase waveform
  applied after a 48-pass clean.
- Full monochrome, full grayscale and differential monochrome have different
  drawing and finalization recipes. They should be represented explicitly in
  the panel abstraction.
- EPDiy has an ED052TC4 definition but borrows the ED097TC2 waveform.
  Its transition-based and temperature-aware architecture is worth adopting;
  the borrowed waveform is an experimental candidate, not a validated default.
- Track the validity of the previous optical state and invalidate it on failed
  or cancelled updates. Power recovery and optical recovery are separate.
- Prioritize vendor-compatible partial monochrome with periodic full refresh
  over speculative reductions in cleaning or development of new gray levels.
- Reported failures on this exact board include truncated I2S rows, edge
  artifacts, DMA timeout after extended uptime, persistent PMIC power-good
  failures, and streaks that survived cleaning and a firmware change.

Evidence distinctions: source inspection proves what software requests, not
what the glass does; user reports are observations on individual devices;
datasheets specify requirements; proposed changes require validation. No
energized Rust refresh or physical waveform measurements were performed for
this research.

## Panel and encoding

The [ED052TC4 Rev 2.0 datasheet](https://www.laskakit.cz/user/related_files/ed052tc4_final_ver_2.pdf)
describes a 5.17-inch, 720 x 1280 active-matrix Carta display, approximately
284 PPI, with a raw parallel interface. The driver uses landscape orientation
(1280 x 720). The display retains its image with power removed. Soldered
supports monochrome and eight physical shades.

A framebuffer shade and a source-driver command are distinct. Four pixels fit
in each transmitted byte:

| Two-bit code | Repeated byte | Meaning in our logical API |
| --- | --- | --- |
| 00 | 0x00 | Hold / neutral |
| 01 | 0x55 | Darken |
| 10 | 0xAA | Lighten |
| 11 | 0xFF | Skip / final discharge command in vendor grayscale path |

Names for codes 00 and 11 differ across projects. For example, ESP-IDF-InkPlate
calls 00 DISCHARGE and 11 SKIP, while the Arduino grayscale comment calls
its final 11 pass discharge. Preserve the literal code and its position in
the recipe rather than assuming the names establish electrical equivalence.

Soldered transmits 336 bytes per row (320 pixel bytes plus 16 trailing bytes)
and reverses columns per row while preserving row order. The classic ESP32
I2S FIFO lane permutation must also be handled. Our host tests can establish
the intended packing, but not physical pin timing or the resulting image.

## Reference implementation comparison

| Implementation | Relevance | Refresh architecture | Evidence limitations |
| --- | --- | --- | --- |
| [Soldered Arduino](https://github.com/SolderedElectronics/Inkplate-Arduino-library) | Exact board | Eight-level full grayscale, full and partial monochrome | Golden software recipe, not a complete electrical specification |
| [Soldered ESPHome](https://github.com/SolderedElectronics/Soldered-Inkplate-ESPHome) | Exact board | Same grayscale table and cleaning; staged asynchronous transfers | Shared ancestry with Arduino, not independent waveform characterization |
| [ESP-IDF-InkPlate](https://github.com/turgu1/ESP-IDF-InkPlate) | Exact board | Native ESP32 implementation of the vendor recipes, with small finalization differences | Monochrome bring-up publicly tested; no exhaustive optical qualification found |
| [EPDiy](https://github.com/vroland/epdiy) | Same panel on different controller hardware | Old/new shade transitions, temperature ranges, mode selection, dirty areas | ED052TC4 listed as should work; ED097TC2 borrowed LUT |
| [FastEPD](https://github.com/bitbank2/FastEPD) | Explicit Inkplate 5 Gen 2 target | Optimized DMA, several bit depths and differential techniques | Generic modes are not automatically qualified for ED052TC4; see earlier research |
| [M5GFX](https://github.com/m5stack/M5GFX) | Related raw-parallel displays | Erase-to-reference-shade, text and fast modes | Existing epdpi M5 modes target another panel; no ED052TC4 qualification |
| Our epdpi integration branch | Exact target under development | Gray4 framebuffer, eight-level vendor table, full clean, generic waveform engine | Energized scanning and optical results not yet verified |

The ESPHome [Inkplate 5 driver](https://github.com/SolderedElectronics/Soldered-Inkplate-ESPHome/blob/main/components/inkplate/inkplate5.cpp)
and [header](https://github.com/SolderedElectronics/Soldered-Inkplate-ESPHome/blob/main/components/inkplate/inkplate5.h)
contain the same V2 cleaning and grayscale tables as Arduino, while keeping
the older V1 recipe distinct. Its staged refresh architecture is useful for
asynchronous APIs, timeout handling and exclusion of overlapping updates.

The [ESP-IDF-InkPlate V2 driver](https://github.com/turgu1/ESP-IDF-InkPlate/blob/master/src/drivers/eink_5_v2.cpp)
uses the same grayscale table, clean sequence, and horizontal reversal.
It blocks ordinary partial monochrome updates after grayscale and adds a
final 0xFF skip pass after partial refresh that Arduino omits. There is no
controlled comparison demonstrating that this extra pass improves quality.
The library is also used by the [EPub-InkPlate reader](https://github.com/turgu1/EPub-InkPlate);
this does not establish that every reader mode has been tested on this panel.

## Soldered waveform recipes

Primary source: [Inkplate5V2Driver.cpp](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/boards/Inkplate5V2/Inkplate5V2Driver.cpp),
especially display3b(), display1b(), partialUpdate(), clean(), burnInClean()
and calculateLUTs(). The [waveform table](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/boards/Inkplate5V2/waveforms.h)
is also reproduced in our [ED052TC4 definition](../epdpi/src/panels/ed052tc4.rs).

### Standard preparation

| Step | Command byte | Passes |
| --- | --- | ---: |
| Lighten | AA | 1 |
| Darken | 55 | 11 |
| Hold | 00 | 1 |
| Lighten | AA | 11 |
| Hold | 00 | 1 |
| Darken | 55 | 11 |
| Hold | 00 | 1 |
| Lighten | AA | 11 |
| Total | | 48 |

The sequence establishes a repeatable white starting state. Do not confuse
clearing the in-memory framebuffer with physically performing these scans.

### Full grayscale

The nine drawing phases address eight target shades. The exact LUT is already
present in the panel definition, so this report does not duplicate it.
Notable properties:

- Black includes a lightening phase.
- Intermediate shades use different pulse orders; simple pulse-count
  interpolation cannot reproduce the table.
- White has no active drawing pulses after the cleaning sequence.
- The ninth drawing phase is neutral for every shade.
- One final uniform 0xFF pass follows the waveform, then panel power is
  normally turned off.

Total: 48 + 9 + 1 = 58 passes. Cleaning accounts for about 83% of them.
Do not remove the neutral phase or shorten cleaning merely because it looks
redundant in a code trace. The electrophoretic response depends on the
sequence and elapsed drive time, not only on the requested final shade.

### Full monochrome

After the same 48 preparation passes, Arduino applies three passes using
LUTB (black pixels darken, white pixels skip), one pass using LUT2 (black
pixels darken, white pixels lighten), and one uniform 0x00 pass.

Total: 53 passes. This is a separate waveform recipe, not the eight-level
grayscale table restricted to its endpoints.

### Differential monochrome

The vendor compares the last displayed one-bit image with the new one:

| Old | New | Drawing command |
| --- | --- | --- |
| White | White | 11 |
| Black | Black | 11 |
| White | Black | 01 |
| Black | White | 10 |

It combines LUTW and LUTB to generate the packed difference buffer, scans
that buffer four times, then sends two uniform 0x00 passes. Total: six.
The ESP-IDF-InkPlate variant adds a seventh pass (uniform 0xFF).

The Arduino path refuses grayscale partial updates, falls back to full
monochrome when partial updates are blocked, and supports a configurable
full-update threshold. A threshold of ten successful partial updates is a
reasonable initial policy; vendor guidance commonly recommends 5–10.
Treat this as a conservative starting policy, not a proven universal bound.

The vendor counts black-to-white transitions because they are especially
visible as residual blur. We could use that count as an additional quality
heuristic later, but a count of all completed partial updates is simpler
for the initial implementation.

### Deep cleaning

The vendor burnInClean() uses a separate 70-pass cycle:

| Step | Passes |
| --- | ---: |
| Darken | 21 |
| Hold | 1 |
| Lighten | 12 |
| Hold | 1 |
| Darken | 21 |
| Hold | 1 |
| Lighten | 12 |
| Hold | 1 |

The function accepts a cycle count and inter-cycle delay. It powers on the
panel but does not itself call einkOff() at the end. A Rust diagnostic API
should bound the operation, handle failures, and always attempt safe shutdown,
rather than reproduce that omission. This stronger sequence is a diagnostic
candidate for persistent image retention, not a regular refresh default.
Do not automatically repeat deep cleaning after a failed attempt.

## Advanced grayscale and waveform design

Three approaches are relevant:

1. **Reset then draw (Soldered):** full black/white conditioning establishes
   a known starting state; target-only LUT determines the new shade.
2. **Erase to a reference shade (M5GFX):** use an old-shade-dependent erase
   followed by a target-shade waveform. Fewer full-screen flashes may be
   possible, but its existing LUTs are panel-specific.
3. **Old-to-new transitions (EPDiy):** index by mode, temperature, old shade,
   target shade and phase. A 16-level waveform has 256 possible transitions,
   including unchanged pixels. The duration of each phase is part of the
   recipe.

The [EPDiy display definitions](https://github.com/vroland/epdiy/blob/main/src/displays.c)
define ED052TC4 as 1280 x 720, an eight-bit bus, bus_speed = 22, horizontally
mirrored, with epdiy_ED097TC2 as the default waveform. Its
[compatibility table](https://github.com/vroland/epdiy/blob/main/README.md)
qualifies support as expected to work. See [discussion #398](https://github.com/vroland/epdiy/discussions/398)
for the adapter/support history and [issue #482](https://github.com/vroland/epdiy/issues/482)
for the earlier geometry error. The prior research records additional
ED047TC2 configuration reports, including mirroring.

Neither borrowing the 9.7-inch ED097TC2 LUT nor transplanting a 4.7-inch
ED047TC2 LUT establishes 16 distinct shades, safe DC balance, consistent
temperature behavior or durability on the Inkplate. These are possible
research inputs only. An exact ED052TC4 waveform with known provenance,
timing and appropriate licensing would be preferable. None was located.

EPDiy's [high-level implementation](https://github.com/vroland/epdiy/blob/main/src/highlevel.c)
maintains front, back and difference buffers and tracks dirty rows/columns.
It provides differential drawing and temperature/mode selection. Its
[waveform timing notes](https://github.com/vroland/epdiy/wiki/Waveform-timings-for-epdiy)
and [pixel-driving notes](https://github.com/vroland/epdiy/wiki/How-pixels-are-driven-in-a-parallel-epaper-with-epdiy)
explain why row/frame duration matters. [Issue #226](https://github.com/vroland/epdiy/issues/226)
covers proprietary waveform formats and the challenges of converting WBF
files to usable transition tables.

We should make room for transition-aware and temperature-dependent recipes,
without inventing ED052TC4 transition data or implying that faster scanning
can be compensated for by arbitrary extra pulses.

## Reported problems and solutions

### Extended partial refresh failures on the exact board

[Soldered community: Inkplate 5V2 partial refresh fails, I2S DMA timeout](https://community.soldered.com/t/inkplate-5v2-partial-refresh-fails-i2s-dma-timeout-line-not-sent/202)
reports a failure after roughly one to two hours, with ghosting and DMA
timeouts. Changing a 32-bit absolute microsecond deadline to int64_t restored
operation for more than eight hours. An unsigned 32-bit microsecond timer
wraps after about 71.6 minutes.

The same report later describes a TPS65186 power-good timeout after around
12 hours (PG=0xA0, missing=0x5A). Vendor support reported related hardware
behavior and suggested power cycling or full updates. The physical mechanism
and a reliable software-only remedy are not established. See the separate
[PMIC research](inkplate5v2-pmic-research.md) for power details.

Implications: use overflow-safe monotonic timing, distinguish transport and
power failures, never scan with unverified PG, invalidate optical state after
an interrupted update, and test for hours rather than only rapid cycles.
Do not automatically retry a persistent PMIC fault or assume MCU reset
is equivalent to removing power from the board.

### Native ESP-IDF vertical stripes, edge glitches and slow scanning

[ESP-IDF-InkPlate issue #27](https://github.com/turgu1/ESP-IDF-InkPlate/issues/27)
is especially relevant because it concerns an independent port to our exact
board. Reported sequence of findings:

| Attempt | Observed result |
| --- | --- |
| Longer initialization delay and expander corrections | Did not resolve stripes |
| I2S divider 5 to 7 | Significant improvement |
| Divider 7 to 8 | Original display defects disappeared |
| Padding initialization and descriptor reinitialization | Investigated, not demonstrated fixes for residual edge glitches |
| Wait for I2S output completion before releasing SPH | Fixed remaining edge glitches |
| Raise rendering task priority | Resolved slow preparation/rendering in example applications |

The key problem was that DMA out_total_eof signalled completion of DMA's
write into the I2S peripheral, not completion of physical transmission.
Raising SPH prematurely truncated the row. See the
[explanation](https://github.com/turgu1/ESP-IDF-InkPlate/issues/27#issuecomment-4063861688)
and [fix](https://github.com/gmasse/ESP-IDF-InkPlate/commit/72511024835dccd9ada563397f0684a0a287ca3b).
A transport completion contract must encompass the last output samples,
not simply descriptor EOF. The exact hardware status condition still needs
verification in our HAL path.

Even after the FIFO fix, divider 8 remained necessary in native ESP-IDF,
while Arduino worked at divider 5. The contributor reported these software
timing measurements for 38,160 row transfers (53 passes):

| Metric | Arduino | Native ESP-IDF |
| --- | ---: | ---: |
| Divider | 5 | 8 |
| Average row transfer | 22 us | 36 us |
| Effective byte clock | 15.27 MHz | 9.33 MHz |
| Total transfer time | 859 ms | 1,400 ms |

[Measurement comment](https://github.com/turgu1/ESP-IDF-InkPlate/issues/27#issuecomment-4074867186).
Extra inter-row delays did not restore operation at divider 5. The cause
remained unresolved. This is evidence for evaluating a slower transport
baseline within datasheet constraints, not for blindly setting divider 8
in a different HAL or declaring arbitrary timing safe.

The issue also documents shutdown-related I2C errors corrected in turn_off()
and sensitivity to task priority
([priority fix](https://github.com/turgu1/ESP-IDF-InkPlate/issues/27#issuecomment-4061734176)).
The 16 trailing bytes should be explicitly initialized. An explanation
posted in the thread for their necessity was speculative, not an established
hardware requirement independent of the actual panel recipe.

### Persistent streaks on black backgrounds

A vendor community report described streaks visible on black but not white,
persisting after extensive cleaning, reseating the cable and trying Arduino
and ESPHome in monochrome and grayscale. Vendor support suspected a damaged
panel. The specific discussion was reported in the preceding research but
its original URL was not retained here; treat this as secondary anecdotal
evidence. A black-background test is useful even when white-background text
looks fine. Do not attribute such defects to a waveform or assert that a
particular refresh caused physical damage without additional evidence.

### Ghosting on related EPDiy hardware

- [EPDiy #30](https://github.com/vroland/epdiy/issues/30): static text remained
  visible after repeated refreshes. A user reported success using an inverted
  image before redrawing. An old-image inversion, new-image inversion, then
  final image is an experimental low-flash recovery idea, not a proven
  ED052TC4 recipe or proof of long-term DC balance.
- [EPDiy #210](https://github.com/vroland/epdiy/issues/210): direct monochrome
  mode accumulated afterimages when LVGL issued many small updates. Periodic
  full refreshes and coalescing drawing operations were recommended.
- [EPDiy #221](https://github.com/vroland/epdiy/issues/221): vertical lines and
  ghosting on a LilyGo panel with the EPDiy example, but not the vendor
  example. The issue illustrates the importance of checking exact hardware,
  waveform selection and a working reference before diagnosing panel damage.

## Gaps in our current Rust implementation

Inspected integration branch revision 9974ed0470291b01e568e0a9b16dcc3ecc57f340;
the preceding source review also inspected earlier branch revisions.

### P0: optical state validity and cancellation

[driver.rs](../epdpi/src/driver.rs) commits its previous framebuffer after
successful refresh and shutdown. This is a good transactional boundary,
but the previous image remains in memory after a failed scan. The glass may
already be partially changed. Builder documentation tells callers to clear
first, but the state is not explicitly enforced.

Track at least unknown, known monochrome, known grayscale, updating, and
electrical fault (the latter can be a separate bus state). Mark the optical
record unknown before the first potentially modifying operation; commit it
only after the complete recipe succeeds. A failed or cancelled async update
must invalidate the optical record even when its future is dropped.
A cancellation-safe power shutdown/containment contract also needs explicit
design; asynchronous cleanup cannot be assumed to execute from Drop.

Only a successful full refresh may establish a new known image after optical
state loss, and only after the electrical fault has been safely recovered.
Retain dirty regions on failure; prevent differential drawing from stale
history. Never silently claim the requested image was displayed after a
failed cleanup. EPDiy's highlevel.c currently copies dirty pixels after
epd_draw_base() without gating the copy on its error result, so its behavior
should not be copied blindly.

### P1: mode-specific recipes

The current [waveform engine](../epdpi/src/waveform.rs) uses a shared
POST_REFRESH_CODE and the generic FastMono path emits 00 for unchanged
pixels, rather than the vendor differential recipe's 11. ED052TC4 correctly
does not enable SUPPORTS_FAST_MONO today. Do not enable it until its
preparation, difference and finalization rules are implemented.

Represent each supported recipe's preparation, per-phase drawing strategy,
unchanged-pixel command and finalization. Keep raw panel electrical/bus
sequencing in epdpi-esp, independent of framebuffer operations.

Potential public API names: FullMono, PartialMono, FullGrayscale and
PartialGrayscale. These describe behavior without baking in the shade count
(the existing Quality16 mode runs eight levels on ED052TC4). Preserve or
deprecate existing public variants deliberately, rather than silently
changing their behavior. Unsupported modes must fail before power-on.

### P1: physical shade quantization

Keep Gray4 storage (460,800 bytes per image; 921,600 bytes for two). The
current ED052TC4 LUT maps logical pairs (0,1), (2,3), ... (14,15) to eight
nominal physical shades. Normalize the prior-image record to the selected
recipe's quantization or retain explicit physical shade metadata. Treat
6 to 7 as unchanged when both use the same physical shade, in a future
differential mode. Do not compare raw Gray4 values alone.

A differential monochrome update requires a known monochrome previous
image. Do not merely threshold a previous grayscale record and assume the
glass has become monochrome. After switching modes, establish the correct
state with a full update. Also review FastMono's existing record update
behavior on other supported panels.

### P2: CPU and memory efficiency

Soldered and ESP-IDF-InkPlate precompute packed-byte grayscale tables.
Our generic engine currently performs per-pixel lookups. A 256-entry table
for a packed input byte per phase can reduce inner-loop work; combine two
lookup results into one output byte. Measure first, keep DMA buffers in
appropriate memory, and do not move source-start/stop edges to a slower
path merely to optimize software overhead.

One-shot full refresh does not inherently require previous-image storage.
Optional history or a compact one-bit monochrome record may be worthwhile
later, but two Gray4 buffers simplify future differential grayscale.

Dithering belongs in the image-processing layer. Ordered Bayer dithering
is deterministic and spatially stable for text/UI and partial updates.
Floyd–Steinberg or Atkinson error diffusion can improve photographs, but
changes may propagate outside a dirty region; compare the final quantized
image, not only drawing commands. Eight physical shades plus spatial
dithering may be sufficient for many applications.

## Refresh performance model

At 720 rows per pass with 230 us between passes:

    estimated scan time = passes * (720 * effective_row_time + 230 us)

This excludes power transitions, frame-start overhead and any unaccounted
processing. Effective row time is not just DMA transmission time.

| Recipe | Passes | Rows | Estimated scan time at 40 us/row |
| --- | ---: | ---: | ---: |
| Full grayscale | 58 | 41,760 | 1.684 s |
| Full monochrome | 53 | 38,160 | 1.539 s |
| Partial monochrome (Arduino) | 6 | 4,320 | 0.174 s |
| Deep clean, one cycle | 70 | 50,400 | 2.032 s |

Partial monochrome reduces the number of scans by almost a factor of ten
relative to full grayscale. Establish this mode before attempting to
shorten the normal cleaning sequence. Rectangular updates may preserve
unmodified pixels but will not necessarily reduce row count, especially in
a transport that must scan all rows.

## Validation plan without physical instruments

Do not alter the existing HV interlock or enable scanning until separately
authorized under the implementation plan. Optical tests are complementary
to electrical constraints and cannot establish safe voltage or edge timing.
Use the vendor firmware as a known-working comparison on the same board
where practical, retaining factory VCOM and power settings.

After the bring-up gates are satisfied:

| Pattern or test | Expected observation |
| --- | --- |
| White and black solid screens | Uniformity; expose streaks hidden by white |
| Eight-step grayscale ramp | Distinct ordered shades, no severe banding or inversion |
| Asymmetric corners, border and one-pixel lines | Correct row/column order, complete edges and byte lanes |
| Checkerboard and inverse | Both transition directions, neighbor effects and residual image |
| 8 x 8 old-to-new gray matrix | Compare each of 64 transitions against full-refresh reference; experimental differential grayscale only |
| Repeated text updates | Little change outside dirty content; compare after each partial and subsequent full refresh |
| Unchanged image | No unnecessary differential transfer |
| Grayscale to monochrome and reverse | Full refresh re-establishes correct physical history |
| Host-injected failure at every phase | Unknown optical state, retained dirty data, bounded shutdown/recovery, no differential retry |
| 2 h, 12 h and 24 h endurance runs | Overflow-safe timers, stable image quality, no unhandled DMA or PMIC failures |

Start with short, supervised tests and stop on abnormal output, missing PG,
timeouts or failed shutdown; longer endurance tests are conditional on prior
success and a safe bounded test procedure. Use fixed lighting, locked camera
exposure and repeatable framing for photographs; compare patch brightness
and residual contrast to the vendor result. Record board revision, firmware
commit, waveform ID, mode, temperature (if available), row/refresh duration,
pass count, partial count and PMIC status. Do not infer analog waveform
compliance from photos or register readback.

## Recommended implementation order

- [ ] P0: complete existing electrical/transport bring-up gates, including
  verified transmission-completion semantics, before energized scans.
- [ ] P0: track unknown optical history after failure, reset or cancellation;
  keep electrical fault handling separate.
- [ ] P1: represent mode-specific preparation, drawing and finalization.
- [ ] P1: implement and verify the exact vendor full monochrome recipe.
- [ ] P1: implement vendor differential monochrome and enforce mode transitions.
- [ ] P1: configurable automatic full refresh after ten successful partial
  updates by default; coalesce small drawing changes.
- [ ] P1: optical test patterns, source-level golden traces, failure injection
  and bounded endurance diagnostics.
- [ ] P2: explicit diagnostic deep cleaning with safe shutdown.
- [ ] P2: benchmark precomputed LUTs and deterministic dithering.
- [ ] P2: represent temperature and phase duration in waveform metadata.
- [ ] P3: investigate provenance and electrical suitability of EPDiy candidate
  waveforms; evaluate only under an explicitly gated experimental procedure.
- [ ] P3: characterize true 16-level and differential grayscale if a suitable
  panel-specific waveform becomes available.

## Conclusion and open questions

Keep the existing Rust framebuffer/driver/bus separation. Evolve the waveform
abstraction into a complete refresh recipe and enforce a valid optical
history. Use Soldered for the exact-panel default, native ESP-IDF-InkPlate for
transport lessons, ESPHome for staged execution, and EPDiy for advanced
transition-based concepts.

Open questions: exact ED052TC4 transition waveform availability and licensing;
whether the native ESP-IDF clock discrepancy is relevant to our HAL; whether
an extra skip pass helps partial refresh; how ghosting varies with temperature,
content and time; and whether alternative 16-level waveforms can be safely
qualified on the actual board. No code or hardware changes are proposed as
already validated by this document.
