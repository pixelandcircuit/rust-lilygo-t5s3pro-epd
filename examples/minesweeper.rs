//! Touch Minesweeper: `cargo run --example minesweeper`.
#![no_std]
#![no_main]

extern crate alloc;

#[path = "minesweeper/game.rs"]
mod game;

use alloc::format;
use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Gray4,
    prelude::*,
    primitives::{PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, StrokeAlignment},
    text::{Alignment, Text},
};
use epaper::driver::{Display, DrawMode, Gt911, Rectangle as ClipRectangle};
use esp_backtrace as _;
use esp_hal::{delay::Delay, main, time::Instant};
use game::{Game, State, CELLS, MINES, SIDE};

esp_bootloader_esp_idf::esp_app_desc!();

const CELL_SIZE: i32 = 48;
const BOARD_X: i32 = 264;
const BOARD_Y: i32 = 96;
const BUTTON_Y: i32 = 28;
const BUTTON_W: u32 = 136;
const BUTTON_H: u32 = 40;
const REVEAL_X: i32 = 264;
const FLAG_X: i32 = 412;
const NEW_X: i32 = 560;
// Conservative maintenance interval while partial updates exhibit panel drift.
const FULL_REFRESH_INTERVAL: u8 = 6;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Reveal,
    Flag,
}

fn label(display: &mut Display, text: &str, x: i32, baseline: i32, color: Gray4) {
    Text::with_alignment(
        text,
        Point::new(x, baseline),
        MonoTextStyle::new(&FONT_10X20, color),
        Alignment::Center,
    )
    .draw(display)
    .unwrap();
}

fn button_bounds(x: i32) -> Rectangle {
    Rectangle::new(Point::new(x, BUTTON_Y), Size::new(BUTTON_W, BUTTON_H))
}

fn draw_button(display: &mut Display, x: i32, text: &str, selected: bool) {
    button_bounds(x)
        .into_styled(
            PrimitiveStyleBuilder::new()
                .fill_color(if selected { Gray4::BLACK } else { Gray4::WHITE })
                .stroke_color(Gray4::BLACK)
                .stroke_width(2)
                .stroke_alignment(StrokeAlignment::Inside)
                .build(),
        )
        .draw(display)
        .unwrap();
    label(
        display,
        text,
        x + BUTTON_W as i32 / 2,
        BUTTON_Y + 26,
        if selected { Gray4::WHITE } else { Gray4::BLACK },
    );
}

fn draw_header(display: &mut Display, game: &Game, mode: Mode) {
    label(display, "MINESWEEPER", 480, 19, Gray4::BLACK);
    draw_button(display, REVEAL_X, "Reveal", mode == Mode::Reveal);
    draw_button(display, FLAG_X, "Flag", mode == Mode::Flag);
    draw_button(display, NEW_X, "New Game", false);
    draw_status(display, game, mode);
}

fn draw(display: &mut Display, game: &Game, mode: Mode) {
    draw_header(display, game, mode);
    for cell in 0..CELLS {
        draw_cell(display, game, cell);
    }
}

fn draw_status(display: &mut Display, game: &Game, mode: Mode) {
    let message = match game.state {
        State::Ready => "First reveal is safe",
        State::Playing => match mode {
            Mode::Reveal => "Tap to reveal",
            Mode::Flag => "Tap to flag/unflag",
        },
        State::Won => "You win!",
        State::Lost => "Mine hit! Game over",
    };
    label(
        display,
        &format!("{} | Mines: {}  Flags: {}", message, MINES, game.flags()),
        480,
        88,
        Gray4::BLACK,
    );
}

fn draw_cell(display: &mut Display, game: &Game, cell: usize) {
    let x = BOARD_X + (cell % SIDE) as i32 * CELL_SIZE;
    let y = BOARD_Y + (cell / SIDE) as i32 * CELL_SIZE;
    Rectangle::new(
        Point::new(x, y),
        Size::new(CELL_SIZE as u32, CELL_SIZE as u32),
    )
    .into_styled(
        PrimitiveStyleBuilder::new()
            .stroke_color(Gray4::BLACK)
            .stroke_width(1)
            .stroke_alignment(StrokeAlignment::Inside)
            .build(),
    )
    .draw(display)
    .unwrap();
    let ascii = [game.symbol(cell)];
    label(
        display,
        core::str::from_utf8(&ascii).unwrap(),
        x + CELL_SIZE / 2,
        y + CELL_SIZE / 2 + 7,
        Gray4::BLACK,
    );
}

fn repaint_region(display: &mut Display, area: Rectangle, draw_content: impl FnOnce(&mut Display)) {
    let clip = ClipRectangle {
        x: area.top_left.x as u16,
        y: area.top_left.y as u16,
        width: area.size.width as u16,
        height: area.size.height as u16,
    };
    // Drawing only this rectangle marks only its rows dirty. Column masking
    // suppresses drive codes outside the rectangle; the panel can still
    // exhibit cumulative fading outside it during larger updates.
    area.into_styled(PrimitiveStyle::with_fill(Gray4::WHITE))
        .draw(display)
        .unwrap();
    display.flush_clip(DrawMode::WhiteOnBlack, clip).unwrap();
    draw_content(display);
    display.flush_clip(DrawMode::BlackOnWhite, clip).unwrap();
}

struct Painted {
    symbols: [u8; CELLS],
    state: State,
    flags: usize,
    mode: Mode,
}

impl Painted {
    fn new(game: &Game, mode: Mode) -> Self {
        Self {
            symbols: core::array::from_fn(|cell| game.symbol(cell)),
            state: game.state,
            flags: game.flags(),
            mode,
        }
    }
}

