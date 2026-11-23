//! E Ink ED052TC4, 1280 x 720, eight grayscale levels.
//!
//! The Inkplate 5 Gen 2 manufacturer supplies this 8-level, 9-pass waveform as
//! `WAVEFORM3BIT` in:
//! <https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/boards/Inkplate5V2/waveforms.h>
//!
//! The core framebuffer remains 4 bits per pixel: levels (0,1) map to shade 0,
//! (2,3) to shade 1, ..., (14,15) to shade 7. This preserves the existing
//! `Gray4` DrawTarget without adding a second framebuffer layout.
//!
//! Soldered's full update cleans the glass using the sequence below before
//! running the waveform. This description does not include the board-specific
//! I2S byte ordering, GPIO expander, power sequencing, or timing. No hardware
//! testing has been performed.
//!
//! The waveform values are attributed to Soldered Electronics' LGPL-3.0
//! Inkplate Arduino driver; this GPL-3.0-only crate is license-compatible.

use crate::traits::{ColorMode, DriveCode, EpdPanel};

/// Nine drive-code passes for each of the 16 logical framebuffer levels.
/// Two adjacent logical levels share one of the panel's eight physical shades.
/// Codes: 0 hold, 1 darken, 2 lighten.
#[rustfmt::skip]
pub const ED052TC4_LUT: [u8; 16 * 9] = [
    0, 0, 1, 1, 2, 1, 1, 1, 0, // Gray4 0 -> physical 0
    0, 0, 1, 1, 2, 1, 1, 1, 0, // Gray4 1 -> physical 0
    1, 1, 2, 2, 1, 2, 1, 1, 0, // Gray4 2 -> physical 1
    1, 1, 2, 2, 1, 2, 1, 1, 0, // Gray4 3 -> physical 1
    0, 1, 2, 2, 1, 1, 2, 1, 0, // Gray4 4 -> physical 2
    0, 1, 2, 2, 1, 1, 2, 1, 0, // Gray4 5 -> physical 2
    0, 0, 1, 1, 1, 1, 1, 2, 0, // Gray4 6 -> physical 3
    0, 0, 1, 1, 1, 1, 1, 2, 0, // Gray4 7 -> physical 3
    1, 2, 1, 2, 1, 1, 1, 2, 0, // Gray4 8 -> physical 4
    1, 2, 1, 2, 1, 1, 1, 2, 0, // Gray4 9 -> physical 4
    0, 1, 1, 1, 2, 0, 1, 2, 0, // Gray4 10 -> physical 5
    0, 1, 1, 1, 2, 0, 1, 2, 0, // Gray4 11 -> physical 5
    1, 1, 1, 2, 2, 2, 1, 2, 0, // Gray4 12 -> physical 6
    1, 1, 1, 2, 2, 2, 1, 2, 0, // Gray4 13 -> physical 6
    0, 0, 0, 0, 0, 0, 0, 0, 0, // Gray4 14 -> physical 7
    0, 0, 0, 0, 0, 0, 0, 0, 0, // Gray4 15 -> physical 7
];

/// E Ink ED052TC4 panel used by Soldered Inkplate 5 Gen 2.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ed052tc4;

impl EpdPanel for Ed052tc4 {
    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 720;
    const COLOR_MODE: ColorMode = ColorMode::Grayscale8;
    // Soldered clocks width / 4 + 16 bytes through I2S DMA per row.
    const LINE_PADDING: usize = 16;
    // VCOM is factory calibrated on the board; no fixed millivolt default.
    const WAVEFORM_LUT: &'static [u8] = &ED052TC4_LUT;
    // From Inkplate5V2Driver.cpp, display3b(): clean(0,1), clean(1,11),
    // clean(2,1), clean(0,11), clean(2,1), clean(1,11),
    // clean(2,1), clean(0,11). clean(0) lightens, clean(1)
    // darkens, and clean(2) holds.
    const CLEAR_SEQUENCE: &'static [(DriveCode, usize)] = &[
        (DriveCode::Lighten, 1),
        (DriveCode::Darken, 11),
        (DriveCode::Hold, 1),
        (DriveCode::Lighten, 11),
        (DriveCode::Hold, 1),
        (DriveCode::Darken, 11),
        (DriveCode::Hold, 1),
        (DriveCode::Lighten, 11),
    ];
    // The manufacturer ends grayscale updates with clean(3, 1),
    // sending drive code 3 (0xff packed) to all pixels.
    const POST_REFRESH_CODE: DriveCode = DriveCode::Skip;
}
