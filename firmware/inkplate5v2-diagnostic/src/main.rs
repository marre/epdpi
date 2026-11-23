//! Inkplate 5 Gen 2 first-stage diagnostics: intentionally **read-only**.
//!
//! This binary does not link the panel library and never configures I2S1,
//! GPIO0, CKV, SPH, LE, the TPS65186, or PCAL6416A output/direction pins.
//! It only selects and reads PCAL6416A register addresses via I2C0.
//! I2C register selection transmits a register number, but it is not a
//! write to a hardware configuration register.

#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    i2c::master::{Config as I2cConfig, I2c},
    main,
    time::{Duration, Instant},
};

// An app descriptor for the standard ESP-IDF second-stage bootloader.
// The actual hardware flash layout is still unverified.
esp_bootloader_esp_idf::esp_app_desc!();

const PCAL_ADDR: u8 = 0x20;
const REGISTERS: [(&str, u8); 6] = [
    ("INPUT0", 0x00),
    ("INPUT1", 0x01),
    ("OUTPUT0", 0x02),
    ("OUTPUT1", 0x03),
    ("CONFIG0", 0x06),
    ("CONFIG1", 0x07),
];

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    esp_println::println!("Inkplate 5 Gen 2: diagnostic-only v0.1.0");
    esp_println::println!("No panel HV, PMIC writes, display GPIO or I2S operations.");
    esp_println::println!("I2C0 SDA=GPIO21 SCL=GPIO22 (verify board revision).");

    // Only SDA and SCL are configured. These default to I2C open-drain.
    let mut i2c = match I2c::new(peripherals.I2C0, I2cConfig::default()) {
        Ok(i2c) => i2c
            .with_sda(peripherals.GPIO21)
            .with_scl(peripherals.GPIO22),
        Err(err) => {
            esp_println::println!("I2C init failed: {err:?}; halted.");
            loop { core::hint::spin_loop(); }
        }
    };

    loop {
        esp_println::println!("--- PCAL6416A 0x20: register snapshot ---");
        let mut input0: Option<u8> = None;
        for (name, register) in REGISTERS {
            let mut value = [0u8; 1];
            match i2c.write_read(PCAL_ADDR, &[register], &mut value) {
                Ok(()) => {
                    esp_println::println!("{name} (0x{register:02X}) = 0x{:02X}", value[0]);
                    if register == 0 { input0 = Some(value[0]); }
                }
                Err(err) => esp_println::println!(
                    "{name} (0x{register:02X}): I2C error: {err:?}"
                ),
            }
        }
        if let Some(value) = input0 {
            esp_println::println!(
                "Physical PWR_GOOD at PCAL P0.7: {} (not a rail-voltage measurement)",
                if (value & 0x80) != 0 { "HIGH" } else { "LOW" }
            );
        }
        esp_println::println!("No registers modified. Repeating in 5 s.");

        // No actions on the panel or GPIO during the interval.
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            core::hint::spin_loop();
        }
    }
}
