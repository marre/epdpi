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
//! CKV/SPH/LE idle levels, reads PMIC standby registers with off indicators
//! verified and configures PCAL Port 1's GPIO0 clock enable. This
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
/// Physical ED052TC4 gate rows, exactly one row advance per scanline.
pub const INKPLATE5V2_ROWS: usize = 720;

/// Transmit the Soldered ED052TC4 row layout via classic ESP32 I2S1.
/// The caller configures the 8-bit data GPIO, GPIO0 clock and DMA channel.
#[allow(async_fn_in_trait)]
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
pub trait Inkplate5v2Dma {
    type Error;
    /// Route owned clock/data GPIOs to I2S after the off-state preflight.
    fn arm_pins(&mut self) -> Result<(), Self::Error>;
    /// Disconnect I2S from the clock/data pads and drive them low.
    /// Must work even if the DMA transfer or its resources were lost.
    fn quiesce_pins(&mut self) -> Result<(), Self::Error>;
    /// Disable output drive and pulls on all clock/data GPIOs.
    fn release_pins(&mut self) -> Result<(), Self::Error>;
    /// Pack and size DMA data without changing GPIO or starting a transfer.
    /// Must complete *before* asserting SPH.
    fn prepare_vendor_row(&mut self, data: &[u8]) -> Result<(), Self::Error>;
    /// Transmit the already prepared row. Returns after DMA completion.
    async fn send_prepared_row(&mut self) -> Result<(), Self::Error>;
}

/// Direct control pin with explicitly reversible output drive.
pub trait Inkplate5v2ControlPin: OutputPin {
    /// Enable output drive at the requested initial level.
    fn arm_output(&mut self, high: bool) -> Result<(), Self::Error>;
    /// Disable output drive and internal pulls.
    fn release_output(&mut self) -> Result<(), Self::Error>;
}

#[cfg(feature = "esp32")]
impl Inkplate5v2ControlPin for esp_hal::gpio::Flex<'_> {
    fn arm_output(&mut self, high: bool) -> Result<(), Self::Error> {
        use esp_hal::gpio::{Level, OutputConfig};
        self.set_output_enable(false);
        self.set_level(if high { Level::High } else { Level::Low });
        self.apply_output_config(&OutputConfig::default());
        self.set_output_enable(true);
        Ok(())
    }
    fn release_output(&mut self) -> Result<(), Self::Error> {
        use esp_hal::gpio::InputConfig;
        self.set_output_enable(false);
        self.apply_input_config(&InputConfig::default());
        self.set_input_enable(true);
        Ok(())
    }
}

/// Inkplate-specific ESP32 I2S transport with retained, exclusive pin handles.
/// The generic HAL transmitter remains reusable for other boards.
#[cfg(feature = "esp32")]
pub struct Inkplate5v2Tx {
    tx: crate::i2s_parallel::Esp32I2sTx,
    // D0..D7, then CL. These handles are never passed into the HAL I2S
    // constructor. It receives NoPin placeholders instead.
    pins: [esp_hal::gpio::AnyPin<'static>; 9],
    armed: bool,
}

#[cfg(feature = "esp32")]
impl Inkplate5v2Tx {
    /// Create the I2S peripheral with no connected pads. GPIO ownership is
    /// retained for disconnection after DMA failure as well as normal off.
    pub fn new(
        i2s: esp_hal::peripherals::I2S1<'static>,
        channel: esp_hal::peripherals::DMA_I2S1<'static>,
        buffer: esp_hal::dma::DmaTxBuf,
        pins: [esp_hal::gpio::AnyPin<'static>; 9],
    ) -> Self {
        use esp_hal::{gpio::NoPin, i2s::parallel::{I2sParallel, TxEightBits}, time::Rate};
        let parallel = I2sParallel::new(
            i2s, channel, Rate::from_mhz(16),
            TxEightBits::new(NoPin, NoPin, NoPin, NoPin, NoPin, NoPin, NoPin, NoPin),
            NoPin,
        );
        #[cfg(not(feature = "blocking"))]
        let parallel = parallel.into_async();
        Self { tx: crate::i2s_parallel::Esp32I2sTx::new(parallel, buffer), pins, armed: false }
    }
}

#[cfg(feature = "esp32")]
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
impl Inkplate5v2Dma for Inkplate5v2Tx {
    type Error = crate::i2s_parallel::I2sTxError;

