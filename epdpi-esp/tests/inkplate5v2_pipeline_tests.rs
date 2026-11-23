#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! End-to-end host verification of panel scanlines through classic ESP32 I2S1 packing.
//!
//! This tests the shared software path, NOT the physical I2S bus timing,
//! control GPIO, power-good, or panel refresh on hardware.

use epdpi::prelude::*;
use epdpi::waveform::WaveformEngine;
use epdpi_esp::i2s_parallel::{PackError, pack_i2s1_scanline, pack_inkplate5v2_scanline};

type Panel = Ed052tc4;

fn simulated_i2s_wire(dma: &[u8]) -> Vec<u8> {
    let mut wire = Vec::with_capacity(dma.len());
    for group in dma.as_chunks::<4>().0 {
        // Classic ESP32 I2S1 8-bit parallel output order, from esp-hal 1.1.
        wire.extend_from_slice(&group[2..4]);
        wire.extend_from_slice(&group[0..2]);
    }
    wire
}

#[test]
fn grayscale_waveforms_survive_i2s_word_reordering() {
    let mut fbrow = [0xffu8; Panel::FB_STRIDE_BYTES];
    // Logical pixels: 0,1,2,3,4,5,6,7. On ED052TC4 these become
    // physical shades 0,0,1,1,2,2,3,3.
    fbrow[..4].copy_from_slice(&[0x01, 0x23, 0x45, 0x67]);

    for pass in 0..Panel::PASSES {
        let mut logical = [0u8; Panel::SCANLINE_BYTES];
        WaveformEngine::gray_scanline::<Panel>(&fbrow, pass, &mut logical);

        let mut dma = [0u8; Panel::SCANLINE_BYTES];
        assert_eq!(
            pack_i2s1_scanline(&logical, &mut dma),
            Ok(Panel::SCANLINE_BYTES),
        );
        assert_eq!(simulated_i2s_wire(&dma), logical, "pass {pass}");

        // Two neighboring Gray4 levels have the same physical shade:
        // the first four pixel drive codes use physical shades 0 and 1.
        let black = ED052TC4_LUT[pass];
        let next = ED052TC4_LUT[2 * Panel::PASSES + pass];
        let expected = (black << 6) | (black << 4) | (next << 2) | next;
        assert_eq!(logical[0], expected, "pass {pass}");
        assert!(logical[Panel::SCANLINE_PIXEL_BYTES..].iter().all(|&b| b == 0));
    }
}

#[test]
fn final_idle_code_three_survives_the_dma_format() {
    let mut logical = [0u8; Panel::SCANLINE_BYTES];
    WaveformEngine::uniform_scanline::<Panel>(Panel::POST_REFRESH_CODE, &mut logical);
    assert!(logical[..Panel::SCANLINE_PIXEL_BYTES].iter().all(|&x| x == 0xff));
    assert!(logical[Panel::SCANLINE_PIXEL_BYTES..].iter().all(|&x| x == 0));

    let mut dma = [0u8; Panel::SCANLINE_BYTES];
    assert_eq!(
        pack_i2s1_scanline(&logical, &mut dma),
        Ok(Panel::SCANLINE_BYTES)
    );
    assert_eq!(simulated_i2s_wire(&dma), logical);
}

// Independent transcription of Soldered's WAVEFORM3BIT macro. The vendor
// framebuffer stores two *3-bit* grayscale values in the low three bits of
// each nibble; our core framebuffer stores two 4-bit values, where pairs
// of adjacent levels represent one physical grayscale shade.
const SOLDERED_WAVEFORM3BIT: [[u8; 9]; 8] = [
    [0, 0, 1, 1, 2, 1, 1, 1, 0],
    [1, 1, 2, 2, 1, 2, 1, 1, 0],
    [0, 1, 2, 2, 1, 1, 2, 1, 0],
    [0, 0, 1, 1, 1, 1, 1, 2, 0],
    [1, 2, 1, 2, 1, 1, 1, 2, 0],
    [0, 1, 1, 1, 2, 0, 1, 2, 0],
    [1, 1, 1, 2, 2, 2, 1, 2, 0],
    [0, 0, 0, 0, 0, 0, 0, 0, 0],
];

