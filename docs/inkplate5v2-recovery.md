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
Alternatively, install [espflash](https://github.com/esp-rs/espflash) for
the backup and restore operations. The examples below use the standalone
[espflash 4.7.0 CLI](https://docs.rs/crate/espflash/4.7.0); they were checked
against its [argument definitions](https://github.com/esp-rs/espflash/blob/v4.7.0/espflash/src/cli/mod.rs),
not executed on an Inkplate.

```sh
cargo install espflash --version 4.7.0 --locked
espflash --version
espflash list-ports --list-all-ports
```

Keep esptool/espefuse available for the classic ESP32 security checks below.
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

Or, with espflash:

```sh
espflash board-info --chip esp32 --port PORT --after no-reset
```

Record the ESP32 chip identification (including MAC address privately) and
detected flash size. Verify that your chosen tool can consistently reconnect
after reset. Neither identification command proves that the current
firmware can be restored.

**Security check for both tools:** espflash 4.7.0's `board-info` prints
`Security features: None` for the classic ESP32 without querying its
security eFuses. Do not rely on that line. Use the read-only Espressif
[eFuse summary](https://docs.espressif.com/projects/esptool/en/latest/esp32/espefuse/summary-cmd.html)
before any flash write:

```sh
espefuse --chip esp32 --port PORT summary
```

Check that flash encryption is disabled (an even number of set bits in
`FLASH_CRYPT_CNT`), secure boot is disabled (`ABS_DONE_0` and
`ABS_DONE_1`, where applicable, are clear), and UART download is not
restricted. If any security configuration is uncertain, stop. Do not burn
or modify any eFuses. Keep the summary private.

## Full-flash backup (read only)

Choose one of the following command sets; do not overwrite existing backups.
Keep the board in ROM download mode between reads to prevent the original
application from changing flash contents (for example NVS).

### esptool

```sh
python -m esptool --chip esp32 --port PORT --after no-reset read-flash 0 ALL inkplate-original-A.bin
python -m esptool --chip esp32 --port PORT --after no-reset read-flash 0 ALL inkplate-original-B.bin
sha256sum inkplate-original-*.bin
```

### espflash

Unlike esptool, espflash requires a numeric byte count rather than `ALL`.
Use the flash capacity reported by `board-info`, not the size of the app
partition or PSRAM. The following sizes are examples, **not an assumption
about your board**:

| Detected flash capacity | Byte count |
| --- | --- |
| 4 MiB | `0x400000` |
| 8 MiB | `0x800000` |
| 16 MiB | `0x1000000` |

Replace both `PORT` and `FLASH_BYTES` with the verified values (for
example `/dev/ttyUSB0` and `0x400000` if the board reports 4 MiB).

```sh
espflash read-flash --chip esp32 --port PORT --after no-reset 0x0 FLASH_BYTES inkplate-original-A.bin
espflash read-flash --chip esp32 --port PORT --after no-reset 0x0 FLASH_BYTES inkplate-original-B.bin
sha256sum inkplate-original-A.bin inkplate-original-B.bin
wc -c inkplate-original-A.bin inkplate-original-B.bin
cmp inkplate-original-A.bin inkplate-original-B.bin
```

Use the default RAM flasher stub; it does not install a new flash
bootloader. If automatic entry fails, use the previously verified manual
ROM entry procedure and the tool's `--before no-reset` option only when
already in ROM download mode.

### Verify and retain the backup

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
mode, disconnect/reconnect the port, and verify `chip-id` (esptool) or
`board-info` (espflash) works again. Confirm the same MAC address and flash
capacity.
Check that you can physically reach the BOOT/RESET controls or use a
reliable USB auto-reset procedure. The CH340C/USB interface is helpful,
but it is not an absolute guarantee of recovery from every boot state.

If the original firmware uses flash encryption, secure boot, or is
locked against serial download, a raw backup may **not** provide a usable
recovery path. Stop and preserve the original device.

## Emergency restoration of the original image

Only if the same classic ESP32 is still accessible through ROM download
mode, the backup was verified and no security restrictions apply, a
full-flash restoration can be attempted. Confirm the backup hash, exact
file length, chip identity and detected flash capacity again before proceeding.
Choose **one** restore method, not both.

### esptool

```sh
python -m esptool --chip esp32 --port PORT --after no-reset write-flash 0x0 inkplate-original-A.bin
python -m esptool --chip esp32 --port PORT --after no-reset verify-flash 0x0 inkplate-original-A.bin
```

### espflash

`write-bin` writes the raw image at address zero, unlike `flash`, which
constructs an application image from an ELF. Keep the device in download
mode until verification is complete. Use the same full capacity as for
the backup:

```sh
espflash write-bin --chip esp32 --port PORT --after no-reset 0x0 inkplate-original-A.bin
espflash read-flash --chip esp32 --port PORT --after no-reset 0x0 FLASH_BYTES inkplate-restored.bin
sha256sum inkplate-original-A.bin inkplate-restored.bin
cmp inkplate-original-A.bin inkplate-restored.bin
```

Both SHA-256 hashes and file lengths must match, and `cmp` must succeed.
Do not reset into the restored application until the byte-for-byte
comparison passes. Then leave ROM mode with the verified normal reset
procedure and confirm the original firmware boots and operates normally.
If verification fails, preserve the backup and keep the device in ROM
mode while investigating; do not boot a partial restoration.

This overwrites the entire flash, including current firmware and
configuration. Never run it against a different device. Do not use
`erase-flash` as part of the initial diagnostic workflow, or use `espflash
flash` to restore a raw full-flash backup.
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
