#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Soldered Inkplate 5V2 panel data and generic refresh integration.

use epdpi::prelude::*;
use epdpi::waveform::WaveformEngine;

mod support;
use support::{BusEvent, RecordingBus};

type P = Ed052tc4;

// The original manufacturer table, at physical shades 0 through 7.
const SOURCE: [[u8; 9]; 8] = [
    [0, 0, 1, 1, 2, 1, 1, 1, 0],
    [1, 1, 2, 2, 1, 2, 1, 1, 0],
    [0, 1, 2, 2, 1, 1, 2, 1, 0],
    [0, 0, 1, 1, 1, 1, 1, 2, 0],
    [1, 2, 1, 2, 1, 1, 1, 2, 0],
    [0, 1, 1, 1, 2, 0, 1, 2, 0],
    [1, 1, 1, 2, 2, 2, 1, 2, 0],
    [0, 0, 0, 0, 0, 0, 0, 0, 0],
];

#[test]
fn factory_vcom_is_not_guessed_for_ed052tc4() {
    let mut prev = vec![0u8; P::FB_BYTES];
    let mut line = vec![0u8; P::SCANLINE_BYTES];
    let display = EpdDisplayBuilder::<P>::new()
        .build(RecordingBus::default(), &mut prev, &mut line)
        .unwrap();
    assert_eq!(display.vcom_mv(), None, "factory VCOM must not be fabricated");
}

#[test]
fn panel_geometry_and_grayscale_capabilities() {
    assert_eq!(P::WIDTH, 1280);
    assert_eq!(P::HEIGHT, 720);
    assert_eq!(P::COLOR_MODE, ColorMode::Grayscale8);
    assert_eq!(P::VCOM_MV, None);
    assert_eq!(P::FB_STRIDE_BYTES, 640);
    assert_eq!(P::FB_BYTES, 460_800);
    assert_eq!(P::SCANLINE_PIXEL_BYTES, 320);
    assert_eq!(P::SCANLINE_BYTES, 336);
    assert_eq!(P::PASSES, 9);
}

#[test]
fn all_waveform_phases_match_the_manufacturer_table() {
    assert_eq!(ED052TC4_LUT.len(), 16 * 9);
    for (logical, row) in ED052TC4_LUT.chunks_exact(9).enumerate() {
        assert_eq!(row, SOURCE[logical / 2], "logical gray {logical}");
        assert!(row.iter().all(|&code| code <= DriveCode::Lighten as u8));
    }
}

#[test]
fn gray4_input_maps_pairs_to_eight_physical_shades() {
    // Adjacent logical levels map to the same physical shade.
    // First 16 pixels: 0..15, left pixel in the high nibble.
    let mut row = [0xFFu8; P::FB_STRIDE_BYTES];
    for (i, b) in row[..8].iter_mut().enumerate() {
        *b = ((2 * i as u8) << 4) | (2 * i as u8 + 1);
    }
    let mut line = [0xCCu8; P::SCANLINE_BYTES];
    for pass in 0..P::PASSES {
        WaveformEngine::gray_scanline::<P>(&row, pass, &mut line);
        for shade in 0..8 {
            let c = SOURCE[shade][pass];
            let expected = c * 0x55;
            // Four input pixels (two adjacent logical shade pairs) per byte.
            // The first two pixels of each physical shade occupy one packed byte.
            let first = shade / 2;
            if shade % 2 == 0 {
                assert_eq!(line[first] >> 4, expected >> 4, "phase {pass} shade {shade}");
            } else {
                assert_eq!(line[first] & 0x0F, expected & 0x0F, "phase {pass} shade {shade}");
            }
        }
        assert!(line[P::SCANLINE_PIXEL_BYTES..].iter().all(|&x| x == 0));
    }
}

#[test]
fn uses_the_manufacturer_clean_and_idle_codes() {
    let expected = [
        (DriveCode::Lighten, 1),
        (DriveCode::Darken, 11),
        (DriveCode::Hold, 1),
        (DriveCode::Lighten, 11),
        (DriveCode::Hold, 1),
        (DriveCode::Darken, 11),
        (DriveCode::Hold, 1),
        (DriveCode::Lighten, 11),
    ];
    assert_eq!(P::CLEAR_SEQUENCE, &expected);
    assert_eq!(P::CLEAR_SEQUENCE.iter().map(|&(_, n)| n).sum::<usize>(), 48);
    assert_eq!(P::POST_REFRESH_CODE, DriveCode::Skip);
}

