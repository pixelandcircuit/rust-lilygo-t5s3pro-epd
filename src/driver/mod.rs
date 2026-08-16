extern crate alloc;

pub mod display;
pub(crate) mod ed047tc1;
pub mod graphics;
pub mod gt911;
pub(crate) mod rmt;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Error {
    Rmt(esp_hal::rmt::Error),
    RmtConfig(esp_hal::rmt::ConfigError),
    Dma(esp_hal::dma::DmaError),
    DmaBuffer(esp_hal::dma::DmaBufError),
    OutOfBounds,
    InvalidColor,
    Unknown,
}

pub type Result<T> = core::result::Result<T, Error>;

pub use crate::driver::{
    display::{Display, DrawMode, Rectangle},
    ed047tc1::PinConfig,
    gt911::Gt911,
};

/// Build a [`PinConfig`] for the T5 E-Paper S3 Pro (hardware V2.3 / ESP32-S3).
#[macro_export]
macro_rules! pin_config {
    ($($name:ident),*) => {
        $(
            #[allow(unused_mut)]
            $crate::driver::PinConfig {
                data0:   $name.GPIO5,
                data1:   $name.GPIO6,
                data2:   $name.GPIO7,
                data3:   $name.GPIO15,
                data4:   $name.GPIO16,
                data5:   $name.GPIO17,
                data6:   $name.GPIO18,
                data7:   $name.GPIO8,
                ckh:     $name.GPIO4,
                sth:     $name.GPIO41,
                leh:     $name.GPIO42,
                stv:     $name.GPIO45,
                ckv:     $name.GPIO48,
                i2c_sda: $name.GPIO39,
                i2c_scl: $name.GPIO40,
            }
        )*
    }
}
