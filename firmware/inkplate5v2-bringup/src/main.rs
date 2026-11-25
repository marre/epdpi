//! Minimal classic-ESP32 Inkplate 5 Gen 2 board bring-up firmware.
//!
//! The default image ONLY reads PCAL6416A registers and is non-driving with
//! respect to the panel. It links the real epdpi-esp board support, but does
//! not configure direct EPD control pins or I2S unless built explicitly with
//! `--features low-voltage-prep`.
//!
//! Even the opt-in low-voltage build DOES NOT request display HV: the
//! Inkplate5v2Bus::set_power(true) interlock is unchanged. It creates an
//! I2S1 transmitter but never sends a byte or starts the peripheral.
//!
//! Do not flash ANY experimental firmware until the board's full-flash
//! backup and ROM recovery procedure have been independently verified.

#![no_std]
#![no_main]

use esp_backtrace as _;
use epdpi_esp::inkplate5v2_power::{ExpanderSnapshot, Inkplate5v2Power};
use esp_hal::{
    delay::Delay,
    i2c::master::{Config as I2cConfig, I2c, SoftwareTimeout},
    main,
    time::{Duration, Instant},
};

#[cfg(feature = "low-voltage-prep")]
use epdpi_esp::{
    inkplate5v2_bus::{Inkplate5v2Bus, Inkplate5v2DirectPins, Inkplate5v2Tx},
    inkplate5v2_power::PmicSnapshot,
};

#[cfg(feature = "low-voltage-prep")]
use esp_hal::{
    dma::{DmaTxBuf},
    dma_buffers,
    gpio::{Flex, Pin},
};

esp_bootloader_esp_idf::esp_app_desc!();

fn log_snapshot(snapshot: ExpanderSnapshot) {
    esp_println::println!(
        "PCAL INPUT0=0x{:02X} OUTPUT0=0x{:02X} DIR0=0x{:02X}",
        snapshot.input_port0,
        snapshot.output_port0,
        snapshot.direction_port0
    );
    esp_println::println!(
        "PCAL OUTPUT1=0x{:02X} DIR1=0x{:02X}; PWR_GOOD={} CLOCK_GATE={}",
        snapshot.output_port1,
        snapshot.direction_port1,
        if snapshot.pgood_pin_high() { "HIGH" } else { "LOW" },
        if snapshot.clock_enabled() { "ON" } else { "OFF" }
    );
    esp_println::println!("PGOOD is a logic reading, NOT a physical rail-voltage measurement.");
}

#[cfg(feature = "low-voltage-prep")]
fn halt() -> ! {
    loop { core::hint::spin_loop(); }
}

#[cfg(feature = "low-voltage-prep")]
fn check_transition(initial: ExpanderSnapshot, actual: ExpanderSnapshot, released: bool) -> bool {
    // Preserve unrelated output latches and directions; ignore unrelated
    // input levels and the floating OE/GMOD/SPV levels after release.
    let input_mask = if released { 0xb8 } else { 0xbf };
    actual.input_port0 & input_mask == 0
        && actual.output_port0 == initial.output_port0 & !0x3f
        && actual.direction_port0 == (initial.direction_port0 & !0x3f) | if released { 7 } else { 0 }
        && actual.direction_port0 & 0x80 != 0
        && actual.output_port1 == (initial.output_port1 & !1) | u8::from(!released)
        && actual.direction_port1 == initial.direction_port1 & !1
        && actual.input_port1 & 1 == u8::from(!released)
}

#[cfg(feature = "low-voltage-prep")]
fn log_pmic(snapshot: PmicSnapshot) {
    esp_println::println!(
        "TPS65186 standby: REVID=0x{:02X} ENABLE=0x{:02X} PG=0x{:02X} VADJ=0x{:02X}",
        snapshot.revision, snapshot.enable, snapshot.power_good, snapshot.vadj
    );
    esp_println::println!(
        "Factory VCOM={} mV (configured, not measured); VCOM1=0x{:02X} VCOM2=0x{:02X}",
        snapshot.configured_vcom_mv(), snapshot.vcom1, snapshot.vcom2
    );
    esp_println::println!(
        "Factory sequencer UP={:02X}/{:02X} DOWN={:02X}/{:02X}",
        snapshot.upseq0, snapshot.upseq1, snapshot.dwnseq0, snapshot.dwnseq1
    );
}