fn repaint(
    display: &mut Display,
    game: &Game,
    mode: Mode,
    previous: &Painted,
    partial_updates: &mut u8,
) {
    let start = Instant::now();
    let mut changed = 0;
    let (mut min_row, mut min_col, mut max_row, mut max_col) = (SIDE, SIDE, 0, 0);
    for cell in 0..CELLS {
        if previous.symbols[cell] != game.symbol(cell) {
            changed += 1;
            min_row = min_row.min(cell / SIDE);
            max_row = max_row.max(cell / SIDE);
            min_col = min_col.min(cell % SIDE);
            max_col = max_col.max(cell % SIDE);
        }
    }
    display.power_on();
    Delay::new().delay_millis(10);
    let full_refresh =
        (changed != 0 && min_row != max_row) || *partial_updates >= FULL_REFRESH_INTERVAL - 1;
    if full_refresh {
        // Restore ALL pixels, including unchanged grid cells which drift
        // during partial updates. Large changes always use this path.
        display.clear().unwrap();
        display.fill(15).unwrap();
        draw(display, game, mode);
        display.flush(DrawMode::BlackOnWhite).unwrap();
        display.power_off();
        *partial_updates = 0;
        esp_println::println!(
            "Full refresh: {} changed cells, {} ms",
            changed,
            start.elapsed().as_millis()
        );
        return;
    }
    if changed != 0 {
        // Batch flood fills and game-over reveals in one bounding rectangle
        // instead of running a separate 15-frame waveform for every cell.
        let area = Rectangle::new(
            Point::new(
                BOARD_X + min_col as i32 * CELL_SIZE,
                BOARD_Y + min_row as i32 * CELL_SIZE,
            ),
            Size::new(
                (max_col - min_col + 1) as u32 * CELL_SIZE as u32,
                (max_row - min_row + 1) as u32 * CELL_SIZE as u32,
            ),
        );
        repaint_region(display, area, |display| {
            // The erase pass clears the whole rectangle, including unchanged
            // cells between the changed ones, so restore those cells too.
            for row in min_row..=max_row {
                for col in min_col..=max_col {
                    draw_cell(display, game, row * SIDE + col);
                }
            }
        });
    }
    if previous.mode != mode {
        let area = Rectangle::new(
            Point::new(REVEAL_X, BUTTON_Y),
            Size::new((FLAG_X - REVEAL_X) as u32 + BUTTON_W, BUTTON_H),
        );
        repaint_region(display, area, |display| {
            draw_button(display, REVEAL_X, "Reveal", mode == Mode::Reveal);
            draw_button(display, FLAG_X, "Flag", mode == Mode::Flag);
        });
    }
    if previous.state != game.state
        || previous.flags != game.flags()
        || (previous.mode != mode && game.state == State::Playing)
    {
        repaint_region(
            display,
            Rectangle::new(Point::new(0, 69), Size::new(960, 25)),
            |display| draw_status(display, game, mode),
        );
    }
    display.power_off();
    *partial_updates += 1;
    esp_println::println!(
        "Partial refresh {}/{}: {} changed cells, {} ms",
        *partial_updates,
        FULL_REFRESH_INTERVAL,
        changed,
        start.elapsed().as_millis()
    );
}

fn tap(game: &mut Game, mode: &mut Mode, point: Point) -> bool {
    if button_bounds(NEW_X).contains(point) {
        *game = Game::new();
        *mode = Mode::Reveal;
        return true;
    }
    for (x, selection) in [(REVEAL_X, Mode::Reveal), (FLAG_X, Mode::Flag)] {
        if button_bounds(x).contains(point) {
            let changed = *mode != selection;
            *mode = selection;
            return changed;
        }
    }
    let board = Rectangle::new(
        Point::new(BOARD_X, BOARD_Y),
        Size::new(
            SIDE as u32 * CELL_SIZE as u32,
            SIDE as u32 * CELL_SIZE as u32,
        ),
    );
    if !board.contains(point) {
        return false;
    }
    let col = (point.x - BOARD_X) as usize / CELL_SIZE as usize;
    let row = (point.y - BOARD_Y) as usize / CELL_SIZE as usize;
    let cell = row * SIDE + col;
    match mode {
        Mode::Reveal => game.reveal(cell, Instant::now().duration_since_epoch().as_micros()),
        Mode::Flag => game.toggle_flag(cell),
    }
}

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

    let touch_addr = display
        .detect_touch_addr()
        .expect("GT911 touch controller not found");
    let mut touch = Gt911::new(touch_addr);
    display.configure_touch(&mut touch, 960, 540);
    delay.delay_millis(200);
    display.init_touch(&mut touch);

    let mut game = Game::new();
    let mut mode = Mode::Reveal;
    display.clear().unwrap();
    draw(&mut display, &game, mode);
    display.flush(DrawMode::BlackOnWhite).unwrap();
    display.power_off();
    esp_println::println!("Minesweeper ready: 9x9, 10 mines");

    let mut partial_updates = 0;
    let mut finger_down = false;
    loop {
        // No fresh report is different from a release. Only an explicit
        // ready report with zero contacts re-arms the next tap.
        let status = display.touch_read_status_raw(&mut touch);
        if status & 0x80 != 0 {
            if status & 0x0f == 0 {
                display.touch_clear_status(&mut touch);
                finger_down = false;
            } else if let Some((x, y)) = display.read_touch(&mut touch) {
                if !finger_down {
                    finger_down = true;
                    let previous = Painted::new(&game, mode);
                    if tap(&mut game, &mut mode, Point::new(x as i32, y as i32)) {
                        repaint(&mut display, &game, mode, &previous, &mut partial_updates);
                    }
                }
            }
        }
        delay.delay_millis(20);
    }
}
