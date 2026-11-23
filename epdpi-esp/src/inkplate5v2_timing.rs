//! Host-testable control signal timing for Soldered Inkplate 5 Gen 2.
//!
//! Reference: Inkplate5V2Driver.cpp::vscan_start, vscan_end and
//! UtilI2S.cpp::sendDataI2S. The intended caller must keep OE inactive
//! until TPS65186 PGOOD is verified, and deassert OE when any operation
//! fails. This is a **signal sequencing primitive**, not a board driver.
//!
//! Nominal microsecond delays in this module do not include PCAL6416A
//! I2C transaction time, HAL overhead, or the DMA transfer. Only a logic
//! analyzer can establish whether the physical edges match the reference.

/// The four panel controls involved in frame and row scan sequencing.
/// CKV, SPH and LE are ESP32 direct GPIO; SPV is on PCAL6416A Port 0.
#[allow(async_fn_in_trait)]
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
pub trait Inkplate5v2Signals {
    /// GPIO or I2C failure.
    type Error;
    /// Source latch signal, directly connected to ESP32 GPIO2.
    async fn set_le(&mut self, high: bool) -> Result<(), Self::Error>;
    /// Gate clock signal, directly connected to ESP32 GPIO32.
    async fn set_ckv(&mut self, high: bool) -> Result<(), Self::Error>;
    /// Source start signal, directly connected to ESP32 GPIO33.
    async fn set_sph(&mut self, high: bool) -> Result<(), Self::Error>;
    /// Gate start signal, expander Port 0 bit 2.
    async fn set_spv(&mut self, high: bool) -> Result<(), Self::Error>;
    /// Delay in microseconds. Actual elapsed time also includes bus operations.
    async fn delay_us(&mut self, us: u32);
}

/// Drive the direct pins to the idle levels used in Soldered's `einkOn()`.
/// This does not energize rails or configure the I2S clock/data pins.
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
pub async fn idle_before_power<S: Inkplate5v2Signals>(
    pins: &mut S,
) -> Result<(), S::Error> {
    pins.set_le(false).await?;
    pins.set_sph(true).await?;
    pins.set_ckv(false).await?;
    Ok(())
}

/// The vendor's vertical scan start. Call this once per pass, **only
/// after** the board driver has independently verified panel power.
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
pub async fn frame_start<S: Inkplate5v2Signals>(
    pins: &mut S,
) -> Result<(), S::Error> {
    pins.set_ckv(true).await?;
    pins.delay_us(7).await;
    pins.set_spv(false).await?;
    pins.delay_us(10).await;
    pins.set_ckv(false).await?;
    pins.set_ckv(true).await?;
    pins.delay_us(8).await;
    pins.set_spv(true).await?;
    pins.delay_us(10).await;
    pins.set_ckv(false).await?;
    pins.set_ckv(true).await?;
    pins.delay_us(18).await;
    pins.set_ckv(false).await?;
    pins.set_ckv(true).await?;
    pins.delay_us(18).await;
    pins.set_ckv(false).await?;
    pins.set_ckv(true).await
}

/// Direct controls immediately before starting the I2S DMA transfer.
/// The caller must start DMA only **after** SPH low and CKV high.
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
pub async fn row_start<S: Inkplate5v2Signals>(
    pins: &mut S,
) -> Result<(), S::Error> {
    pins.set_sph(false).await?;
    pins.set_ckv(true).await
}

/// Direct controls immediately after DMA has finished. Mirrors
/// `sendDataI2S()` SPH high followed by `vscan_end()` CKV low, LE pulse.
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
pub async fn row_end<S: Inkplate5v2Signals>(
    pins: &mut S,
) -> Result<(), S::Error> {
    pins.set_sph(true).await?;
    pins.set_ckv(false).await?;
    pins.set_le(true).await?;
    pins.set_le(false).await
}

/// Vendor inter-pass settle delay following the final row latch.
#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
pub async fn frame_end<S: Inkplate5v2Signals>(pins: &mut S) {
    pins.delay_us(230).await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        Le(bool),
        Ckv(bool),
        Sph(bool),
        Spv(bool),
        Wait(u32),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Fault;

    #[derive(Default)]
    struct Fake {
        events: Vec<Event>,
        fail_on_spv_low: bool,
    }

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    impl Inkplate5v2Signals for Fake {
        type Error = Fault;
        async fn set_le(&mut self, high: bool) -> Result<(), Fault> {
            self.events.push(Event::Le(high)); Ok(())
        }
        async fn set_ckv(&mut self, high: bool) -> Result<(), Fault> {
            self.events.push(Event::Ckv(high)); Ok(())
        }
        async fn set_sph(&mut self, high: bool) -> Result<(), Fault> {
            self.events.push(Event::Sph(high)); Ok(())
        }
        async fn set_spv(&mut self, high: bool) -> Result<(), Fault> {
            self.events.push(Event::Spv(high));
            if !high && self.fail_on_spv_low { return Err(Fault); }
            Ok(())
        }
        async fn delay_us(&mut self, us: u32) {
            self.events.push(Event::Wait(us));
        }
    }

    macro_rules! both_modes {
        ($name:ident, $body:ident) => {
            #[test]
            fn $name() {
                #[cfg(feature = "blocking")]
                { $body(); }
                #[cfg(not(feature = "blocking"))]
                { pollster::block_on($body()); }
            }
        };
    }

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn vendor_frame_and_row_edges_body() {
        let mut fake = Fake::default();
        idle_before_power(&mut fake).await.unwrap();
        frame_start(&mut fake).await.unwrap();
        row_start(&mut fake).await.unwrap();
        // I2S DMA transfer occurs here, between row_start and row_end.
        row_end(&mut fake).await.unwrap();
        frame_end(&mut fake).await;
        assert_eq!(fake.events, vec![
            Event::Le(false), Event::Sph(true), Event::Ckv(false),
            Event::Ckv(true), Event::Wait(7), Event::Spv(false),
            Event::Wait(10), Event::Ckv(false), Event::Ckv(true),
            Event::Wait(8), Event::Spv(true), Event::Wait(10),
            Event::Ckv(false), Event::Ckv(true), Event::Wait(18),
            Event::Ckv(false), Event::Ckv(true), Event::Wait(18),
            Event::Ckv(false), Event::Ckv(true),
            Event::Sph(false), Event::Ckv(true),
            Event::Sph(true), Event::Ckv(false), Event::Le(true),
            Event::Le(false), Event::Wait(230),
        ]);
    }
    both_modes!(vendor_frame_and_row_edges, vendor_frame_and_row_edges_body);

    #[maybe_async_cfg::maybe(
        sync(feature = "blocking", keep_self),
        async(not(feature = "blocking"), keep_self)
    )]
    async fn expander_failure_stops_frame_sequence_body() {
        let mut fake = Fake { fail_on_spv_low: true, ..Fake::default() };
        assert_eq!(frame_start(&mut fake).await, Err(Fault));
        assert_eq!(fake.events, vec![
            Event::Ckv(true), Event::Wait(7), Event::Spv(false)
        ]);
    }
    both_modes!(expander_failure_stops_frame_sequence, expander_failure_stops_frame_sequence_body);
}
