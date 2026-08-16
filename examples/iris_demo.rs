//! Iris UI demo — buttons, labels, toggle group, and touch input on the EPD.
//!
//! Renders an iris-ui scene on the 960×540 display. Tapping buttons or toggle
//! options updates the status label; BOOT and GPIO38 cycle keyboard focus.
//!
//! Run: cargo run --example iris_demo

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{OriginDimensions, Size as EGSize},
    mono_font::ascii::{FONT_10X20, FONT_9X18_BOLD},
    pixelcolor::{Gray4, Rgb565, RgbColor},
    Pixel,
};
use epaper::driver::gt911::GT911_ADDR_PRIMARY;
use epaper::driver::{Display, DrawMode, Gt911, Rectangle};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Input, InputConfig, Pull},
};
use iris_ui::toggle_button::make_toggle_button;
use iris_ui::{
    button::{make_button, make_full_button},
    device::EmbeddedDrawingContext,
    geom::Bounds,
    input::{InputAction, InputEvent, OutputAction},
    label::{make_header_label, make_label},
    layouts::{layout_hbox, layout_std_panel, layout_vbox},
    panel::make_panel,
    scene::{click_at, draw_scene, event_at_focused, layout_scene, Scene},
    toggle_group::make_toggle_group,
    view::{Align, Flex, ViewId},
    FontKind, Theme, ViewStyle,
};

esp_bootloader_esp_idf::esp_app_desc!();

const THEME: Theme = Theme {
    font: FontKind::Bitmap(FONT_10X20),
    bold_font: FontKind::Bitmap(FONT_9X18_BOLD),
    standard: ViewStyle {
        fill: Rgb565::WHITE,
        text: Rgb565::BLACK,
    },
    accented: ViewStyle {
        fill: Rgb565::BLACK,
        text: Rgb565::WHITE,
    },
    selected: ViewStyle {
        fill: Rgb565::BLACK,
        text: Rgb565::WHITE,
    },
    panel: ViewStyle {
        fill: Rgb565::WHITE,
        text: Rgb565::BLACK,
    },
};

// ViewId constants for scene nodes we need to address later
const SPACER_TOP: ViewId = ViewId::new("spacer_top");
const SPACER_BOT: ViewId = ViewId::new("spacer_bot");
const CENTER_PANEL: ViewId = ViewId::new("center");
const BTN_ROW: ViewId = ViewId::new("btn_row");
const BTN1: ViewId = ViewId::new("btn1");
const BTN2: ViewId = ViewId::new("btn2");
const BTN3: ViewId = ViewId::new("btn3");
const TOGGLE: ViewId = ViewId::new("toggle");
const STATUS: ViewId = ViewId::new("status");

// ── Rgb565 → Gray4 adapter ────────────────────────────────────────────────────
// iris-ui's EmbeddedDrawingContext requires DrawTarget<Color = Rgb565>.
// This wrapper converts pixels on the fly so the EPD display can be used.

struct Rgb565Adapter<'a, 'd>(&'a mut Display<'d>);

impl<'a, 'd> DrawTarget for Rgb565Adapter<'a, 'd> {
    type Color = Rgb565;
    type Error = <Display<'d> as DrawTarget>::Error;

    fn draw_iter<I: IntoIterator<Item = Pixel<Rgb565>>>(
        &mut self,
        pixels: I,
    ) -> Result<(), Self::Error> {
        self.0.draw_iter(
            pixels
                .into_iter()
                .map(|Pixel(c, col)| Pixel(c, rgb565_to_gray4(col))),
        )
    }
}

impl<'a, 'd> OriginDimensions for Rgb565Adapter<'a, 'd> {
    fn size(&self) -> EGSize {
        self.0.size()
    }
}