    fn arm_pins(&mut self) -> Result<(), Self::Error> {
        use esp_hal::gpio::{Flex, OutputConfig, OutputSignal};
        // Match esp-hal 1.1 I2S1's 8-bit data_out_signal() and ws_signal().
        // This preserves the existing routing, including its known
        // unmeasured WS-versus-vendor-BCK clock difference.
        let signals = [
            OutputSignal::I2S1O_DATA_0, OutputSignal::I2S1O_DATA_1,
            OutputSignal::I2S1O_DATA_2, OutputSignal::I2S1O_DATA_3,
            OutputSignal::I2S1O_DATA_4, OutputSignal::I2S1O_DATA_5,
            OutputSignal::I2S1O_DATA_6, OutputSignal::I2S1O_DATA_7,
            OutputSignal::I2S1O_WS,
        ];
        for (pin, signal) in self.pins.iter_mut().zip(signals) {
            let mut gpio = Flex::new(pin.reborrow());
            gpio.set_low();
            gpio.apply_output_config(&OutputConfig::default());
            gpio.set_output_enable(true);
            signal.connect_to(&gpio);
        }
        self.armed = true;
        Ok(())
    }

    fn quiesce_pins(&mut self) -> Result<(), Self::Error> {
        use esp_hal::gpio::{Flex, OutputConfig};
        self.armed = false;
        self.tx.discard_prepared();
        // Flex::new() disables output and resets GPIO matrix and IO MUX,
        // including the peripheral-controlled output selection. Merely
        // setting a GPIO latch or dropping I2sParallel is insufficient.
        // Disconnect CL first, so subsequent data changes cannot be clocked.
        for pin in self.pins.iter_mut().rev() {
            let mut gpio = Flex::new(pin.reborrow());
            gpio.set_low();
            gpio.apply_output_config(&OutputConfig::default());
            gpio.set_output_enable(true);
        }
        Ok(())
    }

    fn release_pins(&mut self) -> Result<(), Self::Error> {
        use esp_hal::gpio::{Flex, InputConfig};
        self.armed = false;
        self.tx.discard_prepared();
        for pin in self.pins.iter_mut().rev() {
            let mut gpio = Flex::new(pin.reborrow());
            gpio.apply_input_config(&InputConfig::default());
            gpio.set_input_enable(true);
        }
        Ok(())
    }

    fn prepare_vendor_row(&mut self, data: &[u8]) -> Result<(), Self::Error> {
        if !self.armed {
            return Err(Self::Error::Unprepared);
        }
        self.tx.prepare_inkplate5v2_scanline(data)
    }

    async fn send_prepared_row(&mut self) -> Result<(), Self::Error> {
        if !self.armed {
            return Err(Self::Error::Unprepared);
        }
        self.tx.send_prepared_inkplate5v2_scanline().await
    }
}

/// Direct ESP32 pins. Caller supplies exclusive handles with reversible drive.
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
    /// An incorrect number of physical rows was requested in the frame.
    FrameRowCount { expected: usize, actual: usize },
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
    io_released: bool,
    frame_open: bool,
    row_pending: bool,
    completed_rows: usize,
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
            io_released: false,
            frame_open: false,
            row_pending: false,
            completed_rows: 0,
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
impl<I2C: I2c, DELAY: DelayNs, P: Inkplate5v2ControlPin> Inkplate5v2Signals
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
impl<TX: Inkplate5v2Dma, I2C: I2c, DELAY: DelayNs, P: Inkplate5v2ControlPin>
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
        self.faulted = true;
        let result = self.prepare_sequence().await;
        if result.is_ok() {
            self.faulted = false;
        } else if self.power.state() == PowerState::Idle && !self.io_released {
            let _ = self.shutdown(true).await;
        }
        result
    }

