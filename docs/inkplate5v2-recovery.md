# Inkplate 5 Gen 2 — backup and recovery before the first flash

> **No hardware execution is verified. Do not flash the experimental
> diagnostic image until ROM recovery and an original full-flash backup
> have been confirmed for your specific board.**

This procedure concerns the **classic ESP32 WROVER-E** Inkplate 5 Gen 2
(V1.1.0 schematic), **not** an ESP32-S3 board. The diagnostic firmware
does not drive the e-paper display, but replacing ESP32 flash is still a
destructive operation. An invalid bootloader or partition layout could
make the currently installed firmware stop booting. Wrong high-voltage
sequencing could damage the only e-paper panel; this diagnostic does not
intentionally enable high voltage.

## Preparation — do not write anything

Install [Espressif esptool](https://docs.espressif.com/projects/esptool/en/latest/esp32/esptool/basic-commands.html)
on a host with USB serial access (for example, `python -m pip install esptool`).
Connect through the board's CH340C USB serial bridge. Identify its port:
`/dev/ttyUSB0` on many Linux systems or `COMx` on Windows; this is
an **example**, not a guaranteed port name.

If the normal firmware uses the serial port, close other serial programs.
Enter the ESP32 ROM download mode using its supported BOOT/RESET procedure,
or verified DTR/RTS auto-reset. **Do not modify JP4/JP5 or force GPIO0
levels on your only device unless the board schematic and procedure have
been independently checked.** On this board GPIO0 also feeds the display
clock circuitry. If ROM mode is inaccessible, stop before flashing.

The following `PORT` token is a placeholder: substitute the serial port
reported by your computer.

```sh
python -m esptool --chip esp32 --port PORT chip-id
python -m esptool --chip esp32 --port PORT flash-id
```

Record the ESP32 chip identification and detected flash size. Verify that
esptool can consistently reconnect after reset. A successful `chip-id`
does **not** verify that the current firmware can be restored.

## Full-flash backup (read only)

```sh
python -m esptool --chip esp32 --port PORT read-flash 0 ALL inkplate-original-A.bin
python -m esptool --chip esp32 --port PORT read-flash 0 ALL inkplate-original-B.bin
sha256sum inkplate-original-*.bin
```

The two SHA-256 hashes should match, and both file sizes should equal the
detected flash capacity. If the reads differ, are truncated, or report
flash-encryption/secure-boot restrictions, **stop**. Do not use
`--force`, disable security fuses, erase flash, or attempt to bypass
download-mode restrictions. A binary backup does not include eFuses,
cryptographic keys in eFuses, or hardware calibration stored outside flash.

Retain **both** copies offline. A full-flash image contains original
bootloader, partition table, NVS/configuration and application data;
it may also contain sensitive identifiers and credentials. Do **not**
upload the backup to a public GitHub issue or PR.

You can inspect the backup's likely partition layout with
`esptool`/an ESP-IDF partition tool as an additional precaution. We
have not yet recorded the factory partition table for your physical unit.

## Validate ROM-boot recovery

Before writing anything, practice leaving and reentering ROM download
mode, disconnect/reconnect the port, and verify `chip-id` works again.
Check that you can physically reach the BOOT/RESET controls or use a
reliable USB auto-reset procedure. The CH340C/USB interface is helpful,
but it is not an absolute guarantee of recovery from every boot state.

If the original firmware uses flash encryption, secure boot, or is
locked against serial download, a raw backup may **not** provide a usable
recovery path. Stop and preserve the original device.

## Emergency restoration of the original image

Only if the same classic ESP32 is still accessible through ROM download
mode, the backup was verified and no security restrictions apply, a
full-flash restoration can be attempted:

```sh
python -m esptool --chip esp32 --port PORT write-flash 0x0 inkplate-original-A.bin
python -m esptool --chip esp32 --port PORT verify-flash 0x0 inkplate-original-A.bin
```

This overwrites the entire flash, including current firmware and
configuration. Never run it against a different device. Do not use
`erase-flash` as part of the initial diagnostic workflow.
If any uncertainty remains, stop instead of attempting a restoration.

## What the experimental diagnostic actually does

The independent Cargo crate at `firmware/inkplate5v2-diagnostic`:

- initializes UART0 console and I2C0 on GPIO21 (SDA) and GPIO22 (SCL);
- reads PCAL6416A at address `0x20` through register-pointer read
  transactions (which send an address byte but perform **no data writes**
  to PCAL configuration/output registers);
- reports inputs, output latches, directions and the physical
  PWR_GOOD signal at PCAL Port 0 bit 7;
- repeats serial output approximately every five seconds;
- does **not** link our display library or configure TPS65186, I2S1,
  GPIO0, LE, SPH, CKV, OE, SPV, VCOM or PWRUP.

The exact board revision and I2C pin routing must still be verified before
the first real execution. **Never treat an output register readback as
proof of a safe high-voltage state.**

The optional CI workflow builds an experimental ELF and merged flash
image labeled `UNVALIDATED`. A merged image includes bootloader and
partition-table content and can overwrite factory layout and settings.
**The existence of the artifact does not authorize flashing it.**

## Exit criteria before trying any new firmware

1. Both original flash snapshots are present, identical, and stored offline.
2. The classic ESP32 and flash size have been identified.
3. ROM download mode can be reentered after reset without altering jumpers.
4. No security restrictions or unexpected board revisions are detected.
5. UART/I2C pins and the exact flashing offset/layout have been independently
   checked against the physical hardware and its existing partition table.
6. The diagnostic firmware workflow passes and its binary has been inspected.

Only after these gates should we give the device-specific flashing command.
A later stage will validate display signals separately while panel high
voltage remains gated.
