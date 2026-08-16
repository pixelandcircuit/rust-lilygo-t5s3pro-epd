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
    primitives::{PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};

use epaper::driver::gt911::GT911_ADDR_PRIMARY;
use epaper::driver::{Display, DrawMode, Gt911};

esp_bootloader_esp_idf::esp_app_desc!();

const BTN_W: u32 = 200;
const BTN_H: u32 = 60;
const BTN_X: i32 = (960 - BTN_W as i32) / 2; // centered horizontally
const BTN_Y: i32 = (540 - BTN_H as i32) / 2; // centered vertically
const BTN_X2: i32 = BTN_X + BTN_W as i32;
const BTN_Y2: i32 = BTN_Y + BTN_H as i32;

// ── Drawing helpers ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum ButtonState {
    Empty,
    Filled,
}

fn draw_button(display: &mut Display, state: ButtonState) {
    let (fill, stroke, text_color) = match state {
        ButtonState::Empty => (None, Some(Gray4::BLACK), Gray4::BLACK),
        ButtonState::Filled => (Some(Gray4::BLACK), None, Gray4::WHITE),
    };

    let mut style = PrimitiveStyle::default();
    if let Some(c) = fill {
        style = PrimitiveStyle::with_fill(c);
    }
    if let Some(c) = stroke {
        style = PrimitiveStyle::with_stroke(c, 3);
    }

    Rectangle::new(Point::new(BTN_X, BTN_Y), Size::new(BTN_W, BTN_H))
        .into_styled(style)
        .draw(display)
        .unwrap();

    Text::with_alignment(
        "TAP ME",
        Point::new(BTN_X + BTN_W as i32 / 2, BTN_Y + BTN_H as i32 / 2 + 7),
        MonoTextStyle::new(&FONT_10X20, text_color),
        Alignment::Center,
    )
    .draw(display)
    .unwrap();
}

fn within_button(x: u16, y: u16) -> bool {
    let xi = x as i32;
    let yi = y as i32;
    xi >= BTN_X && xi < BTN_X2 && yi >= BTN_Y && yi < BTN_Y2
}

// ── Main ──────────────────────────────────────────────────────────────────────

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

    // ── Detect GT911 ─────────────────────────────────────────────────────────
    let touch_addr = display.detect_touch_addr().unwrap_or_else(|| {
        println!(
            "WARNING: GT911 not detected — defaulting to 0x{:02X}",
            GT911_ADDR_PRIMARY
        );
        GT911_ADDR_PRIMARY
    });
    println!("GT911 at I2C 0x{:02X}", touch_addr);
    let mut gt911 = Gt911::new(touch_addr);

    // Read product ID — should be "911\0" (0x39 0x31 0x31 0x00) for a real GT911
    let pid = display.touch_product_id(&mut gt911);
    println!(
        "GT911 product ID: {:?} (\"{}{}{}\")",
        pid, pid[0] as char, pid[1] as char, pid[2] as char
    );

    // Write valid configuration block so the GT911 starts scanning.
    // version=0x00 means it was never programmed; without valid config the chip idles.
    display.configure_touch(&mut gt911, 960, 540);
    delay.delay_millis(200); // allow GT911 to reload config and start scanning
                             // Verify config readback.
    let cfg = display.touch_read_config(&mut gt911);
    let x_res = u16::from_le_bytes([cfg[1], cfg[2]]);
    let y_res = u16::from_le_bytes([cfg[3], cfg[4]]);
    println!(
        "GT911 config readback: x_res={} y_res={} max_touch={} int_mode=0x{:02X}",
        x_res, y_res, cfg[5], cfg[6]
    );
    // Clear stale buffer flag and set coordinate-output mode.
    display.init_touch(&mut gt911);

    println!("GT911 ready");

    // ── Initial render ────────────────────────────────────────────────────────
    display.clear().unwrap();

    draw_button(&mut display, ButtonState::Empty);
    display.flush(DrawMode::BlackOnWhite).unwrap();

    println!(
        "Ready. Button: x={}..{} y={}..{}",
        BTN_X, BTN_X2, BTN_Y, BTN_Y2
    );

    // ── Main loop ─────────────────────────────────────────────────────────────
    let mut state = ButtonState::Empty;
    let mut tap_count: u32 = 0;
    let mut last_flush_ms: u64 = 0;

    loop {
        if let Some((tx, ty)) = display.read_touch(&mut gt911) {
            println!("touch ({}, {})", tx, ty);

            if within_button(tx, ty) {
                state = match state {
                    ButtonState::Empty => ButtonState::Filled,
                    ButtonState::Filled => ButtonState::Empty,
                };
                tap_count += 1;

                let t0 = Instant::now();

                // Pass 1: drive the button area to a known-white physical state.
                // WhiteOnBlack with an all-white framebuffer keeps every pixel at
                // 0xAA (drive-white) for all 15 frames — strong enough to clear
                // particles that are physically stuck at black.
                Rectangle::new(Point::new(BTN_X, BTN_Y), Size::new(BTN_W, BTN_H))
                    .into_styled(PrimitiveStyle::with_fill(Gray4::WHITE))
                    .draw(&mut display)
                    .unwrap();
                display.flush(DrawMode::WhiteOnBlack).unwrap();

                // Pass 2: render actual button content onto the clean white canvas.
                // BlackOnWhite: black-target pixels are driven for all 15 frames;
                // white-target pixels float from their now-white physical state.
                draw_button(&mut display, state);
                display.flush(DrawMode::BlackOnWhite).unwrap();

                last_flush_ms = t0.elapsed().as_millis();

                println!("tap #{} flush {}ms", tap_count, last_flush_ms);
            }

            // Debounce: wait for lift
            loop {
                delay.delay_millis(20);
                if display.read_touch(&mut gt911).is_none() {
                    break;
                }
            }
        }

        delay.delay_millis(20);
    }
}
