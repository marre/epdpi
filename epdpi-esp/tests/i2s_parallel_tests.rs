#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Pure host verification of the original ESP32 I2S1 byte order.

use epdpi_esp::i2s_parallel::{PackError, pack_i2s1_scanline};

#[test]
fn a_four_byte_dma_word_is_swizzled_into_esp32_order() {
    let mut output = [0u8; 4];
    assert_eq!(pack_i2s1_scanline(&[0x10, 0x21, 0x32, 0x43], &mut output), Ok(4));
    assert_eq!(output, [0x32, 0x43, 0x10, 0x21]);
}

#[test]
fn multiple_words_keep_the_requested_wire_order() {
    let input: Vec<u8> = (0..=255).collect();
    let mut packed = [0xEEu8; 260];
    assert_eq!(pack_i2s1_scanline(&input, &mut packed), Ok(256));
    assert_eq!(&packed[256..], &[0xEE; 4]);

    // I2S1 on the classic ESP32 reverses 16-bit halves of each DMA word.
    // Simulate the peripheral's on-wire ordering to ensure the bytes match.
    let mut on_wire = Vec::new();
    for dma_word in packed[..256].as_chunks::<4>().0 {
        on_wire.extend_from_slice(&dma_word[2..4]);
        on_wire.extend_from_slice(&dma_word[0..2]);
    }
    assert_eq!(on_wire, input);
}

#[test]
fn refuses_invalid_lengths_without_touching_the_buffer() {
    let mut data = [0x5Au8; 8];
    assert_eq!(pack_i2s1_scanline(&[], &mut data), Err(PackError::Empty));
    assert_eq!(
        pack_i2s1_scanline(&[0x11; 6], &mut data),
        Err(PackError::NotWordAligned)
    );
    assert_eq!(
        pack_i2s1_scanline(&[0x12; 12], &mut data),
        Err(PackError::TooLong)
    );
    assert!(data.iter().all(|&b| b == 0x5A));
}

#[test]
fn can_pack_a_336_byte_scanline_with_trailing_clocks() {
    // Soldered's Inkplate 5V2 uses 1280 / 4 = 320 pixel-drive bytes
    // followed by 16 dummy clock bytes.
    let mut line = [0xFFu8; 336];
    line[320..].fill(0);
    let mut dma = [0x77u8; 336];
    assert_eq!(pack_i2s1_scanline(&line, &mut dma), Ok(336));
    assert_eq!(&dma[320..], &[0; 16]);
    assert!(dma[..320].iter().all(|&b| b == 0xFF));
}
