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

/// Delay provider with a monotonic elapsed-time source. I2C operations and
/// scheduler delays must advance this clock, not just explicit delay calls.
/// Implementations must provide a nondecreasing microsecond counter for
/// the duration of a power operation.
pub trait PowerDelay: DelayNs {
    fn now_us(&self) -> u64;
}

#[cfg(feature = "esp32")]
impl PowerDelay for esp_hal::delay::Delay {
    fn now_us(&self) -> u64 {
        esp_hal::time::Instant::now().duration_since_epoch().as_micros()
    }
}

const EXPANDER_ADDR: u8 = 0x20;
const PMIC_ADDR: u8 = 0x48;
const PCAL_INPUT0: u8 = 0x00;
const PCAL_INPUT1: u8 = 0x01;
const PCAL_POLARITY0: u8 = 0x04;
const PCAL_POLARITY1: u8 = 0x05;
const PCAL_INPUT_LATCH0: u8 = 0x44;
const PCAL_INPUT_LATCH1: u8 = 0x45;
const PCAL_PULL_ENABLE0: u8 = 0x46;
const PCAL_OUTPUT0: u8 = 0x02;
const PCAL_OUTPUT1: u8 = 0x03;
const PCAL_CONFIG0: u8 = 0x06;
const PCAL_CONFIG1: u8 = 0x07;
const PMIC_ENABLE: u8 = 0x01;
const PMIC_VADJ: u8 = 0x02;
const PMIC_VCOM1: u8 = 0x03;
const PMIC_VCOM2: u8 = 0x04;
const PMIC_REVID: u8 = 0x10;
const PMIC_INT1: u8 = 0x07;
const PMIC_INT2: u8 = 0x08;
const INT1_POWER_FAULTS: u8 = 0x64; // TSD, HOT, UVLO
const INT2_POWER_FAULTS: u8 = 0xfe; // all except temperature ADC completion
const PMIC_UPSEQ0: u8 = 0x09;
const PMIC_DWNSEQ0: u8 = 0x0b;
const PMIC_UPSEQ1: u8 = 0x0a;
const PMIC_DWNSEQ1: u8 = 0x0c;
const V3P3_ENABLE: u8 = 0x20;
const HV_ENABLE_MASK: u8 = 0x1f;
const PMIC_PWRGOOD: u8 = 0x0f;
const PGOOD_ALL_RAILS: u8 = 0xfa;
// TI SLVSB04A Fig. 2: at least 1.8 ms from WAKEUP to I2C readiness.
const WAKE_DELAY_MS: u32 = 5;
const STARTUP_TIMEOUT_MS: u16 = 500;
const SHUTDOWN_TIMEOUT_MS: u16 = 250;

// TI §8.6.13: DDLY2–4 are 6/12/24/48 ms and DFCTR can multiply
// them by 16. Preserve the EEPROM settings but give the sequencer enough
// time to finish, including the final 50 ms negative-boost interval.
fn shutdown_poll_budget(down_delay: u8) -> u16 {
    let multiplier = if down_delay & 1 != 0 { 16 } else { 1 };
    let step = |shift: u8| 6u16 << ((down_delay >> shift) & 3);
    let initial = if down_delay & 2 != 0 { 6 } else { 3 };
    SHUTDOWN_TIMEOUT_MS.max(initial + multiplier * (step(2) + step(4) + step(6)) + 50 + 50)
}

const OE: u8 = 1 << 0;
const GMOD: u8 = 1 << 1;
const SPV: u8 = 1 << 2;
const WAKEUP: u8 = 1 << 3;
const PWRUP: u8 = 1 << 4;
const VCOM: u8 = 1 << 5;
const PANEL_CONTROL: u8 = OE | GMOD | SPV;
const PMIC_CONTROL: u8 = WAKEUP | PWRUP | VCOM;
const ALL_CONTROL: u8 = PANEL_CONTROL | PMIC_CONTROL;

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
    /// PMIC off indicators and control pins verified during initialization or shutdown.
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
    /// Actual Port 1 input levels, including the clock gate on bit 0.
    pub input_port1: u8,
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
        self.direction_port1 & GPIO0_ENABLE == 0
            && self.output_port1 & GPIO0_ENABLE != 0
            && self.input_port1 & GPIO0_ENABLE != 0
    }
}

/// Cached read-only PMIC register snapshot taken while awake with HV off.
/// No VCOM/voltage programming, EEPROM operation or live voltage measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PmicSnapshot {
    pub enable: u8,
    pub power_good: u8,
    pub revision: u8,
    pub vadj: u8,
    pub vcom1: u8,
    pub vcom2: u8,
    pub upseq0: u8,
    pub upseq1: u8,
    pub dwnseq0: u8,
    pub dwnseq1: u8,
}

impl PmicSnapshot {
    /// Configured negative bias (10 mV per code), not measured voltage.
    pub fn configured_vcom_mv(self) -> i16 {
        -10 * i16::from(self.vcom1) - 2560 * i16::from(self.vcom2 & 1)
    }
}

/// Raw interrupt flags captured before sleep resets them. Each register is
/// read-to-clear; None means no successful read, not absence of faults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PmicFaultSnapshot {
    pub int1: Option<u8>,
    pub int2: Option<u8>,
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
    /// PMIC power-good/enable indicators did not clear during shutdown.
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
    /// PCAL readback did not confirm all panel control outputs inactive.
    LowVoltageReadbackMismatch,
    /// Input polarity or latch configuration would make safety readings unreliable.
    ExpanderInputConfig,
    /// The programmed PMIC sequencer did not read back correctly.
    SequencerReadbackMismatch,
    /// PMIC enable status does not match the expected state.
    EnableReadbackMismatch(u8),
    /// PMIC reported a supply or thermal fault while active.
    PmicFault { int1: u8, int2: u8 },
    /// VCOM2 has acquisition, EEPROM programming or Hi-Z mode asserted.
    InvalidVcomMode(u8),
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
    idle_snapshot: Option<PmicSnapshot>,
    fault_snapshot: PmicFaultSnapshot,
    shutdown_budget: u16,
}

impl<I2C, DELAY> Inkplate5v2Power<I2C, DELAY> {
    /// Construct without touching a board. The rails must be considered
    /// untrusted until `initialize` checks them.
    pub fn new(i2c: I2C, delay: DELAY) -> Self {
        Self {
            i2c,
            delay,
            state: PowerState::Uninitialized,
            idle_snapshot: None,
            fault_snapshot: PmicFaultSnapshot::default(),
            shutdown_budget: SHUTDOWN_TIMEOUT_MS,
        }
    }

