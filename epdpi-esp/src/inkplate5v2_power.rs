//! Inkplate 5 Gen 2 PCAL6416A / TPS65186 power controller.
//!
//! This module is deliberately separate from the ESP32 I2S transmitter. It
//! drives the board's six PCAL6416A Port 0 signals and programs the TPS65186
//! sequencer through a **shared** `embedded-hal` I2C bus. It uses the Soldered
//! Inkplate5V2 pin map and power-on ordering.
//!
//! This implementation has host tests, but it is **not hardware-validated**.
//! In particular, `enable` requires the caller to have already configured
//! the ESP32's direct SPH/CKV/LE/CL/data pins to the manufacturer's idle states.
//! There is not yet a full board-level `EpdParallelBus`. Do not energize the
//! panel based on software-only testing.
//!
//! No method writes TPS65186 VCOM voltage or EEPROM calibration registers.
//! A failed or unverified shutdown latches a fault, preventing further
//! automatic power-up attempts. On I2C failures, shutdown is best effort;
//! no software can guarantee physical rails turn off if hardware is stuck.
//!
//! Manufacturer references:
//! - Inkplate5V2Driver.cpp::einkOn/einkOff, pins.h
//! - TPS65186.cpp::begin/powerUp/powerDown
//! - pcalExpander.h PCAL6416A register map

#[cfg(feature = "blocking")]
use embedded_hal::{delay::DelayNs, i2c::I2c};
#[cfg(not(feature = "blocking"))]
use embedded_hal_async::{delay::DelayNs, i2c::I2c};

const EXPANDER_ADDR: u8 = 0x20;
const PMIC_ADDR: u8 = 0x48;
const PCAL_INPUT0: u8 = 0x00;
const PCAL_OUTPUT0: u8 = 0x02;
const PCAL_OUTPUT1: u8 = 0x03;
const PCAL_CONFIG0: u8 = 0x06;
const PCAL_CONFIG1: u8 = 0x07;
const PMIC_ENABLE: u8 = 0x01;
const PMIC_UPSEQ0: u8 = 0x09;
const PMIC_DWNSEQ0: u8 = 0x0b;
const PMIC_PWRGOOD: u8 = 0x0f;
const PGOOD_ALL_RAILS: u8 = 0xfa;
const STARTUP_TIMEOUT_MS: u16 = 500;
const SHUTDOWN_TIMEOUT_MS: u16 = 250;

const OE: u8 = 1 << 0;
const GMOD: u8 = 1 << 1;
const SPV: u8 = 1 << 2;
const WAKEUP: u8 = 1 << 3;
const PWRUP: u8 = 1 << 4;
const VCOM: u8 = 1 << 5;
const ALL_CONTROL: u8 = OE | GMOD | SPV | WAKEUP | PWRUP | VCOM;

/// Port 1, bit 0 (expander pin 8) enables the ESP32 GPIO0 clock path.
/// Only the explicit low-voltage clock setup method may modify this pin.
const GPIO0_ENABLE: u8 = 1;
/// P0.7 is hard-wired to the TPS65186 PWR_GOOD net in KiCad V1.1.0.
const PWR_GOOD_PIN: u8 = 1 << 7;

/// Whether the controller will allow an attempt to enable panel supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    /// Hardware has not been inspected and configured yet.
    Uninitialized,
    /// Power rails confirmed off during initialization or shutdown.
    Idle,
    /// PGOOD verified and VCOM/OE enabled.
    On,
    /// An operation failed; further enable attempts are forbidden.
    Faulted,
}

/// Read-only snapshot of PCAL6416A output latches and directions.
///
/// Bits are numbered 0..7 in each port. A direction bit of 1 means input,
/// 0 means output. An output-latch bit alone does **not** prove its GPIO
/// voltage, or that the panel's high-voltage rails are off.
/// Port 0 bits 0..5 control OE, GMOD, SPV, WAKEUP, PWRUP and VCOM.
/// Port 1 bit 0 (logical expander pin 8) is the GPIO0 clock enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpanderSnapshot {
    /// Actual P0 input levels, including physical PWR_GOOD on bit 7.
    pub input_port0: u8,
    pub output_port0: u8,
    pub direction_port0: u8,
    pub output_port1: u8,
    pub direction_port1: u8,
}

impl ExpanderSnapshot {
    /// Logic level of the PMIC's physical PWR_GOOD signal on Port 0 bit 7.
    /// This is a pin reading, not a substitute for PMIC register 0x0F
    /// or for observing the actual high-voltage rails.
    pub fn pgood_pin_high(self) -> bool {
        self.input_port0 & PWR_GOOD_PIN != 0
    }

    /// Whether GPIO0 clock enable is configured as an active HIGH output.
    pub fn clock_enabled(self) -> bool {
        self.direction_port1 & GPIO0_ENABLE == 0 && self.output_port1 & GPIO0_ENABLE != 0
    }
}

/// A power sequencing or I2C failure.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PowerError<E> {
    /// An I2C transaction failed.
    I2c(E),
    /// This operation requires a different power state.
    InvalidState(PowerState),
    /// Some control pins were actively driven high on entry; refuse to
    /// reconfigure a potentially powered display without an explicit recovery.
    ActiveControlPins,
    /// PGOOD was unexpectedly nonzero when initializing with rails disabled.
    RailsAlreadyOn(u8),
    /// Not all rails became good within the startup timeout.
    PowerGoodTimeout(u8),
    /// A previously energized panel lost one or more PMIC rails.
    PowerGoodLost(u8),
    /// Source/PMIC control signals are no longer asserted while scanning.
    PanelControlLost,
    /// Rails did not all discharge during the bounded shutdown wait.
    DischargeTimeout(u8),
    /// Clock gate is absent or is not configured as an output.
    ClockNotEnabled,
    /// Clock enable state cannot be verified by register readback.
    ClockReadbackMismatch,
    /// PMIC reports all rails ready but the separate physical PGOOD pin is low.
    PhysicalPowerGoodLow,
    /// PMIC reports discharged rails but the independent PGOOD pin is still high.
    PhysicalPowerGoodStillHigh,
    /// The PGOOD pin is unexpectedly high before touching power/control pins.
    PhysicalPowerGoodAlreadyHigh,
    /// Expander P0.7 must be configured as an input to observe PGOOD.
    PowerGoodPinNotInput,
}

