#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;

use esp_backtrace as _;
use esp_hal::rtc_cntl::{
    reset_reason,
    sleep::{Ext0WakeupSource, TimerWakeupSource, WakeupLevel},
    wakeup_cause, Rtc, SocResetReason,
};
use esp_hal::{
    main,
    system::{Cpu, SleepSource},
};
use esp_println::println;

use embedded_graphics::{
    mono_font::{
        ascii::{FONT_10X20, FONT_7X13},
        MonoTextStyle,
    },
    pixelcolor::Gray4,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};

use epaper::driver::{Display, DrawMode};

esp_bootloader_esp_idf::esp_app_desc!();

// ── User configuration ────────────────────────────────────────────────────────
// Set these to the current local time before flashing.
// The clock will drift by about 1 second per 10-second sleep cycle due to
// the ~1 s spent drawing and flushing; adjust SLEEP_SECS if needed.
const INITIAL_HH: u64 = 12;
const INITIAL_MM: u64 = 0;
const INITIAL_SS: u64 = 0;

const SLEEP_SECS: u64 = 10;

// ── Drawing ───────────────────────────────────────────────────────────────────

fn draw_clock(display: &mut Display, time_str: &str, status: &str) {
    let heading = MonoTextStyle::new(&FONT_10X20, Gray4::BLACK);
    let small = MonoTextStyle::new(&FONT_7X13, Gray4::BLACK);
    let border2 = PrimitiveStyle::with_stroke(Gray4::BLACK, 2);
    let border8 = PrimitiveStyle::with_stroke(Gray4::BLACK, 8);
    let rule = PrimitiveStyle::with_stroke(Gray4::BLACK, 1);

    // Outer border
    Rectangle::new(Point::new(16, 16), Size::new(928, 508))
        .into_styled(border2)
        .draw(display)
        .unwrap();

    // Title
    Text::with_alignment(
        "E-Paper Clock",
        Point::new(480, 60),
        heading,
        Alignment::Center,
    )
    .draw(display)
    .unwrap();

    // Divider below title
    Line::new(Point::new(80, 80), Point::new(880, 80))
        .into_styled(rule)
        .draw(display)
        .unwrap();

    // Large time box — 600×120 px centred on screen
    Rectangle::new(Point::new(180, 210), Size::new(600, 120))
        .into_styled(border8)
        .draw(display)
        .unwrap();

    // Time string centred inside the box (baseline at y=285 = box centre + ~10)
    Text::with_alignment(time_str, Point::new(480, 282), heading, Alignment::Center)
        .draw(display)
        .unwrap();

    // Status lines at bottom
    Text::with_alignment(status, Point::new(480, 390), small, Alignment::Center)
        .draw(display)
        .unwrap();

    let hint = format!(
        "Sleeping {} s between updates  \u{2022}  Edit INITIAL_HH/MM/SS to set time",
        SLEEP_SECS,
    );
    Text::with_alignment(&hint, Point::new(480, 420), small, Alignment::Center)
        .draw(display)
        .unwrap();

    Text::with_alignment(
        "Press RESET to re-initialise to the compile-time start time.",
        Point::new(480, 450),
        small,
        Alignment::Center,
    )
    .draw(display)
    .unwrap();
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::_240MHz);
    let peripherals = esp_hal::init(config);

    let psram_config = esp_hal::psram::PsramConfig {
        mode: esp_hal::psram::PsramMode::OctalSpi,
        ..Default::default()
    };
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram, psram_config);

    // Take GPIO0 here, before Display::new() consumes peripherals via pin_config!.
    // GPIO0 (BOOT button) is RTC-capable and used as the Ext0 deep-sleep wakeup pin.
    let gpio0 = peripherals.GPIO0;

    let mut rtc = Rtc::new(peripherals.LPWR);

    // Detect whether this boot is a wakeup from deep sleep or a fresh reset.
    let is_deep_sleep_wakeup = reset_reason(Cpu::ProCpu) == Some(SocResetReason::CoreDeepSleep);
    let is_first_boot = !is_deep_sleep_wakeup;

    let status_str = if is_first_boot {
        "First boot — time initialised"
    } else {
        match wakeup_cause() {
            SleepSource::Ext0 => "Woke: BOOT button pressed",
            SleepSource::Timer => "Woke: timer (10 s)",
            _ => "Woke from deep sleep",
        }
    };

    if is_first_boot {
        // Seed the RTC clock. The stored offset in STORE2/STORE3 survives
        // deep sleep, so subsequent wakeups just read current_time_us().
        let initial_us = (INITIAL_HH * 3600 + INITIAL_MM * 60 + INITIAL_SS) * 1_000_000;
        rtc.set_current_time_us(initial_us);
        println!(
            "clock: first boot — time set to {:02}:{:02}:{:02}",
            INITIAL_HH, INITIAL_MM, INITIAL_SS
        );
    }

    // Read current time from the RTC (accurate across deep-sleep cycles).
    let total_secs = (rtc.current_time_us() / 1_000_000) as u32;
    let hh = (total_secs / 3600) % 24;
    let mm = (total_secs / 60) % 60;
    let ss = total_secs % 60;
    let time_str = format!("{:02}:{:02}:{:02}", hh, mm, ss);

    println!(
        "clock: {} | {} — sleeping {}s",
        time_str, status_str, SLEEP_SECS
    );

    // ── Display ───────────────────────────────────────────────────────────────
    let mut display = Display::new(
        epaper::pin_config!(peripherals),
        peripherals.DMA_CH0,
        peripherals.LCD_CAM,
        peripherals.RMT,
        peripherals.I2C0,
    )
    .expect("display init");

    display.power_on();
    display.clear().unwrap();
    draw_clock(&mut display, &time_str, status_str);
    display.flush(DrawMode::BlackOnWhite).unwrap();
    display.power_off(); // e-paper retains image with no power

    // ── Deep sleep ────────────────────────────────────────────────────────────
    // Wakes on either: timer expiry OR BOOT button (GPIO0, active-low).
    let timer = TimerWakeupSource::new(core::time::Duration::from_secs(SLEEP_SECS));
    let boot = Ext0WakeupSource::new(gpio0, WakeupLevel::Low);
    rtc.sleep_deep(&[&timer, &boot]); // → ! chip reboots on wakeup
}
