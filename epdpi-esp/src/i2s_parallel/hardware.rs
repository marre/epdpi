//! Hardware implementation of the classic ESP32 I2S1 DMA transmitter.

#[cfg(not(feature = "blocking"))]
use esp_hal::Async;
#[cfg(feature = "blocking")]
use esp_hal::Blocking;
use esp_hal::dma::{DmaError, DmaTxBuf};
use esp_hal::i2s::parallel::I2sParallel;

use super::{PackError, pack_i2s1_scanline, pack_inkplate5v2_scanline};

#[cfg(feature = "blocking")]
type Mode = Blocking;
#[cfg(not(feature = "blocking"))]
type Mode = Async;

/// Errors encountered preparing or transmitting a scanline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum I2sTxError {
    /// Invalid input length or DMA buffer capacity.
    Pack(PackError),
    /// Peripheral or DMA rejected the transfer.
    Dma(DmaError),
    /// Resources were lost during a previous unexpected failure.
    Lost,
}

/// DMA-backed 8-bit parallel data transmitter for the original ESP32.
///
/// This is a transport helper, not a complete e-paper bus. The owner must
/// implement SPH/CKV/LE/SPV timing and safe panel power sequencing. The I2S
/// clock and data pins must be configured to match the board before use.
///
/// The caller builds the I2S peripheral with `I2sParallel::new`, using
/// I2S1 and DMA_I2S1, and supplies a DMA-capable buffer. Its capacity must
/// cover a full scanline plus any trailing padding clocks. Prefer a length
/// that is a multiple of four bytes.
pub struct Esp32I2sTx {
    i2s: Option<I2sParallel<'static, Mode>>,
    buffer: Option<DmaTxBuf>,
}

impl Esp32I2sTx {
    /// Wrap an already configured I2S1 peripheral and DMA transmit buffer.
    pub fn new(i2s: I2sParallel<'static, Mode>, buffer: DmaTxBuf) -> Self {
        Self {
            i2s: Some(i2s),
            buffer: Some(buffer),
        }
    }

    /// Release the underlying peripherals after stopping transmission.
    pub fn release(self) -> (Option<I2sParallel<'static, Mode>>, Option<DmaTxBuf>) {
        (self.i2s, self.buffer)
    }
}

#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
impl Esp32I2sTx {
    /// Send one complete scanline, preserving the logical on-wire byte order.
    ///
    /// The caller is responsible for taking SPH low/high and holding CKV at
    /// the appropriate level for the entire transfer. This method returns
    /// after DMA has completed. For accurate pulse timing, account for
    /// startup and await overhead, then validate on a logic analyzer.
    pub async fn send_scanline(&mut self, line: &[u8]) -> Result<(), I2sTxError> {
        self.send_with_packer(line, pack_i2s1_scanline).await
    }

    /// Send a 336-byte ED052TC4 row using the Soldered Inkplate 5V2 DMA
    /// pixel ordering. This is a transmitter only, not a board power driver.
    /// Do not call during physical bring-up before GPIO timing is verified.
    pub async fn send_inkplate5v2_scanline(&mut self, line: &[u8]) -> Result<(), I2sTxError> {
        self.send_with_packer(line, pack_inkplate5v2_scanline).await
    }

    async fn send_with_packer(
        &mut self,
        line: &[u8],
        packer: fn(&[u8], &mut [u8]) -> Result<usize, PackError>,
    ) -> Result<(), I2sTxError> {
        let Some(i2s) = self.i2s.take() else {
            return Err(I2sTxError::Lost);
        };
        let Some(mut buffer) = self.buffer.take() else {
            self.i2s = Some(i2s);
            return Err(I2sTxError::Lost);
        };

        let packed = packer(line, buffer.as_mut_slice());
        let size = match packed {
            Ok(len) => len,
            Err(error) => {
                self.i2s = Some(i2s);
                self.buffer = Some(buffer);
                return Err(I2sTxError::Pack(error));
            }
        };
        buffer.set_length(size);

        match i2s.send(buffer) {
            Ok(transfer) => {
                #[cfg(feature = "blocking")]
                {
                    let (i2s, buffer) = transfer.wait();
                    self.i2s = Some(i2s);
                    self.buffer = Some(buffer);
                    Ok(())
                }
                #[cfg(not(feature = "blocking"))]
                {
                    let mut transfer = transfer;
                    let result = transfer.wait_for_done().await;
                    let (i2s, buffer) = transfer.wait();
                    self.i2s = Some(i2s);
                    self.buffer = Some(buffer);
                    result.map_err(I2sTxError::Dma)
                }
            }
            Err((error, i2s, buffer)) => {
                self.i2s = Some(i2s);
                self.buffer = Some(buffer);
                Err(I2sTxError::Dma(error))
            }
        }
    }
}
