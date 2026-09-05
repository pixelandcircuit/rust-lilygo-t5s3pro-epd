# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

Embedded Rust examples and driver for the **Lilygo T5 E-Paper S3 Pro** — an ESP32-S3 board with a 960×540 ED047TC1 e-paper display (4-bit grayscale). The toolchain targets `xtensa-esp32s3-none-elf` by default; `.cargo/config.toml` wires `cargo run` to flash via `espflash`.

## Commands

```bash
# Check for errors without linking (fast, no hardware needed)
cargo check
cargo check --example <name>

# Flash and monitor an example
cargo run --example <name>

# Build release binary (then flash manually)
cargo build --release
espflash flash --chip esp32s3 target/xtensa-esp32s3-none-elf/release/epaper

# Host simulator (no hardware, Apple Silicon Mac)
cargo run --example iris_demo_sim --features sim \
    --target aarch64-apple-darwin \
    --config 'unstable.build-std=["std"]'
# Requires: brew install sdl2
```

There is no test harness (`no_std` bare-metal). `cargo check` is the primary way to validate code.

## Architecture

### Crate layout

- `src/lib.rs` — crate root; conditionally `no_std` on xtensa, exposes `pub mod driver` only on xtensa
- `src/main.rs` — default binary (shapes + text demo)
- `src/driver/` — the epaper driver
- `examples/` — standalone examples, each a separate binary

### Driver modules (`src/driver/`)

| Module | Role |
|---|---|
| `mod.rs` | Re-exports, `Error`/`Result` types, `pin_config!` macro |
| `display.rs` | `Display<'a>` — framebuffer, dirty-row tracking, flush/clear, touch delegation |
| `ed047tc1.rs` | Low-level panel: I8080 parallel bus, DMA, RMT row clock, TPS65185 PMIC, PCA9555 I/O expander |
| `graphics.rs` | `DrawTarget<Color=Gray4>` impl for `Display` (routes through rotation) |
| `gt911.rs` | GT911 capacitive touch (polling, no INT pin); Y-axis inversion corrected in driver |
| `rmt.rs` | RMT pulse helper for CKV row clock on GPIO48 |

### Display lifecycle

Every example follows this sequence:

```rust
esp_bootloader_esp_idf::esp_app_desc!();   // required — omitting causes linker error

let peripherals = esp_hal::init(config);
esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);  // must come first

let gpio0 = peripherals.GPIO0;  // extract GPIOs you need BEFORE pin_config! moves them
let mut display = Display::new(
    epaper::pin_config!(peripherals),      // moves GPIO5-8, 15-18, 41-42, 45, 48, 39-40
    peripherals.DMA_CH0, peripherals.LCD_CAM, peripherals.RMT, peripherals.I2C0,
).expect("display init");

display.power_on();
display.clear().unwrap();     // hardware white-cycle; does NOT touch framebuffer

// draw via embedded-graphics (Gray4 color) …

display.flush(DrawMode::BlackOnWhite).unwrap();  // runs 15-frame waveform on dirty rows
display.power_off();
loop {}
```

### Partial refresh / dirty tracking

`flush()` only sends rows touched since the last flush. Any `set_pixel` call marks that row dirty; `flush()` processes dirty rows through the full 15-frame waveform and skips clean rows with a fast RMT pulse. The dirty bitmask is 68 bytes (1 bit per row).

### DrawMode

- `BlackOnWhite` — normal rendering (dark ink on light). Use after a `clear()` or a `WhiteOnBlack` pass.
- `WhiteOnBlack` — clearing pass. Use to reset physically-black pixels to white before re-rendering.
- When content changes over a non-white background, always double-flush: `WhiteOnBlack` then `BlackOnWhite`.

### iris-ui integration

`iris-ui` (`../iris-ui`) expects `DrawTarget<Color=Rgb565>`. Hardware examples wrap `Display` in an inline `Rgb565Adapter` that converts pixels via perceptual luma weights. The simulator skips this because `embedded-graphics-simulator` natively uses `Rgb565`.

Key iris-ui rendering pattern:
```rust
let SCALE: u32 = 2;   // renders at half resolution, then scaled up
let mut scene = Scene::new_with_scale(Bounds::new(0, 0, 960/SCALE, 540/SCALE), SCALE);
// add views …
scene.mark_dirty_all();
scene.mark_layout_dirty();

// in the render loop:
render(&mut display, &mut scene, SCALE);         // layout_scene + draw_scene
display.flush(DrawMode::WhiteOnBlack).unwrap();
scene.mark_dirty_all();
render(&mut display, &mut scene, SCALE);
display.flush(DrawMode::BlackOnWhite).unwrap();
```

### Dual-target (xtensa vs host simulator)

- All `esp-*` / `embassy-*` deps are under `[target.'cfg(target_arch = "xtensa")'.dependencies]`
- `src/lib.rs` gates `no_std`, `extern crate alloc`, and `pub mod driver` with `#[cfg(target_arch = "xtensa")]`
- `build.rs` gates `-Tlinkall.x` and `--error-handling-script` behind `CARGO_CFG_TARGET_ARCH == "xtensa"`
- The `sim` feature enables `iris-ui/std` + `embedded-graphics-simulator` for host builds
- Host builds pass `--config 'unstable.build-std=["std"]'` to override the workspace's bare-metal `build-std = ["alloc", "core"]`

### Async / WiFi examples

WiFi examples (`wifi_ntp`) use `#[esp_rtos::main]` instead of `#[esp_hal::main]` and add a heap allocator:
```rust
esp_alloc::heap_allocator!(size: 72 * 1024);
```
`WIFI_SSID` and `WIFI_PASS` must be set as environment variables at build time.