/// Controls the PCAL6416A Port 0 power pins and TPS65186 high-voltage rails.
///
/// Owns an I2C bus and delay because the PCAL6416A and TPS65186 share that
/// bus. External consumers must not change Port 0's power-related output
/// bits while this controller is active.
///
/// A faulted controller never retries power-up. A reset/recovery workflow
/// must first establish the physical rail state.
pub struct Inkplate5v2Power<I2C, DELAY> {
    i2c: I2C,
    delay: DELAY,
    state: PowerState,
}

impl<I2C, DELAY> Inkplate5v2Power<I2C, DELAY> {
    /// Construct without touching a board. The rails must be considered
    /// untrusted until `initialize` checks them.
    pub fn new(i2c: I2C, delay: DELAY) -> Self {
        Self {
            i2c,
            delay,
            state: PowerState::Uninitialized,
        }
    }

    /// Current latched state. `Faulted` must not be treated as powered off.
    pub fn state(&self) -> PowerState {
        self.state
    }

    /// Recover the owned peripheral and delay. Does not change any pins.
    pub fn release(self) -> (I2C, DELAY) {
        (self.i2c, self.delay)
    }
}

#[allow(dead_code)] // enable/disable remain crate-private until an audited board bus uses them.
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
impl<I2C: I2c, DELAY: DelayNs> Inkplate5v2Power<I2C, DELAY> {
    async fn read(&mut self, address: u8, register: u8) -> Result<u8, PowerError<I2C::Error>> {
        let mut value = [0];
        self.i2c
            .write_read(address, &[register], &mut value)
            .await
            .map_err(PowerError::I2c)?;
        Ok(value[0])
    }

    async fn write(
        &mut self,
        address: u8,
        register: u8,
        values: &[u8],
    ) -> Result<(), PowerError<I2C::Error>> {
        // All current board register writes are one or four bytes.
        // Keep a fixed local array so this works without an allocator.
        let mut packet = [0; 5];
        if values.len() > 4 {
            return Err(PowerError::InvalidState(self.state));
        }
        packet[0] = register;
        packet[1..1 + values.len()].copy_from_slice(values);
        self.i2c
            .write(address, &packet[..1 + values.len()])
            .await
            .map_err(PowerError::I2c)
    }

    /// Change only the six reserved Port 0 pins; leave other GPIOs alone.
    async fn expander_levels(
        &mut self,
        mask: u8,
        levels: u8,
    ) -> Result<(), PowerError<I2C::Error>> {
        let old = self.read(EXPANDER_ADDR, PCAL_OUTPUT0).await?;
        let new = (old & !mask) | (levels & mask);
        self.write(EXPANDER_ADDR, PCAL_OUTPUT0, &[new]).await
    }

    /// Read the expander register state without changing GPIOs or high-voltage
    /// rails. This is safe as a first diagnostic after the I2C bus is ready.
    /// It does not access a possibly sleeping TPS65186.
    pub async fn inspect_expander(&mut self) -> Result<ExpanderSnapshot, PowerError<I2C::Error>> {
        Ok(ExpanderSnapshot {
            input_port0: self.read(EXPANDER_ADDR, PCAL_INPUT0).await?,
            direction_port0: self.read(EXPANDER_ADDR, PCAL_CONFIG0).await?,
            output_port0: self.read(EXPANDER_ADDR, PCAL_OUTPUT0).await?,
            direction_port1: self.read(EXPANDER_ADDR, PCAL_CONFIG1).await?,
            output_port1: self.read(EXPANDER_ADDR, PCAL_OUTPUT1).await?,
        })
    }

    /// Read-only safety preflight, before configuring any display output
    /// GPIO, I2S routing, or PMIC sequencer. This cannot prove that rails
    /// are electrically off; it only rejects known dangerous states.
    pub async fn preflight_low_voltage(&mut self) -> Result<ExpanderSnapshot, PowerError<I2C::Error>> {
        if self.state != PowerState::Uninitialized {
            return Err(PowerError::InvalidState(self.state));
        }
        let snapshot = self.inspect_expander().await?;
        Self::verify_initial_snapshot(snapshot)?;
        Ok(snapshot)
    }

    fn verify_initial_snapshot(snapshot: ExpanderSnapshot) -> Result<(), PowerError<I2C::Error>> {
        if snapshot.direction_port0 & PWR_GOOD_PIN == 0 {
            return Err(PowerError::PowerGoodPinNotInput);
        }
        if snapshot.pgood_pin_high() {
            return Err(PowerError::PhysicalPowerGoodAlreadyHigh);
        }
        // During a partially completed vendor power-on, WAKEUP, GMOD or
        // SPV can be asserted before OE/PWRUP/VCOM. None is safe to ignore.
        if ((!snapshot.direction_port0) & snapshot.output_port0 & ALL_CONTROL) != 0 {
            return Err(PowerError::ActiveControlPins);
        }
        Ok(())
    }

