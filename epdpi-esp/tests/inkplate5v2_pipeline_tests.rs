#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! End-to-end host verification of panel scanlines through classic ESP32 I2S1 packing.
//!
//! This tests the shared software path, NOT the physical I2S bus timing,
//! control GPIO, power-good, or panel refresh on hardware.

use epdpi::prelude::*;
use epdpi::waveform::WaveformEngine;
use epdpi_esp::i2s_parallel::pack_i2s1_scanline;

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