fn wait_for_next_snapshot() {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        core::hint::spin_loop();
    }
}

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    esp_println::println!("Inkplate 5 Gen 2 / epdpi board bring-up v0.1.0");
    esp_println::println!("Target: classic ESP32 WROVER-E, ED052TC4");
    esp_println::println!("High-voltage and display scanning are DISABLED.");
    esp_println::println!("Before flashing, verify the original full-flash backup and ROM recovery.");

    let i2c = match I2c::new(peripherals.I2C0, I2cConfig::default()
        .with_software_timeout(SoftwareTimeout::Transaction(Duration::from_millis(20)))) {
        Ok(i2c) => i2c
            .with_sda(peripherals.GPIO21)
            .with_scl(peripherals.GPIO22),
        Err(error) => {
            esp_println::println!("I2C initialization failed: {error:?}. Halted.");
            loop { core::hint::spin_loop(); }
        }
    };
    let power = Inkplate5v2Power::new(i2c, Delay::new());

    // Default build: create NO display output pins, I2S, or PMIC writes.
    // The read-only first-stage diagnostic remains independently buildable.
    #[cfg(not(feature = "low-voltage-prep"))]
    {
        esp_println::println!("Mode: READ ONLY (no PCAL output/configuration register writes)");
        let mut power = power;
        loop {
            match power.inspect_expander() {
                Ok(snapshot) => log_snapshot(snapshot),
                Err(error) => esp_println::println!("Read-only inspection failed: {error:?}"),
            }
            wait_for_next_snapshot();
        }
    }

    // Explicit experimental mode. Builds with:
    //   cargo build --release --features low-voltage-prep --target xtensa-esp32-none-elf
    // This configures GPIO and reads the awake PMIC while keeping HV off.
    // DO NOT FLASH before checking the recovery instructions and original
    // read-only snapshot on the exact physical board.
    #[cfg(feature = "low-voltage-prep")]
    {
        esp_println::println!("Mode: EXPLICIT LOW-VOLTAGE PREPARATION");
        esp_println::println!("No DMA transfers, waveform scans, or high-voltage requests.");

        // First record the state as found. The power controller will refuse
        // to initialize if the existing OE/PWRUP/VCOM pins are actively HIGH.
        let mut power = power;
        let initial = match power.inspect_expander() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                esp_println::println!("Initial PCAL inspection failed: {error:?}. Halted.");
                loop { core::hint::spin_loop(); }
            }
        };
        log_snapshot(initial);
        // A diagnostic readout alone is not permission to drive GPIO0, CKV,
        // SPH or LE. Abort before even constructing the I2S pin matrix if
        // the panel may be on, the physical PGOOD input is misconfigured,
        // or any power control output is asserted.
        if let Err(error) = power.preflight_low_voltage() {
            esp_println::println!(
                "Unsafe low-voltage entry: {error:?}. No display GPIO or I2S configured."
            );
            loop { core::hint::spin_loop(); }
        }
        esp_println::println!("Read-only power-entry preflight passed.");

        // Keep the direct control pins flexible and initially non-driving.
        // The bus arms the pins only after its own off-state checks.
        let pins = Inkplate5v2DirectPins {
            le: Flex::new(peripherals.GPIO2),
            ckv: Flex::new(peripherals.GPIO32),
            sph: Flex::new(peripherals.GPIO33),
        };

        // Keep exclusive GPIO ownership in the board transport; the HAL
        // I2S peripheral itself is configured with NoPin placeholders.
        let pixel_pins = [
            peripherals.GPIO4.degrade(), peripherals.GPIO5.degrade(),
            peripherals.GPIO18.degrade(), peripherals.GPIO19.degrade(),
            peripherals.GPIO23.degrade(), peripherals.GPIO25.degrade(),
            peripherals.GPIO26.degrade(), peripherals.GPIO27.degrade(),
            peripherals.GPIO0.degrade(),
        ];
        let (_, _, buffer, descriptors) = dma_buffers!(0, 336);
        let tx_buffer = match DmaTxBuf::new(descriptors, buffer) {
            Ok(buffer) => buffer,
            Err(error) => {
                esp_println::println!("I2S DMA buffer creation failed: {error:?}. Halted.");
                loop { core::hint::spin_loop(); }
            }
        };
        let tx = Inkplate5v2Tx::new(peripherals.I2S1, peripherals.DMA_I2S1, tx_buffer, pixel_pins);
        let mut bus = Inkplate5v2Bus::new(tx, power, pins);

        let cycles = if cfg!(feature = "low-voltage-cycles") { 100 } else { 1 };
        let mut baseline: Option<PmicSnapshot> = None;
        let mut prep_min = u64::MAX;
        let mut prep_max = 0u64;
        let mut off_min = u64::MAX;
        let mut off_max = 0u64;
        esp_println::println!("Low-voltage cycle test: {cycles} cycle(s); HV locked.");
        for cycle in 1..=cycles {
            let started = Instant::now();
            if let Err(error) = bus.prepare_low_voltage() {
                esp_println::println!("Cycle {cycle}/{cycles} preparation failed: {error:?}. Halted.");
                halt();
            }
            let prep_us = started.elapsed().as_micros();
            // Do not return early on a diagnostic mismatch before attempting
            // shutdown. The PMIC is asleep and HV has never been requested.
            let prepared = bus.inspect();
            let pmic = bus.power().idle_pmic_snapshot();
            let prepared_ok = match prepared {
                Ok(snapshot) => {
                    if cycle == 1 { log_snapshot(snapshot); }
                    let ok = check_transition(initial, snapshot, false);
                    if !ok {
                        esp_println::println!("Cycle {cycle}/{cycles} unexpected prepared registers:");
                        log_snapshot(snapshot);
                    }
                    ok
                }
                Err(error) => {
                    esp_println::println!("Cycle {cycle}/{cycles} prepared inspection failed: {error:?}");
                    false
                }
            };
            let pmic_ok = match pmic {
                Some(snapshot) => {
                    if let Some(reference) = baseline {
                        if snapshot != reference {
                            esp_println::println!("Cycle {cycle}/{cycles} PMIC standby configuration changed:");
                            log_pmic(snapshot);
                            false
                        } else {
                            true
                        }
                    } else {
                        baseline = Some(snapshot);
                        log_pmic(snapshot);
                        snapshot.enable == 0 && snapshot.power_good == 0
                    }
                }
                None => {
                    esp_println::println!("Cycle {cycle}/{cycles} missing fresh PMIC snapshot");
                    false
                }
            };
            let started = Instant::now();
            if let Err(error) = bus.shutdown_panel() {
                esp_println::println!("Cycle {cycle}/{cycles} shutdown failed: {error:?}. Halted.");
                halt();
            }
            let off_us = started.elapsed().as_micros();
            let released_ok = match bus.inspect() {
                Ok(snapshot) => {
                    if cycle == 1 || cycle == cycles { log_snapshot(snapshot); }
                    let ok = check_transition(initial, snapshot, true);
                    if !ok {
                        esp_println::println!("Cycle {cycle}/{cycles} unexpected released registers:");
                        log_snapshot(snapshot);
                    }
                    ok
                }
                Err(error) => {
                    esp_println::println!("Cycle {cycle}/{cycles} released inspection failed: {error:?}");
                    false
                }
            };
            if !prepared_ok || !pmic_ok || !released_ok {
                esp_println::println!("FAIL: cycle {cycle}/{cycles}; no further cycles. Halted.");
                halt();
            }
            prep_min = prep_min.min(prep_us);
            prep_max = prep_max.max(prep_us);
            off_min = off_min.min(off_us);
            off_max = off_max.max(off_us);
            esp_println::println!("PASS cycle {cycle}/{cycles}: prep={prep_us} us shutdown={off_us} us");
            // Let the PMIC settle in sleep between cycles. This is a
            // low-voltage diagnostic interval, not an HV discharge proof.
            if cycle < cycles { Delay::new().delay_millis(100); }
        }
        esp_println::println!("PASS: {cycles}/{cycles} low-voltage cycles; PMIC configuration unchanged; HV never enabled.");
        esp_println::println!("Preparation min/max: {prep_min}/{prep_max} us; shutdown min/max: {off_min}/{off_max} us");
        esp_println::println!("Panel GPIO shutdown complete; no further cycles.");
        loop {
            match bus.inspect() {
                Ok(snapshot) => log_snapshot(snapshot),
                Err(error) => {
                    esp_println::println!("Post-prepare inspection failed: {error:?}. Halted.");
                    loop { core::hint::spin_loop(); }
                }
            }
            wait_for_next_snapshot();
        }
    }
}