    /// Configure the GPIO0 clock-enable expander output (Port 1, bit 0).
    ///
    /// Only callable with rails confirmed idle. Preload output HIGH before
    /// configuring the pin as output, as in the manufacturer setup. Other
    /// Port 1 pins retain their current latches and directions. This does
    /// **not** activate the TPS65186 high-voltage rails or send display data.
    pub async fn configure_clock_gate(&mut self) -> Result<(), PowerError<I2C::Error>> {
        if self.state != PowerState::Idle {
            return Err(PowerError::InvalidState(self.state));
        }
        // Once any operation starts, an I2C error has an uncertain board
        // state, and we refuse to re-enable the panel without reinspection.
        self.state = PowerState::Faulted;
        let outputs = self.read(EXPANDER_ADDR, PCAL_OUTPUT1).await?;
        let direction = self.read(EXPANDER_ADDR, PCAL_CONFIG1).await?;
        self.write(EXPANDER_ADDR, PCAL_OUTPUT1, &[outputs | GPIO0_ENABLE]).await?;
        self.write(EXPANDER_ADDR, PCAL_CONFIG1, &[direction & !GPIO0_ENABLE]).await?;
        let check = self.inspect_expander().await?;
        if !check.clock_enabled() {
            return Err(PowerError::ClockReadbackMismatch);
        }
        self.state = PowerState::Idle;
        Ok(())
    }

    /// Set the gate start signal on PCAL6416A Port 0 bit 2.
    /// This is only valid after PGOOD is established.
    pub(crate) async fn set_spv(&mut self, high: bool) -> Result<(), PowerError<I2C::Error>> {
        if self.state != PowerState::On {
            return Err(PowerError::InvalidState(self.state));
        }
        let result = self.expander_levels(SPV, if high { SPV } else { 0 }).await;
        if result.is_err() {
            self.state = PowerState::Faulted;
        }
        result
    }

    /// Inspect the initial state, disable the six control outputs, and
    /// initialize the PMIC sequencer. This never enables high-voltage rails.
    ///
    /// If PWRUP/VCOM/OE are already configured as outputs and high, refuse
    /// to touch the board: it might still be powered by another firmware.
    pub async fn initialize(&mut self) -> Result<(), PowerError<I2C::Error>> {
        if self.state != PowerState::Uninitialized {
            return Err(PowerError::InvalidState(self.state));
        }
        // Any failure latches Faulted, including an inconsistent initial state.
        self.state = PowerState::Faulted;
        let snapshot = self.inspect_expander().await?;
        Self::verify_initial_snapshot(snapshot)?;
        let direction = snapshot.direction_port0;
        let outputs = snapshot.output_port0;
        // PCAL6416A powers up with outputs HIGH. Program safe output latches
        // *before* setting direction to output, to avoid a high-going glitch.
        self.write(EXPANDER_ADDR, PCAL_OUTPUT0, &[outputs & !ALL_CONTROL]).await?;
        self.write(EXPANDER_ADDR, PCAL_CONFIG0, &[direction & !ALL_CONTROL]).await?;

        self.expander_levels(WAKEUP, WAKEUP).await?;
        self.delay.delay_ms(1).await;
        // Check PGOOD before writing PMIC sequencer registers. A nonzero
        // value is an unexpected powered or partially powered condition.
        let checked = match self.read(PMIC_ADDR, PMIC_PWRGOOD).await {
            Ok(0) => self.write(PMIC_ADDR, PMIC_UPSEQ0, &[0x1b, 0x00, 0x1b, 0x00]).await,
            Ok(value) => Err(PowerError::RailsAlreadyOn(value)),
            Err(err) => Err(err),
        };
        self.delay.delay_ms(1).await;
        let asleep = self.expander_levels(WAKEUP, 0).await;
        checked?;
        asleep?;
        self.state = PowerState::Idle;
        Ok(())
    }