fn rgb565_to_gray4(c: Rgb565) -> Gray4 {
    // Expand 5/6/5-bit channels to 0–255, then compute perceptual luminance.
    // BW_THEME only uses pure black/white so this degenerates to 0→0 and 255→15.
    let r = (c.r() as u32 * 255 + 15) / 31;
    let g = (c.g() as u32 * 255 + 31) / 63;
    let b = (c.b() as u32 * 255 + 15) / 31;
    let luma = (299 * r + 587 * g + 114 * b) / 1000;
    Gray4::new((luma * 15 / 255) as u8)
}

// ── Scene rendering ───────────────────────────────────────────────────────────

fn render(display: &mut Display, scene: &mut Scene, scale: u32) {
    // EmbeddedDrawingContext::new() initializes clip to Bounds::new_empty()
    // which has size {w:-99, h:-99}. bounds_to_rect casts w/h to u32, causing
    // "width is too large" panic. Must set ctx.clip to a valid region first.
    // draw_scene only runs when scene.dirty=true; mark_dirty_all/mark_dirty_view
    // set dirty_rect to valid non-negative bounds before we call render.
    let clip = scene.dirty_rect.scaled(scale);
    let mut adapter = Rgb565Adapter(display);
    // let mut ctx = EmbeddedDrawingContext::new(&mut adapter);
    let mut ctx = EmbeddedDrawingContext::new_with_scale(&mut adapter, scale);
    ctx.clip = clip;
    layout_scene(scene, &THEME);
    draw_scene(scene, &mut ctx, &THEME);
    // draw_scene resets scene.dirty_rect to new_empty() and scene.dirty to false
}

// ── Input action handler ──────────────────────────────────────────────────────

