# What Is This?

This repo has Embedded Rust examples and test code for the [**Lilygo T5 E-Paper S3 Pro**](https://lilygo.cc/en-us/products/t5-e-paper-s3-pro?srsltid=AfmBOoqgbUZPjZB0YhJQvo--ttNd97urgJmZNkQHwxGHqyUpBLoUz6wV) — an ESP32-S3 board with a 4.7" ED047TC1
e-paper display (960×540, 16 grayscale levels). It also includes a vibe coded epaper driver adapted from 
other rust epaper drivers.  This is *super* experimental. I've only just barely started testing it out. In particular
I'm trying to drive the display faster than normal, so it might cause fading or overdraw. Don't use it for
anything in production.

Lilygo T5 E-Paper S3 Pro

* [Product page](https://lilygo.cc/en-us/products/t5-e-paper-s3-pro)
* [official GitHub page](https://github.com/Xinyuan-LilyGO/T5S3-4.7-e-paper-PRO)

Everything below this line was written by Claude
-----------




## Hardware

| Component       | Detail                                                                               |
|-----------------|--------------------------------------------------------------------------------------|
| MCU             | ESP32-S3 (Xtensa LX7, 240 MHz)                                                       |
| Display         | ED047TC1, 960×540, 4-bit grayscale                                                   |
| Interface       | Parallel I8080 via ESP32-S3 LCD_CAM + DMA + RMT                                      |
| PMIC            | TPS65185 (I2C 0x68) — controls display voltage rails and VCOM                        |
| I/O expander    | PCA9555 (I2C 0x20, SDA=GPIO39, SCL=GPIO40) — controls OE/MODE/PWRUP/VCOM_CTRL/WAKEUP |
| PSRAM           | 8 MB OctalSPI — required for the 325 KB framebuffer                                  |
| Battery charger | BQ25896 (I2C) — single-cell LiPo charging with USB power-path                        |
| Fuel gauge      | BQ27220 (I2C) — state of charge, voltage, current, runtime estimate                  |
| Backlight       | GPIO11 (BOARD_BL_EN) — PWM-controllable frontlight                                   |

## Prerequisites

1. Install the Espressif Xtensa toolchain:
   ```
   cargo install espup
   espup install
   source ~/export-esp.sh
   ```
2. Install `espflash`:
   ```
   cargo install espflash
   ```

## Build and Flash

Connect the board via USB, then:

```
cargo run
```

This builds in dev mode and flashes via `espflash flash --monitor --chip esp32s3`.

To build release:

```
cargo build --release
espflash flash --chip esp32s3 target/xtensa-esp32s3-none-elf/release/epaper
```

## Running Examples

Flash and monitor an example with:

```
cargo run --example <name>
```

| Example          | Description                                                                                                  |
|------------------|--------------------------------------------------------------------------------------------------------------|
| `iris_demo`      | Iris UI demo: label + toggle button rendered with `iris-ui`; touch = tap, BOOT/GPIO38 = focus navigation     |
| `clock`          | Deep-sleep clock; draws HH:MM:SS, sleeps 10 s or wakes on BOOT press, redraws; set `INITIAL_HH/MM/SS` before flashing |
| `ebook`          | 3-page e-book demo; BOOT (GPIO0) = previous page, GPIO38 = next page; hold GPIO38 to cycle orientation       |
| `ereader_full`   | Full Moby Dick reader; TrueType antialiased text; header dropdown menus for backlight, font size, rotation, and battery stats; deep sleep after 60 s of inactivity; reading position persists across full power cycles via NVS flash |
| `graphics_test`  | 7-screen graphics test: shapes, typography, grayscale, images, animation, timing                             |
| `touch_button`   | Capacitive touch demo; tap the button to toggle fill, coordinates shown in status bar                        |
| `minesweeper`    | Touch game: 9x9 board, 10 mines, Reveal/Flag mode buttons, safe first reveal, empty-cell flood fill, and New Game |
| `backlight`      | Frontlight demo; fades the LED frontlight in and out using LEDC PWM on GPIO11                                |
| `finger_draw`    | Touch drawing demo; paint 16×16 px dots wherever your finger moves; partial-refresh timing printed to serial |
| `battery_status` | Dashboard showing live readings from the BQ27220 fuel gauge and BQ25896 charger; refreshes every 10 s        |
| `flash_demo`     | Minimal demo: detects hardware reset reason (power-on / deep-sleep / software-reset / WDT), loads a persistent boot counter from NVS flash (sequential-storage map), increments and saves it; shows `None` on first-ever flash use and `Some(n)` on subsequent boots |
| `gps`            | Enables the onboard GPS module via PCA9555 I/O expander, reads NMEA sentences over UART1 (GPIO44 RX / GPIO43 TX, 9600 baud), and prints `$GNGGA` location fixes in both NMEA DDMM and decimal-degree format; compatible with L76K and MIA-M10Q |
| `lora_rx`        | Puts the onboard SX1262 into continuous-receive mode and prints each received packet (hex dump + ASCII + RSSI/SNR) to serial; frequency, SF, BW, and CR are constants at the top of the file |
| `sd_list`        | Detects whether a micro-SD card is inserted (CS=GPIO12, SPI2), mounts the FAT filesystem via `embedded-sdmmc`, and recursively prints the full directory tree with file sizes; gracefully reports "no card detected" if the slot is empty |
| `partial_repaint_bench` | Benchmarks partial repaint speed at three rectangle sizes (50×30, 100×60, 200×120); alternates black-on-white / white-on-black fills 20× per size and reports min/max/avg round-trip ms and µs/pixel to serial |

**Example:**

```
cargo run --example touch_button
```

The `--monitor` flag is included automatically via `.cargo/config.toml`, so serial output appears in the terminal after
flashing.

### Minesweeper

Run `cargo run --example minesweeper`. Choose **Reveal** or **Flag**, then tap a
square. The selected mode button is black. In Flag mode, tap again to remove a
flag; flagged cells are protected from revealing. Cells show `#` for hidden,
`F` for flagged, `1`-`8` for adjacent mines, and a blank for revealed empty cells.
Mines (`*`) are shown when the game ends. Reveal all 71 safe cells to win.
The first reveal is always safe, and connected empty cells open automatically.
**New Game** resets the board and selects Reveal mode. Mine placement uses the
timing of the first reveal, so subsequent games get a new layout.

Single-row changes use partial refresh. Because repeated partial updates have
shown fading and darkening on hardware, changes spanning multiple board rows
use a full clear/redraw, and every sixth update also refreshes the full screen.
This maintenance refresh visibly flashes and takes longer; it is a workaround
for accumulated pixel drift, not a verified waveform or VCOM calibration fix.
`FULL_REFRESH_INTERVAL` in the example controls the interval (use at least 1).
Display power is switched off between updates.
See [Minesweeper refresh notes](docs/minesweeper-refresh.md) for driver changes,
hardware observations, and the manufacturer VCOM reference.

The game rules can be tested without hardware:

```sh
rustc +stable --edition=2021 --test examples/minesweeper/game.rs -o /tmp/minesweeper-tests
/tmp/minesweeper-tests
```

## LoRa (`lora_rx`)

The `lora_rx` example uses raw SPI commands against the onboard SX1262 — no external driver crate is needed. It
configures the chip for continuous receive and prints every packet it hears.

**Configuration** — edit the four constants at the top of `examples/lora_rx.rs`:

| Constant | Default | Notes |
|---|---|---|
| `FREQ_HZ` | `915_000_000` | 915 MHz (US/AU). Use `868_000_000` for EU. **Must match the transmitter.** |
| `SF` | `7` | Spreading factor 5–12. Higher SF = longer range, slower data rate. |
| `BW` | `0x04` | `0x04`=125 kHz · `0x05`=250 kHz · `0x06`=500 kHz |
| `CR` | `0x01` | Coding rate: `0x01`=4/5 · `0x02`=4/6 · `0x03`=4/7 · `0x04`=4/8 |

**Frequency is the most important setting** — a mismatch means no packets will be received at all. SF, BW, and CR
mismatches may still receive packets but will produce CRC errors.

The chip is put into **continuous RX mode** (`timeout=0xFFFFFF`), so it never returns to standby between packets.
DIO1 fires an interrupt after each packet; the code busy-polls the pin.

Low data-rate optimisation (LDRO) is automatically enabled when SF ≥ 11 with BW = 125 kHz, which is the only
combination where it is required.

## Simulator (no hardware required)

`iris_demo_sim` is a host-native variant of `iris_demo` that renders into an SDL2 window using
[`embedded-graphics-simulator`](https://crates.io/crates/embedded-graphics-simulator).  It is useful
for iterating on UI layouts without flashing the device.

**Prerequisites:** SDL2 must be installed.

```
brew install sdl2
```

**Run (Apple Silicon Mac):**

```
cargo run --example iris_demo_sim --features sim \
    --target aarch64-apple-darwin \
    --config 'unstable.build-std=["std"]'
```

The `--config` flag is required because `.cargo/config.toml` sets `build-std = ["alloc", "core"]`
for the bare-metal xtensa target; this overrides it so the host standard library is used instead.

**Controls:**

| Input              | Action                        |
|--------------------|-------------------------------|
| Mouse click        | Touch / tap                   |
| Left / Up arrow    | Move focus to previous widget |
| Right / Down arrow | Move focus to next widget     |
| Close window / Q   | Quit                          |

**How it works:** the `sim` feature enables `iris-ui/std` (which activates the SDL2 backend in
`iris-ui`) and `embedded-graphics-simulator`. ESP32-specific dependencies and the `src/driver`
module are gated behind `#[cfg(target_arch = "xtensa")]`, so the crate compiles cleanly for both
targets.

## Project Structure

```
src/
  main.rs              — demo: draws shapes and text with embedded-graphics
  driver/
    mod.rs             — public re-exports and pin_config! macro
    ed047tc1.rs        — low-level panel driver (I8080, RMT, I2C power management)
    rmt.rs             — RMT pulse helper for CKV row clock (GPIO48)
    display.rs         — framebuffer, waveform engine, flush/clear logic
    graphics.rs        — embedded-graphics DrawTarget<Color=Gray4> impl
```

## API

```rust
let mut display = Display::new(
pin_config!(peripherals),
peripherals.DMA_CH0,
peripherals.LCD_CAM,
peripherals.RMT,
peripherals.I2C0,
) ?;

display.power_on();
display.clear() ?;                          // hardware white clear cycle

// draw with embedded-graphics into the framebuffer …

display.flush(DrawMode::BlackOnWhite) ?;    // push framebuffer to panel
display.power_off();
```

Colors are `Gray4` from `embedded-graphics`. `Gray4::BLACK` (luma 0x0) = black;
`Gray4::WHITE` (luma 0xF) = white. The framebuffer starts white after each `flush`.

## Partial Refresh

`flush()` only sends rows that have been touched since the last flush — the driver tracks a per-row dirty bitmap (1 bit
per row, 68 bytes total). Any `set_pixel` call marks that row dirty; `flush()` sends exactly those rows through the full
15-frame waveform and skips the rest with a fast CKV clock pulse.

**What you can control**

- Any subset of the 540 rows, in any combination — including non-contiguous rows (e.g., row 10, row 200, and row 500 all
  in one flush)
- As few as 1 row or as many as all 540

**What you cannot control**

- Columns — a dirty row always sends all 960 pixels across that row; there is no column masking
- Sub-row granularity — touching any pixel in a row marks the entire row dirty

**Performance characteristics**
The 15-frame waveform runs in full regardless of how many rows are updated. Each frame iterates all 540 rows; dirty rows
get the full I8080 data transfer (~240 bytes), clean rows get only a CKV pulse (microseconds). Speedup is roughly
proportional to the fraction of rows updated, minus a fixed per-frame overhead. In practice, updating ~10% of rows (54
rows) takes roughly 10–15% of the time of a full flush.

## Waveform Engine & DrawMode

### How the waveform engine works

`flush()` drives the panel through 15 sequential frames. Each frame uses a 65 536-entry lookup table (LUT) indexed by a
`u16` value encoding 4 consecutive framebuffer pixels (4 × 4bpp = 16 bits). The LUT output is one byte containing four
2-bit waveform codes — one per pixel — that set the source-driver voltage for that pixel during the frame's CKV gate
pulse. The gate pulse duration comes from `contrast_cycles[k]`, which increases from ~8–30 µs in early frames up to 300
µs in the final frame.

The LUT starts at a uniform default (all pixels get the same 2-bit code) and is progressively modified across frames to
drive pixels of each brightness level for a calibrated number of frame-cycles, then switch them to VCOM (no drive). The
bistable nature of the e-paper panel holds each pixel at the voltage it was last actively driven to.

**2-bit waveform codes used in this driver:**

| Bits | Meaning                                                                        |
|------|--------------------------------------------------------------------------------|
| `01` | Source positive — drives particles toward the black electrode (darkens pixel)  |
| `10` | Source negative — drives particles toward the white electrode (lightens pixel) |
| `00` | VCOM — no drive; pixel holds its last electrically-set state                   |

### DrawMode semantics

Three modes are available, differing in their LUT default and update direction:

| Mode           | LUT default                    | Frame-k direction       | Best used when…                                                                                                                                                                                              |
|----------------|--------------------------------|-------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `BlackOnWhite` | `0x55` (all `01`, drive dark)  | k = 15−frame (high→low) | …the area is physically white; drives target-black pixels for all 15 frames, floats target-white pixels from frame 0 onward.                                                                                 |
| `WhiteOnBlack` | `0xAA` (all `10`, drive light) | k = frame (low→high)    | …you need to physically clear black pixels to white; drives target-white pixels (brightness 0xF) for all 15 frames (they're never cleared since k only reaches 14), floats target-black pixels from frame 0. |
| `WhiteOnWhite` | `0xAA` (all `10`, drive light) | k = 15−frame (high→low) | …same update order as `BlackOnWhite` but starting from drive-light; mainly used for full-panel resets.                                                                                                       |

**Critical constraint:** `BlackOnWhite` reliably renders white pixels as white only when those pixels are *already
physically white* on the panel. A pixel that is physically black receives only the frame-0 short pulse before being
floated — not enough to move the particles. If pixels may be physically black, always run a `WhiteOnBlack` clear pass
first.

### The two-pass pattern for reliable rendering

Any time a region may contain physically-black pixels that need to appear white in the new image (including white text
on a black fill, or clearing a filled button), use two passes:

```rust
// Pass 1 — reset region to known-white physical state.
// WhiteOnBlack with all-white framebuffer pixels drives 0xAA (source-negative)
// for the full 15 frames on every pixel, because brightness-0xF pixels are never
// cleared (k only reaches 14). This moves all particles to white regardless of
// their prior state.
Rectangle::new(origin, size)
.into_styled(PrimitiveStyle::with_fill(Gray4::WHITE))
.draw( & mut display) ?;
display.flush(DrawMode::WhiteOnBlack) ?;

// Pass 2 — render actual content onto the clean white canvas.
// BlackOnWhite drives black-target pixels for all 15 frames; white-target pixels
// float from their now-confirmed-white physical state and hold.
draw_your_content( & mut display);
display.flush(DrawMode::BlackOnWhite) ?;
```

This costs two flush passes but eliminates the need for `clear_area()`, which runs 32 full hardware frame scans (~4×
slower on a small region).

### Latency vs quality tradeoffs

The table below lists the levers available, from lowest-impact to most aggressive:

| Technique                                                               | Latency gain                         | Quality cost                                                         |
|-------------------------------------------------------------------------|--------------------------------------|----------------------------------------------------------------------|
| **Partial refresh** — draw only the rows you need                       | Large; proportional to row count     | None for untouched rows                                              |
| **Skip pass 1** when you know the area is already white                 | ~50 % off two-pass time              | Faded text / ghosting if area was not white                          |
| **Single `BlackOnWhite` pass only**                                     | ~50 % off two-pass time              | White-on-black text will appear faded or missing if previously black |
| **Reduce `DRAW_IMAGE_FRAME_COUNT`** (default 15, `display.rs:304`)      | Proportional to frames cut           | Reduced contrast, lighter blacks, more inter-frame ghosting          |
| **Shorten `CONTRAST_CYCLES_4BPP`** (default sum ≈ 1020, `display.rs:8`) | Proportional to cycle-time reduction | Incomplete particle drive; lighter blacks, grayer whites             |
| **Use `CONTRAST_CYCLES_4BPP_WHITE`** for `BlackOnWhite`                 | ~40 % (sum ≈ 280 vs 1020)            | Much weaker black drive; acceptable for text at larger font sizes    |
| **Accept ghosting** — skip `clear()` / `clear_area()` for updates       | Large for full-screen changes        | Ghost of previous image visible in unchanged regions                 |

**Practical guidance:**

- For a **clock or counter** updating a small region every second: use partial refresh + single `BlackOnWhite` pass. If
  the digits are always black on a pre-cleared white background, one pass is sufficient and ghosting is minimal.
- For a **toggle button or icon** that flips between states: the two-pass pattern is needed; partial refresh keeps it
  fast. 60 rows × 2 passes typically completes in under 300 ms.
- For **animations** at the cost of quality: reduce `DRAW_IMAGE_FRAME_COUNT` to 8–10 and cut the last two high-contrast
  entries from `CONTRAST_CYCLES_4BPP` (the 200 and 300 µs entries account for ~50 % of total frame time).
- For **full-page reflows** (e-reader page turn): a full `display.clear()` followed by a single `BlackOnWhite` flush is
  the cleanest approach; the clear dominates the time budget so the waveform cost is secondary.

## Font Rendering

The `ereader_full` example renders body text using `fontdue` — a pure-Rust TrueType rasterizer that
runs on the ESP32-S3 PSRAM heap. The font file is embedded in flash at compile time via `include_bytes!`.

### Placing the font file

The font lives at `fonts/<name>.ttf` in the project root. That directory is `.gitignore`'d, so you must
place the file there before building.

To use the default Georgia (already on macOS):

```
cp /System/Library/Fonts/Supplemental/Georgia.ttf fonts/
```

Or download a freely-licensed alternative such as Noto Serif:

```
curl -L "https://github.com/notofonts/noto-fonts/raw/main/hinted/ttf/NotoSerif/NotoSerif-Regular.ttf" \
     -o fonts/NotoSerif-Regular.ttf
```

### Switching fonts

Edit the one `include_bytes!` line in `src/font.rs`:

```rust
// Before
static FONT_DATA: &[u8] = include_bytes!("../fonts/Georgia.ttf");

// After (example: Noto Serif)
static FONT_DATA: &[u8] = include_bytes!("../fonts/NotoSerif-Regular.ttf");
```

Any TTF or OTF file that `fontdue` can parse will work. The font is loaded once in `main()` after the PSRAM
allocator is initialised; parsing takes a few milliseconds and the rasterized glyph cache lives in PSRAM.

### Header controls

The header bar is divided into 5 equal tap zones. Tapping a zone opens a dropdown panel directly below
the header. Tap an option to apply it; tap anywhere outside the panel to dismiss without changing anything.
BOOT and Next buttons also dismiss an open panel before paging.

| Zone (left → right) | Label   | Panel contents                                      |
|---------------------|---------|-----------------------------------------------------|
| 1 (leftmost)        | time    | not tappable — shows HH:MM from RTC                 |
| 2                   | battery | read-only: SoC %, charging status, voltage, current, capacity |
| 3                   | BL:xxx  | backlight level — Off / Low / Med / Hi              |
| 4                   | Sz:xxx  | font size — Sm / Md / Lg / XL                       |
| 5 (rightmost)       | Rot:xxx | orientation — Landscape / Portrait / Inverted / CCW |

All selections survive deep sleep (stored in RTC STORE5).

### Persistent reading position

The current reading position (byte offset into the text) survives full power-off and reflash via the NVS flash partition. On the next boot the reader reopens to the same page and briefly shows "Resumed" in the footer status bar.

| State                | Persistence mechanism                       |
|----------------------|---------------------------------------------|
| Reading position     | NVS flash (`0x9000..0xF000`, sequential-storage map) |
| Backlight / font / orientation | RTC STORE registers (deep sleep only) |

The NVS partition is declared in `partitions.csv`. Position is saved on every page turn; a failed flash write is logged and silently ignored — position is still correct in RAM for the current session.

### Changing font size

**At runtime:** tap the "Sz:" zone (zone 4) to open the font size dropdown, then select Sm / Md / Lg / XL.
The selection survives deep sleep and is restored on wakeup.

**Adding or adjusting sizes:** edit the two arrays near the top of `examples/ereader_full.rs`:

```rust
const FONT_SIZES:  [(f32, f32); 4] = [(15.0, 13.0), (18.0, 16.0), (22.0, 20.0), (28.0, 26.0)];
const FONT_LABELS: [&str; 4]       = ["Sm", "Md", "Lg", "XL"];
```

Each entry is `(landscape_px, portrait_px)`. Add or remove entries freely — line count and
word-wrap are derived automatically. The RTC register stores the index in 2 bits, so the
array can hold up to 4 entries without any other changes.

## Key Implementation Notes

- **Pixel bit ordering**: the ED047TC1 reads the parallel bus MSB-first — bits 6–7 of each output byte are the leftmost
  pixel in a 4-pixel group. The LUT converts 4×4bpp pixels to one byte, then a 2-bit-pair reversal (
  `display.rs: prepare_dma_buffer`) corrects the ordering. Solid fills are unaffected (0x55/0xAA are palindromes under
  this transform), which is why `clear()` works correctly without this fix.
- **PSRAM allocator**: must be initialized before `Display::new()`.
- **Waveform**: 15-frame grayscale waveform via a 65536-entry LUT; supports `BlackOnWhite`, `WhiteOnWhite`, and
  `WhiteOnBlack` draw modes.

## License

The `src/driver/` module is derived from [lilygo-epd47](https://crates.io/crates/lilygo-epd47) (GPL-3.0).
