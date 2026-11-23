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
const PCAL_OUTPUT0: u8 = 0x02;
const PCAL_CONFIG0: u8 = 0x06;
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
    /// Rails did not all discharge during the bounded shutdown wait.
    DischargeTimeout(u8),
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
        let direction = self.read(EXPANDER_ADDR, PCAL_CONFIG0).await?;
        let outputs = self.read(EXPANDER_ADDR, PCAL_OUTPUT0).await?;
        if ((!direction) & outputs & (OE | PWRUP | VCOM)) != 0 {
            return Err(PowerError::ActiveControlPins);
        }
        // PCAL6416A powers up with outputs HIGH. Program safe output latches
        // *before* setting direction to output, to avoid a high-going glitch.
        self.write(EXPANDER_ADDR, PCAL_OUTPUT0, &[outputs & !ALL_CONTROL]).await?;
        self.write(EXPANDER_ADDR, PCAL_CONFIG0, &[direction & !ALL_CONTROL]).await?;

        self.expander_levels(WAKEUP, WAKEUP).await?;
        self.delay.delay_ms(1).await;
        let configured = self.write(PMIC_ADDR, PMIC_UPSEQ0, &[0x1b, 0x00, 0x1b, 0x00]).await;
        let checked = if configured.is_ok() {
            let pg = self.read(PMIC_ADDR, PMIC_PWRGOOD).await;
            match pg {
                Ok(0) => Ok(()),
                Ok(value) => Err(PowerError::RailsAlreadyOn(value)),
                Err(err) => Err(err),
            }
        } else {
            configured
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
                self.expander_levels(VCOM, VCOM).await?;
                self.expander_levels(OE, OE).await?;
                return Ok(());
            }
            self.delay.delay_ms(1).await;
        }
        Err(PowerError::PowerGoodTimeout(last))
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
        if let Err(e) = self.expander_levels(VCOM, 0).await {
            if first.is_none() { first = Some(e); }
        }
        if let Err(e) = self.expander_levels(PWRUP, 0).await {
            if first.is_none() { first = Some(e); }
        }

        // The manufacturer's off sequence waits up to 250 ms for PGOOD=0.
        let mut pgood = 0xff;
        let mut discharged = false;
        for _ in 0..SHUTDOWN_TIMEOUT_MS {
            match self.read(PMIC_ADDR, PMIC_PWRGOOD).await {
                Ok(0) => { discharged = true; break; }
                Ok(value) => { pgood = value; self.delay.delay_ms(1).await; }
                Err(e) => {
                    if first.is_none() { first = Some(e); }
                    break;
                }
            }
        }
        if !discharged && first.is_none() {
            first = Some(PowerError::DischargeTimeout(pgood));
        }
        // Keep attempting cleanup even after one step fails.
        if let Err(e) = self.write(PMIC_ADDR, PMIC_ENABLE, &[0]).await {
            if first.is_none() { first = Some(e); }
        }
        if let Err(e) = self.expander_levels(WAKEUP, 0).await {
            if first.is_none() { first = Some(e); }
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
        fail_pgood_read_once: bool,
        stuck_high_after_shutdown: bool,
    }

    impl FakeI2c {
        fn new() -> Self {
            let mut pcal = [0u8; 8];
            pcal[PCAL_CONFIG0 as usize] = 0xff;
            pcal[PCAL_OUTPUT0 as usize] = 0xff;
            Self {
                pcal,
                pmic: [0u8; 16],
                events: Vec::new(),
                rails_ready: true,
                fail_sequence_write_once: false,
                fail_pgood_read_once: false,
                stuck_high_after_shutdown: false,
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
                        if addr == PMIC_ADDR && register == PMIC_PWRGOOD
                            && self.fail_pgood_read_once
                        {
                            self.fail_pgood_read_once = false;
                            return Err(FakeError);
                        }
                        for byte in out.iter_mut() {
                            let value = if addr == EXPANDER_ADDR {
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
    async fn vcom_and_oe_wait_until_pgood_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
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
    async fn powers_down_to_verified_zero_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.enable().await.unwrap();
        power.disable().await.unwrap();
        assert_eq!(power.state(), PowerState::Idle);
        let (i2c, _) = power.release();
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, 0);
        assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0);
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
    async fn timeout_never_enables_vcom_or_oe_body() {
        let mut i2c = FakeI2c::new();
        i2c.rails_ready = false;
        let mut power = Inkplate5v2Power::new(i2c, FakeDelay::default());
        power.initialize().await.unwrap();
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
    async fn pgood_i2c_failure_blocks_panel_drive_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
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
}
