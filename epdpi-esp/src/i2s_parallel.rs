//! Classic ESP32 8-bit parallel data transmitter using I2S1 DMA.
//!
//! The ESP32-S3 uses LCD_CAM, but the original ESP32 has an I2S1 parallel
//! transmitter. With `esp-hal` 1.1 its `I2sParallel` driver can send 8-bit
//! parallel bytes and a clock using DMA.
//!
//! The hardware moves four consecutive DMA bytes in the order [2, 3, 0, 1].
//! [`pack_i2s1_scanline`] compensates for this before DMA so the panel sees
//! bytes in their original order. The packing helper has no chip dependency
//! and is tested on the host with and without the blocking feature.
//!
//! On a classic ESP32 `I2sParallel::new` must be configured with I2S1,
//! `DMA_I2S1`, `TxEightBits`, the desired clock frequency, and a clock pin.
//! This crate does not own those pins; the caller creates the HAL peripheral.
//! For example, the Soldered Inkplate 5V2 board wiring is:
//!
//! | Signal | GPIO |
//! |---|---:|
//! | CL (I2S clock) | 0 |
//! | D0..D7 | 4, 5, 18, 19, 23, 25, 26, 27 |
//!
//! The board's SPH, CKV, LE, SPV, OE, PMIC, expander and power-good checks
//! are **not** handled here. These must be added in a board-specific
//! `EpdParallelBus` implementation, with the manufacturer's power sequence
//! and timing. Do not attach the panel or power up its voltage rails until
//! that support has been verified.
//!
//! ESP HAL documentation:
//! <https://docs.espressif.com/projects/rust/esp-hal/1.1.0/esp32/esp_hal/i2s/parallel/index.html>

/// Invalid scanline for the classic ESP32 I2S1 DMA packing format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PackError {
    /// DMA must not be started on a zero-length transfer.
    Empty,
    /// A scanline must be padded to whole four-byte DMA words.
    NotWordAligned,
    /// The supplied DMA buffer cannot fit the line.
    TooLong,
}

/// Convert logical on-wire scanline bytes into classic ESP32 I2S1 DMA order.
///
/// The peripheral emits every 4-byte DMA word in the order [2, 3, 0, 1].
/// Swapping the two 16-bit halves here compensates for that behavior.
/// The returned byte count must be used to set the DMA transfer length.
/// Bytes after the returned length in `output` are left unchanged.
pub fn pack_i2s1_scanline(input: &[u8], output: &mut [u8]) -> Result<usize, PackError> {
    if input.is_empty() {
        return Err(PackError::Empty);
    }
    if !input.len().is_multiple_of(4) {
        return Err(PackError::NotWordAligned);
    }
    if input.len() > output.len() {
        return Err(PackError::TooLong);
    }
    let (src_words, _) = input.as_chunks::<4>();
    let (dst_words, _) = output[..input.len()].as_chunks_mut::<4>();
    for (src, dst) in src_words.iter().zip(dst_words) {
        dst.copy_from_slice(&[src[2], src[3], src[0], src[1]]);
    }
    Ok(input.len())
}

#[cfg(feature = "esp32")]
mod hardware;
#[cfg(feature = "esp32")]
pub use hardware::{Esp32I2sTx, I2sTxError};
