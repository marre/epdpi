//! Inkplate 5 Gen 2 board composition: classic ESP32 I2S1, direct
//! GPIO controls, PCAL6416A gate signals and TPS65186 rails.
//!
//! # Bring-up lock
//!
//! This driver is **deliberately locked**: `set_power(true)` returns
//! `HardwareNotValidated` before any panel signals are clocked. The
//! underlying PMIC controller's high-voltage `enable` remains crate-private.
//! We have not measured the I2S1 `tx_right_first` difference versus Soldered
//! or verified rail/clock timing on a physical Inkplate 5 Gen 2.
//! Do not use this as a working display driver yet.
//!
//! `prepare_low_voltage()` is a separate, explicit operation that sets
//! CKV/SPH/LE idle levels, initializes PMIC sequencer registers with rails
//! verified off and configures PCAL Port 1's GPIO0 clock enable. This
//! changes real low-voltage GPIO output states, so it is **not read-only**.
//! The safer first diagnostic is `power().inspect_expander()`.
//!
//! The three ESP32 control pins are LE GPIO2, CKV GPIO32, SPH GPIO33.
//! SPV lives on expander Port 0 bit 2. DMA uses I2S1, GPIO0 CL and data
//! GPIO4/5/18/19/23/25/26/27, supplied in a preconfigured transmitter.
//! The same I2C owner sequences SPV and PMIC, preventing conflicting writes.
//!
//! Failure in a signal or DMA transfer latches a bus fault and prevents
//! further scanning. It makes a best-effort power shutdown. Software
//! cancellation/reset can still interrupt cleanup; physical validation is
//! required before enabling any panel bias supplies.

use embedded_hal::digital::OutputPin;
#[cfg(feature = "blocking")]
use embedded_hal::{delay::DelayNs, i2c::I2c};
#[cfg(not(feature = "blocking"))]
use embedded_hal_async::{delay::DelayNs, i2c::I2c};

use epdpi::EpdParallelBus;
use crate::inkplate5v2_power::{ExpanderSnapshot, Inkplate5v2Power, PowerError, PowerState};
use crate::inkplate5v2_timing::{self as timing, Inkplate5v2Signals};

/// Bytes in one ED052TC4 row including Soldered's trailing clock bytes.
pub const INKPLATE5V2_ROW_BYTES: usize = 336;

/// Transmit the Soldered ED052TC4 row layout via classic ESP32 I2S1.
/// The caller configures the 8-bit data GPIO, GPIO0 clock and DMA channel.
#[allow(async_fn_in_trait)]
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
pub trait Inkplate5v2Dma {
    type Error;
    /// Returns only after the transmission completes.
    async fn send_vendor_row(&mut self, data: &[u8]) -> Result<(), Self::Error>;
}

#[cfg(feature = "esp32")]
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
impl Inkplate5v2Dma for crate::i2s_parallel::Esp32I2sTx {
    type Error = crate::i2s_parallel::I2sTxError;
    async fn send_vendor_row(&mut self, data: &[u8]) -> Result<(), Self::Error> {
        self.send_inkplate5v2_scanline(data).await
    }
}

/// Direct ESP32 pins. Caller must supply already-configured output GPIOs.
/// These are independent from the expander pin SPV.
pub struct Inkplate5v2DirectPins<P> {
    pub le: P,
    pub ckv: P,
    pub sph: P,
}

/// Board bus errors. A hardware fault does not imply the PMIC rails are off.
#[derive(Debug)]
#[non_exhaustive]
pub enum Inkplate5v2BusError<I2C, GPIO, DMA> {
    /// PCAL6416A / TPS65186 I2C operation failed.
    Power(PowerError<I2C>),
    /// ESP32 control pin failure.
    Gpio(GPIO),
    /// DMA failure.
    Dma(DMA),
    /// Row size must be exactly 336 bytes.
    RowLength,
    /// Attempted a frame operation without verified power.
    NotPowered,
    /// A previous error leaves the board state uncertain.
    Faulted,
    /// Physical driver enable is locked pending hardware validation.
    HardwareNotValidated,
}

/// Shared state machine for the board. Does not silently initialize or
/// energize the panel in its constructor.
pub struct Inkplate5v2Bus<TX, I2C, DELAY, P> {
    tx: TX,
    power: Inkplate5v2Power<I2C, DELAY>,
    pins: Inkplate5v2DirectPins<P>,
    faulted: bool,
    frame_open: bool,
    row_pending: bool,
}

impl<TX, I2C, DELAY, P> Inkplate5v2Bus<TX, I2C, DELAY, P> {
    /// Construct from an I2S1 DMA transmitter, shared PMIC/expander I2C
    /// owner and the three control pins. No GPIO operations happen here.
    pub fn new(
        tx: TX,
        power: Inkplate5v2Power<I2C, DELAY>,
        pins: Inkplate5v2DirectPins<P>,
    ) -> Self {
        Self {
            tx,
            power,
            pins,
            faulted: false,
            frame_open: false,
            row_pending: false,
        }
    }