#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
async fn full_refresh_uses_panel_specific_clear_and_gray_lut_body() {
    let mut prev = vec![0xFFu8; P::FB_BYTES];
    let mut line = vec![0; P::SCANLINE_BYTES];
    let mut fbdata = vec![0; P::FB_BYTES]; // all black (physical shade 0)
    let fb = EpdFrameBuffer::<P>::new(&mut fbdata).unwrap();
    let mut d = EpdDisplayBuilder::<P>::new()
        .build(RecordingBus::default(), &mut prev, &mut line)
        .unwrap();
    d.refresh(&fb, RefreshMode::Quality16).await.unwrap();
    let bus = d.release();

    const ROWS: usize = P::HEIGHT as usize;
    const CLEAR_FRAMES: usize = 48;
    const TOTAL_FRAMES: usize = CLEAR_FRAMES + 9 + 1;
    assert_eq!(bus.count(|e| *e == BusEvent::StartFrame), TOTAL_FRAMES);
    assert_eq!(bus.events.first(), Some(&BusEvent::Power(true)));
    assert_eq!(bus.events.last(), Some(&BusEvent::Power(false)));
    assert_eq!(bus.lines.len(), TOTAL_FRAMES * ROWS);

    let mut phase = 0;
    for &(code, repeats) in P::CLEAR_SEQUENCE {
        for _ in 0..repeats {
            let first = &bus.lines[phase * ROWS];
            assert!(first[..320].iter().all(|&b| b == (code as u8) * 0x55));
            assert!(first[320..].iter().all(|&b| b == 0));
            phase += 1;
        }
    }
    assert_eq!(phase, CLEAR_FRAMES);
    for pass in 0..9 {
        assert!(bus.lines[(CLEAR_FRAMES + pass) * ROWS][..320]
            .iter()
            .all(|&b| b == SOURCE[0][pass] * 0x55));
    }
    assert!(bus.lines[(TOTAL_FRAMES - 1) * ROWS][..320]
        .iter()
        .all(|&b| b == 0xFF));
    drop(bus);
    assert!(prev.iter().all(|&b| b == 0));
}

epd_test!(
    full_refresh_uses_panel_specific_clear_and_gray_lut,
    full_refresh_uses_panel_specific_clear_and_gray_lut_body
);


#[maybe_async_cfg::maybe(
    sync(feature = "blocking", keep_self),
    async(not(feature = "blocking"), keep_self)
)]
async fn unsupported_ed052tc4_modes_fail_before_power_or_record_changes_body() {
    const UNSUPPORTED: [RefreshMode; 5] = [
        RefreshMode::M5Quality,
        RefreshMode::M5Text,
        RefreshMode::M5Fast,
        RefreshMode::M5Fastest,
        RefreshMode::FastMono,
    ];
    assert!(RefreshMode::Quality16.supported_for::<P>());
    assert!(RefreshMode::Clear.supported_for::<P>());
    for mode in UNSUPPORTED {
        assert!(!mode.supported_for::<P>(), "{mode:?}");
        let mut prev = vec![0xA5; P::FB_BYTES];
        let mut line = vec![0x77; P::SCANLINE_BYTES];
        let mut fbmem = vec![0; P::FB_BYTES];
        let mut fb = EpdFrameBuffer::<P>::new(&mut fbmem).unwrap();
        let mut d = EpdDisplayBuilder::<P>::new()
            .build(RecordingBus::default(), &mut prev, &mut line)
            .unwrap();
        assert!(matches!(
            d.refresh(&fb, mode).await,
            Err(RefreshError::UnsupportedMode(failed)) if failed == mode
        ));
        assert!(matches!(
            d.refresh_rect(&fb, mode, Rect::new(10, 10, 40, 40)).await,
            Err(RefreshError::UnsupportedMode(failed)) if failed == mode
        ));
        assert!(matches!(
            d.refresh_dirty(&mut fb, mode).await,
            Err(RefreshError::UnsupportedMode(failed)) if failed == mode
        ));
        assert_eq!(d.release().events.len(), 0, "{mode:?}: bus must stay idle");
        assert!(prev.iter().all(|&b| b == 0xA5), "{mode:?}: previous image unchanged");
        assert!(fb.dirty().is_some(), "{mode:?}: dirty region must survive failed refresh");
    }
}
epd_test!(
    unsupported_ed052tc4_modes_fail_before_power_or_record_changes,
    unsupported_ed052tc4_modes_fail_before_power_or_record_changes_body
);
