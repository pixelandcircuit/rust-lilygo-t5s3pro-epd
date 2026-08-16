#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::{delay::Delay, main, time::Instant};
use esp_println::println;

use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Gray4,
    prelude::*,
    primitives::{PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, StrokeAlignment},
    text::{Alignment, Text},
};

use epaper::driver::gt911::GT911_ADDR_PRIMARY;
use epaper::driver::{Display, DrawMode, Gt911};

esp_bootloader_esp_idf::esp_app_desc!();

// Size of each painted dot in pixels (keep small to minimise dirty rows per stroke)
const DOT: u32 = 16;

// Top strip reserved for the title; drawing is clipped below it
const HEADER_H: i32 = 50;

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

    let delay = Delay::new();

    let mut display = Display::new(
        epaper::pin_config!(peripherals),
        peripherals.DMA_CH0,
        peripherals.LCD_CAM,
        peripherals.RMT,
        peripherals.I2C0,
    )
    .expect("display init");

    delay.delay_millis(100);
    display.power_on();
    delay.delay_millis(10);

    // ── Touch setup (same sequence as touch_button) ───────────────────────────
    let touch_addr = display.detect_touch_addr().unwrap_or_else(|| {
        println!(
            "WARNING: GT911 not detected — defaulting to 0x{:02X}",
            GT911_ADDR_PRIMARY
        );
        GT911_ADDR_PRIMARY
    });
    println!("GT911 at I2C 0x{:02X}", touch_addr);
    let mut gt911 = Gt911::new(touch_addr);

    let pid = display.touch_product_id(&mut gt911);
    println!(
        "GT911 product ID: \"{}{}{}\"",
        pid[0] as char, pid[1] as char, pid[2] as char
    );

    display.configure_touch(&mut gt911, 960, 540);
    delay.delay_millis(200);
    display.init_touch(&mut gt911);

    // ── Initial screen ────────────────────────────────────────────────────────
    display.clear().unwrap();

    // Outer border
    Rectangle::new(Point::zero(), Size::new(960, 540))
        .into_styled(
            PrimitiveStyleBuilder::new()
                .stroke_color(Gray4::BLACK)
                .stroke_width(3)
                .stroke_alignment(StrokeAlignment::Inside)
                .build(),
        )
        .draw(&mut display)
        .unwrap();

    // Header divider line
    Rectangle::new(Point::new(0, HEADER_H), Size::new(960, 2))
        .into_styled(PrimitiveStyle::with_fill(Gray4::BLACK))
        .draw(&mut display)
        .unwrap();

    Text::with_alignment(
        "Finger Draw — draw anywhere below this line",
        Point::new(480, HEADER_H / 2 + 7),
        MonoTextStyle::new(&FONT_10X20, Gray4::BLACK),
        Alignment::Center,
    )
    .draw(&mut display)
    .unwrap();

    display.flush(DrawMode::BlackOnWhite).unwrap();

    println!("Ready. Draw on the screen.");

    // ── Drawing loop ──────────────────────────────────────────────────────────
    // After each flush the framebuffer resets to white, so only the new dot's
    // rows are dirty on the next flush — giving fast partial refresh per stroke.
    let mut dot_count: u32 = 0;

    loop {
        if let Some((tx, ty)) = display.read_touch(&mut gt911) {
            // Clamp dot to the drawing area (below the header)
            let x = (tx as i32 - DOT as i32 / 2)
                .max(0)
                .min(Display::WIDTH as i32 - DOT as i32);
            let y = (ty as i32 - DOT as i32 / 2)
                .max(HEADER_H + 2)
                .min(Display::HEIGHT as i32 - DOT as i32);

            Rectangle::new(Point::new(x, y), Size::new(DOT, DOT))
                .into_styled(PrimitiveStyle::with_fill(Gray4::BLACK))
                .draw(&mut display)
                .unwrap();

            let t0 = Instant::now();
            display.flush(DrawMode::BlackOnWhite).unwrap();
            let ms = t0.elapsed().as_millis();

            dot_count += 1;
            println!("dot #{} ({}, {}) flush={}ms", dot_count, tx, ty, ms);
        }
    }
}