    /// Access the PMIC / expander for read-only inspection.
    pub fn power(&mut self) -> &mut Inkplate5v2Power<I2C, DELAY> {
        &mut self.power
    }

    /// Return the constituent devices, without changing hardware state.
    /// Do not call this while a DMA transfer or power sequence is active.
    pub fn release(self) -> (TX, Inkplate5v2Power<I2C, DELAY>, Inkplate5v2DirectPins<P>) {
        (self.tx, self.power, self.pins)
    }
}

/// Borrowed view over the direct pins and the expander signal owned by
/// the PMIC controller. No independent I2C access is possible.
struct Signals<'a, I2C, DELAY, P> {
    power: &'a mut Inkplate5v2Power<I2C, DELAY>,
    pins: &'a mut Inkplate5v2DirectPins<P>,
}

#[allow(async_fn_in_trait)]
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
impl<I2C: I2c, DELAY: DelayNs, P: OutputPin> Inkplate5v2Signals
    for Signals<'_, I2C, DELAY, P>
{
    type Error = Inkplate5v2BusError<I2C::Error, P::Error, ()>;

    async fn set_le(&mut self, high: bool) -> Result<(), Self::Error> {
        let pin = &mut self.pins.le;
        if high { pin.set_high() } else { pin.set_low() }.map_err(Inkplate5v2BusError::Gpio)
    }
    async fn set_ckv(&mut self, high: bool) -> Result<(), Self::Error> {
        let pin = &mut self.pins.ckv;
        if high { pin.set_high() } else { pin.set_low() }.map_err(Inkplate5v2BusError::Gpio)
    }
    async fn set_sph(&mut self, high: bool) -> Result<(), Self::Error> {
        let pin = &mut self.pins.sph;
        if high { pin.set_high() } else { pin.set_low() }.map_err(Inkplate5v2BusError::Gpio)
    }
    async fn set_spv(&mut self, high: bool) -> Result<(), Self::Error> {
        self.power.set_spv(high).await.map_err(Inkplate5v2BusError::Power)
    }
    async fn delay_us(&mut self, us: u32) {
        self.power.delay_us(us).await;
    }
}

