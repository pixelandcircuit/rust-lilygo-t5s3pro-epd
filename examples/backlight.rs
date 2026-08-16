#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    gpio::DriveMode,
    ledc::{
        channel::{self, ChannelIFace},
        timer::{self, TimerIFace},
        LSGlobalClkSource, Ledc, LowSpeed,
    },
    main,
    time::Rate,
};
use esp_println::println;

use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Gray4,
    prelude::*,
    primitives::{PrimitiveStyleBuilder, Rectangle, StrokeAlignment},
    text::{Alignment, Text},
};

use epaper::driver::{Display, DrawMode};

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

    let delay = Delay::new();

    // ── Display ───────────────────────────────────────────────────────────────
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

    // ── Draw static content ───────────────────────────────────────────────────
    display.clear().unwrap();

    Rectangle::new(Point::zero(), Size::new(960, 540))
        .into_styled(
            PrimitiveStyleBuilder::new()
                .stroke_color(Gray4::BLACK)
                .stroke_width(4)
                .stroke_alignment(StrokeAlignment::Inside)
                .build(),
        )
        .draw(&mut display)
        .unwrap();

    let style = MonoTextStyle::new(&FONT_10X20, Gray4::BLACK);

    Text::with_alignment(
        "Backlight Demo",
        Point::new(480, 230),
        style,
        Alignment::Center,
    )
    .draw(&mut display)
    .unwrap();

    Text::with_alignment(
        "Frontlight fades in, holds, then fades out",
        Point::new(480, 270),
        style,
        Alignment::Center,
    )
    .draw(&mut display)
    .unwrap();

    Text::with_alignment(
        "BOARD_BL_EN = GPIO11",
        Point::new(480, 310),
        style,
        Alignment::Center,
    )
    .draw(&mut display)
    .unwrap();

    display.flush(DrawMode::BlackOnWhite).unwrap();
    display.power_off();

    // ── LEDC backlight (1 kHz PWM, 8-bit duty) ───────────────────────────────
    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let mut lstimer0 = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    lstimer0
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty8Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_khz(1),
        })
        .unwrap();

    let mut channel0 = ledc.channel(channel::Number::Channel0, peripherals.GPIO11);
    channel0
        .configure(channel::config::Config {
            timer: &lstimer0,
            duty_pct: 0,
            drive_mode: DriveMode::PushPull,
        })
        .unwrap();

    println!("Backlight demo running on GPIO11 (BOARD_BL_EN)");

    // ── Fade loop ─────────────────────────────────────────────────────────────
    loop {
        // Fade in: 0% → 100% over ~2 s
        for pct in 0u8..=100 {
            channel0.set_duty(pct).unwrap();
            delay.delay_millis(20);
        }
        println!("backlight: full");
        delay.delay_millis(1_000);

        // Fade out: 100% → 0% over ~2 s
        for pct in (0u8..=100).rev() {
            channel0.set_duty(pct).unwrap();
            delay.delay_millis(20);
        }
        println!("backlight: off");
        delay.delay_millis(1_000);
    }
}