/// Direct transcription of Inkplate5V2Driver.cpp::display3b()'s GLUT2 | GLUT
/// computation. Kept separate from the implementation under test.
fn soldered_reference_dma_row(vendor_framebuffer: &[u8], pass: usize) -> Vec<u8> {
    assert_eq!(vendor_framebuffer.len(), 640);
    let mut dma = vec![0u8; Panel::SCANLINE_BYTES];
    let lookup = |raw: u8| {
        let right_pixel = SOLDERED_WAVEFORM3BIT[usize::from(raw & 7)][pass];
        let left_pixel = SOLDERED_WAVEFORM3BIT[usize::from((raw >> 4) & 7)][pass];
        (right_pixel << 2) | left_pixel
    };
    // Source pointer begins at last byte of a 640-byte framebuffer row.
    for group in 0..80 {
        let offset = group * 8;
        let last = vendor_framebuffer.len() - 1 - offset;
        let j = group * 4;
        dma[j + 2] = (lookup(vendor_framebuffer[last]) << 4)
            | lookup(vendor_framebuffer[last - 1]);
        dma[j + 3] = (lookup(vendor_framebuffer[last - 2]) << 4)
            | lookup(vendor_framebuffer[last - 3]);
        dma[j] = (lookup(vendor_framebuffer[last - 4]) << 4)
            | lookup(vendor_framebuffer[last - 5]);
        dma[j + 1] = (lookup(vendor_framebuffer[last - 6]) << 4)
            | lookup(vendor_framebuffer[last - 7]);
    }
    dma
}

#[test]
fn all_vendor_waveforms_and_spatial_pixel_order_match_dma_reference() {
    let mut fbrow = [0xffu8; Panel::FB_STRIDE_BYTES];
    let mut vendor = [0u8; Panel::FB_STRIDE_BYTES];
    for (index, (ours, vendor_byte)) in fbrow.iter_mut().zip(vendor.iter_mut()).enumerate() {
        let left = ((index * 5 + index / 9 + 1) % 8) as u8;
        let right = ((index * 3 + index / 13 + 2) % 8) as u8;
        *vendor_byte = (left << 4) | right;
        *ours = ((left * 2) << 4) | (right * 2);
    }

    for pass in 0..9 {
        let mut logical = [0u8; Panel::SCANLINE_BYTES];
        WaveformEngine::gray_scanline::<Panel>(&fbrow, pass, &mut logical);
        let mut dma = [0xa5u8; Panel::SCANLINE_BYTES];
        assert_eq!(
            pack_inkplate5v2_scanline(&logical, &mut dma),
            Ok(Panel::SCANLINE_BYTES)
        );
        let reference = soldered_reference_dma_row(&vendor, pass);
        // Compare exact DMA payload built by the real vendor algorithm,
        // not just the generic packer undone by its own simulated inverse.
        assert_eq!(dma[..320], reference[..320], "vendor DMA pass {pass}");
        assert_eq!(&dma[320..], &[0; 16], "trailing clocks, pass {pass}");
    }
}

#[test]
fn inkplate_packer_requires_full_row_and_preserves_output_on_error() {
    let input = [0u8; Panel::SCANLINE_BYTES];
    let mut dma = [0xa5u8; Panel::SCANLINE_BYTES];
    assert_eq!(
        pack_inkplate5v2_scanline(&input[..335], &mut dma),
        Err(PackError::WrongInkplateScanlineLength)
    );
    assert_eq!(
        pack_inkplate5v2_scanline(&input, &mut dma[..335]),
        Err(PackError::TooLong)
    );
    assert!(dma.iter().all(|&byte| byte == 0xa5));
}

#[test]
fn uniform_cleaning_codes_are_unchanged_by_board_pixel_reversal() {
    for code in [
        DriveCode::Lighten,
        DriveCode::Darken,
        DriveCode::Hold,
        DriveCode::Skip,
    ] {
        let mut logical = [0u8; Panel::SCANLINE_BYTES];
        WaveformEngine::uniform_scanline::<Panel>(code, &mut logical);
        let mut dma = [0u8; Panel::SCANLINE_BYTES];
        pack_inkplate5v2_scanline(&logical, &mut dma).unwrap();
        assert_eq!(simulated_i2s_wire(&dma), logical);
        assert_eq!(&dma[320..], &[0; 16]);
    }
}