    /// Current latched state. `Faulted` must not be treated as powered off.
    pub fn state(&self) -> PowerState {
        self.state
    }

    /// Most recent standby register snapshot, captured without requesting HV.
    /// A cached value, not a live reading from the sleeping PMIC.
    pub fn idle_pmic_snapshot(&self) -> Option<PmicSnapshot> {
        self.idle_snapshot
    }

    /// Accumulated interrupt flags observed since construction.
    pub fn pmic_fault_snapshot(&self) -> PmicFaultSnapshot {
        self.fault_snapshot
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
impl<I2C: I2c, DELAY: PowerDelay> Inkplate5v2Power<I2C, DELAY> {
    fn before_deadline(&self, started: u64, budget_ms: u16) -> bool {
        self.delay.now_us().saturating_sub(started) < u64::from(budget_ms) * 1000
    }

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
        // NXP PCAL6416A §§7.4.3, 7.4.6: input inversion and input
        // latching can turn a pin read into an inverted or stale observation.
        // Refuse safety decisions based on such a configuration. Do not
        // silently change other expander users' settings.
        let mask0 = ALL_CONTROL | PWR_GOOD_PIN;
        if self.read(EXPANDER_ADDR, PCAL_POLARITY0).await? & mask0 != 0
            || self.read(EXPANDER_ADDR, PCAL_INPUT_LATCH0).await? & mask0 != 0
            || self.read(EXPANDER_ADDR, PCAL_POLARITY1).await? & GPIO0_ENABLE != 0
            || self.read(EXPANDER_ADDR, PCAL_INPUT_LATCH1).await? & GPIO0_ENABLE != 0
        {
            return Err(PowerError::ExpanderInputConfig);
        }
        Ok(ExpanderSnapshot {
            input_port0: self.read(EXPANDER_ADDR, PCAL_INPUT0).await?,
            input_port1: self.read(EXPANDER_ADDR, PCAL_INPUT1).await?,
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
        if (snapshot.input_port0 & ALL_CONTROL) != 0
            || ((!snapshot.direction_port0) & snapshot.output_port0 & ALL_CONTROL) != 0
        {
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

    /// Reconfigure previously released panel signals with low output latches.
    /// PMIC control pins must still be driven low and PG must be low.
    pub(crate) async fn rearm_panel_controls(&mut self) -> Result<(), PowerError<I2C::Error>> {
        if self.state != PowerState::Idle {
            return Err(PowerError::InvalidState(self.state));
        }
        self.state = PowerState::Faulted;
        let snapshot = self.inspect_expander().await?;
        Self::verify_released_snapshot(snapshot)?;
        self.expander_levels(PANEL_CONTROL, 0).await?;
        self.write(EXPANDER_ADDR, PCAL_CONFIG0, &[snapshot.direction_port0 & !PANEL_CONTROL]).await?;
        let check = self.inspect_expander().await?;
        Self::verify_initial_snapshot(check)?;
        if check.direction_port0 & ALL_CONTROL != 0 || check.output_port0 & ALL_CONTROL != 0 {
            return Err(PowerError::LowVoltageReadbackMismatch);
        }
        self.inspect_standby_and_sleep().await?;
        self.state = PowerState::Idle;
        Ok(())
    }

    fn verify_released_snapshot(snapshot: ExpanderSnapshot) -> Result<(), PowerError<I2C::Error>> {
        // Floating panel inputs may read either level. Never interpret
        // those inputs as driven controls. PMIC controls must remain low.
        if snapshot.pgood_pin_high()
            || snapshot.direction_port0 & (PWR_GOOD_PIN | ALL_CONTROL) != PWR_GOOD_PIN | PANEL_CONTROL
            || snapshot.input_port0 & PMIC_CONTROL != 0
            || snapshot.output_port0 & ALL_CONTROL != 0
            || snapshot.direction_port1 & GPIO0_ENABLE != 0
            || (snapshot.input_port1 | snapshot.output_port1) & GPIO0_ENABLE != 0
        {
            return Err(PowerError::LowVoltageReadbackMismatch);
        }
        Ok(())
    }

    /// Release OE/GMOD/SPV to inputs without pulls after a verified shutdown.
    /// Keep WAKEUP/PWRUP/VCOM low and disconnect the GPIO0 clock gate.
    /// Never release the expander panel outputs while PMIC shutdown is uncertain.
    pub(crate) async fn release_panel_controls(&mut self) -> Result<(), PowerError<I2C::Error>> {
        if self.state != PowerState::Idle {
            return Err(PowerError::InvalidState(self.state));
        }
        self.state = PowerState::Faulted;
        let snapshot = self.inspect_expander().await?;
        Self::verify_initial_snapshot(snapshot)?;
        if snapshot.direction_port0 & ALL_CONTROL != 0 || snapshot.output_port0 & ALL_CONTROL != 0 {
            return Err(PowerError::LowVoltageReadbackMismatch);
        }
        // Gate off before releasing panel signals. Leave unrelated Port 1
        // latches and directions untouched.
        self.write(EXPANDER_ADDR, PCAL_OUTPUT1, &[snapshot.output_port1 & !GPIO0_ENABLE]).await?;
        self.write(EXPANDER_ADDR, PCAL_CONFIG1, &[snapshot.direction_port1 & !GPIO0_ENABLE]).await?;
        let pulls = self.read(EXPANDER_ADDR, PCAL_PULL_ENABLE0).await?;
        self.write(EXPANDER_ADDR, PCAL_PULL_ENABLE0, &[pulls & !PANEL_CONTROL]).await?;
        self.write(EXPANDER_ADDR, PCAL_CONFIG0, &[snapshot.direction_port0 | PANEL_CONTROL]).await?;
        let check = self.inspect_expander().await?;
        Self::verify_released_snapshot(check)?;
        if self.read(EXPANDER_ADDR, PCAL_PULL_ENABLE0).await? & PANEL_CONTROL != 0 {
            return Err(PowerError::LowVoltageReadbackMismatch);
        }
        self.state = PowerState::Idle;
        Ok(())
    }

    /// Verify the off pin configuration without changing released outputs.
    pub(crate) async fn verify_released_controls(&mut self) -> Result<(), PowerError<I2C::Error>> {
        if self.state != PowerState::Idle {
            return Err(PowerError::InvalidState(self.state));
        }
        self.state = PowerState::Faulted;
        Self::verify_released_snapshot(self.inspect_expander().await?)?;
        if self.read(EXPANDER_ADDR, PCAL_PULL_ENABLE0).await? & PANEL_CONTROL != 0 {
            return Err(PowerError::LowVoltageReadbackMismatch);
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

    /// Shutdown-only recovery of a previously configured expander after
    /// MCU reset. Call before any scan GPIO/I2S configuration, and only
    /// with exclusive board ownership. Does not enable or rearm anything.
    ///
    /// Refuses unknown/mixed pin directions, unreliable input configuration
    /// or a sleeping PMIC: software cannot certify discharge in those cases.
    /// Even success leaves this controller Faulted; a separate recovery
    /// decision is required before any subsequent use.
    pub async fn recover_retained_power(&mut self) -> Result<(), PowerError<I2C::Error>> {
        if self.state != PowerState::Uninitialized {
            return Err(PowerError::InvalidState(self.state));
        }
        self.state = PowerState::Faulted;
        // Use the worst possible factory delay: no trustworthy snapshot
        // survives an MCU reset. Never assert WAKEUP during recovery.
        self.shutdown_budget = shutdown_poll_budget(0xff);
        let snapshot = self.inspect_expander().await?;
        if snapshot.direction_port0 & (ALL_CONTROL | PWR_GOOD_PIN) != PWR_GOOD_PIN {
            return Err(PowerError::LowVoltageReadbackMismatch);
        }
        let result = self.shutdown_sequence().await;
        // Gate off after attempted shutdown even on PMIC error. Do not
        // release expander panel controls or permit any restart.
        let clock_off = self.disable_clock_gate().await;
        result.and(clock_off)
    }

    async fn disable_clock_gate(&mut self) -> Result<(), PowerError<I2C::Error>> {
        let output = self.read(EXPANDER_ADDR, PCAL_OUTPUT1).await?;
        let direction = self.read(EXPANDER_ADDR, PCAL_CONFIG1).await?;
        self.write(EXPANDER_ADDR, PCAL_OUTPUT1, &[output & !GPIO0_ENABLE]).await?;
        self.write(EXPANDER_ADDR, PCAL_CONFIG1, &[direction & !GPIO0_ENABLE]).await?;
        let check = self.inspect_expander().await?;
        if (check.input_port1 | check.output_port1 | check.direction_port1) & GPIO0_ENABLE != 0 {
            return Err(PowerError::ClockReadbackMismatch);
        }
        Ok(())
    }

    /// Inspect the initial state, disable the six control outputs, and
    /// read the awake PMIC configuration. This never requests high-voltage rails.
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

        // An acknowledged direction/latch write is not proof of the actual
        // pins. In particular WAKEUP rising with PWRUP high starts HV.
        let idle = self.inspect_expander().await?;
        Self::verify_initial_snapshot(idle)?;
        if idle.direction_port0 & ALL_CONTROL != 0 {
            return Err(PowerError::LowVoltageReadbackMismatch);
        }
        self.inspect_standby_and_sleep().await?;
        self.state = PowerState::Idle;
        Ok(())
    }

    // Both initial preparation and rearm take a fresh standby snapshot.
    // Never expose an earlier successful snapshot after a failed wake.
    async fn inspect_standby_and_sleep(&mut self) -> Result<(), PowerError<I2C::Error>> {
        self.idle_snapshot = None;
        let checked = self.inspect_awake_idle().await;
        // Even if WAKEUP assertion failed after reaching the expander,
        // attempt to deassert it. Never access PMIC registers after sleep.
        let asleep = self.expander_levels(WAKEUP, 0).await;
        if let Err(error) = checked.and(asleep) {
            self.idle_snapshot = None;
            return Err(error);
        }
        // A successful I2C ACK is not proof the configuration latched.
        // Before declaring the board idle, check that power-control
        // outputs really are low, configured as outputs, and PGOOD is low.
        let idle = match self.inspect_expander().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.idle_snapshot = None;
                return Err(error);
            }
        };
        if idle.input_port0 & ALL_CONTROL != 0
            || idle.output_port0 & ALL_CONTROL != 0
            || idle.direction_port0 & ALL_CONTROL != 0
            || idle.direction_port0 & PWR_GOOD_PIN == 0
            || idle.pgood_pin_high()
        {
            self.idle_snapshot = None;
            return Err(PowerError::LowVoltageReadbackMismatch);
        }
        Ok(())
    }

    async fn inspect_awake_idle(&mut self) -> Result<(), PowerError<I2C::Error>> {
        self.expander_levels(WAKEUP, WAKEUP).await?;
        self.delay.delay_ms(WAKE_DELAY_MS).await;
        let pg = self.read(PMIC_ADDR, PMIC_PWRGOOD).await?;
        if pg != 0 {
            return Err(PowerError::RailsAlreadyOn(pg));
        }
        let enable = self.read(PMIC_ADDR, PMIC_ENABLE).await?;
        if enable != 0 {
            return Err(PowerError::EnableReadbackMismatch(enable));
        }
        let snapshot = PmicSnapshot {
            enable,
            power_good: pg,
            revision: self.read(PMIC_ADDR, PMIC_REVID).await?,
            vadj: self.read(PMIC_ADDR, PMIC_VADJ).await?,
            vcom1: self.read(PMIC_ADDR, PMIC_VCOM1).await?,
            vcom2: self.read(PMIC_ADDR, PMIC_VCOM2).await?,
            upseq0: self.read(PMIC_ADDR, PMIC_UPSEQ0).await?,
            upseq1: self.read(PMIC_ADDR, PMIC_UPSEQ1).await?,
            dwnseq0: self.read(PMIC_ADDR, PMIC_DWNSEQ0).await?,
            dwnseq1: self.read(PMIC_ADDR, PMIC_DWNSEQ1).await?,
        };
        self.idle_snapshot = Some(snapshot);
        self.shutdown_budget = shutdown_poll_budget(snapshot.dwnseq1);
        // SLEEP reloads factory defaults. Do not write volatile sequencing
        // registers here only to lose them again when returning to sleep.
        Ok(())
    }

    // INT1/2 clear on read (TI §8.3.8). Preserve each successfully
    // read byte even if the other register transaction fails. Never use
    // this operation to retry a faulted power-up.
    async fn read_interrupts(&mut self) -> Result<(u8, u8), PowerError<I2C::Error>> {
        let int1 = self.read(PMIC_ADDR, PMIC_INT1).await;
        if let Ok(value) = &int1 {
            self.fault_snapshot.int1 = Some(self.fault_snapshot.int1.unwrap_or(0) | *value);
        }
        let int2 = self.read(PMIC_ADDR, PMIC_INT2).await;
        if let Ok(value) = &int2 {
            self.fault_snapshot.int2 = Some(self.fault_snapshot.int2.unwrap_or(0) | *value);
        }
        Ok((int1?, int2?))
    }

    async fn verify_no_pmic_fault(&mut self) -> Result<(), PowerError<I2C::Error>> {
        let (int1, int2) = self.read_interrupts().await?;
        if int1 & INT1_POWER_FAULTS != 0 || int2 & INT2_POWER_FAULTS != 0 {
            return Err(PowerError::PmicFault { int1, int2 });
        }
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
        if let Err(error) = Self::verify_initial_snapshot(clock) {
            self.state = PowerState::Faulted;
            return Err(error);
        }
        if clock.direction_port0 & ALL_CONTROL != 0 {
            self.state = PowerState::Faulted;
            return Err(PowerError::LowVoltageReadbackMismatch);
        }
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
        self.delay.delay_ms(WAKE_DELAY_MS).await;
        let pg = self.read(PMIC_ADDR, PMIC_PWRGOOD).await?;
        if pg != 0 {
            return Err(PowerError::RailsAlreadyOn(pg));
        }
        let enable = self.read(PMIC_ADDR, PMIC_ENABLE).await?;
        if enable != 0 {
            return Err(PowerError::EnableReadbackMismatch(enable));
        }
        // Preserve factory-loaded delay values. The vendor's begin() writes
        // are lost in sleep; its effective active-cycle delays are the
        // EEPROM defaults. Configure the active rail order on every wake.
        let vcom_mode = self.read(PMIC_ADDR, PMIC_VCOM2).await?;
        if vcom_mode & 0xe0 != 0 {
            return Err(PowerError::InvalidVcomMode(vcom_mode));
        }
        let up_delay = self.read(PMIC_ADDR, PMIC_UPSEQ1).await?;
        let down_delay = self.read(PMIC_ADDR, PMIC_DWNSEQ1).await?;
        self.shutdown_budget = shutdown_poll_budget(down_delay);
        self.write(PMIC_ADDR, PMIC_UPSEQ0, &[0xe4, up_delay]).await?;
        self.write(PMIC_ADDR, PMIC_DWNSEQ0, &[0x1b, down_delay]).await?;
        for (reg, expected) in [
            (PMIC_UPSEQ0, 0xe4), (PMIC_UPSEQ1, up_delay),
            (PMIC_DWNSEQ0, 0x1b), (PMIC_DWNSEQ1, down_delay),
        ] {
            if self.read(PMIC_ADDR, reg).await? != expected {
                return Err(PowerError::SequencerReadbackMismatch);
            }
        }
        self.write(PMIC_ADDR, PMIC_ENABLE, &[V3P3_ENABLE]).await?;
        let enable = self.read(PMIC_ADDR, PMIC_ENABLE).await?;
        if enable != V3P3_ENABLE {
            return Err(PowerError::EnableReadbackMismatch(enable));
        }
        self.expander_levels(PWRUP, PWRUP).await?;

        let started = self.delay.now_us();
        let mut last = 0;
        while self.before_deadline(started, STARTUP_TIMEOUT_MS) {
            last = self.read(PMIC_ADDR, PMIC_PWRGOOD).await?;
            if !self.before_deadline(started, STARTUP_TIMEOUT_MS) {
                return Err(PowerError::PowerGoodTimeout(last));
            }
            if last == PGOOD_ALL_RAILS {
                // Both independent indicators must agree; allow the PCAL
                // input to settle until the bounded startup deadline rather
                // than faulting on a transient disagreement.
                let snapshot = self.inspect_expander().await?;
                if !self.before_deadline(started, STARTUP_TIMEOUT_MS) {
                    return Err(PowerError::PowerGoodTimeout(last));
                }
                if snapshot.pgood_pin_high() {
                    let expected = GMOD | SPV | WAKEUP | PWRUP;
                    if snapshot.direction_port0 & ALL_CONTROL != 0
                        || snapshot.direction_port0 & PWR_GOOD_PIN == 0
                        || snapshot.input_port0 & ALL_CONTROL != expected
                        || snapshot.output_port0 & ALL_CONTROL != expected
                        || !snapshot.clock_enabled()
                    {
                        return Err(PowerError::PanelControlLost);
                    }
                    self.expander_levels(VCOM, VCOM).await?;
                    // Allow VCOM_CTRL deglitch (TI pin specification).
                    self.delay.delay_ms(1).await;
                    let enable = self.read(PMIC_ADDR, PMIC_ENABLE).await?;
                    if enable & (V3P3_ENABLE | HV_ENABLE_MASK)
                        != V3P3_ENABLE | HV_ENABLE_MASK
                    {
                        return Err(PowerError::EnableReadbackMismatch(enable));
                    }
                    let bias = self.inspect_expander().await?;
                    if bias.input_port0 & ALL_CONTROL != expected | VCOM
                        || bias.output_port0 & ALL_CONTROL != expected | VCOM
                        || bias.direction_port0 & ALL_CONTROL != 0
                        || !bias.pgood_pin_high()
                        || self.read(PMIC_ADDR, PMIC_PWRGOOD).await? != PGOOD_ALL_RAILS
                    {
                        return Err(PowerError::PanelControlLost);
                    }
                    self.verify_no_pmic_fault().await?;
                    self.expander_levels(OE, OE).await?;
                    self.verify_live_rails_inner().await?;
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
        let enable = self.read(PMIC_ADDR, PMIC_ENABLE).await?;
        if enable & (V3P3_ENABLE | HV_ENABLE_MASK) != V3P3_ENABLE | HV_ENABLE_MASK {
            return Err(PowerError::EnableReadbackMismatch(enable));
        }
        self.verify_no_pmic_fault().await?;
        let active = ALL_CONTROL;
        if snapshot.direction_port0 & active != 0
            || snapshot.direction_port0 & PWR_GOOD_PIN == 0
            || snapshot.output_port0 & active != active
            || snapshot.input_port0 & active != active
            || !snapshot.clock_enabled()
        {
            return Err(PowerError::PanelControlLost);
        }
        Ok(())
    }

    /// Suppress panel output before changing direct scan GPIOs.
    /// Board shutdown calls this first, and disable() repeats the request.
    /// An error must not prevent the remaining shutdown steps.
    pub(crate) async fn quiesce_panel(&mut self) -> Result<(), PowerError<I2C::Error>> {
        if matches!(self.state, PowerState::Idle | PowerState::Uninitialized) {
            return Ok(());
        }
        let result = self.expander_levels(OE | GMOD, 0).await;
        if result.is_err() {
            self.state = PowerState::Faulted;
        }
        result
    }

    /// Disable VCOM and panel output, request rails off and wait until PGOOD
    /// reports zero. An I2C error or a timeout latches `Faulted`.
    ///
    /// Caller must separately quiesce ESP32 direct control lines before
    /// calling this method. The physical shutdown cannot be guaranteed if
    /// the I2C bus or the expander itself is stuck.
    pub(crate) async fn disable(&mut self) -> Result<(), PowerError<I2C::Error>> {
        if self.state == PowerState::Idle {
            // A cached Idle must not hide an externally altered pin state.
            self.state = PowerState::Faulted;
            let snapshot = self.inspect_expander().await?;
            Self::verify_initial_snapshot(snapshot)?;
            if snapshot.direction_port0 & ALL_CONTROL != 0
                || snapshot.output_port0 & ALL_CONTROL != 0
            {
                return Err(PowerError::LowVoltageReadbackMismatch);
            }
            self.state = PowerState::Idle;
            return Ok(());
        }
        if self.state == PowerState::Uninitialized {
            return Err(PowerError::InvalidState(self.state));
        }
        let previously_faulted = self.state == PowerState::Faulted;
        // Cancellation during shutdown must not leave a usable On state.
        self.state = PowerState::Faulted;
        let result = self.shutdown_sequence().await;
        if !previously_faulted && result.is_ok() {
            self.state = PowerState::Idle;
        } else {
            self.state = PowerState::Faulted;
        }
        result
    }

    async fn disable_logic_supply(&mut self) -> Result<(), PowerError<I2C::Error>> {
        let enable = self.read(PMIC_ADDR, PMIC_ENABLE).await?;
        if enable & !V3P3_ENABLE != 0 {
            return Err(PowerError::EnableReadbackMismatch(enable));
        }
        self.write(PMIC_ADDR, PMIC_ENABLE, &[0]).await?;
        let enable = self.read(PMIC_ADDR, PMIC_ENABLE).await?;
        if enable != 0 {
            return Err(PowerError::EnableReadbackMismatch(enable));
        }
        Ok(())
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
        // VCOM_CTRL has 62.52 us of deglitch; allow the disable to
        // settle before PWRUP falling initiates rail power-down.
        self.delay.delay_ms(1).await;
        if let Err(e) = self.expander_levels(PWRUP, 0).await
            && first.is_none()
        {
            first = Some(e);
        }

        // Wait for PG and enable bits to clear, allowing for factory delays.
        let started = self.delay.now_us();
        let mut pgood = 0xff;
        let mut discharged = false;
        let mut physical_high = false;
        // A failed startup may never have woken the PMIC. Do not wake it
        // just to shut down: WAKEUP rising with PWRUP high starts the rails.
        let awake = match self.inspect_expander().await {
            Ok(snapshot) => snapshot.input_port0 & WAKEUP != 0,
            Err(e) => {
                if first.is_none() { first = Some(e); }
                false
            }
        };
        if awake {
            // A failed WAKEUP transaction may have asserted the pin just now.
            self.delay.delay_ms(WAKE_DELAY_MS).await;
            while self.before_deadline(started, self.shutdown_budget) {
                match self.read(PMIC_ADDR, PMIC_PWRGOOD).await {
                    Ok(0) => {
                        pgood = 0;
                        match self.inspect_expander().await {
                            Ok(snapshot) if !snapshot.pgood_pin_high() => {
                                // PG may drop before individual enables clear.
                                // Do not bypass the PMIC's sequencer by writing
                                // ENABLE until all HV/VCOM enables are off.
                                match self.read(PMIC_ADDR, PMIC_ENABLE).await {
                                    Ok(enable) if enable & !V3P3_ENABLE == 0
                                        && self.before_deadline(started, self.shutdown_budget) => {
                                        discharged = true;
                                        break;
                                    }
                                    Ok(_) => self.delay.delay_ms(1).await,
                                    Err(e) => {
                                        if first.is_none() { first = Some(e); }
                                        break;
                                    }
                                }
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
            // Preserve fault evidence before WAKEUP low resets it. Capture
            // is best effort and must not prevent the remaining shutdown.
            let _ = self.read_interrupts().await;
            // ENABLE bits reflect actual rail state. Never clear individual
            // HV bits while a sequenced shutdown may still be in progress.
            if discharged {
                let disabled = self.disable_logic_supply().await;
                if let Err(e) = disabled
                    && first.is_none()
                {
                    first = Some(e);
                }
            }
        } else if first.is_none() {
            // PMIC already asleep: internal status is unavailable. Never
            // report this as a verified healthy shutdown.
            first = Some(PowerError::PanelControlLost);
        }
        // WAKEUP low requests sequenced power-down even on PG/I2C failure.
        if let Err(e) = self.expander_levels(WAKEUP, 0).await
            && first.is_none()
        {
            first = Some(e);
        }
        // PG=0 is not a measurement of zero volts. Allow the final discharge
        // interval shown in TI Fig. 2 before a healthy cycle may restart.
        self.delay.delay_ms(50).await;
        match self.inspect_expander().await {
            Ok(snapshot) => {
                if (snapshot.input_port0 & ALL_CONTROL != 0
                    || snapshot.output_port0 & ALL_CONTROL != 0
                    || snapshot.direction_port0 & (ALL_CONTROL | PWR_GOOD_PIN) != PWR_GOOD_PIN
                    || snapshot.pgood_pin_high())
                    && first.is_none()
                {
                    first = Some(PowerError::PanelControlLost);
                }
            }
            Err(e) => if first.is_none() { first = Some(e); },
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
    use std::rc::Rc;
    use core::cell::Cell;

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
        pcal: [u8; 0x50],
        pmic: [u8; 17],
        events: Vec<Event>,
        rails_ready: bool,
        fail_sequence_write_once: bool,
        fail_on_write_attempt: Option<usize>,
        fail_after_write_attempt: Option<usize>,
        clock: Option<Rc<Cell<u64>>>,
        transaction_ns: u64,
        wake_started_ns: u64,
        rail_started_ns: u64,
        rail_stopped_ns: Option<u64>,
        factory_down_delay: u8,
        write_attempts: usize,
        fail_pgood_read_once: bool,
        fail_expander_read_once: bool,
        ignore_config0_write_once: bool,
        stuck_high_after_shutdown: bool,
        pgood_pin_stuck_low: bool,
        pgood_pin_stuck_high: bool,
    }

    impl FakeI2c {
        fn new() -> Self {
            let mut pcal = [0u8; 0x50];
            pcal[PCAL_CONFIG0 as usize] = 0xff;
            pcal[PCAL_OUTPUT0 as usize] = 0xff;
            pcal[PCAL_CONFIG1 as usize] = 0xff;
            pcal[PCAL_OUTPUT1 as usize] = 0xff;
            Self {
                pcal,
                pmic: Self::sleep_defaults(),
                events: Vec::new(),
                rails_ready: true,
                fail_sequence_write_once: false,
                fail_on_write_attempt: None,
                fail_after_write_attempt: None,
                clock: None,
                transaction_ns: 0,
                wake_started_ns: 0,
                rail_started_ns: 0,
                rail_stopped_ns: None,
                factory_down_delay: 0x20,
                write_attempts: 0,
                fail_pgood_read_once: false,
                fail_expander_read_once: false,
                ignore_config0_write_once: false,
                stuck_high_after_shutdown: false,
                pgood_pin_stuck_low: false,
                pgood_pin_stuck_high: false,
            }
        }

        fn sleep_defaults() -> [u8; 17] {
            let mut registers = [0; 17];
            registers[PMIC_VCOM1 as usize] = 0x2c;
            registers[PMIC_VCOM2 as usize] = 1;
            registers[PMIC_REVID as usize] = 0x65;
            registers[PMIC_UPSEQ1 as usize] = 0x55;
            registers[PMIC_DWNSEQ1 as usize] = 0x20;
            registers
        }

        fn physical_controls(&self) -> u8 {
            // Model pull-downs on input pins and physical output levels.
            self.pcal[PCAL_OUTPUT0 as usize] & !self.pcal[PCAL_CONFIG0 as usize] & ALL_CONTROL
        }

        fn now_ns(&self) -> u64 {
            self.clock.as_ref().map_or(0, |clock| clock.get())
        }

        // In the timed fixture, PG may be gone well before the remaining
        // enables clear. This tests shutdown ordering, not analogue voltage.
        fn hv_enables(&self) -> u8 {
            if self.physical_controls() & PWRUP != 0
                || (self.clock.is_some()
                    && self.rail_stopped_ns.is_some_and(|time| self.now_ns() - time < 300_000_000))
            {
                0x0f
            } else {
                0
            }
        }

        fn pgood(&self) -> u8 {
            if self.stuck_high_after_shutdown {
                return PGOOD_ALL_RAILS;
            }
            if self.rails_ready
                && self.physical_controls() & (PWRUP | WAKEUP) == PWRUP | WAKEUP
                && self.pmic[PMIC_ENABLE as usize] == 0x20
                && (self.clock.is_none() || self.now_ns() - self.rail_started_ns >= 28_000_000)
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
            if let Some(clock) = &self.clock {
                clock.set(clock.get() + self.transaction_ns);
            }
            if addr == PMIC_ADDR
                && (self.physical_controls() & WAKEUP == 0
                    || (self.clock.is_some() && self.now_ns() - self.wake_started_ns < 1_800_000))
            {
                return Err(FakeError);
            }
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
                                        if register == PCAL_CONFIG0 && self.ignore_config0_write_once {
                                            self.ignore_config0_write_once = false;
                                            continue; // Simulate an ACKed write the chip did not latch.
                                        }
                                        let before = self.physical_controls();
                                        self.pcal[index] = *value;
                                        let after = self.physical_controls();
                                        if before & WAKEUP == 0 && after & WAKEUP != 0 {
                                            self.wake_started_ns = self.now_ns();
                                        }
                                        if before & PWRUP == 0 && after & PWRUP != 0 {
                                            self.rail_started_ns = self.now_ns();
                                            self.rail_stopped_ns = None;
                                        }
                                        if before & PWRUP != 0 && after & PWRUP == 0 {
                                            self.rail_stopped_ns = Some(self.now_ns());
                                        }
                                        if before & WAKEUP != 0 && after & WAKEUP == 0 {
                                            self.pmic = Self::sleep_defaults();
                                            self.pmic[PMIC_DWNSEQ1 as usize] = self.factory_down_delay;
                                            self.rail_stopped_ns = None;
                                        }
                                    } else if addr == PMIC_ADDR {
                                        self.pmic[index] = *value;
                                    } else {
                                        return Err(FakeError);
                                    }
                                }
                                if self.fail_after_write_attempt == Some(self.write_attempts) {
                                    self.fail_after_write_attempt = None;
                                    return Err(FakeError);
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
                                let base = (self.pcal[PCAL_INPUT0 as usize] & !PWR_GOOD_PIN)
                                    | self.physical_controls();
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
                            } else if addr == EXPANDER_ADDR && register == PCAL_INPUT1 {
                                self.pcal[PCAL_INPUT1 as usize]
                                    | (self.pcal[PCAL_OUTPUT1 as usize] & !self.pcal[PCAL_CONFIG1 as usize])
                            } else if addr == EXPANDER_ADDR {
                                self.pcal[register as usize]
                            } else if addr == PMIC_ADDR && register == PMIC_ENABLE {
                                let controls = self.physical_controls();
                                self.pmic[PMIC_ENABLE as usize]
                                    | self.hv_enables()
                                    | if controls & VCOM != 0 { 0x10 } else { 0 }
                            } else if addr == PMIC_ADDR && register == PMIC_PWRGOOD {
                                self.pgood()
                            } else if addr == PMIC_ADDR {
                                let value = self.pmic[register as usize];
                                if matches!(register, PMIC_INT1 | PMIC_INT2) {
                                    self.pmic[register as usize] = 0;
                                }
                                value
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
        clock: Option<Rc<Cell<u64>>>,
        #[cfg(not(feature = "blocking"))]
        suspend_once: bool,
    }

    impl PowerDelay for FakeDelay {
        fn now_us(&self) -> u64 {
            self.clock.as_ref().map_or(self.total_ns, |clock| clock.get()) / 1000
        }
    }

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    impl DelayNs for FakeDelay {
        async fn delay_ns(&mut self, ns: u32) {
            #[cfg(not(feature = "blocking"))]
            if self.suspend_once {
                self.suspend_once = false;
                core::future::pending::<()>().await;
            }
            self.total_ns += u64::from(ns);
            if let Some(clock) = &self.clock {
                clock.set(clock.get() + u64::from(ns));
            }
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
        let snapshot = power.idle_pmic_snapshot().unwrap();
        assert_eq!(snapshot.configured_vcom_mv(), -3000);
        assert_eq!(snapshot.revision, 0x65);
        assert_eq!(snapshot.upseq1, 0x55);
        let (i2c, _) = power.release();
        assert_eq!(i2c.pcal[PCAL_CONFIG0 as usize] & ALL_CONTROL, 0);
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, 0);
        assert_eq!(i2c.pmic, FakeI2c::sleep_defaults());
        assert!(!i2c.events.iter().any(|e| matches!(e, Event::Write(PMIC_ADDR, _, _))));
        assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0x00);
    }
    run_test!(starts_without_activating_rails, starts_without_activating_rails_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn acked_but_ignored_pcal_direction_write_must_fault_body() {
        let mut i2c = FakeI2c::new();
        i2c.ignore_config0_write_once = true;
        let mut power = Inkplate5v2Power::new(i2c, FakeDelay::default());
        assert_eq!(power.initialize().await, Err(PowerError::LowVoltageReadbackMismatch));
        assert_eq!(power.state(), PowerState::Faulted);
        let (i2c, _) = power.release();
        // Data pins are still high-Z. Do not mistake this for an initialized
        // board and allow a power-on with incorrectly configured OE/PWRUP.
        assert_eq!(i2c.pcal[PCAL_CONFIG0 as usize] & ALL_CONTROL, ALL_CONTROL);
        assert_eq!(i2c.pmic[PMIC_ENABLE as usize], 0);
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, 0);
    }
    run_test!(
        acked_but_ignored_pcal_direction_write_must_fault,
        acked_but_ignored_pcal_direction_write_must_fault_body
    );


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
        assert!(delay.total_ns >= 11_000_000);
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
        assert!(rails_disabled < wakeup_low);
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

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn sleeping_pmic_rejects_i2c_and_factory_delays_survive_cycles_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        assert_eq!(power.read(PMIC_ADDR, PMIC_ENABLE).await, Err(PowerError::I2c(FakeError)));
        power.configure_clock_gate().await.unwrap();
        for _ in 0..2 {
            power.enable().await.unwrap();
            assert_eq!(power.read(PMIC_ADDR, PMIC_UPSEQ0).await.unwrap(), 0xe4);
            assert_eq!(power.read(PMIC_ADDR, PMIC_UPSEQ1).await.unwrap(), 0x55);
            assert_eq!(power.read(PMIC_ADDR, PMIC_DWNSEQ0).await.unwrap(), 0x1b);
            assert_eq!(power.read(PMIC_ADDR, PMIC_DWNSEQ1).await.unwrap(), 0x20);
            power.disable().await.unwrap();
            assert_eq!(power.state(), PowerState::Idle);
            assert_eq!(power.read(PMIC_ADDR, PMIC_ENABLE).await, Err(PowerError::I2c(FakeError)));
        }
    }
    run_test!(sleeping_pmic_rejects_i2c_and_factory_delays_survive_cycles,
        sleeping_pmic_rejects_i2c_and_factory_delays_survive_cycles_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn physical_pwrup_high_with_low_latch_blocks_wake_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        power.i2c.pcal[PCAL_INPUT0 as usize] |= PWRUP;
        assert_eq!(power.enable().await, Err(PowerError::ActiveControlPins));
        assert_eq!(power.state(), PowerState::Faulted);
        assert_eq!(power.i2c.physical_controls() & WAKEUP, 0);
    }
    run_test!(physical_pwrup_high_with_low_latch_blocks_wake,
        physical_pwrup_high_with_low_latch_blocks_wake_body);

    #[test]
    fn shutdown_budget_accounts_for_factory_delay_multiplier() {
        assert_eq!(shutdown_poll_budget(0), SHUTDOWN_TIMEOUT_MS);
        assert_eq!(shutdown_poll_budget(0xff), 2410);
    }

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn unsafe_input_configuration_prevents_wake_body() {
        for (reg, bit) in [
            (PCAL_POLARITY0, PWR_GOOD_PIN), (PCAL_INPUT_LATCH0, PWR_GOOD_PIN),
            (PCAL_POLARITY0, PWRUP), (PCAL_POLARITY1, GPIO0_ENABLE),
            (PCAL_INPUT_LATCH1, GPIO0_ENABLE),
        ] {
            let mut i2c = FakeI2c::new();
            i2c.pcal[reg as usize] = bit;
            let mut power = Inkplate5v2Power::new(i2c, FakeDelay::default());
            assert_eq!(power.initialize().await, Err(PowerError::ExpanderInputConfig));
            assert_eq!(power.state(), PowerState::Faulted);
            assert!(!power.i2c.events.iter().any(|event| matches!(event, Event::Write(..))));
        }
    }
    run_test!(unsafe_input_configuration_prevents_wake,
        unsafe_input_configuration_prevents_wake_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn cached_idle_is_revalidated_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.i2c.pcal[PCAL_INPUT0 as usize] |= PWRUP;
        assert_eq!(power.disable().await, Err(PowerError::ActiveControlPins));
        assert_eq!(power.state(), PowerState::Faulted);
    }
    run_test!(cached_idle_is_revalidated, cached_idle_is_revalidated_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn timed_power_cycle_waits_for_wake_ramp_and_enable_clear_body() {
        let clock = Rc::new(Cell::new(0));
        let mut i2c = FakeI2c::new();
        i2c.clock = Some(clock.clone());
        i2c.factory_down_delay = 0xff;
        i2c.pmic[PMIC_DWNSEQ1 as usize] = 0xff;
        let delay = FakeDelay { clock: Some(clock.clone()), ..FakeDelay::default() };
        let mut power = Inkplate5v2Power::new(i2c, delay);
        power.initialize().await.unwrap();
        assert!(clock.get() >= 5_000_000);
        power.configure_clock_gate().await.unwrap();
        for _ in 0..2 {
            let start = clock.get();
            power.enable().await.unwrap();
            assert!(clock.get() - start >= 33_000_000);
            let start = clock.get();
            power.disable().await.unwrap();
            assert!(clock.get() - start >= 350_000_000);
            assert_eq!(power.state(), PowerState::Idle);
        }
    }
    run_test!(timed_power_cycle_waits_for_wake_ramp_and_enable_clear,
        timed_power_cycle_waits_for_wake_ramp_and_enable_clear_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn post_side_effect_startup_failures_are_cleaned_up_body() {
        for offset in 1..=8 {
            let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
            power.initialize().await.unwrap();
            power.configure_clock_gate().await.unwrap();
            power.i2c.fail_after_write_attempt = Some(power.i2c.write_attempts + offset);
            assert_eq!(power.enable().await, Err(PowerError::I2c(FakeError)), "write {offset}");
            assert_eq!(power.state(), PowerState::Faulted);
            assert_eq!(power.i2c.physical_controls(), 0, "write {offset}");
            assert_eq!(power.i2c.pmic[PMIC_ENABLE as usize], 0);
        }
    }
    run_test!(post_side_effect_startup_failures_are_cleaned_up,
        post_side_effect_startup_failures_are_cleaned_up_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn pmic_fault_flags_block_scanning_and_survive_sleep_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        power.enable().await.unwrap();
        power.i2c.pmic[PMIC_INT2 as usize] = 0x04; // VCOMF
        assert_eq!(power.verify_live_rails().await, Err(PowerError::PmicFault { int1: 0, int2: 4 }));
        assert_eq!(power.state(), PowerState::Faulted);
        power.disable().await.unwrap();
        assert_eq!(power.state(), PowerState::Faulted);
        assert_eq!(power.pmic_fault_snapshot().int2, Some(4));
        assert_eq!(power.i2c.pmic[PMIC_INT2 as usize], 0);
    }
    run_test!(pmic_fault_flags_block_scanning_and_survive_sleep,
        pmic_fault_flags_block_scanning_and_survive_sleep_body);

    #[cfg(not(feature = "blocking"))]
    #[test]
    fn cancelled_shutdown_never_reuses_on_state() {
        use core::future::Future;
        use core::task::{Context, Poll, Waker};
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        pollster::block_on(async {
            power.initialize().await.unwrap();
            power.configure_clock_gate().await.unwrap();
            power.enable().await.unwrap();
        });
        power.delay.suspend_once = true;
        {
            let mut future = core::pin::pin!(power.disable());
            let mut cx = Context::from_waker(Waker::noop());
            assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));
        }
        assert_eq!(power.state(), PowerState::Faulted);
        assert_eq!(pollster::block_on(power.enable()), Err(PowerError::InvalidState(PowerState::Faulted)));
        pollster::block_on(power.disable()).unwrap();
        assert_eq!(power.i2c.physical_controls(), 0);
        assert_eq!(power.state(), PowerState::Faulted);
    }

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn unsafe_vcom_modes_prevent_rail_startup_body() {
        for mode in [0x20, 0x40, 0x80] {
            let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
            power.initialize().await.unwrap();
            power.configure_clock_gate().await.unwrap();
            power.i2c.pmic[PMIC_VCOM2 as usize] = mode;
            assert_eq!(power.enable().await, Err(PowerError::InvalidVcomMode(mode)));
            assert_eq!(power.state(), PowerState::Faulted);
            assert_eq!(power.i2c.physical_controls(), 0);
            assert!(!power.i2c.events.iter().any(|e|
                matches!(e, Event::Write(EXPANDER_ADDR, PCAL_OUTPUT0, bytes)
                    if bytes[0] & PWRUP != 0)
            ));
        }
    }
    run_test!(unsafe_vcom_modes_prevent_rail_startup, unsafe_vcom_modes_prevent_rail_startup_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn polling_deadline_counts_i2c_time_body() {
        let clock = Rc::new(Cell::new(0));
        let mut i2c = FakeI2c::new();
        i2c.clock = Some(clock.clone());
        i2c.transaction_ns = 10_000_000;
        i2c.rails_ready = false;
        let delay = FakeDelay { clock: Some(clock), ..FakeDelay::default() };
        let mut power = Inkplate5v2Power::new(i2c, delay);
        power.initialize().await.unwrap();
        power.configure_clock_gate().await.unwrap();
        let first = power.i2c.events.len();
        let result = power.enable().await;
        assert_eq!(result, Err(PowerError::PowerGoodTimeout(0)));
        let (i2c, delay) = power.release();
        // Iteration-count polling would wait ~5.5 seconds here. The ramp
        // phase consumes its real 500 ms budget, including I2C latency.
        let polls = i2c.events[first..].iter().filter(|e|
            matches!(e, Event::Read(PMIC_ADDR, PMIC_PWRGOOD, _))).count();
        assert!(polls < 65, "too many PG reads: {polls}");
        assert!(delay.total_ns < 200_000_000);
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, 0);
    }
    run_test!(polling_deadline_counts_i2c_time, polling_deadline_counts_i2c_time_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn retained_power_recovery_never_rearms_body() {
        let mut original = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        original.initialize().await.unwrap();
        original.configure_clock_gate().await.unwrap();
        original.enable().await.unwrap();
        let (mut i2c, _) = original.release();
        i2c.events.clear();
        let mut recovered = Inkplate5v2Power::new(i2c, FakeDelay::default());
        recovered.recover_retained_power().await.unwrap();
        assert_eq!(recovered.state(), PowerState::Faulted);
        let next = recovered.enable().await;
        assert_eq!(next, Err(PowerError::InvalidState(PowerState::Faulted)));
        let (i2c, _) = recovered.release();
        assert_eq!(i2c.pcal[PCAL_OUTPUT0 as usize] & ALL_CONTROL, 0);
        assert_eq!(i2c.pcal[PCAL_OUTPUT1 as usize] & GPIO0_ENABLE, 0);
        assert!(!i2c.events.iter().any(|event| matches!(event,
            Event::Write(PMIC_ADDR, PMIC_UPSEQ0 | PMIC_DWNSEQ0, _))));
        assert!(!i2c.events.iter().any(|event| matches!(event,
            Event::Write(EXPANDER_ADDR, PCAL_OUTPUT0, values)
                if values[0] & (OE | VCOM | PWRUP) == (OE | VCOM | PWRUP))));
    }
    run_test!(retained_power_recovery_never_rearms, retained_power_recovery_never_rearms_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn retained_power_recovery_rejects_unknown_directions_body() {
        let mut power = Inkplate5v2Power::new(FakeI2c::new(), FakeDelay::default());
        let result = power.recover_retained_power().await;
        assert_eq!(result, Err(PowerError::LowVoltageReadbackMismatch));
        assert_eq!(power.state(), PowerState::Faulted);
        let (i2c, _) = power.release();
        assert!(!i2c.events.iter().any(|e| matches!(e, Event::Write(..))));
    }
    run_test!(retained_power_recovery_rejects_unknown_directions,
        retained_power_recovery_rejects_unknown_directions_body);

}
