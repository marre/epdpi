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
    i2c::master::{Config as I2cConfig, I2c},
    main,
    time::{Duration, Instant},
};

#[cfg(feature = "low-voltage-prep")]
use epdpi_esp::{
    i2s_parallel::Esp32I2sTx,
    inkplate5v2_bus::{Inkplate5v2Bus, Inkplate5v2DirectPins},
};

#[cfg(feature = "low-voltage-prep")]
use esp_hal::{
    dma::{DmaTxBuf},
    dma_buffers,
    gpio::{Level, Output, OutputConfig},
    i2s::parallel::{I2sParallel, TxEightBits},
    time::Rate,
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

    let i2c = match I2c::new(peripherals.I2C0, I2cConfig::default()) {
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
    // This configures GPIO and PMIC sequencing registers while keeping HV off.
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

        // Direct control pins are brought to the manufacturer's idle levels
        // *before* I2S is attached to GPIO0 and before PMIC setup.
        let pins = Inkplate5v2DirectPins {
            le: Output::new(peripherals.GPIO2, Level::Low, OutputConfig::default()),
            ckv: Output::new(peripherals.GPIO32, Level::Low, OutputConfig::default()),
            sph: Output::new(peripherals.GPIO33, Level::High, OutputConfig::default()),
        };

        // Construct the existing esp-hal I2S1 DMA transport. This sets up
        // low-voltage clock/data GPIO routing but sends NO bytes. The classic
        // ESP32 I2S1 data pin mapping follows Soldered's V2 board schematic.
        let pixel_pins = TxEightBits::new(
            peripherals.GPIO4,
            peripherals.GPIO5,
            peripherals.GPIO18,
            peripherals.GPIO19,
            peripherals.GPIO23,
            peripherals.GPIO25,
            peripherals.GPIO26,
            peripherals.GPIO27,
        );
        let parallel = I2sParallel::new(
            peripherals.I2S1,
            peripherals.DMA_I2S1,
            Rate::from_mhz(16),
            pixel_pins,
            peripherals.GPIO0,
        );
        let (_, _, buffer, descriptors) = dma_buffers!(0, 336);
        let tx_buffer = match DmaTxBuf::new(descriptors, buffer) {
            Ok(buffer) => buffer,
            Err(error) => {
                esp_println::println!("I2S DMA buffer creation failed: {error:?}. Halted.");
                loop { core::hint::spin_loop(); }
            }
        };
        let tx = Esp32I2sTx::new(parallel, tx_buffer);
        let mut bus = Inkplate5v2Bus::new(tx, power, pins);

        match bus.prepare_low_voltage() {
            Ok(()) => esp_println::println!(
                "Low-voltage preparation succeeded; panel HV remains LOCKED."
            ),
            Err(error) => {
                esp_println::println!("Low-voltage preparation failed: {error:?}. Halted.");
                // The bus latches the error. Do not retry or silently assume
                // the physical rails were discharged successfully.
                loop { core::hint::spin_loop(); }
            }
        }

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
