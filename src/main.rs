#![no_std]
#![no_main]

extern crate alloc;

mod driver;

use esp_backtrace as _;
use esp_hal::{delay::Delay, main};
use esp_println::println;

use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Gray4,
    prelude::*,
    primitives::{
        Circle, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, StrokeAlignment, Triangle,
    },
    text::{Alignment, Text},
};

use crate::driver::{Display, DrawMode};

esp_bootloader_esp_idf::esp_app_desc!();

#[main]
fn main() -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::_240MHz);
    let peripherals = esp_hal::init(config);

    let psram_config = esp_hal::psram::PsramConfig {
        mode: esp_hal::psram::PsramMode::OctalSpi,
        ..Default::default()
    };
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram, psram_config);

    let mut display = Display::new(
        pin_config!(peripherals),
        peripherals.DMA_CH0,
        peripherals.LCD_CAM,
        peripherals.RMT,
        peripherals.I2C0,
    )
    .expect("to initialize display");

    let delay = Delay::new();
    delay.delay_millis(100);
    display.power_on();
    delay.delay_millis(10);

    // Hardware clear to white background
    display.clear().unwrap();

    println!("drawing shapes...");

    // Border around the whole screen
    Rectangle::new(Point::zero(), Size::new(960, 540))
        .into_styled(
            PrimitiveStyleBuilder::new()
                .stroke_color(Gray4::BLACK)
                .stroke_width(6)
                .stroke_alignment(StrokeAlignment::Inside)
                .build(),
        )
        .draw(&mut display)
        .unwrap();

    // Filled circle (left third)
    Circle::new(Point::new(80, 160), 180)
        .into_styled(PrimitiveStyle::with_fill(Gray4::BLACK))
        .draw(&mut display)
        .unwrap();

    // Stroked rectangle (center third)
    Rectangle::new(Point::new(370, 160), Size::new(220, 180))
        .into_styled(PrimitiveStyle::with_stroke(Gray4::BLACK, 6))
        .draw(&mut display)
        .unwrap();

    // Stroked triangle (right third)
    Triangle::new(
        Point::new(700, 340),
        Point::new(820, 160),
        Point::new(940, 340),
    )
    .into_styled(PrimitiveStyle::with_stroke(Gray4::BLACK, 6))
    .draw(&mut display)
    .unwrap();

    // Centred title text
    let large = MonoTextStyle::new(&FONT_10X20, Gray4::BLACK);
    Text::with_alignment(
        "Hello from embedded Rust!",
        Point::new(480, 430),
        large,
        Alignment::Center,
    )
    .draw(&mut display)
    .unwrap();

    // Smaller subtitle
    Text::with_alignment(
        "Lilygo T5 E-Paper S3 Pro  |  960 x 540  |  16 grey",
        Point::new(480, 460),
        large,
        Alignment::Center,
    )
    .draw(&mut display)
    .unwrap();

    println!("flushing...");
    display.flush(DrawMode::BlackOnWhite).unwrap();

    println!("done.");
    display.power_off();

    loop {}
}
