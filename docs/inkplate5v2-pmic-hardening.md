# TPS65186 power-state hardening

Source: [TI TPS65186 SLVSB04A](https://www.ti.com/lit/ds/symlink/tps65186.pdf), Fig. 2, §§8.3.5–6, 8.4 and 8.6.2, 8.6.10–13. Vendor: [TPS65186.cpp](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/features/TPS65186/TPS65186.cpp).

- Wait 5 ms after WAKEUP before I2C (TI minimum 1.8 ms).
- Check actual expander input levels as well as latches before wake, especially PWRUP. Initial standby inspection does not write PMIC registers.
- Sleep reloads factory defaults. Configure and read back the four sequencing registers on each actual power-up, retaining the factory-loaded delay bytes rather than forcing shorter delays. Keep the vendor active order e4/1b.
- Check PCAL input inversion and input latch registers before using pin readings (NXP PCAL6416A Rev. 7.1 §§7.4.3, 7.4.6); read physical Port 1 clock gate as well as its direction and latch. Check ENABLE status at each new frame, not only at startup.
- Ordinary and emergency bus shutdown share one path. Disable OE/GMOD before changing direct GPIOs and latch the bus before awaiting shutdown. I2S clock/data pads now remain exclusively owned by the board-specific transport, which can reset their GPIO matrix routing and drive them low before shutdown, then remove drive and pulls afterward. OE/GMOD/SPV are released only if the PMIC off status is verified; PMIC controls stay low.
- Confirm ENABLE state and physical controls before and after enabling VCOM, then enable OE. ENABLE and PG are status indicators, not voltage measurements.
- Power-down waits for both PG indicators, verifies HV/VCOM enable bits cleared before clearing V3P3 while awake, then requests sleep. Never write ENABLE after lowering WAKEUP. On failed PG or I2C do not directly clear individual HV enable bits; request sequenced shutdown via WAKEUP. No automatic restart.
- Mark shutdown faulted before asynchronous operations; cancellation cannot leave a reusable On state. This does not guarantee cleanup runs on cancellation.
- The explicit low-voltage preparation now caches and logs read-only factory VCOM, VADJ, revision and sequencer registers while the PMIC is awake. The default read-only firmware still never wakes the PMIC.
- Shutdown poll budget accounts for DFCTR and DDLY1–4, and waits for enable bits to clear as well as PG. Factory delay multiplier can extend the power-down interval past the vendor 250 ms timeout.
- VCOM_CTRL low is held for 1 ms before PWRUP falls, longer than the 62.52 us input deglitch. Reject VCOM2 acquisition/programming/Hi-Z flags before enabling the panel. Capture read-to-clear INT1/INT2 flags in RAM, reject supply/thermal faults before scanning, and attempt a final capture before sleep.
- A timed host fixture rejects premature PMIC accesses (<1.8 ms), holds PG low during a simulated 28 ms ramp, and holds HV enable status for 300 ms after PG drops. The chosen test times exercise ordering; they are not measured rail behavior. Added post-side-effect write failure injection and an async cancellation regression.
- Host mock rejects I2C in sleep, restores factory delay defaults, models control input levels and reports ENABLE status. Includes repeated cycles and mismatched PWRUP latch/input regression.

## Open limitations

HV stays locked. The maintainer reported successful read-only, single-cycle and 100/100 repeated low-voltage board tests; see [hardware validation](inkplate5v2-hardware-validation.md). The fake still does not model analogue ramp/discharge, real I2C elapsed timing, all PMIC faults or arbitrary cancellation. Physical verification of data/clock/panel high-Z release, HAL start-time timeout, recovery ownership remain open. PG=0 does not measure zero volts. A 50 ms post-sleep guard is conservative and not proof of electrical discharge. Software cannot ensure external expander shutdown after loss of the I2C bus or MCU reset.

The low-voltage preparation firmware exercises normal GPIO shutdown without requesting HV. It never sends DMA rows. The public HV lock remains unchanged. The I2S pin routing matches esp-hal 1.1 (WS clock); vendor BCK difference remains unverified. No custom HAL or unsafe pin stealing. If rail status is uncertain, direct pads are released but expander OE/GMOD/SPV are kept driven low as containment. Re-arm requires explicit low-voltage preparation and fresh off-state checks.

Each rearm now takes a fresh standby snapshot using the same wake/read/sleep checks as initialization. The 100-cycle diagnostic compares every snapshot against its first cycle; it does not compare cached readings. No PMIC register writes or HV requests are made by the test.

## Bounded polling and reset containment

The controller requires a monotonic `PowerDelay` and checks elapsed time, including I2C and scheduling overhead, in the startup and shutdown PG polling phases. A late successful startup observation cannot authorize VCOM/OE. The ESP32 diagnostic configurations use esp-hal's 20 ms software transaction timeout. The HAL also attempts bus clearing for up to 50 ms before a busy transaction and after timeout; this is not a hard end-to-end 20 ms bound. The generic I2C caller must configure a bounded transport; embedded-hal itself has no timeout contract. No HAL patch.

The V1.1.0 [MCU schematic](https://github.com/SolderedElectronics/Soldered-Inkplate-5-Gen2-hardware-design/blob/main/CAD/V1.1.0/MCU.kicad_sch) connects U10 PCAL RESET and U11 ESP32 EN to RST. External reset can release expander outputs; CPU-only/watchdog resets need not assert the external RST net. The earlier observed FF/FF directions after flashing are consistent with external reset, not proof of behavior during energized faults. Do not assume a watchdog turns the PMIC off. A stuck I2C bus, ROM boot mode, or power loss can prevent software recovery.

The explicit shutdown-only `recover_retained_power` API is for known configured expander outputs after reset, before configuring direct scan pins. It refuses unknown directions, never raises WAKEUP, uses the largest possible shutdown delay budget, requests OE/GMOD/SPV, VCOM, PWRUP and WAKEUP low and gates off the clock. Even success leaves the controller faulted. Default and low-voltage diagnostics still do not automatically recover or enable HV. This is not a guarantee of electrical containment; physical reset/discharge behavior remains unverified. A future active firmware must run appropriate boot containment before considering display output; independent hardware fail-off would be necessary for guaranteed protection while firmware cannot run.
