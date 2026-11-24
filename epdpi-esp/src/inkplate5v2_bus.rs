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
//! The CKV/SPV/SPH/LE microsecond delays call `esp_hal::delay::Delay`,
//! whose elapsed-time loop **busy-waits** on classic ESP32 in blocking
//! and async builds alike. Host builds use a no-op delay only in the
//! permanently HV-locked simulation path. No hardware timing validation.
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
    /// Pack and size DMA data without changing GPIO or starting a transfer.
    /// Must complete *before* asserting SPH.
    fn prepare_vendor_row(&mut self, data: &[u8]) -> Result<(), Self::Error>;
    /// Transmit the already prepared row. Returns after DMA completion.
    async fn send_prepared_row(&mut self) -> Result<(), Self::Error>;
}

#[cfg(feature = "esp32")]
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
impl Inkplate5v2Dma for crate::i2s_parallel::Esp32I2sTx {
    type Error = crate::i2s_parallel::I2sTxError;
    fn prepare_vendor_row(&mut self, data: &[u8]) -> Result<(), Self::Error> {
        self.prepare_inkplate5v2_scanline(data)
    }
    async fn send_prepared_row(&mut self) -> Result<(), Self::Error> {
        self.send_prepared_inkplate5v2_scanline().await
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

/// Compact result alias for an operation that may fail in I2C, GPIO or DMA.
pub type Inkplate5v2Result<I2cError, PinError, DmaError> =
    Result<(), Inkplate5v2BusError<I2cError, PinError, DmaError>>;

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
    fn busy_wait_us(&mut self, us: u32) {
        #[cfg(feature = "esp32")]
        {
            // Directly invoke the HAL's elapsed-time busy loop. Never await
            // an async timer or yield between time-critical GPIO transitions.
            esp_hal::delay::Delay::new().delay_micros(us);
        }
        #[cfg(not(feature = "esp32"))]
        {
            // Host-only simulation cannot activate HV (set_power(true) is
            // locked), so no real frame runs. Fake signal tests separately
            // record and verify the requested minimum delays.
            let _ = us;
        }
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
    ) -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error> {
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
    ) -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error> {
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
    ) -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error> {
        if self.faulted {
            return Err(Inkplate5v2BusError::Faulted);
        }
        if self.power.state() != PowerState::On {
            return Err(Inkplate5v2BusError::NotPowered);
        }
        Ok(())
    }

    async fn start_frame_inner(&mut self)
        -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error>
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
        -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error>
    {
        if !self.frame_open || self.row_pending || data.len() != INKPLATE5V2_ROW_BYTES {
            return Err(Inkplate5v2BusError::RowLength);
        }
        // Pack/size DMA data while SPH is still high. This avoids moving
        // 336 bytes and resizing descriptors in the time-critical interval.
        self.tx.prepare_vendor_row(data).map_err(Inkplate5v2BusError::Dma)?;
        {
            let mut s = Signals { power: &mut self.power, pins: &mut self.pins };
            timing::row_start(&mut s).await.map_err(Self::signal_error)?;
        }
        // Still not cycle-equivalent to Soldered: esp-hal sets up its DMA
        // descriptor and starts TX here, after SPH/CKV. Measure this
        // interval before enabling any panel bias rails.
        self.tx.send_prepared_row().await.map_err(Inkplate5v2BusError::Dma)?;
        self.row_pending = true;
        Ok(())
    }

    async fn next_row_inner(&mut self)
        -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error>
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
        -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error>
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
        // A successful refresh must be allowed to power down and refresh
        // again. Do not use emergency_off() for an ordinary shutdown: it
        // deliberately latches faulted and invalidates the reusable bus.
        // Always request a PMIC shutdown even if one GPIO operation fails.
        let mut first_gpio_error = None;
        if let Err(error) = self.pins.sph.set_high() {
            first_gpio_error = Some(error);
        }
        if let Err(error) = self.pins.ckv.set_low()
            && first_gpio_error.is_none()
        {
            first_gpio_error = Some(error);
        }
        if let Err(error) = self.pins.le.set_low()
            && first_gpio_error.is_none()
        {
            first_gpio_error = Some(error);
        }

        let off = self.power.disable().await;
        self.frame_open = false;
        self.row_pending = false;
        if let Some(error) = first_gpio_error {
            self.faulted = true;
            return Err(Inkplate5v2BusError::Gpio(error));
        }
        if let Err(error) = off {
            self.faulted = true;
            return Err(Inkplate5v2BusError::Power(error));
        }
        if self.faulted || self.power.state() == PowerState::Faulted {
            return Err(Inkplate5v2BusError::Faulted);
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    extern crate std;

    use super::*;
    use core::convert::Infallible;
    use embedded_hal::digital::ErrorType as PinErrorType;
    use embedded_hal::i2c::{ErrorType as I2cErrorType, Error as I2cError, ErrorKind, Operation};
    use std::vec::Vec;
    use std::rc::Rc;
    use std::cell::RefCell;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct BusFault;
    impl I2cError for BusFault {
        fn kind(&self) -> ErrorKind { ErrorKind::Other }
    }

    #[derive(Default)]
    struct FakeI2c {
        regs0: [u8; 8],
        pmic_enable: u8,
        writes: Vec<(u8, u8, u8)>,
    }
    impl FakeI2c {
        fn fresh() -> Self {
            let mut x = Self::default();
            // All 16 expander pins are inputs at power-on.
            x.regs0[6] = 0xff;
            x.regs0[7] = 0xff;
            x.regs0[2] = 0xff;
            x.regs0[3] = 0xff;
            x
        }
    }
    impl I2cErrorType for FakeI2c { type Error = BusFault; }

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    impl I2c for FakeI2c {
        async fn transaction(
            &mut self,
            addr: u8,
            operations: &mut [Operation<'_>],
        ) -> Result<(), BusFault> {
            let mut register = 0;
            for op in operations {
                match op {
                    Operation::Write(b) => {
                        if let Some((&first, values)) = b.split_first() {
                            register = first;
                            for &v in values {
                                self.writes.push((addr, register, v));
                                if addr == 0x20 && register < 8 {
                                    self.regs0[usize::from(register)] = v;
                                }
                                if addr == 0x48 && register == 0x01 {
                                    self.pmic_enable = v;
                                }
                                register += 1;
                            }
                        }
                    }
                    Operation::Read(b) => for byte in b.iter_mut() {
                        *byte = if addr == 0x20 && register < 8 {
                            self.regs0[usize::from(register)]
                        } else if addr == 0x48 && register == 0x0f
                            && self.pmic_enable == 0x20
                            && self.regs0[2] & 0x10 != 0
                        {
                            0xfa // The fake supplies all rails when the PMIC is enabled.
                        } else {
                            0 // Other unmodeled PMIC registers read zero.
                        };
                        register += 1;
                    },
                }
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeDelay;
    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    impl DelayNs for FakeDelay {
        async fn delay_ns(&mut self, _: u32) {}
    }

    #[derive(Default)]
    struct FakePin {
        writes: Vec<bool>,
        name: &'static str,
        trace: Option<Rc<RefCell<Vec<&'static str>>>>,
    }
    impl PinErrorType for FakePin {
        type Error = Infallible;
    }
    impl OutputPin for FakePin {
        fn set_low(&mut self) -> Result<(), Infallible> {
            self.writes.push(false);
            if let Some(trace) = &self.trace {
                let event = match self.name {
                    "sph" => "sph-low",
                    "ckv" => "ckv-low",
                    _ => "le-low",
                };
                trace.borrow_mut().push(event);
            }
            Ok(())
        }
        fn set_high(&mut self) -> Result<(), Infallible> {
            self.writes.push(true);
            if let Some(trace) = &self.trace {
                let event = match self.name {
                    "sph" => "sph-high",
                    "ckv" => "ckv-high",
                    _ => "le-high",
                };
                trace.borrow_mut().push(event);
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeTx {
        transmissions: usize,
        preparations: usize,
        prepared: bool,
        trace: Option<Rc<RefCell<Vec<&'static str>>>>,
    }
    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    impl Inkplate5v2Dma for FakeTx {
        type Error = Infallible;
        fn prepare_vendor_row(&mut self, _: &[u8]) -> Result<(), Self::Error> {
            self.preparations += 1;
            self.prepared = true;
            if let Some(trace) = &self.trace { trace.borrow_mut().push("dma-prepared"); }
            Ok(())
        }
        async fn send_prepared_row(&mut self) -> Result<(), Self::Error> {
            assert!(self.prepared);
            self.prepared = false;
            self.transmissions += 1;
            if let Some(trace) = &self.trace { trace.borrow_mut().push("dma-sent"); }
            Ok(())
        }
    }

    fn bus() -> Inkplate5v2Bus<FakeTx, FakeI2c, FakeDelay, FakePin> {
        Inkplate5v2Bus::new(
            FakeTx::default(),
            Inkplate5v2Power::new(FakeI2c::fresh(), FakeDelay),
            Inkplate5v2DirectPins {
                le: FakePin::default(),
                ckv: FakePin::default(),
                sph: FakePin::default(),
            },
        )
    }

    macro_rules! run_both {
        ($name:ident, $body:ident) => {
            #[test]
            fn $name() {
                #[cfg(feature = "blocking")]
                { $body(); }
                #[cfg(not(feature = "blocking"))]
                { pollster::block_on($body()); }
            }
        };
    }

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn read_only_diagnostics_are_non_driving_body() {
        let mut b = bus();
        let p = b.inspect().await.unwrap();
        assert!(!p.clock_enabled());
        let (tx, power, pins) = b.release();
        let (i2c, _) = power.release();
        assert!(i2c.writes.is_empty());
        assert_eq!(tx.transmissions, 0);
        assert!(pins.ckv.writes.is_empty());
        assert!(pins.sph.writes.is_empty());
        assert!(pins.le.writes.is_empty());
    }
    run_both!(read_only_diagnostics_are_non_driving, read_only_diagnostics_are_non_driving_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn hardware_refresh_is_locked_even_after_preparation_body() {
        let mut b = bus();
        b.prepare_low_voltage().await.unwrap();
        assert!(b.inspect().await.unwrap().clock_enabled());
        let attempt = b.set_power(true).await;
        assert!(matches!(attempt, Err(Inkplate5v2BusError::HardwareNotValidated)));
        let attempt = b.start_frame().await;
        assert!(matches!(attempt, Err(Inkplate5v2BusError::NotPowered)));
        let (tx, power, pins) = b.release();
        let (i2c, _) = power.release();
        assert_eq!(tx.transmissions, 0);
        assert_eq!(pins.ckv.writes, &[false]);
        assert_eq!(pins.sph.writes, &[true]);
        assert_eq!(pins.le.writes, &[false]);
        assert!(!i2c.writes.iter().any(|&(addr, reg, value)|
            addr == 0x48 && reg == 0x01 && value == 0x20
        ));
        assert_eq!(i2c.regs0[2] & 0x3f, 0);
        assert_eq!(i2c.regs0[6] & 0x3f, 0);
        assert_eq!(i2c.regs0[3] & 0x01, 1);
        assert_eq!(i2c.regs0[7] & 0x01, 0);
    }
    run_both!(hardware_refresh_is_locked_even_after_preparation, hardware_refresh_is_locked_even_after_preparation_body);

    // Exercise the internal sequencing helper in isolation, with a fake DMA
    // sink and GPIO recorder. This does NOT bypass the public HV lock.
    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn packs_before_sph_then_waits_for_dma_before_latch_body() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let mut b = bus();
        b.tx.trace = Some(trace.clone());
        b.pins.sph.name = "sph";
        b.pins.ckv.name = "ckv";
        b.pins.le.name = "le";
        b.pins.sph.trace = Some(trace.clone());
        b.pins.ckv.trace = Some(trace.clone());
        b.pins.le.trace = Some(trace.clone());

        // The helper is private and the public bus continues to reject
        // power-on. Setting this test-only frame flag does not affect PMIC.
        b.frame_open = true;
        b.write_row_inner(&[0u8; INKPLATE5V2_ROW_BYTES]).await.unwrap();
        b.next_row_inner().await.unwrap();
        assert_eq!(*trace.borrow(), [
            "dma-prepared", "sph-low", "ckv-high", "dma-sent",
            "sph-high", "ckv-low", "le-high", "le-low",
        ]);
        assert_eq!(b.tx.preparations, 1);
        assert_eq!(b.tx.transmissions, 1);
        assert_eq!(b.power.state(), PowerState::Uninitialized);
    }
    run_both!(packs_before_sph_then_waits_for_dma_before_latch, packs_before_sph_then_waits_for_dma_before_latch_body);


    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn normal_power_off_preserves_bus_and_allows_repeated_updates_body() {
        let mut b = bus();
        b.prepare_low_voltage().await.unwrap();
        for _ in 0..2 {
            // Exercise the existing crate-private power controller to simulate
            // a fully powered state. The public HV interlock stays locked.
            b.power.enable().await.unwrap();
            assert_eq!(b.power.state(), PowerState::On);
            b.set_power(false).await.unwrap();
            assert_eq!(b.power.state(), PowerState::Idle);
            assert!(!b.faulted);
            assert!(!b.frame_open);
            assert!(!b.row_pending);
        }
        let (tx, power, _) = b.release();
        let (i2c, _) = power.release();
        assert_eq!(tx.transmissions, 0);
        assert_eq!(i2c.pmic_enable, 0);
    }
    run_both!(
        normal_power_off_preserves_bus_and_allows_repeated_updates,
        normal_power_off_preserves_bus_and_allows_repeated_updates_body
    );

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn normal_power_off_does_not_bypass_hv_lock_body() {
        let mut b = bus();
        b.prepare_low_voltage().await.unwrap();
        let first = b.set_power(true).await;
        assert!(matches!(first, Err(Inkplate5v2BusError::HardwareNotValidated)));
        b.set_power(false).await.unwrap();
        let second = b.set_power(true).await;
        assert!(matches!(second, Err(Inkplate5v2BusError::HardwareNotValidated)));
    }
    run_both!(normal_power_off_does_not_bypass_hv_lock, normal_power_off_does_not_bypass_hv_lock_body);

}