#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
impl<TX: Inkplate5v2Dma, I2C: I2c, DELAY: DelayNs, P: OutputPin>
    Inkplate5v2Bus<TX, I2C, DELAY, P>
{
    /// Low-voltage preparation, **not** a read-only diagnostic.
    /// Does not enable high-voltage rails, source outputs or send DMA data.
    pub async fn prepare_low_voltage(
        &mut self,
    ) -> Result<(), Inkplate5v2BusError<I2C::Error, P::Error, TX::Error>> {
        if self.faulted {
            return Err(Inkplate5v2BusError::Faulted);
        }
        let result = self.prepare_sequence().await;
        if result.is_err() {
            self.faulted = true;
        }
        result
    }

    async fn prepare_sequence(
        &mut self,
    ) -> Result<(), Inkplate5v2BusError<I2C::Error, P::Error, TX::Error>> {
        if self.power.state() != PowerState::Uninitialized {
            return Err(Inkplate5v2BusError::Power(
                PowerError::InvalidState(self.power.state())
            ));
        }
        {
            let mut s = Signals { power: &mut self.power, pins: &mut self.pins };
            timing::idle_before_power(&mut s).await.map_err(|e| match e {
                Inkplate5v2BusError::Power(e) => Inkplate5v2BusError::Power(e),
                Inkplate5v2BusError::Gpio(e) => Inkplate5v2BusError::Gpio(e),
                _ => Inkplate5v2BusError::Faulted,
            })?;
        }
        self.power.initialize().await.map_err(Inkplate5v2BusError::Power)?;
        self.power.configure_clock_gate().await.map_err(Inkplate5v2BusError::Power)?;
        Ok(())
    }

    /// Non-driving snapshot. Does not require or establish an initialized
    /// board; no GPIO values are modified.
    pub async fn inspect(
        &mut self,
    ) -> Result<ExpanderSnapshot, PowerError<I2C::Error>> {
        self.power.inspect_expander().await
    }

    fn signal_error(
        e: Inkplate5v2BusError<I2C::Error, P::Error, ()>,
    ) -> Inkplate5v2BusError<I2C::Error, P::Error, TX::Error> {
        match e {
            Inkplate5v2BusError::Power(e) => Inkplate5v2BusError::Power(e),
            Inkplate5v2BusError::Gpio(e) => Inkplate5v2BusError::Gpio(e),
            _ => Inkplate5v2BusError::Faulted,
        }
    }

    async fn emergency_off(&mut self) {
        self.faulted = true;
        // Best effort: leave clock/data signals idle and request PMIC off.
        // There is no guarantee this runs on cancellation or hard reset.
        let _ = self.pins.sph.set_low();
        let _ = self.pins.ckv.set_low();
        let _ = self.pins.le.set_low();
        let _ = self.power.disable().await;
        self.frame_open = false;
        self.row_pending = false;
    }

    async fn require_ready(
        &mut self,
    ) -> Result<(), Inkplate5v2BusError<I2C::Error, P::Error, TX::Error>> {
        if self.faulted {
            return Err(Inkplate5v2BusError::Faulted);
        }
        if self.power.state() != PowerState::On {
            return Err(Inkplate5v2BusError::NotPowered);
        }
        Ok(())
    }

    async fn start_frame_inner(&mut self)
        -> Result<(), Inkplate5v2BusError<I2C::Error, P::Error, TX::Error>>
    {
        if self.frame_open || self.row_pending {
            return Err(Inkplate5v2BusError::Faulted);
        }
        let mut s = Signals { power: &mut self.power, pins: &mut self.pins };
        timing::frame_start(&mut s).await.map_err(Self::signal_error)?;
        self.frame_open = true;
        Ok(())
    }

    async fn write_row_inner(&mut self, data: &[u8])
        -> Result<(), Inkplate5v2BusError<I2C::Error, P::Error, TX::Error>>
    {
        if !self.frame_open || self.row_pending || data.len() != INKPLATE5V2_ROW_BYTES {
            return Err(Inkplate5v2BusError::RowLength);
        }
        {
            let mut s = Signals { power: &mut self.power, pins: &mut self.pins };
            timing::row_start(&mut s).await.map_err(Self::signal_error)?;
        }
        // SPH low and CKV high before I2S starts. The electrical delay
        // between these and esp-hal DMA TX start is not yet verified.
        self.tx.send_vendor_row(data).await.map_err(Inkplate5v2BusError::Dma)?;
        self.row_pending = true;
        Ok(())
    }

    async fn next_row_inner(&mut self)
        -> Result<(), Inkplate5v2BusError<I2C::Error, P::Error, TX::Error>>
    {
        if !self.frame_open || !self.row_pending {
            return Err(Inkplate5v2BusError::Faulted);
        }
        let mut s = Signals { power: &mut self.power, pins: &mut self.pins };
        timing::row_end(&mut s).await.map_err(Self::signal_error)?;
        self.row_pending = false;
        Ok(())
    }

    async fn end_frame_inner(&mut self)
        -> Result<(), Inkplate5v2BusError<I2C::Error, P::Error, TX::Error>>
    {
        if !self.frame_open || self.row_pending {
            return Err(Inkplate5v2BusError::Faulted);
        }
        let mut s = Signals { power: &mut self.power, pins: &mut self.pins };
        timing::frame_end(&mut s).await;
        self.frame_open = false;
        Ok(())
    }
}

#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
impl<TX: Inkplate5v2Dma, I2C: I2c, DELAY: DelayNs, P: OutputPin>
    EpdParallelBus for Inkplate5v2Bus<TX, I2C, DELAY, P>
{
    type Error = Inkplate5v2BusError<I2C::Error, P::Error, TX::Error>;

    async fn set_power(&mut self, on: bool) -> Result<(), Self::Error> {
        if on {
            // Intentionally deny all HV activation. The implementation is
            // ready for electrical validation but NOT an approved panel bus.
            return Err(Inkplate5v2BusError::HardwareNotValidated);
        }
        if self.power.state() == PowerState::Uninitialized {
            return Ok(());
        }
        self.emergency_off().await;
        if self.power.state() == PowerState::Faulted {
            Err(Inkplate5v2BusError::Faulted)
        } else {
            Ok(())
        }
    }

    async fn start_frame(&mut self) -> Result<(), Self::Error> {
        self.require_ready().await?;
        let result = self.start_frame_inner().await;
        if result.is_err() { self.emergency_off().await; }
        result
    }

    async fn write_scanline(&mut self, data: &[u8]) -> Result<(), Self::Error> {
        self.require_ready().await?;
        let result = self.write_row_inner(data).await;
        if result.is_err() { self.emergency_off().await; }
        result
    }

    async fn next_row(&mut self) -> Result<(), Self::Error> {
        self.require_ready().await?;
        let result = self.next_row_inner().await;
        if result.is_err() { self.emergency_off().await; }
        result
    }

    async fn end_frame(&mut self) -> Result<(), Self::Error> {
        self.require_ready().await?;
        let result = self.end_frame_inner().await;
        if result.is_err() { self.emergency_off().await; }
        result
    }
}
