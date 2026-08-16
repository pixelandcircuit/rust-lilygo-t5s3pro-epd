//! Iris UI demo — simulator variant.
//!
//! Mirrors iris_demo.rs but runs on the host using embedded-graphics-simulator.
//! Mouse click = touch; left/right arrow keys = focus navigation.
//!
//! Run (macOS Apple Silicon): cargo run --example iris_demo_sim --features sim \
//!                  --target aarch64-apple-darwin \
//!                  --config 'unstable.build-std=["std"]'
//!
//! The --config flag overrides the workspace's bare-metal build-std setting so
//! Cargo uses the host's pre-built standard library instead of core+alloc only.

use embedded_graphics::{
    geometry::Size,
    mono_font::ascii::{FONT_10X20, FONT_9X18_BOLD},
    pixelcolor::Rgb565,
    prelude::RgbColor,
};
use embedded_graphics_simulator::{
    OutputSettingsBuilder, SimulatorDisplay, SimulatorEvent, Window,
};
use iris_ui::{
    device::EmbeddedDrawingContext,
    geom::{Bounds, Point},
    input::{InputAction, InputEvent, OutputAction},
    label::make_label,
    layouts::layout_vbox,
    panel::make_panel,
    scene::{click_at, draw_scene, event_at_focused, layout_scene, Scene},
    toggle_button::make_toggle_button,
    view::ViewId,
    FontKind, Theme, ViewStyle,
};

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

const SCALE: u32 = 2;

fn build_scene() -> Scene {
    let mut scene = Scene::new_with_scale(
        Bounds::new(0, 0, (960 / SCALE) as i32, (540 / SCALE) as i32),
        SCALE,
    );
    let panel_id = ViewId::new("panel1");
    let panel = make_panel(&panel_id)
        .with_layout(Some(layout_vbox))
        .with_visible(true);
    scene.add_view_to_parent(make_label("l1", "The first label"), &panel_id);
    scene.add_view_to_parent(
        make_toggle_button(&ViewId::new("toggle1"), "Toggle"),
        &panel.name,
    );
    scene.add_view_to_root(panel);
    scene.mark_dirty_all();
    scene.mark_layout_dirty();
    scene
}

fn handle_action(action: Option<OutputAction>, scene: &mut Scene) {
    let text = match action {
        Some(OutputAction::Command(cmd)) => format!("Command: {}", cmd),
        Some(OutputAction::Selected(lbl, idx)) => format!("Selected: {} ({})", lbl, idx),
        Some(OutputAction::Focused(id)) => format!("Focused: {}", id.as_str()),
        _ => return,
    };
    println!("[iris] {}", text);
    scene.mark_dirty_all();
}

fn main() {
    let mut scene = build_scene();
    let mut display: SimulatorDisplay<Rgb565> = SimulatorDisplay::new(Size::new(960, 540));
    let output_settings = OutputSettingsBuilder::new().scale(1).build();
    let mut window = Window::new("Iris Demo (Simulator)", &output_settings);

    'running: loop {
        let mut ctx = EmbeddedDrawingContext::new_with_scale(&mut display, SCALE);
        ctx.clip = scene.dirty_rect.scaled(SCALE);
        layout_scene(&mut scene, &THEME);
        draw_scene(&mut scene, &mut ctx, &THEME);
        window.update(&display);

        for event in window.events() {
            match event {
                SimulatorEvent::Quit => break 'running,
                SimulatorEvent::MouseButtonUp { point, .. } => {
                    let pt = Point::new(point.x / SCALE as i32, point.y / SCALE as i32);
                    if let Some(result) = click_at(&mut scene, &vec![], pt) {
                        handle_action(result.action, &mut scene);
                    }
                }
                SimulatorEvent::KeyDown { keycode, .. } => {
                    use embedded_graphics_simulator::sdl2::Keycode;
                    let evt = match keycode {
                        Keycode::LEFT | Keycode::UP => InputEvent::Action(InputAction::FocusPrev),
                        Keycode::RIGHT | Keycode::DOWN => {
                            InputEvent::Action(InputAction::FocusNext)
                        }
                        _ => continue,
                    };
                    event_at_focused(&mut scene, &evt);
                    scene.mark_dirty_all();
                }
                _ => {}
            }
        }
    }
}
