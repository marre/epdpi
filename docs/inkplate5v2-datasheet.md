# ED052TC4 datasheet — reference and redistribution notice

The current target is **E Ink ED052TC4** (on Soldered Inkplate 5 Gen 2).

## Download / identification

- **Document:** E Ink technical specification `P-511(A77)-003(V:2)`
- **Revision:** 2.0
- **Issue date:** 2017-11-30
- **Panel:** ED052TC4, 5.17-inch electrophoretic, native 720 × 1280 pixels (1280 × 720 landscape on Inkplate 5V2)
- **Public PDF:** [ED052TC4 final version 2 datasheet (LaskaKit)](https://www.laskakit.cz/user/related_files/ed052tc4_final_ver_2.pdf)
- **Alternate distributor listing:** [DATA MODUL ED052TC4/VB3300-EBA](https://www.data-modul.com/en/products/product-finder/find-your-e-paper-display/ed052tc4vb3300-eba)

## Why the PDF is not checked in

The specification identifies E Ink Holdings Inc. as the exclusive rights holder and **explicitly forbids redistribution, reproduction or disclosure without prior written permission**. Its availability on a distributor's website does not grant permission to mirror it into this public GitHub repository.

For this reason, **this repository links to the datasheet but does not store a copy**. Obtain E Ink's written permission before committing the PDF or placing it in a release artifact.

## What to consult in the specification

- AC timing diagrams for XCL, XSTL/SPH, XLE/LE, CKV, SPV and parallel pixel-data setup/hold
- Positive/negative source and gate rails, VCOM requirements and power-down discharge constraints
- Native 720 × 1280 geometry, 50-pin interface and panel operating limits
- The distinction between the panel's possible 2–16 gray levels and the *specific* eight-gray Soldered waveform used here

The electrical limits in the manufacturer's specification, rather than literal delays in a particular firmware, are the relevant hardware requirements.

## Related documents

- [Archived comparative ED052TC4 research](inkplate5v2-research.md)
- [Implementation plan for the available no-instrument bring-up](inkplate5v2-implementation-plan.md)
- [Detailed integration notes](inkplate5v2-bringup.md)
- [Flash backup and recovery](inkplate5v2-recovery.md)
