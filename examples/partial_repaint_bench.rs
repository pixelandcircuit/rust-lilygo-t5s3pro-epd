//! Partial repaint benchmark — measures how long it takes to flip a filled
//! rectangle between black-on-white and white-on-black at three sizes.
//!
//! Each flip is a two-flush round-trip:
//!   1. flush_clip(WhiteOnBlack, rect) — erase previous content
//!   2. flush_clip(BlackOnWhite, rect) — render new content
//!
//! `flush_clip` confines the waveform to the rectangle by masking out-of-clip
//! pixels to VCOM (no drive) in the DMA buffer, so rows outside the rectangle
//! are unaffected and there are no ghost bands.
//!
//! Rectangle sizes: 50×30, 100×60, 200×120 (centred on screen)
//! Iterations per size: 20
//! Statistics reported: min / max / avg round-trip ms, avg µs per pixel
//!
//! Run: cargo run --example partial_repaint_bench

#![no_std]
#![no_main]

use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Gray4,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};
use epaper::driver::{Display, DrawMode, Rectangle as EpdRect}; // EpdRect avoids collision with embedded-graphics Rectangle
use esp_backtrace as _;
use esp_hal::{clock::CpuClock, time::Instant};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

const ITERS: usize = 20;
const SIZES: [(u32, u32); 3] = [(50, 30), (100, 60), (200, 120)];

fn draw_frame(display: &mut Display, rect: Rectangle, fill: Gray4, text_color: Gray4) {
    rect.into_styled(PrimitiveStyle::with_fill(fill))
        .draw(display)
        .unwrap();
    let c = rect.center();
    // FONT_10X20 baseline sits ~6px below the visual centre of the glyph box
    let pt = Point::new(c.x, c.y + 6);
    Text::with_alignment(
        "hello",
        pt,
        MonoTextStyle::new(&FONT_10X20, text_color),
        Alignment::Center,
    )
    .draw(display)
    .unwrap();
}

fn run_bench(display: &mut Display, w: u32, h: u32) {
    let x = (960u32.saturating_sub(w)) / 2;
    let y = (540u32.saturating_sub(h)) / 2;
    let rect = Rectangle::new(Point::new(x as i32, y as i32), Size::new(w, h));
    let clip = EpdRect {
        x: x as u16,
        y: y as u16,
        width: w as u16,
        height: h as u16,
    };
    let mut times = [0u64; ITERS];

    for i in 0..ITERS {
        let (fill, text_color) = if i % 2 == 0 {
            (Gray4::BLACK, Gray4::WHITE)
        } else {
            (Gray4::WHITE, Gray4::BLACK)
        };

        // Pre-draw marks the dirty rows; timer starts before first hardware pass.
        draw_frame(display, rect, fill, text_color);
        let t0 = Instant::now();
        display.flush_clip(DrawMode::WhiteOnBlack, clip).unwrap(); // erase pass (clip confines waveform to rect)
        draw_frame(display, rect, fill, text_color); // re-draw into fresh FB
        display.flush_clip(DrawMode::BlackOnWhite, clip).unwrap(); // render pass
        times[i] = t0.elapsed().as_millis();
    }

    let sum: u64 = times.iter().sum();
    let avg = sum / ITERS as u64;
    let min = times.iter().copied().min().unwrap_or(0);
    let max = times.iter().copied().max().unwrap_or(0);
    let pixels = w as u64 * h as u64;
    let us_per_pixel = avg * 1000 / pixels.max(1);

    println!(
        "[bench] {:4}x{:3}  min={:5}ms  max={:5}ms  avg={:5}ms  {:5}µs/pixel",
        w, h, min, max, avg, us_per_pixel
    );
}

#[esp_hal::main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::_240MHz);
    let peripherals = esp_hal::init(config);
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    let mut display = Display::new(
        epaper::pin_config!(peripherals),
        peripherals.DMA_CH0,
        peripherals.LCD_CAM,
        peripherals.RMT,
        peripherals.I2C0,
    )
    .expect("display init");

    display.power_on();

    let t0 = Instant::now();
    display.clear().unwrap();
    println!("[bench] clear: {} ms", t0.elapsed().as_millis());

    println!("[bench] {} iterations per size", ITERS);

    for &(w, h) in &SIZES {
        println!("[bench] {}x{} ...", w, h);
        run_bench(&mut display, w, h);
        display.clear().unwrap();
    }

    println!("[bench] done");
    display.power_off();
    loop {}
}