    async fn prepare_sequence(
        &mut self,
    ) -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error> {
        // Fail closed before driving any direct panel pins. On a repeat
        // cycle, verify the released GPIO configuration and rearm only
        // while PMIC controls are still low.
        if self.io_released {
            self.power.rearm_panel_controls().await.map_err(Inkplate5v2BusError::Power)?;
            self.io_released = false;
        } else {
            self.power.preflight_low_voltage().await.map_err(Inkplate5v2BusError::Power)?;
            self.power.initialize().await.map_err(Inkplate5v2BusError::Power)?;
        }
        self.pins.le.arm_output(false).map_err(Inkplate5v2BusError::Gpio)?;
        self.pins.ckv.arm_output(false).map_err(Inkplate5v2BusError::Gpio)?;
        self.pins.sph.arm_output(true).map_err(Inkplate5v2BusError::Gpio)?;
        {
            let mut s = Signals { power: &mut self.power, pins: &mut self.pins };
            timing::idle_before_power(&mut s).await.map_err(|e| match e {
                Inkplate5v2BusError::Power(e) => Inkplate5v2BusError::Power(e),
                Inkplate5v2BusError::Gpio(e) => Inkplate5v2BusError::Gpio(e),
                _ => Inkplate5v2BusError::Faulted,
            })?;
        }
        self.tx.arm_pins().map_err(Inkplate5v2BusError::Dma)?;
        self.power.configure_clock_gate().await.map_err(Inkplate5v2BusError::Power)?;
        self.io_released = false;
        Ok(())
    }

    /// Shut down prepared panel GPIOs and request PMIC off. This is also
    /// available to the HV-locked low-voltage diagnostic firmware.
    pub async fn shutdown_panel(&mut self) -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error> {
        self.set_power(false).await
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
        let _ = self.shutdown(true).await;
    }