    /// Enable power rails using the manufacturer's sequence.
    ///
    /// **Precondition:** the *separate* ESP32 direct GPIO lines (SPH, CKV,
    /// LE and data/CL) are already in manufacturer-defined idle levels.
    /// There is intentionally no board-facing entrypoint until the complete
    /// control bus is implemented and verified.
    pub(crate) async fn enable(&mut self) -> Result<(), PowerError<I2C::Error>> {
        if self.state != PowerState::Idle {
            return Err(PowerError::InvalidState(self.state));
        }
        // GPIO0 is a boot strapping pin on classic ESP32. Refuse panel
        // power unless the vendor's expander clock gate is configured.
        let clock = match self.inspect_expander().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                // Read-only doesn't mean failure can be retried blindly:
                // an I2C failure leaves current clock-gate state unknown.
                self.state = PowerState::Faulted;
                return Err(error);
            }
        };
        if !clock.clock_enabled() {
            return Err(PowerError::ClockNotEnabled);
        }
        self.state = PowerState::Faulted;
        let result = self.enable_sequence().await;
        if result.is_err() {
            // Best-effort shutdown, even on the first I2C error. Never
            // silently downgrade a failed power-up back to Idle.
            let _ = self.shutdown_sequence().await;
            return result;
        }
        self.state = PowerState::On;
        Ok(())
    }

    async fn enable_sequence(&mut self) -> Result<(), PowerError<I2C::Error>> {
        // Soldered's einkOn: GMOD/SPV high, OE low, before PMIC power-up.
        self.expander_levels(OE | GMOD | SPV | VCOM | PWRUP, GMOD | SPV).await?;
        self.expander_levels(WAKEUP, WAKEUP).await?;
        self.delay.delay_ms(5).await;
        self.write(PMIC_ADDR, PMIC_ENABLE, &[0x20]).await?;
        self.write(PMIC_ADDR, PMIC_UPSEQ0, &[0xe4]).await?;
        self.write(PMIC_ADDR, PMIC_DWNSEQ0, &[0x1b]).await?;
        self.expander_levels(PWRUP, PWRUP).await?;

        let mut last = 0;
        for _ in 0..STARTUP_TIMEOUT_MS {
            last = self.read(PMIC_ADDR, PMIC_PWRGOOD).await?;
            if last == PGOOD_ALL_RAILS {
                // Both independent indicators must agree; allow the PCAL
                // input to settle until the bounded startup deadline rather
                // than faulting on a transient disagreement.
                if self.inspect_expander().await?.pgood_pin_high() {
                    self.expander_levels(VCOM, VCOM).await?;
                    self.expander_levels(OE, OE).await?;
                    return Ok(());
                }
            }
            self.delay.delay_ms(1).await;
        }
        if last == PGOOD_ALL_RAILS {
            Err(PowerError::PhysicalPowerGoodLow)
        } else {
            Err(PowerError::PowerGoodTimeout(last))
        }
    }

    /// Confirm the supply and output gates are still valid immediately
    /// before a new frame is clocked. Cached `PowerState::On` alone does
    /// not prove rails remained good following an earlier frame.
    pub(crate) async fn verify_live_rails(&mut self) -> Result<(), PowerError<I2C::Error>> {
        if self.state != PowerState::On {
            return Err(PowerError::InvalidState(self.state));
        }
        let result = self.verify_live_rails_inner().await;
        if result.is_err() {
            self.state = PowerState::Faulted;
        }
        result
    }

    async fn verify_live_rails_inner(&mut self) -> Result<(), PowerError<I2C::Error>> {
        let status = self.read(PMIC_ADDR, PMIC_PWRGOOD).await?;
        if status != PGOOD_ALL_RAILS {
            return Err(PowerError::PowerGoodLost(status));
        }
        let snapshot = self.inspect_expander().await?;
        if !snapshot.pgood_pin_high() {
            return Err(PowerError::PhysicalPowerGoodLow);
        }
        let active = OE | VCOM | PWRUP | GMOD | SPV;
        if snapshot.direction_port0 & active != 0 || snapshot.output_port0 & active != active {
            return Err(PowerError::PanelControlLost);
        }
        Ok(())
    }

    /// Disable VCOM and panel output, request rails off and wait until PGOOD
    /// reports zero. An I2C error or a timeout latches `Faulted`.
    ///
    /// Caller must separately quiesce ESP32 direct control lines before
    /// calling this method. The physical shutdown cannot be guaranteed if
    /// the I2C bus or the expander itself is stuck.
    pub(crate) async fn disable(&mut self) -> Result<(), PowerError<I2C::Error>> {
        if self.state == PowerState::Idle {
            return Ok(());
        }
        if self.state == PowerState::Uninitialized {
            return Err(PowerError::InvalidState(self.state));
        }
        let result = self.shutdown_sequence().await;
        if self.state != PowerState::Faulted && result.is_ok() {
            self.state = PowerState::Idle;
        } else {
            self.state = PowerState::Faulted;
        }
        result
    }

    async fn shutdown_sequence(&mut self) -> Result<(), PowerError<I2C::Error>> {
        let mut first: Option<PowerError<I2C::Error>> = None;
        // Stop source driving first. Then VCOM off, then request rails off.
        if let Err(e) = self.expander_levels(OE | GMOD | SPV, 0).await {
            first = Some(e);
        }
        if let Err(e) = self.expander_levels(VCOM, 0).await
            && first.is_none()
        {
            first = Some(e);
        }
        if let Err(e) = self.expander_levels(PWRUP, 0).await
            && first.is_none()
        {
            first = Some(e);
        }

        // The manufacturer's off sequence waits up to 250 ms for PGOOD=0.
        let mut pgood = 0xff;
        let mut discharged = false;
        let mut physical_high = false;
        for _ in 0..SHUTDOWN_TIMEOUT_MS {
            match self.read(PMIC_ADDR, PMIC_PWRGOOD).await {
                Ok(0) => {
                    pgood = 0;
                    match self.inspect_expander().await {
                        Ok(snapshot) if !snapshot.pgood_pin_high() => {
                            discharged = true;
                            break;
                        }
                        Ok(_) => {
                            physical_high = true;
                            self.delay.delay_ms(1).await;
                        }
                        Err(e) => {
                            if first.is_none() { first = Some(e); }
                            break;
                        }
                    }
                }
                Ok(value) => { pgood = value; self.delay.delay_ms(1).await; }
                Err(e) => {
                    if first.is_none() { first = Some(e); }
                    break;
                }
            }
        }
        if !discharged && first.is_none() {
            first = if physical_high && pgood == 0 {
                Some(PowerError::PhysicalPowerGoodStillHigh)
            } else {
                Some(PowerError::DischargeTimeout(pgood))
            };
        }
        // Match Soldered TPS65186::powerDown(): drop WAKEUP first, then
        // write ENABLE=0. Continue both attempts even if one fails.
        if let Err(e) = self.expander_levels(WAKEUP, 0).await
            && first.is_none()
        {
            first = Some(e);
        }
        if let Err(e) = self.write(PMIC_ADDR, PMIC_ENABLE, &[0]).await
            && first.is_none()
        {
            first = Some(e);
        }
        if let Some(e) = first { Err(e) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    extern crate std;

    use super::*;
    use embedded_hal::i2c::{ErrorKind, ErrorType, Operation};
    use std::vec::Vec;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct FakeError;
    impl embedded_hal::i2c::Error for FakeError {
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Event {
        Read(u8, u8, u8),
        Write(u8, u8, Vec<u8>),
    }

    struct FakeI2c {
        pcal: [u8; 8],
        pmic: [u8; 16],
        events: Vec<Event>,
        rails_ready: bool,
        fail_sequence_write_once: bool,
        fail_on_write_attempt: Option<usize>,
        write_attempts: usize,
        fail_pgood_read_once: bool,
        fail_expander_read_once: bool,
        stuck_high_after_shutdown: bool,
        pgood_pin_stuck_low: bool,
        pgood_pin_stuck_high: bool,
    }

    impl FakeI2c {
        fn new() -> Self {
            let mut pcal = [0u8; 8];
            pcal[PCAL_CONFIG0 as usize] = 0xff;
            pcal[PCAL_OUTPUT0 as usize] = 0xff;
            pcal[PCAL_CONFIG1 as usize] = 0xff;
            pcal[PCAL_OUTPUT1 as usize] = 0xff;
            Self {
                pcal,
                pmic: [0u8; 16],
                events: Vec::new(),
                rails_ready: true,
                fail_sequence_write_once: false,
                fail_on_write_attempt: None,
                write_attempts: 0,
                fail_pgood_read_once: false,
                fail_expander_read_once: false,
                stuck_high_after_shutdown: false,
                pgood_pin_stuck_low: false,
                pgood_pin_stuck_high: false,
            }
        }

        fn pgood(&self) -> u8 {
            if self.stuck_high_after_shutdown {
                return PGOOD_ALL_RAILS;
            }
            if self.rails_ready
                && self.pcal[PCAL_OUTPUT0 as usize] & PWRUP != 0
                && self.pmic[PMIC_ENABLE as usize] == 0x20
            {
                PGOOD_ALL_RAILS
            } else {
                0
            }
        }
    }

    impl ErrorType for FakeI2c {
        type Error = FakeError;
    }

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    impl I2c for FakeI2c {
        async fn transaction(
            &mut self,
            addr: u8,
            operations: &mut [Operation<'_>],
        ) -> Result<(), Self::Error> {
            let mut register = 0;
            for operation in operations {
                match operation {
                    Operation::Write(bytes) => {
                        if let Some((&first, tail)) = bytes.split_first() {
                            register = first;
                            if !tail.is_empty() {
                                self.write_attempts += 1;
                                if self.fail_on_write_attempt == Some(self.write_attempts) {
                                    self.fail_on_write_attempt = None;
                                    return Err(FakeError);
                                }
                                if addr == PMIC_ADDR && register == PMIC_UPSEQ0
                                    && tail[0] == 0xe4 && self.fail_sequence_write_once
                                {
                                    self.fail_sequence_write_once = false;
                                    return Err(FakeError);
                                }
                                self.events.push(Event::Write(addr, register, tail.to_vec()));
                                for (offset, value) in tail.iter().enumerate() {
                                    let index = register as usize + offset;
                                    if addr == EXPANDER_ADDR {
                                        self.pcal[index] = *value;
                                    } else if addr == PMIC_ADDR {
                                        self.pmic[index] = *value;
                                    } else {
                                        return Err(FakeError);
                                    }
                                }
                            }
                        }
                    }
                    Operation::Read(out) => {
                        if addr == EXPANDER_ADDR && self.fail_expander_read_once {
                            self.fail_expander_read_once = false;
                            return Err(FakeError);
                        }
                        if addr == PMIC_ADDR && register == PMIC_PWRGOOD
                            && self.fail_pgood_read_once
                        {
                            self.fail_pgood_read_once = false;
                            return Err(FakeError);
                        }
                        for byte in out.iter_mut() {
                            let value = if addr == EXPANDER_ADDR && register == PCAL_INPUT0 {
                                let base = self.pcal[PCAL_INPUT0 as usize] & !PWR_GOOD_PIN;
                                // The pin can be asserted independently
                                // of PMIC register state in a read-only test.
                                // Normally simulate the physical PGOOD output
                                // tracking all-rail readiness.
                                base | if !self.pgood_pin_stuck_low
                                    && (self.pgood_pin_stuck_high
                                        || self.pcal[PCAL_INPUT0 as usize] & PWR_GOOD_PIN != 0
                                        || self.pgood() == PGOOD_ALL_RAILS)
                                {
                                    PWR_GOOD_PIN
                                } else {
                                    0
                                }
                            } else if addr == EXPANDER_ADDR {
                                self.pcal[register as usize]
                            } else if addr == PMIC_ADDR && register == PMIC_PWRGOOD {
                                self.pgood()
                            } else if addr == PMIC_ADDR {
                                self.pmic[register as usize]
                            } else {
                                return Err(FakeError);
                            };
                            self.events.push(Event::Read(addr, register, value));
                            *byte = value;
                            register += 1;
                        }
                    }
                }
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeDelay {
        total_ns: u64,
    }

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    impl DelayNs for FakeDelay {
        async fn delay_ns(&mut self, ns: u32) {
            self.total_ns += u64::from(ns);
        }
    }

    macro_rules! run_test {
        ($test:ident, $body:ident) => {
            #[test]
            fn $test() {
                #[cfg(feature = "blocking")]
                {
                    $body();
                }
                #[cfg(not(feature = "blocking"))]
                {
                    pollster::block_on($body());
                }
            }
        };
    }

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn starts_without_activating_rails_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        assert_eq!(power.state(), PowerState::Uninitialized);
        power.initialize().await.unwrap();
        assert_eq!(power.state(), PowerState::Idle);
        let (i2c, _) = power.release();
        assert_eq!(i2c.pcal[PCAL_CONFIG0 as usize] & ALL_CONTROL, 0);
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, 0);
        assert_eq!(i2c.pmic[PMIC_UPSEQ0 as usize], 0x1b);
        assert_eq!(i2c.pmic[(PMIC_UPSEQ0 + 1) as usize], 0x00);
        assert_eq!(i2c.pmic[PMIC_DWNSEQ0 as usize], 0x1b);
        assert_eq!(i2c.pmic[(PMIC_DWNSEQ0 + 1) as usize], 0x00);
        assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0x00);
    }
    run_test!(starts_without_activating_rails, starts_without_activating_rails_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn read_only_diagnostics_do_not_write_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        let snapshot = power.inspect_expander().await.unwrap();
        assert_eq!(snapshot.input_port0, 0);
        assert!(!snapshot.pgood_pin_high());
        assert_eq!(snapshot.output_port0, 0xff);
        assert_eq!(snapshot.direction_port0, 0xff);
        assert!(!snapshot.clock_enabled());
        assert_eq!(snapshot.output_port1, 0xff);
        assert_eq!(snapshot.direction_port1, 0xff);
        assert_eq!(power.state(), PowerState::Uninitialized);
        let (i2c, _) = power.release();
        assert!(!i2c.events.iter().any(|e| matches!(e, Event::Write(..))));
    }
    run_test!(read_only_diagnostics_do_not_write, read_only_diagnostics_do_not_write_body);
    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn pgood_pin_is_read_from_port0_input_not_output_latch_body() {
        let mut i2c = FakeI2c::new();
        i2c.pcal[PCAL_INPUT0 as usize] = PWR_GOOD_PIN;
        let mut power = Inkplate5v2Power::new(i2c, FakeDelay::default());
        let snapshot = power.inspect_expander().await.unwrap();
        assert!(snapshot.pgood_pin_high());
        assert_eq!(snapshot.output_port0, 0xff);
        let (i2c, _) = power.release();
        assert!(!i2c.events.iter().any(|event| matches!(event, Event::Write(..))));
    }
    run_test!(pgood_pin_is_read_from_port0_input_not_output_latch, pgood_pin_is_read_from_port0_input_not_output_latch_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn clock_gate_enable_preloads_output_and_preserves_other_bits_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        let snapshot = power.inspect_expander().await.unwrap();
        assert!(snapshot.clock_enabled());
        assert_eq!(snapshot.output_port1, 0xff);
        assert_eq!(snapshot.direction_port1, 0xfe);
        let (i2c, _) = power.release();
        let initial_output = i2c.events.iter().position(|event| matches!(
            event, Event::Write(EXPANDER_ADDR, PCAL_OUTPUT1, _)
        )).unwrap();
        let direction = i2c.events.iter().position(|event| matches!(
            event, Event::Write(EXPANDER_ADDR, PCAL_CONFIG1, _)
        )).unwrap();
        assert!(initial_output < direction);
    }
    run_test!(clock_gate_enable_preloads_output_and_preserves_other_bits, clock_gate_enable_preloads_output_and_preserves_other_bits_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn rails_cannot_start_without_clock_gate_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        assert_eq!(power.enable().await, Err(PowerError::ClockNotEnabled));
        assert_eq!(power.state(), PowerState::Idle);
        power.configure_clock_gate().await.unwrap();
        power.enable().await.unwrap();
        assert_eq!(power.state(), PowerState::On);
    }
    run_test!(rails_cannot_start_without_clock_gate, rails_cannot_start_without_clock_gate_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn enable_clock_preflight_i2c_fault_is_not_retriable_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        power.i2c.fail_expander_read_once = true;
        assert_eq!(power.enable().await, Err(PowerError::I2c(FakeError)));
        assert_eq!(power.state(), PowerState::Faulted);
        assert_eq!(power.enable().await, Err(PowerError::InvalidState(PowerState::Faulted)));
        let (i2c, _) = power.release();
        assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0);
        assert!(!i2c.events.iter().any(|e|
            matches!(e, Event::Write(EXPANDER_ADDR, PCAL_OUTPUT0, bytes)
                if bytes[0] & (OE | VCOM) != 0)
        ));
    }
    run_test!(
        enable_clock_preflight_i2c_fault_is_not_retriable,
        enable_clock_preflight_i2c_fault_is_not_retriable_body
    );



    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn rejects_rails_already_good_without_programming_pmic_body() {
        let mut i2c = FakeI2c::new();
        i2c.stuck_high_after_shutdown = true;
        // Isolate PMIC's internal PGOOD register from the separate PCAL
        // P0.7 input. The read-only preflight must pass first.
        i2c.pgood_pin_stuck_low = true;
        let mut power = Inkplate5v2Power::new(i2c, FakeDelay::default());
        assert_eq!(power.initialize().await, Err(PowerError::RailsAlreadyOn(PGOOD_ALL_RAILS)));
        assert_eq!(power.state(), PowerState::Faulted);
        let (i2c, _) = power.release();
        assert!(!i2c.events.iter().any(|e|
            matches!(e, Event::Write(PMIC_ADDR, PMIC_UPSEQ0, _))));
        assert!(i2c.events.iter().any(|e|
            matches!(e, Event::Read(PMIC_ADDR, PMIC_PWRGOOD, PGOOD_ALL_RAILS))));
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, 0);
    }
    run_test!(rejects_rails_already_good_without_programming_pmic, rejects_rails_already_good_without_programming_pmic_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn vcom_and_oe_wait_until_pgood_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        power.enable().await.unwrap();
        assert_eq!(power.state(), PowerState::On);
        let (mut i2c, delay) = power.release();

        let good_index = i2c.events.iter().position(|e|
            matches!(e, Event::Read(PMIC_ADDR, PMIC_PWRGOOD, PGOOD_ALL_RAILS))
        ).unwrap();
        let vcom_index = i2c.events.iter().position(|e|
            matches!(e, Event::Write(EXPANDER_ADDR, PCAL_OUTPUT0, bytes)
                if bytes[0] & VCOM != 0)
        ).unwrap();
        let oe_index = i2c.events.iter().position(|e|
            matches!(e, Event::Write(EXPANDER_ADDR, PCAL_OUTPUT0, bytes)
                if bytes[0] & OE != 0)
        ).unwrap();
        assert!(good_index < vcom_index && vcom_index < oe_index);
        assert!(delay.total_ns >= 7_000_000);
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, ALL_CONTROL);
        // Keep variables accessible for completeness.
        i2c.events.clear();
    }
    run_test!(vcom_and_oe_wait_until_pgood, vcom_and_oe_wait_until_pgood_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn live_rails_check_accepts_healthy_supply_and_rejects_a_dropped_gate_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        power.enable().await.unwrap();
        power.verify_live_rails().await.unwrap();
        // PGOOD remains ready, but a source output-enable pin unexpectedly
        // drops. PGOOD alone cannot demonstrate the panel is ready to scan.
        power.i2c.pcal[PCAL_OUTPUT0 as usize] &= !OE;
        assert_eq!(power.verify_live_rails().await, Err(PowerError::PanelControlLost));
        assert_eq!(power.state(), PowerState::Faulted);
    }
    run_test!(
        live_rails_check_accepts_healthy_supply_and_rejects_a_dropped_gate,
        live_rails_check_accepts_healthy_supply_and_rejects_a_dropped_gate_body
    );

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn live_rails_i2c_failure_latches_fault_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        power.enable().await.unwrap();
        power.i2c.fail_pgood_read_once = true;
        assert_eq!(power.verify_live_rails().await, Err(PowerError::I2c(FakeError)));
        assert_eq!(power.state(), PowerState::Faulted);
        assert_eq!(power.enable().await, Err(PowerError::InvalidState(PowerState::Faulted)));
    }
    run_test!(
        live_rails_i2c_failure_latches_fault,
        live_rails_i2c_failure_latches_fault_body
    );


    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn powers_down_to_verified_zero_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        power.enable().await.unwrap();
        power.disable().await.unwrap();
        assert_eq!(power.state(), PowerState::Idle);
        let (i2c, _) = power.release();
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, 0);
        assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0);
        let wakeup_low = i2c.events.iter().rposition(|e|
            matches!(e, Event::Write(EXPANDER_ADDR, PCAL_OUTPUT0, bytes)
                if bytes[0] & WAKEUP == 0)
        ).unwrap();
        let rails_disabled = i2c.events.iter().rposition(|e|
            matches!(e, Event::Write(PMIC_ADDR, PMIC_ENABLE, bytes) if bytes[0] == 0)
        ).unwrap();
        assert!(wakeup_low < rails_disabled);
        assert!(i2c.events.iter().any(|e|
            matches!(e, Event::Read(PMIC_ADDR, PMIC_PWRGOOD, 0))
        ));
    }
    run_test!(powers_down_to_verified_zero, powers_down_to_verified_zero_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn rejects_active_control_pins_on_entry_body() {
        let mut i2c = FakeI2c::new();
        i2c.pcal[PCAL_CONFIG0 as usize] &= !PWRUP;
        i2c.pcal[PCAL_OUTPUT0 as usize] |= PWRUP;
        let mut power = Inkplate5v2Power::new(i2c, FakeDelay::default());
        assert_eq!(power.initialize().await, Err(PowerError::ActiveControlPins));
        assert_eq!(power.state(), PowerState::Faulted);
        let (i2c, _) = power.release();
        assert!(!i2c.events.iter().any(|e| matches!(e, Event::Write(..))));
    }
    run_test!(rejects_active_control_pins_on_entry, rejects_active_control_pins_on_entry_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn preflight_rejects_physical_pgood_without_writes_body() {
        let mut i2c = FakeI2c::new();
        i2c.pcal[PCAL_INPUT0 as usize] = PWR_GOOD_PIN;
        let mut power = Inkplate5v2Power::new(i2c, FakeDelay::default());
        assert_eq!(power.preflight_low_voltage().await, Err(PowerError::PhysicalPowerGoodAlreadyHigh));
        assert_eq!(power.state(), PowerState::Uninitialized);
        assert_eq!(power.initialize().await, Err(PowerError::PhysicalPowerGoodAlreadyHigh));
        assert_eq!(power.state(), PowerState::Faulted);
        let (i2c, _) = power.release();
        assert!(!i2c.events.iter().any(|e| matches!(e, Event::Write(..))));
    }
    run_test!(preflight_rejects_physical_pgood_without_writes, preflight_rejects_physical_pgood_without_writes_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn preflight_rejects_pgood_configured_as_output_body() {
        let mut i2c = FakeI2c::new();
        i2c.pcal[PCAL_CONFIG0 as usize] &= !PWR_GOOD_PIN;
        let mut power = Inkplate5v2Power::new(i2c, FakeDelay::default());
        assert_eq!(power.preflight_low_voltage().await, Err(PowerError::PowerGoodPinNotInput));
        let (i2c, _) = power.release();
        assert!(!i2c.events.iter().any(|e| matches!(e, Event::Write(..))));
    }
    run_test!(preflight_rejects_pgood_configured_as_output, preflight_rejects_pgood_configured_as_output_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn preflight_rejects_interrupted_power_on_controls_body() {
        for bit in [GMOD, SPV, WAKEUP] {
            let mut i2c = FakeI2c::new();
            i2c.pcal[PCAL_CONFIG0 as usize] &= !bit;
            i2c.pcal[PCAL_OUTPUT0 as usize] |= bit;
            // Physical PGOOD can still be low during an interrupted ramp.
            assert_eq!(i2c.pcal[PCAL_INPUT0 as usize] & PWR_GOOD_PIN, 0);
            let mut power = Inkplate5v2Power::new(i2c, FakeDelay::default());
            assert_eq!(power.preflight_low_voltage().await, Err(PowerError::ActiveControlPins));
            let (i2c, _) = power.release();
            assert!(!i2c.events.iter().any(|e| matches!(e, Event::Write(..))));
        }
    }
    run_test!(
        preflight_rejects_interrupted_power_on_controls,
        preflight_rejects_interrupted_power_on_controls_body
    );

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn preflight_leaves_the_board_unchanged_when_idle_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        let snapshot = power.preflight_low_voltage().await.unwrap();
        assert!(!snapshot.pgood_pin_high());
        assert_eq!(power.state(), PowerState::Uninitialized);
        let (i2c, _) = power.release();
        assert!(!i2c.events.iter().any(|e| matches!(e, Event::Write(..))));
    }
    run_test!(preflight_leaves_the_board_unchanged_when_idle, preflight_leaves_the_board_unchanged_when_idle_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn timeout_never_enables_vcom_or_oe_body() {
        let mut i2c = FakeI2c::new();
        i2c.rails_ready = false;
        let mut power = Inkplate5v2Power::new(i2c, FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        assert_eq!(
            power.enable().await,
            Err(PowerError::PowerGoodTimeout(0))
        );
        assert_eq!(power.state(), PowerState::Faulted);
        assert_eq!(power.enable().await, Err(PowerError::InvalidState(PowerState::Faulted)));
        let (i2c, delay) = power.release();
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, 0);
        assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0);
        assert!(!i2c.events.iter().any(|e|
            matches!(e, Event::Write(EXPANDER_ADDR, PCAL_OUTPUT0, bytes)
                if bytes[0] & (OE | VCOM) != 0)
        ));
        assert!(delay.total_ns >= u64::from(STARTUP_TIMEOUT_MS) * 1_000_000);
    }
    run_test!(timeout_never_enables_vcom_or_oe, timeout_never_enables_vcom_or_oe_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn i2c_failure_attempts_shutdown_and_latches_fault_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        power.i2c.fail_sequence_write_once = true;
        assert_eq!(power.enable().await, Err(PowerError::I2c(FakeError)));
        assert_eq!(power.state(), PowerState::Faulted);
        let (i2c, _) = power.release();
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, 0);
        assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0);
    }
    run_test!(
        i2c_failure_attempts_shutdown_and_latches_fault,
        i2c_failure_attempts_shutdown_and_latches_fault_body
    );

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn every_power_up_write_failure_shuts_off_controls_and_latches_body() {
        // There are eight distinct register writes from the beginning of
        // enable_sequence() through the OE assertion. Fail each one in
        // isolation, then require the compensating shutdown to run.
        for offset in 1..=8 {
            let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
            power.initialize().await.unwrap();
            power.configure_clock_gate().await.unwrap();
            power.i2c.fail_on_write_attempt = Some(power.i2c.write_attempts + offset);
            assert_eq!(power.enable().await, Err(PowerError::I2c(FakeError)), "write {offset}");
            assert_eq!(power.state(), PowerState::Faulted, "write {offset}");
            let (i2c, _) = power.release();
            assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & (OE | VCOM | PWRUP), 0, "write {offset}");
            assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0, "write {offset}");
        }
    }
    run_test!(
        every_power_up_write_failure_shuts_off_controls_and_latches,
        every_power_up_write_failure_shuts_off_controls_and_latches_body
    );


    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn pgood_i2c_failure_blocks_panel_drive_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        power.i2c.fail_pgood_read_once = true;
        assert_eq!(power.enable().await, Err(PowerError::I2c(FakeError)));
        assert_eq!(power.state(), PowerState::Faulted);
        let (i2c, _) = power.release();
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & (OE | VCOM | PWRUP), 0);
        assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0);
    }
    run_test!(pgood_i2c_failure_blocks_panel_drive, pgood_i2c_failure_blocks_panel_drive_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn discharge_timeout_latches_fault_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        power.enable().await.unwrap();
        power.i2c.stuck_high_after_shutdown = true;
        assert_eq!(
            power.disable().await,
            Err(PowerError::DischargeTimeout(PGOOD_ALL_RAILS))
        );
        assert_eq!(power.state(), PowerState::Faulted);
        assert_eq!(power.enable().await, Err(PowerError::InvalidState(PowerState::Faulted)));
        let (i2c, _) = power.release();
        // Even with a broken power-good reading, we attempted all safe pin
        // and PMIC disable writes. An actual rail failure remains unsafe.
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, 0);
        assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0);
    }
    run_test!(discharge_timeout_latches_fault, discharge_timeout_latches_fault_body);
    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn physical_pgood_must_match_pmic_before_vcom_and_oe_body() {
        let mut i2c = FakeI2c::new();
        i2c.pgood_pin_stuck_low = true;
        let mut power = Inkplate5v2Power::new(i2c, FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        assert_eq!(power.enable().await, Err(PowerError::PhysicalPowerGoodLow));
        assert_eq!(power.state(), PowerState::Faulted);
        let (i2c, _) = power.release();
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & (OE | VCOM | PWRUP), 0);
        assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0);
        assert!(!i2c.events.iter().any(|e|
            matches!(e, Event::Write(EXPANDER_ADDR, PCAL_OUTPUT0, bytes)
                if bytes[0] & (OE | VCOM) != 0)
        ));
    }
    run_test!(
        physical_pgood_must_match_pmic_before_vcom_and_oe,
        physical_pgood_must_match_pmic_before_vcom_and_oe_body
    );


    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn physical_pgood_stuck_high_after_pmic_shutdown_latches_fault_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        power.enable().await.unwrap();
        // Simulate a broken PCAL P0.7 input while PMIC's register reads 0.
        power.i2c.pgood_pin_stuck_high = true;
        assert_eq!(power.disable().await, Err(PowerError::PhysicalPowerGoodStillHigh));
        assert_eq!(power.state(), PowerState::Faulted);
        let (i2c, delay) = power.release();
        assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0);
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, 0);
        assert!(delay.total_ns >= u64::from(SHUTDOWN_TIMEOUT_MS) * 1_000_000);
    }
    run_test!(
        physical_pgood_stuck_high_after_pmic_shutdown_latches_fault,
        physical_pgood_stuck_high_after_pmic_shutdown_latches_fault_body
    );

}
