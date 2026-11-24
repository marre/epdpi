# TPS65186 power-state hardening

Source: [TI TPS65186 SLVSB04A](https://www.ti.com/lit/ds/symlink/tps65186.pdf), Fig. 2, §§8.3.5–6, 8.4 and 8.6.2, 8.6.10–13. Vendor: [TPS65186.cpp](https://github.com/SolderedElectronics/Inkplate-Arduino-library/blob/master/src/features/TPS65186/TPS65186.cpp).

- Wait 5 ms after WAKEUP before I2C (TI minimum 1.8 ms).
- Check actual expander input levels as well as latches before wake, especially PWRUP. Initial standby inspection does not write PMIC registers.
- Sleep reloads factory defaults. Configure and read back the four sequencing registers on each actual power-up, retaining the factory-loaded delay bytes rather than forcing shorter delays. Keep the vendor active order e4/1b.
- Check PCAL input inversion and input latch registers before using pin readings (NXP PCAL6416A Rev. 7.1 §§7.4.3, 7.4.6); read physical Port 1 clock gate as well as its direction and latch. Check ENABLE status at each new frame, not only at startup.
- Ordinary and emergency bus shutdown share one path. Disable OE/GMOD before changing direct GPIOs and latch the bus before awaiting shutdown. Complete I2S/CL/data high-Z still open.
- Confirm ENABLE state and physical controls before and after enabling VCOM, then enable OE. ENABLE and PG are status indicators, not voltage measurements.
- Power-down waits for both PG indicators, verifies HV/VCOM enable bits cleared before clearing V3P3 while awake, then requests sleep. Never write ENABLE after lowering WAKEUP. On failed PG or I2C do not directly clear individual HV enable bits; request sequenced shutdown via WAKEUP. No automatic restart.
- Mark shutdown faulted before asynchronous operations; cancellation cannot leave a reusable On state. This does not guarantee cleanup runs on cancellation.
- The explicit low-voltage preparation now caches and logs read-only factory VCOM, VADJ, revision and sequencer registers while the PMIC is awake. The default read-only firmware still never wakes the PMIC.
- Shutdown poll budget accounts for DFCTR and DDLY1–4, and waits for enable bits to clear as well as PG. Factory delay multiplier can extend the power-down interval past the vendor 250 ms timeout.
- Host mock rejects I2C in sleep, restores factory delay defaults, models control input levels and reports ENABLE status. Includes repeated cycles and mismatched PWRUP latch/input regression.

## Open limitations

HV stays locked. No physical board test. The fake still does not model analogue ramp/discharge, real elapsed timing, all PMIC faults or cancellation. Direct data/clock/panel high-Z release, HAL start-time timeout, fault capture and recovery ownership remain open. PG=0 does not measure zero volts. A 50 ms post-sleep guard is conservative and not proof of electrical discharge. Software cannot ensure external expander shutdown after loss of the I2C bus or MCU reset.