    // One ordering for ordinary and emergency shutdown. Mark the bus
    // unusable before the first await so a cancelled future cannot resume
    // scanning. A healthy ordinary shutdown restores reusability.
    async fn shutdown(
        &mut self,
        emergency: bool,
    ) -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error> {
        let previously_faulted = self.faulted;
        self.faulted = true;
        if self.io_released {
            self.power.verify_released_controls().await.map_err(Inkplate5v2BusError::Power)?;
            if emergency || previously_faulted {
                return Err(Inkplate5v2BusError::Faulted);
            }
            self.faulted = false;
            return Ok(());
        }
        let gate = self.power.quiesce_panel().await;
        let clock_data_low = self.tx.quiesce_pins();
        // Match the vendor's OE/GMOD-low before direct control changes.
        // Clock and data have been disconnected from I2S above.
        let mut first_gpio_error = None;
        if let Err(error) = self.pins.sph.set_low() {
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
        // Even an OE or direct GPIO failure must not skip PMIC shutdown.
        let off = self.power.disable().await;
        // Always remove direct pad drive, even if PMIC status is uncertain.
        // Keep expander OE/GMOD/SPV driven low on uncertain rail shutdown.
        let clock_data_z = self.tx.release_pins();
        for pin in [&mut self.pins.le, &mut self.pins.ckv, &mut self.pins.sph] {
            if let Err(error) = pin.release_output()
                && first_gpio_error.is_none()
            {
                first_gpio_error = Some(error);
            }
        }
        let expander_z = if off.is_ok() && self.power.state() == PowerState::Idle {
            self.power.release_panel_controls().await
        } else {
            Ok(())
        };
        self.frame_open = false;
        self.row_pending = false;
        self.completed_rows = 0;
        // Record a successfully released state even if quiescing reported
        // an earlier error. A retry must not drive the pins again.
        self.io_released = off.is_ok()
            && clock_data_z.is_ok()
            && expander_z.is_ok()
            && first_gpio_error.is_none()
            && self.power.state() == PowerState::Idle;
        gate.map_err(Inkplate5v2BusError::Power)?;
        clock_data_low.map_err(Inkplate5v2BusError::Dma)?;
        if let Some(error) = first_gpio_error {
            return Err(Inkplate5v2BusError::Gpio(error));
        }
        off.map_err(Inkplate5v2BusError::Power)?;
        clock_data_z.map_err(Inkplate5v2BusError::Dma)?;
        expander_z.map_err(Inkplate5v2BusError::Power)?;
        if emergency || previously_faulted || self.power.state() == PowerState::Faulted {
            return Err(Inkplate5v2BusError::Faulted);
        }
        self.faulted = false;
        Ok(())
    }

    async fn require_ready(
        &mut self,
    ) -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error> {
        if self.faulted {
            return Err(Inkplate5v2BusError::Faulted);
        }
        if self.io_released || self.power.state() != PowerState::On {
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
        self.completed_rows = 0;
        Ok(())
    }

    async fn write_row_inner(&mut self, data: &[u8])
        -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error>
    {
        if !self.frame_open || self.row_pending {
            return Err(Inkplate5v2BusError::Faulted);
        }
        if self.completed_rows >= INKPLATE5V2_ROWS {
            return Err(Inkplate5v2BusError::FrameRowCount {
                expected: INKPLATE5V2_ROWS,
                actual: self.completed_rows + 1,
            });
        }
        if data.len() != INKPLATE5V2_ROW_BYTES {
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
        self.completed_rows += 1;
        Ok(())
    }

    async fn end_frame_inner(&mut self)
        -> Inkplate5v2Result<I2C::Error, P::Error, TX::Error>
    {
        if !self.frame_open || self.row_pending {
            return Err(Inkplate5v2BusError::Faulted);
        }
        if self.completed_rows != INKPLATE5V2_ROWS {
            return Err(Inkplate5v2BusError::FrameRowCount {
                expected: INKPLATE5V2_ROWS,
                actual: self.completed_rows,
            });
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
impl<TX: Inkplate5v2Dma, I2C: I2c, DELAY: DelayNs, P: Inkplate5v2ControlPin>
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
            return if self.faulted { Err(Inkplate5v2BusError::Faulted) } else { Ok(()) };
        }
        self.shutdown(false).await
    }

    async fn start_frame(&mut self) -> Result<(), Self::Error> {
        self.require_ready().await?;
        // PowerState::On is only a cached fact from the initial ramp. Check
        // both PGOOD sources and the actual output latches before *each*
        // vertical scan; do not drive another frame after rails disappear.
        let health = self.power.verify_live_rails().await.map_err(Inkplate5v2BusError::Power);
        if health.is_err() {
            self.emergency_off().await;
            return health;
        }
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
    use std::cell::{Cell, RefCell};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct BusFault;
    impl I2cError for BusFault {
        fn kind(&self) -> ErrorKind { ErrorKind::Other }
    }

    struct FakeI2c {
        regs0: [u8; 0x50],
        pmic_enable: u8,
        pmic_registers: [u8; 17],
        writes: Vec<(u8, u8, u8)>,
        rails_ready: Rc<Cell<bool>>,
        physical_pgood: Rc<Cell<Option<bool>>>,

    }
    impl FakeI2c {
        fn fresh() -> Self {
            let mut x = Self {
                regs0: [0; 0x50],
                pmic_enable: 0,
                pmic_registers: [0; 17],
                writes: Vec::new(),
                rails_ready: Rc::new(Cell::new(false)),
                physical_pgood: Rc::new(Cell::new(None)),
            };
            // All 16 expander pins are inputs at power-on.
            x.regs0[6] = 0xff;
            x.regs0[7] = 0xff;
            x.regs0[2] = 0xff;
            x.regs0[3] = 0xff;
            x.rails_ready.set(true);
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
            if addr == 0x48 && self.regs0[2] & !self.regs0[6] & 0x08 == 0 {
                return Err(BusFault);
            }
            let mut register = 0;
            for op in operations {
                match op {
                    Operation::Write(b) => {
                        if let Some((&first, values)) = b.split_first() {
                            register = first;
                            for &v in values {
                                self.writes.push((addr, register, v));
                                if addr == 0x20 && register < 0x50 {
                                    self.regs0[usize::from(register)] = v;
                                    if self.regs0[2] & !self.regs0[6] & 0x08 == 0 {
                                        self.pmic_enable = 0;
                                        self.pmic_registers = [0; 17];
                                    }
                                }
                                if addr == 0x48 {
                                    self.pmic_registers[usize::from(register)] = v;
                                    if register == 0x01 {
                                        self.pmic_enable = v;
                                    }
                                }
                                register += 1;
                            }
                        }
                    }
                    Operation::Read(b) => for byte in b.iter_mut() {
                        *byte = if addr == 0x20 && register == 0x00 {
                            // The board routes physical PMIC PGOOD to P0.7.
                            // Simulate that wire separately from status 0x0F.
                            let powered = self.pmic_enable == 0x20
                                && self.regs0[2] & 0x10 != 0
                                && self.rails_ready.get();
                            let physical = self.physical_pgood.get().unwrap_or(powered);
                            (self.regs0[0] & !0x80) | (self.regs0[2] & !self.regs0[6] & 0x3f)
                                | if physical { 0x80 } else { 0 }
                        } else if addr == 0x20 && register == 0x01 {
                            self.regs0[1] | (self.regs0[3] & !self.regs0[7])
                        } else if addr == 0x20 && register < 0x50 {
                            self.regs0[usize::from(register)]
                        } else if addr == 0x48 && register == 0x0f
                            && self.pmic_enable == 0x20
                            && self.regs0[2] & 0x10 != 0
                            && self.rails_ready.get()
                        {
                            0xfa // The fake supplies all rails when the PMIC is enabled.
                        } else if addr == 0x48 && register == 0x01 {
                            let controls = self.regs0[2] & !self.regs0[6];
                            self.pmic_enable
                                | if controls & 0x10 != 0 { 0x0f } else { 0 }
                                | if controls & 0x20 != 0 { 0x10 } else { 0 }
                        } else if addr == 0x48 && register < 17 && register != 0x0f {
                            self.pmic_registers[usize::from(register)]
                        } else {
                            0
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
        released: bool,
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

    impl Inkplate5v2ControlPin for FakePin {
        fn arm_output(&mut self, _high: bool) -> Result<(), Infallible> {
            self.released = false;
            Ok(())
        }
        fn release_output(&mut self) -> Result<(), Infallible> {
            self.released = true;
            if let Some(trace) = &self.trace { trace.borrow_mut().push("control-high-z"); }
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeTx {
        transmissions: usize,
        preparations: usize,
        prepared: bool,
        released: bool,
        fail_send: bool,
        fail_quiesce: bool,
        trace: Option<Rc<RefCell<Vec<&'static str>>>>,
    }
    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    impl Inkplate5v2Dma for FakeTx {
        type Error = BusFault;
        fn arm_pins(&mut self) -> Result<(), Self::Error> {
            self.released = false;
            Ok(())
        }
        fn quiesce_pins(&mut self) -> Result<(), Self::Error> {
            self.prepared = false;
            if let Some(trace) = &self.trace { trace.borrow_mut().push("clock-data-low"); }
            if self.fail_quiesce { Err(BusFault) } else { Ok(()) }
        }
        fn release_pins(&mut self) -> Result<(), Self::Error> {
            self.released = true;
            if let Some(trace) = &self.trace { trace.borrow_mut().push("clock-data-high-z"); }
            Ok(())
        }
        fn prepare_vendor_row(&mut self, _: &[u8]) -> Result<(), Self::Error> {
            self.preparations += 1;
            self.prepared = true;
            if let Some(trace) = &self.trace { trace.borrow_mut().push("dma-prepared"); }
            Ok(())
        }
        async fn send_prepared_row(&mut self) -> Result<(), Self::Error> {
            assert!(self.prepared);
            self.prepared = false;
            if self.fail_send { return Err(BusFault); }
            self.transmissions += 1;
            if let Some(trace) = &self.trace { trace.borrow_mut().push("dma-sent"); }
            Ok(())
        }
    }

    fn bus_with_i2c(i2c: FakeI2c) -> Inkplate5v2Bus<FakeTx, FakeI2c, FakeDelay, FakePin> {
        Inkplate5v2Bus::new(
            FakeTx::default(),
            Inkplate5v2Power::new(i2c, FakeDelay),
            Inkplate5v2DirectPins {
                le: FakePin::default(),
                ckv: FakePin::default(),
                sph: FakePin::default(),
            },
        )
    }

    fn bus() -> Inkplate5v2Bus<FakeTx, FakeI2c, FakeDelay, FakePin> {
        bus_with_i2c(FakeI2c::fresh())
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
    async fn unsafe_entry_must_not_touch_any_output_body() {
        let fake = FakeI2c::fresh();
        // P0.7 high means the e-paper rails may still be energized.
        // Model a physical input, not the PCAL input register's stale
        // snapshot, to keep the mock faithful to the live pin override.
        fake.physical_pgood.set(Some(true));
        let mut b = bus_with_i2c(fake);
        let result = b.prepare_low_voltage().await;
        assert!(matches!(
            result,
            Err(Inkplate5v2BusError::Power(PowerError::PhysicalPowerGoodAlreadyHigh))
        ));
        let off = b.set_power(false).await;
        assert!(matches!(off, Err(Inkplate5v2BusError::Faulted)));
        let (tx, power, pins) = b.release();
        let (i2c, _) = power.release();
        assert!(i2c.writes.is_empty(), "No PCAL or PMIC writes on unsafe entry");
        assert!(pins.ckv.writes.is_empty());
        assert!(pins.sph.writes.is_empty());
        assert!(pins.le.writes.is_empty());
        assert_eq!(tx.transmissions, 0);
    }
    run_both!(unsafe_entry_must_not_touch_any_output, unsafe_entry_must_not_touch_any_output_body);

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
        for cycle in 0..2 {
            if cycle != 0 {
                b.prepare_low_voltage().await.unwrap();
            }
            // Exercise the existing crate-private power controller to simulate
            // a fully powered state. The public HV interlock stays locked.
            b.power.enable().await.unwrap();
            assert_eq!(b.power.state(), PowerState::On);
            b.set_power(false).await.unwrap();
            assert_eq!(b.power.state(), PowerState::Idle);
            assert!(!b.faulted);
            assert!(!b.frame_open);
            assert!(!b.row_pending);
            assert!(b.io_released);
            assert!(b.tx.released);
            assert!(b.pins.le.released && b.pins.ckv.released && b.pins.sph.released);
            let snapshot = b.inspect().await.unwrap();
            assert_eq!(snapshot.direction_port0 & 0x3f, 7);
            assert!(!snapshot.clock_enabled());
            // Repeated shutdown must not briefly turn the pins back on.
            b.set_power(false).await.unwrap();
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

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn refuses_next_frame_if_rails_drop_after_prior_success_body() {
        let fake = FakeI2c::fresh();
        let ready = fake.rails_ready.clone();
        let mut b = bus_with_i2c(fake);
        b.prepare_low_voltage().await.unwrap();
        // Simulation-only: internal PMIC may be powered for tests, public
        // set_power(true) remains permanently hardware-locked.
        b.power.enable().await.unwrap();
        b.start_frame().await.unwrap();
        for _ in 0..INKPLATE5V2_ROWS {
            b.write_scanline(&[0; INKPLATE5V2_ROW_BYTES]).await.unwrap();
            b.next_row().await.unwrap();
        }
        b.end_frame().await.unwrap();
        let gpio_calls = b.pins.ckv.writes.len();
        ready.set(false);
        let result = b.start_frame().await;
        assert!(matches!(
            result,
            Err(Inkplate5v2BusError::Power(PowerError::PowerGoodLost(0)))
        ));
        assert!(b.faulted);
        assert_eq!(b.power.state(), PowerState::Faulted);
        assert_eq!(b.tx.transmissions, INKPLATE5V2_ROWS);
        // Only the emergency shutdown may touch GPIO after failed health.
        // It must not enter the next vertical scan sequence.
        assert_eq!(&b.pins.ckv.writes[gpio_calls..], &[false]);
        let (tx, power, _) = b.release();
        let (i2c, _) = power.release();
        assert_eq!(tx.transmissions, INKPLATE5V2_ROWS);
        assert_eq!(i2c.pmic_enable, 0);
    }
    run_both!(
        refuses_next_frame_if_rails_drop_after_prior_success,
        refuses_next_frame_if_rails_drop_after_prior_success_body
    );

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn refuses_next_frame_if_physical_pgood_drops_body() {
        let fake = FakeI2c::fresh();
        let physical = fake.physical_pgood.clone();
        let mut b = bus_with_i2c(fake);
        b.prepare_low_voltage().await.unwrap();
        b.power.enable().await.unwrap();
        physical.set(Some(false));
        let result = b.start_frame().await;
        assert!(matches!(
            result,
            Err(Inkplate5v2BusError::Power(PowerError::PhysicalPowerGoodLow))
        ));
        assert!(b.faulted);
        assert_eq!(b.tx.transmissions, 0);
        assert!(!b.frame_open);
        // Force the mock physical input low but allow logical shutdown.
        assert_eq!(b.power.state(), PowerState::Faulted);
    }
    run_both!(
        refuses_next_frame_if_physical_pgood_drops,
        refuses_next_frame_if_physical_pgood_drops_body
    );



    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn short_frame_fails_closed_without_declaring_success_body() {
        let mut b = bus();
        b.prepare_low_voltage().await.unwrap();
        b.power.enable().await.unwrap(); // test-only, public HV interlock unchanged
        b.start_frame().await.unwrap();
        let result = b.end_frame().await;
        assert!(matches!(
            result,
            Err(Inkplate5v2BusError::FrameRowCount { expected: 720, actual: 0 })
        ));
        assert!(b.faulted);
        // Emergency shutdown can leave the power controller Idle while
        // the bus itself remains permanently fault-latched.
        assert_eq!(b.power.state(), PowerState::Idle);
        assert_eq!(b.tx.transmissions, 0);
    }
    run_both!(
        short_frame_fails_closed_without_declaring_success,
        short_frame_fails_closed_without_declaring_success_body
    );

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn extra_row_is_rejected_before_dma_body() {
        let mut b = bus();
        b.prepare_low_voltage().await.unwrap();
        b.power.enable().await.unwrap(); // test-only
        b.start_frame().await.unwrap();
        for _ in 0..INKPLATE5V2_ROWS {
            b.write_scanline(&[0; INKPLATE5V2_ROW_BYTES]).await.unwrap();
            b.next_row().await.unwrap();
        }
        let result = b.write_scanline(&[0; INKPLATE5V2_ROW_BYTES]).await;
        assert!(matches!(
            result,
            Err(Inkplate5v2BusError::FrameRowCount { expected: 720, actual: 721 })
        ));
        assert!(b.faulted);
        assert_eq!(b.tx.transmissions, INKPLATE5V2_ROWS);
        // Emergency shutdown can leave the power controller Idle while
        // the bus itself remains permanently fault-latched.
        assert_eq!(b.power.state(), PowerState::Idle);
    }
    run_both!(extra_row_is_rejected_before_dma, extra_row_is_rejected_before_dma_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn exact_row_count_survives_multiple_complete_frames_body() {
        let mut b = bus();
        b.prepare_low_voltage().await.unwrap();
        b.power.enable().await.unwrap(); // test-only
        for _ in 0..2 {
            b.start_frame().await.unwrap();
            for _ in 0..INKPLATE5V2_ROWS {
                b.write_scanline(&[0; INKPLATE5V2_ROW_BYTES]).await.unwrap();
                b.next_row().await.unwrap();
            }
            b.end_frame().await.unwrap();
            assert_eq!(b.completed_rows, INKPLATE5V2_ROWS);
            assert!(!b.frame_open);
            assert!(!b.row_pending);
        }
        b.set_power(false).await.unwrap();
        assert_eq!(b.power.state(), PowerState::Idle);
        assert_eq!(b.completed_rows, 0);
        assert!(!b.faulted);
        assert_eq!(b.tx.transmissions, INKPLATE5V2_ROWS * 2);
    }
    run_both!(
        exact_row_count_survives_multiple_complete_frames,
        exact_row_count_survives_multiple_complete_frames_body
    );

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn dma_failure_releases_all_panel_pins_body() {
        let mut b = bus();
        b.prepare_low_voltage().await.unwrap();
        b.power.enable().await.unwrap(); // test only
        b.start_frame().await.unwrap();
        b.tx.fail_send = true;
        let failed = b.write_scanline(&[0; INKPLATE5V2_ROW_BYTES]).await;
        assert!(matches!(failed, Err(Inkplate5v2BusError::Dma(BusFault))));
        assert!(b.faulted);
        assert!(b.tx.released);
        assert!(b.pins.le.released && b.pins.ckv.released && b.pins.sph.released);
        let snapshot = b.inspect().await.unwrap();
        assert_eq!(snapshot.direction_port0 & 0x3f, 7);
        assert!(!snapshot.clock_enabled());
        assert!(!snapshot.pgood_pin_high());
        let next = b.start_frame().await;
        assert!(matches!(next, Err(Inkplate5v2BusError::Faulted)));
    }
    run_both!(dma_failure_releases_all_panel_pins, dma_failure_releases_all_panel_pins_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn quiesce_failure_does_not_skip_power_or_high_z_body() {
        let mut b = bus();
        b.prepare_low_voltage().await.unwrap();
        b.power.enable().await.unwrap();
        b.tx.fail_quiesce = true;
        let off = b.set_power(false).await;
        assert!(matches!(off, Err(Inkplate5v2BusError::Dma(BusFault))));
        assert!(b.faulted);
        assert!(b.tx.released);
        assert!(b.pins.le.released && b.pins.ckv.released && b.pins.sph.released);
        let snapshot = b.inspect().await.unwrap();
        assert_eq!(snapshot.direction_port0 & 0x3f, 7);
        assert!(!snapshot.pgood_pin_high());
    }
    run_both!(quiesce_failure_does_not_skip_power_or_high_z, quiesce_failure_does_not_skip_power_or_high_z_body);

}