fn handle_action(action: Option<OutputAction>, scene: &mut Scene) {
    let text = match action {
        Some(OutputAction::Command(cmd)) => alloc::format!("Command: {}", cmd),
        Some(OutputAction::Selected(lbl, idx)) => alloc::format!("Selected: {} ({})", lbl, idx),
        Some(OutputAction::Focused(id)) => alloc::format!("Focused: {}", id.as_str()),
        _ => return,
    };
    esp_println::println!("[iris] {}", text);
    if let Some(v) = scene.get_view_mut(&STATUS) {
        v.title = text;
    }
    scene.mark_dirty_view(&STATUS);
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[esp_hal::main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    // Extract GPIO0 and GPIO38 before pin_config! consumes the EPD pins
    let gpio0 = peripherals.GPIO0;
    let gpio38 = peripherals.GPIO38;

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

    // Touch controller
    let touch_addr = display.detect_touch_addr().unwrap_or_else(|| {
        esp_println::println!(
            "[iris] GT911 not detected; defaulting to 0x{:02X}",
            GT911_ADDR_PRIMARY
        );
        GT911_ADDR_PRIMARY
    });
    let mut gt911 = Gt911::new(touch_addr);
    display.configure_touch(&mut gt911, 960, 540);
    delay.delay_millis(200);
    display.init_touch(&mut gt911);

    // Physical buttons (active-low with pull-up)
    let boot_btn = Input::new(gpio0, InputConfig::default().with_pull(Pull::Up));
    let next_btn = Input::new(gpio38, InputConfig::default().with_pull(Pull::Up));

    let SCALE: u32 = 2;
    // ── Build scene ───────────────────────────────────────────────────────────
    // let mut scene = Scene::new_with_bounds(Bounds::new(0, 0, 960, 540));
    let mut scene = Scene::new_with_scale(
        Bounds::new(0, 0, (960 / SCALE) as i32, (540 / SCALE) as i32),
        SCALE,
    );
    let panel1 = ViewId::new("panel1");
    let pan = make_panel(&panel1)
        .with_layout(Some(layout_vbox))
        .with_visible(true);
    let l1 = make_label("l1", "The first label");
    scene.add_view_to_parent(l1, &panel1);
    // let b1 = make_full_button(&ViewId::new("b1"), "The first button","toggle",false);
    // scene.add_view_to_parent(b1, &pan.name);
    let b2 = make_full_button(&ViewId::new("b2"), "The second button", "toggle2", false);
    // scene.add_view_to_parent(b2, &pan.name);
    //
    let t1 = make_toggle_button(&ViewId::new("toggle1"), "Toggle");
    scene.add_view_to_parent(t1, &pan.name);

    scene.add_view_to_root(pan);

    scene.mark_dirty_all();
    scene.mark_layout_dirty();

    // Initial two-flush to clear any ghost ink from previous display state.
    // draw_scene resets dirty_rect to new_empty() after the first pass, so
    // mark_dirty_all() re-enables drawing for the second pass.
    render(&mut display, &mut scene, SCALE);
    display.flush(DrawMode::WhiteOnBlack).unwrap();
    scene.mark_dirty_all();
    render(&mut display, &mut scene, SCALE);
    display.flush(DrawMode::BlackOnWhite).unwrap();

    // ── Main loop ─────────────────────────────────────────────────────────────
    let empty_handlers: Vec<iris_ui::Callback> = Vec::new();

    loop {
        let mut needs_flush = false;

        // Touch input → hit-test the scene, dispatch Tap event
        if let Some((tx, ty)) = display.read_touch(&mut gt911) {
            let pt = iris_ui::geom::Point::new(
                ((tx as u32) / SCALE) as i32,
                ((ty as u32) / SCALE) as i32,
            );
            esp_println::println!("[iris] touch {}", pt);

            if let Some(result) = click_at(&mut scene, &empty_handlers, pt) {
                handle_action(result.action, &mut scene);

                // Partial redraw: dirty_rect was set to just the clicked view's bounds
                // by the toggle button input handler. Use flush_clip to confine the
                // waveform to that region so the rest of the screen is not disturbed.
                let dirty = scene.dirty_rect;
                let scaled = dirty.scaled(SCALE);
                let clip_rect = Rectangle {
                    x: scaled.position.x.max(0) as u16,
                    y: scaled.position.y.max(0) as u16,
                    width: scaled.size.w.max(0) as u16,
                    height: scaled.size.h.max(0) as u16,
                };
                esp_println::println!("[iris] partial flush {:?}", clip_rect);
                render(&mut display, &mut scene, SCALE);
                display
                    .flush_clip(DrawMode::WhiteOnBlack, clip_rect)
                    .unwrap();
                // mark_dirty_all sets dirty=true; then override dirty_rect to restore
                // the precise clip for the second (BlackOnWhite) pass.
                scene.mark_dirty_all();
                scene.dirty_rect = dirty;
                render(&mut display, &mut scene, SCALE);
                display
                    .flush_clip(DrawMode::BlackOnWhite, clip_rect)
                    .unwrap();
            }

            // Wait for finger lift before continuing
            loop {
                delay.delay_millis(20);
                if display.read_touch(&mut gt911).is_none() {
                    break;
                }
            }
        }

        // BOOT button (GPIO0) — navigate to previous focusable element
        if boot_btn.is_low() {
            delay.delay_millis(50);
            while boot_btn.is_low() {}
            delay.delay_millis(50);
            event_at_focused(&mut scene, &InputEvent::Action(InputAction::FocusPrev));
            needs_flush = true;
        }

        // GPIO38 button — navigate to next focusable element
        if next_btn.is_low() {
            delay.delay_millis(50);
            while next_btn.is_low() {}
            delay.delay_millis(50);
            event_at_focused(&mut scene, &InputEvent::Action(InputAction::FocusNext));
            needs_flush = true;
        }

        if needs_flush {
            esp_println::println!("[iris] redrawing");
            scene.mark_dirty_all();
            render(&mut display, &mut scene, SCALE);
            display.flush(DrawMode::WhiteOnBlack).unwrap();
            scene.mark_dirty_all();
            render(&mut display, &mut scene, SCALE);
            display.flush(DrawMode::BlackOnWhite).unwrap();
        }
    }
}
