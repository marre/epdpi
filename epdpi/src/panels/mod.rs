//! Supported panels, one file per glass.

mod ed047tc1;
mod ed052tc4;

pub use ed047tc1::{
    ED047TC1_LUT, Ed047tc1, M5_ERASER_LUT, M5_FAST_LUT, M5_FASTEST_LUT, M5_QUALITY_LUT, M5_TEXT_LUT,
};
pub use ed052tc4::{ED052TC4_LUT, Ed052tc4};
