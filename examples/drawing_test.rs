#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;

use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::primitives::{Line, Primitive, PrimitiveStyle};
use embedded_graphics::text::{Alignment, Text};
use embedded_graphics_core::Drawable;
use embedded_graphics_core::geometry::{Point, Size};
use embedded_graphics_core::pixelcolor::{Gray4, GrayColor};
use embedded_graphics_core::primitives::Rectangle;
use esp_hal::delay::Delay;
use esp_hal::main;
use esp_hal::time::Instant;
use epaper::driver::{Display, DrawMode};
use micromath::F32Ext;

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
        epaper::pin_config!(peripherals),
        peripherals.DMA_CH0,
        peripherals.LCD_CAM,
        peripherals.RMT,
        peripherals.I2C0,
    )
        .expect("display init");

    let delay = Delay::new();
    delay.delay_millis(100);
    display.power_on();
    delay.delay_millis(10);

    esp_println::println!("Graphics test — 6 screens. Press BOOT (GPIO0) to advance.");

    // Measure hardware clear time once at start
    esp_println::println!("Measuring hardware clear...");
    let t0 = Instant::now();
    display.clear().unwrap();
    let initial_clear_ms = t0.elapsed().as_millis();
    esp_println::println!("Hardware clear: {}ms", initial_clear_ms);

    {
        let large = MonoTextStyle::new(&FONT_10X20, Gray4::BLACK);
        // Rectangle::new(Point::new(16, 16), Size::new(928, 508))
        //     .into_styled(PrimitiveStyle::with_stroke(Gray4::BLACK, 2))
        //     .draw(&mut display)
        //     .unwrap();
        //
        // Text::with_alignment(
        //     "Graphics Test",
        //     Point::new(480, 80),
        //     large,
        //     Alignment::Center,
        // )
        //     .draw(&mut display)
        //     .unwrap();

        let gs = PrimitiveStyle::with_stroke(Gray4::new(12), 1);
        let chunk = 100;
        for j in 0..1000 {
            for i in 0..chunk {
                let t = i + j*chunk;
                let step = 2;
                let theta = (t * step) as f32 / 100.0;
                let theta2 = ((t + 1) * step) as f32 / 100.0;
                let pt1 = fun(theta);
                let pt2 = fun(theta2);
                let scalex = 450f32;
                let scaley = 250f32;
                let tx = 485.0;
                let ty = 275.0;
                Line::new(
                    Point::new(((pt1.0 * scalex) + tx) as i32, ((pt1.1 * scaley) + ty) as i32),
                    Point::new(((pt2.0 * scalex) + tx) as i32, ((pt2.1 * scaley) + ty) as i32))
                    .into_styled(gs)
                    .draw(&mut display)
                    .unwrap();
            }
            display.flush(DrawMode::BlackOnWhite).unwrap();
        }

        // Line::new(Point::new(50, 50), Point::new(50, 700))
        //     .into_styled(gs)
        //     .draw(&mut display)
        //     .unwrap();
        //
        esp_println::println!("Flushing title...");
        display.flush(DrawMode::BlackOnWhite).unwrap();
    }



    loop {}
}

fn fun(theta: f32) -> (f32, f32) {
    ((theta*2.051f32).sin(), (theta*1.1f32).cos())
}
