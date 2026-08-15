## 2026-08-15 15:00

**Updated: `inspect_demo` — two-pass erase before each screen refresh**

- Added `WhiteOnBlack` erase pass before each 30-second render cycle so
  previously-dark pixels (old QR modules, text) are actively driven to white
  before the new frame is drawn. Without this the QR code ghosted on top of
  the previous display state.
- Pattern: `fill(0xF)` + `flush(WhiteOnBlack)` → `render_status()` + `flush(BlackOnWhite)`.

---

## 2026-08-15 14:00

**Updated: `inspect_demo` — QR code for the debug URL on the e-paper display**

- Added `render_qr(display, url)` using `qrcode-core 2.0` (no_std + alloc, zero dependencies).
- When connected, the display shows a scannable QR code centered below the status text,
  pointing at `http://<device-ip>:3000/`. Scale 8 px/module with 4-module quiet zone.
- Added `qrcode-core = { version = "2", optional = true }` to Cargo.toml, gated under
  the existing `debug-inspect` feature — no cost when the feature is disabled.
- Build: `cargo run --example inspect_demo --features debug-inspect`

---

## 2026-08-15 13:00

**Updated: `inspect_demo` — enforce debug infrastructure isolation rules**

Three correctness rules enforced:

1. **Debug infrastructure must not own or globally lock AppState.**
   - `build_schema` was previously called while holding `CriticalSectionRawMutex`
     (which disables all interrupts), performing recursive allocation inside the
     critical section. Fixed by building the schema from `AppState::default()` —
     schema is purely structural (`'static` field metadata), so no lock is needed.

2. **State must be inspected through snapshots, not repeated lock acquisitions.**
   - The 2-second diff loop was taking one `state.lock()` per leaf field, causing
     N separate interrupt-disabled windows per tick. Fixed by taking a single
     `state.lock(|c| c.borrow().clone())` snapshot, then doing all comparison and
     WebSocket I/O outside the lock with interrupts re-enabled.
   - Added `#[derive(Clone)]` to all five AppState structs to support this.

3. **The debugger must be optional and removable with a Cargo feature.**
   - Added `debug-inspect` feature to `Cargo.toml` that gates all debug deps
     (`embassy-sync`, `embedded-inspect`, `embedded-websocket`, `httparse`) and
     activates `embassy-net/tcp`. The `tcp` feature is no longer unconditional.
   - Added `required-features = ["debug-inspect"]` to the `inspect_demo` example
     entry so `cargo build` without the feature flag will not compile it.
   - Build: `cargo run --example inspect_demo --features debug-inspect`

---

## 2026-08-15 12:00

**Added: `inspect_demo` example — embedded-inspect WebSocket debug server on ESP32-S3**

Integrates the `embedded-inspect` workspace (milestones 1–4) into a new ESP32-S3 example that runs a live debug WebSocket server alongside the display.

### New files
- **`examples/inspect_demo.rs`** — full example binary (no_std, xtensa-esp32s3-none-elf).
- **`examples/assets/inspect_index.html`** — copy of the browser debug UI from `embedded-inspect-demo`; connects to `ws://<device-ip>:3000/debug`.

### Architecture
- **Embassy task structure**: `main` owns `Display`; `connection` and `net_task` drive WiFi/TCP; `debug_server` is a separate Embassy task that never blocks display rendering.
- **Shared state**: `AppState` wrapped in `Mutex<CriticalSectionRawMutex, RefCell<AppState>>` in a `StaticCell`; lock always released before every `.await`.
- **AppState fields** (all read-only in this milestone):
  - `NetworkState` — `connected`, `ip_a/b/c/d`, `rssi`
  - `DisplayInfo` — `refresh_count`, `power_on`
  - `SystemInfo` — `uptime_secs`, `free_heap`
  - `ContentState` — `current_page`, `touch_x`, `touch_y`
- **Protocol**: JSON text WebSocket frames so the existing browser UI works unmodified. Manual `format!()` for output, manual string scanning for input (avoids `serde_json_core` internally-tagged enum limitation).
- **WebSocket**: `embedded-websocket 0.9` for no_std handshake + frame codec; `httparse` for HTTP Upgrade header parsing.
- **Push events**: every 2 s the debug task diffs all leaf values against a snapshot and sends `ValueChanged` frames to the browser.
- **HTTP**: `GET /` serves `inspect_index.html`; WebSocket Upgrade runs the debug session on port 3000.

### Cargo.toml additions
- `embedded-inspect = { path = "../embedded-inspect/embedded-inspect" }` (xtensa target)
- `embassy-net` — added `"tcp"` feature; `StackResources<4>` (was 3)
- `embassy-sync = "0.6"`, `embedded-websocket = { version = "0.9", default-features = false }`, `httparse = { version = "1", default-features = false }`
- `serde = { version = "1", default-features = false, features = ["derive"] }` (top-level)

### Usage
```
export WIFI_SSID=MyNetwork WIFI_PASS=secret
cargo run --example inspect_demo
# serial log prints the device IP after DHCP
# open http://<device-ip>:3000 in a browser
```

---

## 2026-07-29

**Modified: `examples/iris_demo.rs` — partial redraw on toggle button click**
- Touch clicks on the toggle button now use `flush_clip` to confine the two-pass waveform (WhiteOnBlack + BlackOnWhite) to just the toggle button's bounding rectangle, instead of redrawing the full 960×540 display.
- After `click_at`, the scene's `dirty_rect` already holds precisely the touched view's bounds (set by `input_toggle_button` via `mark_dirty_view`). The render clip is taken from that rect before the first pass; `dirty_rect` is restored for the second pass via `mark_dirty_all()` + override.
- Physical button navigation (BOOT/GPIO38) still uses the full `flush` path since focus changes may affect multiple views.

## 2026-07-28 (10)

**Added: `docs/drawing-algorithms.md` — detailed drawing algorithm documentation**
- Covers framebuffer layout (4bpp packed, 259 KB on PSRAM), tainted-row dirty tracking (68-byte bitmask), DrawMode (BlackOnWhite / WhiteOnBlack / WhiteOnWhite), the 15-frame waveform flush pipeline, the 65536-entry LUT system (indexed by 16-bit 4-pixel groups, evolved each frame via `update_lut`), DMA buffer preparation (LUT lookup → u32 packing → bit reversal for MSB-first panel format), column clipping via `flush_clip`, the hardware row protocol (I8080 + RMT CKV + STV/LEH signals), the row-skip optimization, the hardware clear cycle, power sequence, rotation transform, and double-flush pattern.

## 2026-07-28 (9)

**Added: `Display::flush_clip` — column-clipped waveform flush**
- Added `flush_clip(mode, clip: Rectangle)` to `src/driver/display.rs`. Confines the waveform to the clip rectangle by masking out-of-clip pixels to VCOM (0b00, no drive) in the DMA buffer before each row is sent to the panel. Pixels outside the clip receive no drive signal even though the row is still transmitted; the e-paper panel ignores VCOM-coded pixels.
- Added private `clip_dma_buffer(buf, x_start, x_end)`: zeros the 2-bit waveform codes for out-of-clip pixels. Handles full-byte masking for completely out-of-clip groups and per-pixel bit-pair masking for boundary groups. Byte `b` after bit-reversal encodes pixels `b*4..b*4+3` with the leftmost pixel at bits 7-6.
- Refactored internal `draw()` → `draw_inner(mode, clip_x_start, clip_x_end)`; `flush()` passes `(0, WIDTH)` (no clipping); `flush_clip` passes the clip bounds.
- Re-exported `Rectangle` from `epaper::driver` (was only public within the crate).
- **Updated `partial_repaint_bench`**: reverted to original centered sizes `[(50,30),(100,60),(200,120)]`, replaced `flush` calls with `flush_clip`, restored µs/pixel metric (now meaningful since only the rectangle pixels are driven).

## 2026-07-28 (8)

**Fixed: `partial_repaint_bench` — ghost bands from partial-width rows**
- Changed rectangle sizes from centered partial-width `[(50,30),(100,60),(200,120)]` to full-width `[(960,30),(960,80),(960,200)]`.
- Root cause: the e-paper waveform always processes complete rows (no per-column masking). With a partial-width rectangle, the pixels to the left/right of the rect in each dirty row are repeatedly driven white by every WhiteOnBlack pass while rows above/below are never touched. After ~10 iterations, the accumulated particle-state difference becomes visible as horizontal ghost bands extending from the sides of the rectangle.
- Full-width rectangles eliminate the outside-rect area entirely, so all pixels in dirty rows receive identical waveform treatment.
- Changed the per-pixel statistic to per-row (`µs/row`) since e-paper cost scales with row count, not pixel count within a row.

## 2026-07-28 (7)

**Added: `partial_repaint_bench` example — partial repaint speed benchmark**
- Created `examples/partial_repaint_bench.rs`: uses embedded-graphics (no iris-ui) to benchmark e-paper partial refresh performance.
- Fills a centred rectangle with "hello" text and alternates between black-on-white and white-on-black 20 times per size.
- Tests three rectangle sizes: 50×30, 100×60, 200×120 (centred on the 960×540 screen).
- Each flip uses the correct two-pass sequence (WhiteOnBlack erase + BlackOnWhite render) to avoid ghosting.
- Reports min/max/avg round-trip ms and µs/pixel to serial; prints `display.clear()` time at startup.
- Added `partial_repaint_bench` row to README examples table.
- Run: `cargo run --example partial_repaint_bench`

## 2026-07-28 (6)

**Updated: `sd_list` — add timing output**
- Prints "looking for SD card..." before attempting `open_volume`.
- Reports elapsed milliseconds on each outcome: card detected, not found, or error.
- Reports elapsed milliseconds for the directory listing phase.
- Uses `esp_hal::time::Instant::now()` / `.elapsed().as_millis()`.

## 2026-07-28 (5)

**Added: `sd_list` example — detect SD card and recursively list filesystem**
- Created `examples/sd_list.rs`: initialises SPI2 (GPIO14/13/21, CS=GPIO12), wraps the bus in `ExclusiveDevice` (from `embedded-hal-bus`), creates an `embedded-sdmmc` `SdCard` and `VolumeManager`, opens Volume 0, and recursively prints the entire directory tree.
- Prints "no SD card detected" on `SdCardError::CardNotFound`; distinguishes other hardware errors.
- Recursive listing capped at depth 8; uses `PSRAM` heap via `psram_allocator!` for `Vec` entry collection (required because `iterate_dir` prohibits VolumeManager calls inside its callback).
- `VolumeManager` created with `MAX_DIRS=16` so up to 15 levels of nesting can be open simultaneously.
- Added `embedded-sdmmc = "0.9"` and `embedded-hal-bus = "0.2"` to Cargo.toml (xtensa target deps).
- Run: `cargo run --example sd_list`

## 2026-07-28 (4)

**Updated: README — added LoRa (`lora_rx`) section with configuration table and notes**
- Added `## LoRa (lora_rx)` section explaining the four config constants (`FREQ_HZ`, `SF`, `BW`, `CR`), continuous RX mode behaviour, LDRO auto-enable rule, and why frequency mismatch is the most common failure mode.

## 2026-07-28 (3)

**Added: `lora_rx` example — passively receive LoRa packets on the onboard SX1262**
- Created `examples/lora_rx.rs`: powers LoRa via PCA9555 I/O expander, initialises SPI2 (GPIO14/13/21, CS=GPIO46) and the SX1262 (RST=GPIO1, BUSY=GPIO47, DIO1=GPIO10) in continuous-RX mode, and prints each received packet as a hex dump with RSSI/SNR to serial.
- Uses raw SPI commands (no external driver crate): SetStandby → SetPacketType → SetRfFrequency → SetModulationParams → SetPacketParams → SetDioIrqParams → SetRx(continuous).
- Configurable via constants at the top of the file: `FREQ_HZ` (default 915 MHz), `SF` (default 7), `BW` (default 125 kHz), `CR` (default 4/5). All must match the transmitter.
- Run: `cargo run --example lora_rx`

## 2026-07-28 (2)

**Added: `gps` example — read NMEA location fixes from onboard GPS module**
- Created `examples/gps.rs`: powers up the GPS module via PCA9555 I/O expander (I2C 0x20, port-0 all-high), opens UART1 on GPIO44 (RX) / GPIO43 (TX) at 9600 baud, reads NMEA bytes into a line buffer, and parses `$GNGGA` / `$GPGGA` sentences for location, fix quality, satellite count, HDOP, and altitude.
- Compatible with both the Quectel L76K and u-blox MIA-M10Q GPS chips auto-detected by the Lilygo T5 E-Paper S3 Pro (no chip-specific init commands needed).
- Run: `cargo run --example gps`

## 2026-07-28

**Added: `iris_demo_sim` example — simulator variant of `iris_demo` for local development**
- Added `embedded-graphics-simulator = "0.8.0"` as an optional dependency.
- Added `sim` feature that enables `iris-ui/std` (SDL2 window) and the simulator dep.
- Created `examples/iris_demo_sim.rs`: mirrors the scene and theme from `iris_demo.rs` but uses `SimulatorDisplay<Rgb565>` instead of the real EPD, and maps mouse clicks to touch and arrow keys to focus navigation.
- Run command (macOS Apple Silicon): `cargo run --example iris_demo_sim --features sim --target aarch64-apple-darwin --config 'unstable.build-std=["std"]'`
- The `--config` flag overrides the workspace's bare-metal `build-std` so the host stdlib is used.
- Requires SDL2 installed (`brew install sdl2` on macOS).
- Also moved `rustflags = ["-C", "link-arg=-nostartfiles"]` from `[build]` to `[target.xtensa-esp32s3-none-elf]` in `.cargo/config.toml` so it no longer applies to host builds.
- Gated `build.rs` linker flags (`-Tlinkall.x` and `--error-handling-script`) behind `CARGO_CFG_TARGET_ARCH == "xtensa"` so they don't fire for host builds.

## 2026-07-27

**Updated: link iris-ui to local path and update iris_demo for new API**
- Changed `iris-ui` dependency in `Cargo.toml` from git URL to `path = "../rust-embedded-gui"`.
- Added `FontKind` to imports in `examples/iris_demo.rs`.
- Updated `THEME` constant to wrap bitmap fonts in `FontKind::Bitmap(...)` to match the new `Theme` struct where `font`/`bold_font` fields are `FontKind` instead of raw `MonoFont`.
- The existing `Rgb565Adapter` continues to work with the now-generic `EmbeddedDrawingContext<T>` since `FromRgb565 for Rgb565` is implemented in the library.

## 2026-07-26

**Added: `wifi_ntp` example — WiFi NTP time sync with EPD console display**
- Connects to WiFi (SSID/password from `WIFI_SSID`/`WIFI_PASS` env vars at build time).
- Queries `time.google.com:123` via a raw 48-byte UDP NTP packet (no DNS required).
- Parses the NTP transmit timestamp and converts to Unix microseconds, then sets the ESP32-S3 hardware RTC via `rtc.set_current_time_us()`.
- Shows a scrolling console-style status log on the EPD display (landscape 960×540) and serial simultaneously.
- After sync, displays the current UTC time every 10 seconds from the RTC.
- Uses `esp-radio 0.18.0` (WiFi), `esp-rtos 0.3.0` (async scheduler), `embassy-net 0.8.0` (TCP/IP stack), and `embassy-executor 0.10.0`; all added to `Cargo.toml`.
- Build: `export WIFI_SSID=MyNetwork WIFI_PASS=secret && cargo run --example wifi_ntp`

## 2026-07-26 (12)

**font_compare: adjust size list to [15, 18, 22, 26, 32]**
- Changed from `[15, 18, 22, 28, 32]` to `[15, 18, 22, 26, 32]` (28 → 26).

## 2026-07-26 (11)

**font_compare: single-font view grouped by weight**
- Reorganised single-font view: outer loop is now weight (Regular / Bold / Italic), inner loop is all five sizes smallest-to-largest. Previously the outer loop was size and inner loop was weight.

## 2026-07-26 (10)

**font_compare: fix loading-text clear, fix label overlap, add single-font view**
- **Loading text clear**: replaced `display.clear()` + single `flush(BlackOnWhite)` with the two-flush pattern — `flush(WhiteOnBlack)` actively drives dark pixels to white (the only way to erase them on this EPD), then `render()` again + `flush(BlackOnWhite)` draws the fonts cleanly.
- **Label overlap**: `draw_label()` now computes the y-advancement as `13 + 3 + font_px * 0.75` so that the main TTF font's ascenders (which extend upward from the baseline) always clear the label by ~3 px at every size.
- **Single-font view** (NEXT button / GPIO38): shows all four sizes × three weights for one font family. NEXT cycles through faces; BOOT returns to all-fonts view.
- Header and group/size labels all use bitmap fonts (FONT_10X20 / FONT_7X13) — no TTF loaded just for labels.

## 2026-07-26 (9)

**font_compare: portrait mode, 6 families, subsetted fonts**
- Added Crimson Pro, Atkinson Hyperlegible, and Alegreya (Regular/Bold/Italic each) downloaded from Google Fonts and auto-renamed from actual font name-table style records.
- Switched to portrait orientation (`DisplayRotation::Rotate90`). All drawing goes through `put_pixel()` which applies the Rotate90 transform (physical_x = Width−1−ly, physical_y = lx) before calling `set_pixel`, so the framebuffer stores portrait content correctly.
- Subsetted all 18 font files to the 62 glyphs actually used (pyftsubset, `kern`/`liga`/`calt` features kept). Fonts shrank from 50–225 KB to 12–19 KB (~10× reduction); fontdue now parses each in a fraction of a second.
- Added `hline()` helper for drawing horizontal rules in portrait space.
- Tightened sample text slightly to fit portrait width at all four sizes.

## 2026-07-26 (8)

**font_compare: loading screen + right-aligned font labels**
- Added `show_loading()`: draws "Rasterizing fonts at Xpx — please wait…" using the embedded-graphics `FONT_10X20` bitmap font (no TTF parsing, instant) before the slow fontdue render begins; the subsequent `BlackOnWhite` flush clears it automatically.
- Moved font name labels to the right margin (right-aligned via `measure_width`) so they no longer overlap the sample text lines on the left.

## 2026-07-26 (7)

**Fix font_compare crash: load fonts one at a time instead of all 9 simultaneously**
- fontdue eagerly parses all glyph outlines at `Font::from_bytes` time. Loading 9 fonts simultaneously via a `[FontGroup; 3]` array literal exhausted the PSRAM (each font ~500-700KB × 9 = memory pressure at startup).
- Replaced the struct-per-group approach with `draw_line()`: each call loads the font, draws one text line, then drops the font. Peak heap is now ~1 font object at a time.
- Added `esp_alloc::heap_allocator!(size: 256 * 1024)` alongside the existing `psram_allocator!` as an allocator bootstrap for small/early allocations.
- Removed the `FontGroup` struct; the font table is now a simple `const` array of `(&str, &[u8], &[u8], &[u8])` tuples.

## 2026-07-26 (6)

**Add font_compare example: Literata, Vollkorn, Noticia Text side by side**
- New `examples/font_compare.rs` renders the same three sample lines (Regular, Bold, Italic) in each of three Google Fonts across the full 960×540 display.
- BOOT button cycles through four font sizes: 15, 18, 22, 28 px.
- Added `TextRenderer::with_font(data: &'static [u8])` constructor to `src/font.rs` so any embedded TTF can be used without modifying the library.
- Flash footprint: ~1.83 MB (9 embedded TTFs ≈ 1.5 MB + code); fits within the 4 MB app partition.
- Fonts are gitignored; download instructions are in the plan file.

## 2026-07-26 (5)

**Fast-scroll: hold BOOT/next button for 1 second to jump many pages**
- Holding either navigation button for ≥1 second enters fast-scroll mode: a centered dialog appears and the page counter advances as fast as the EPD can refresh while the button is held.
- On release the display does a full clear+render at the new page offset. Single short-press behaviour is unchanged.
- Clamps at chapter start/end (no cross-chapter jumps during fast-scroll).
- New helpers: `compute_chars_per_page`, `draw_fast_scroll_dialog`, `advance_page_offset`, `fast_scroll`.
- Dialog uses the proven two-flush pattern (WhiteOnBlack to clear EPD rows, then BlackOnWhite to render) so previous page text doesn't bleed through.
- Dialog is drawn through `RotatedDisplay` so it matches the current reading orientation.
- In portrait mode (Deg90/270) the dialog uses 70px logical-X × 300px logical-Y (swapped from landscape's 300×70) so the logical-X-to-physical-row mapping always taints only ~70 rows in all orientations — same flush cost as landscape.

## 2026-07-26 (4)

**Fixed dropdown background bleed-through (full-screen clear + re-render on open)**
- Dropdown opening now uses the same pattern as `restore_after_dropdown`: `fill(white)` + `flush(WhiteOnBlack)` to clear the entire screen, then `render_page` to redraw the current page, then draw the dropdown overlay, then `flush(BlackOnWhite)`.
- Previous attempts with `clear_area` (physical rect) caused two problems: (1) `push_pixels` column addressing uses a different byte reorder than the framebuffer path, so the clear was shifted relative to the drawn content, leaving bleed-through at one edge; (2) the expanded margin calculation allowed `area.x + area.width > 960`, causing an index-out-of-bounds panic for the Rotation dropdown in landscape mode.
- The full-screen clear approach is simpler and provably correct in all orientations — the same logic that works for closing dropdowns now also works for opening them.

## 2026-07-26 (3)

**UI fixes: header font, dropdown background, footer page count**
- Header buttons now use `FONT_10X20` (up from `FONT_9X18`) with `HEADER_H` increased to 52 px for comfortable fit.
- Option dropdowns (`draw_option_dropdown`) now fill the full panel area with white before drawing rows, preventing page text from bleeding through.
- Footer now shows `Ch.N/M p.P/T` — chapter number, chapter count, estimated page within chapter, and estimated total pages in chapter — instead of just the chapter number.

## 2026-07-26 (2)

**Fixed blank first page and added full settings persistence (`ereader_full`)**
- Fixed blank first page: spine items whose stripped text is under 50 characters (e.g. cover image pages) are now skipped on startup and on chapter transitions in both directions.
- Font size, orientation, and backlight level now survive full power-off: saved to NVS flash keys 2/3/4 on every page turn and every dropdown setting change; restored on cold boot alongside reading position. Previously these only survived deep sleep via RTC STORE registers.

## 2026-07-26

**Wired EPUB reader into `ereader_full` example**
- Replaced hardcoded `moby_dick.txt` with `moby_dick.epub` (Project Gutenberg, 710 KB, 28 chapters, `include_bytes!`; git-ignored).
- Added `EpubArchive::new(EPUB_DATA)` + `spine()` at startup; current chapter text is loaded on demand via `chapter_text()` from `epub.rs`.
- Added `chapter_idx: usize` state. `paginate()`/`wrap_line_px()` generalized from references into a global static to references into any `&str` (lifetime follows the chapter text).
- Chapter transitions: forward button advances to chapter N+1 when the last page is reached; back button at page 0 goes to start of previous chapter.
- Persistence updated: flash key 0 = `page_offset` within chapter, key 1 = `chapter_idx`; RTC STORE6 = `chapter_idx` for deep-sleep wakeup.
- Footer now shows `Ch.N/28` instead of a byte-offset page estimate.
- `draw_footer` signature updated to take `chapter`/`chapter_count` directly.

## 2026-07-25 (3)

**Added: EPUB reader library modules (`src/epub.rs`, `src/layout.rs`, `src/reader.rs`)**
- `src/layout.rs`: word-wrap paginator. `layout_chapter(text, cfg)` takes a `LayoutConfig` (screen dimensions, margins, `FontMetrics` with a `fn(&str)->u32` measure pointer) and returns a `Layout` with `Vec<Page>` of `(start, end)` byte offsets into the text. Handles paragraph breaks (`\n\n`), forced breaks (`\n`), paragraph spacing, and an ASCII glyph-width cache to avoid redundant `measure` calls.
- `src/epub.rs`: no_std EPUB archive reader backed by `&[u8]` (e.g. `include_bytes!`). Parses the ZIP central directory once at `EpubArchive::new()`; extracts and decompresses entries on demand using `miniz_oxide` (DEFLATE) or direct copy (stored). Parses `META-INF/container.xml` and the OPF manifest/spine via `xmlparser` to build the ordered chapter list. XHTML chapters are stripped to plain text via a byte-level scanner that normalises whitespace, decodes common HTML entities (`&amp;`, `&nbsp;`, `&#NN;` etc.), and maps block tags to `\n`/`\n\n` paragraph breaks. Uses `xmlparser 0.13` (no_std + alloc) instead of `quick-xml` (std-only).
- `src/reader.rs`: `ReaderState` holds one chapter's text, its `Layout`, and `current_page`/`anchor_byte`. `relayout()` re-paginates after a font-size change and repositions to the page containing `anchor_byte`. `turn_page()`, `current_text()`, `go_to_page()`, `page_count()`.
- All three modules are `no_std + alloc`, no new dependencies beyond `miniz_oxide` and `xmlparser` (already in `Cargo.toml`).
- Added `examples/epub_test.rs`: smoke-tests all three modules using `include_bytes!("test.epub")` (a 2.6 KB generated EPUB with two chapters); exercises spine parsing, XHTML stripping, layout, page turning, and relayout after font-size change. No display hardware needed. Added `examples/*.epub` to `.gitignore`.

## 2026-07-26 (2)

**Updated: `iris_demo` — centered panel layout**
- Wrapped all content in a `CENTER_PANEL` (layout_vbox, h_flex=Shrink, v_flex=Shrink, h_align=Center) so iris-ui's layout engine horizontally centers it within the 960px root vbox.
- Added invisible `SPACER_TOP` / `SPACER_BOT` panels (v_flex=Grow, draw=None) flanking the center panel; they split remaining vertical space 50/50, pushing content to the vertical center of the screen.
- Changed `btn_row.h_flex` from Grow to Shrink so the panel width is sized to its content rather than full-screen width.

## 2026-07-26

**Added: `iris_demo` example — iris-ui integration with EPD display**
- Integrates the [iris-ui](https://github.com/joshmarinacci/iris-ui) no_std embedded UI toolkit as a git dependency.
- Added a thin `Rgb565Adapter` wrapper (implements `DrawTarget<Color=Rgb565>` over the Gray4 EPD display) to bridge iris-ui's color requirement with the e-paper display. Uses luminance conversion (BW_THEME maps to pure black/white anyway).
- Scene has: a header label, an hbox row of 3 buttons (two normal, one primary), a 3-option toggle group, and a status label that updates with the last command/selection.
- Touch input routes through `iris_ui::scene::click_at`; BOOT and GPIO38 buttons navigate focus via `event_at_focused`.
- Initial render uses double-flush (WhiteOnBlack then BlackOnWhite) to clear ghost ink; subsequent updates use a single partial flush via iris-ui's dirty-rect tracking.

## 2026-07-25 (2)

**Added: `flash_demo` example**
- Standalone example demonstrating how to detect boot type and use sequential-storage for persistent NVS flash reads/writes without any display or PSRAM dependency.
- Shows `reset_reason` variants (power-on, deep-sleep wakeup, software reset, watchdog, brownout).
- `fetch_item` returning `None` vs `Some(n)` is used to distinguish first-ever flash use from subsequent boots.
- Maintains a boot counter (key 42) and a payload value (key 43) — different keys from `ereader_full` (key 0) so both examples can coexist in the same NVS partition.

## 2026-07-25

**Added: persistent reading position across full power cycles (`ereader_full`)**
- Reading position (`page_offset` byte offset) is now saved to flash on every page turn (forward and back) and loaded on startup, surviving full power-off/reflash cycles.
- Uses `sequential-storage 3.0` map API with a 6-sector NVS partition (`0x9000..0xF000`, matching `partitions.csv`), keyed by `u8` key `0` with `u32` value.
- Thin `FlashAdapter` wrapper bridges `esp-storage`'s synchronous `NorFlash` impl to the async `embedded_storage_async` trait required by `sequential-storage 3.0`, run via a minimal noop-waker `block_on` (safe because flash ops never yield).
- On full power-on: loads saved position from flash; shows "Resumed" in the status bar if non-zero. Deep-sleep wakeup continues to restore from RTC STORE registers (unchanged).
- Flash errors on load fall back to position 0 and are logged; flash errors on save are logged and ignored (position is still correct in RAM).
- Added `nor-flash` feature to `esp-storage` and `embedded-storage-async = "0.4"` as a direct dependency.

## 2026-07-24 14:00

**Added: header dropdown menus to `ereader_full`**
- Replaced in-place header cycling with proper dropdown panels that open directly below the header.
- Tapping a header zone opens a panel; tapping an option applies it and closes the panel; tapping outside dismisses without change; BOOT/Next buttons also dismiss any open dropdown before paging.
- **Backlight dropdown** — 4 levels (Off / Low / Med / Hi); backlight updates immediately on selection.
- **Font size dropdown** — Sm / Md / Lg / XL; repaginates on close.
- **Rotation dropdown** — Landscape / Portrait / Inverted / CCW; repaginates on close.
- **Battery panel** — read-only display of SoC %, charging status, voltage, current, and capacity from BQ27220/BQ25896; tap anywhere to dismiss.
- Closing any dropdown uses a two-pass e-paper redraw (`fill(0xF)` + `flush(WhiteOnBlack)` then `render_page` + `flush(BlackOnWhite)`) to eliminate ghosting from the dropdown overlay.
- New helpers: `dropdown_x_and_w`, `draw_option_dropdown`, `draw_battery_panel`, `restore_after_dropdown`.
- New state: `open_dropdown: Option<Dropdown>` enum (`Backlight | Battery | FontSize | Rotation`).

## 2026-07-24 08:45

**Added: XL font size to `ereader_full`**
- Expanded `FONT_SIZES` from 3 to 4 entries, adding XL at 28px landscape / 26px portrait.
- `FONT_LABELS` updated to `["Sm", "Md", "Lg", "XL"]`.
- RTC STORE5 bit field for font size index is 2 bits wide, so all 4 entries fit without other changes.

## 2026-07-24 08:30

**Added: runtime font size cycling to `ereader_full`**
- Added `FONT_SIZES` array (3 entries: Sm=15/13px, Md=18/16px, Lg=22/20px) replacing the two compile-time constants.
- Header expanded from 4 to 5 equal zones; the new 4th zone ("Sz:Sm/Md/Lg") cycles font size on tap.
- Touch zone boundaries updated to fifths of canvas width: BL=[2z..3z], Sz=[3z..4z], Rot=[4z..5z].
- Font size index stored in bits 10–11 of RTC STORE5 and restored on wakeup from deep sleep.
- Font size change triggers a full page redraw (repagination required since line count changes).

## 2026-07-24 08:10

**Added: TrueType font rendering with antialiasing (`fontdue` crate + `src/font.rs`)**
- Added `fontdue = "0.9"` dependency (pure Rust, no_std+alloc, runs on ESP32-S3 PSRAM heap).
- New `src/font.rs` with `TextRenderer` struct: loads `fonts/Georgia.ttf` from flash via `include_bytes!`, rasterizes glyphs on demand with per-glyph caching, and alpha-blends coverage (0–255) to Gray4 (0–15) for smooth antialiased output.
- Public API: `new()`, `draw_str(text, x, baseline_y, font_px, bg, closure)`, `measure_width()`, `line_height()`, `char_advance()`.
- Updated `examples/ereader_full.rs` to use `TextRenderer` for body text:
  - Removed fixed-character-count layout constants; replaced with pixel-width layout (`LAND_FONT_PX = 18.0`, `PORT_FONT_PX = 16.0`, `LEADING = 4`).
  - `wrap_line()` replaced by `wrap_line_px()` — wraps at exact pixel widths using `renderer.char_advance()`.
  - `paginate()` now derives `max_lines` dynamically from `renderer.line_height()`.
  - `draw_content()` bypasses embedded-graphics for body text; applies rotation transform in the pixel write closure.
  - Header and footer retain bitmap fonts (`FONT_9X18`, `FONT_7X13`) — unchanged.
- Added `fonts/Georgia.ttf` (system font, not committed — place in `fonts/` before building); `fonts/*.ttf` added to `.gitignore`.

## 2026-07-23

**Updated: `examples/ereader_full.rs` — larger book font; header style**
- Book text switched from `FONT_9X18` to `FONT_10X20` for improved readability; layout constants updated (landscape: 88 chars × 19 lines; portrait: 48 chars × 36 lines).
- Header changed from white-text-on-black-fill to black-text-on-white with a 2px black border.

**Added: `examples/ereader_full.rs` — full Moby Dick e-reader with status bar and deep sleep**
- Displays the complete text of Moby Dick (1.25 MB, embedded via `include_str!`) with word-wrap pagination.
- Header bar (black-on-white): current time (HH:MM from RTC), battery % with charging indicator (`[+]`), backlight level (tappable to cycle Off/Low/Med/High), orientation label (tappable to cycle Land/Port/Inv/CCW through four 90° rotations).
- BOOT button (GPIO0) = previous page; GPIO38 = next page.
- Orientation uses the `RotatedDisplay` wrapper from `ebook.rs`; text is repaginated at each rotation (landscape: ~97 chars × 21 lines; portrait: ~53 chars × 40 lines).
- Auto deep-sleep after 60 s of inactivity: displays sleep message in footer, turns backlight off, writes page offset + backlight level + orientation to RTC STORE registers (STORE0/1/5), then calls `rtc.sleep_deep()` with `Ext0WakeupSource` on GPIO0.
- On BOOT wakeup: restores page, backlight, and orientation from STORE registers; shows "Awake!" in footer.
- Header time refreshes every 60 s using partial flush (only tainted rows sent to panel).
- Added: `examples/moby_dick.txt` — Moby Dick plain text (~1.25 MB, Project Gutenberg, boilerplate stripped).

## 2026-07-21 (4)

**Updated: `examples/clock.rs` — add BOOT button (GPIO0) as deep-sleep wakeup source**
- Added `Ext0WakeupSource::new(gpio0, WakeupLevel::Low)` alongside the existing `TimerWakeupSource`.
- Pressing BOOT immediately wakes the device and redraws the clock, in addition to the 10-second timer.
- `wakeup_cause()` is checked on each wakeup; the status line on the display and serial output now report whether the wakeup was triggered by the button (`Ext0`) or the timer.
- GPIO0 is taken from `peripherals` before `Display::new()` so the pin is available for `Ext0WakeupSource` while `pin_config!` still gets the 15 display pins it needs.

## 2026-07-21 (3)

**Added: `examples/clock.rs` — deep-sleep e-paper clock**
- On every boot: reads `rtc.current_time_us()` (survives deep sleep via RTC STORE2/STORE3), formats as `HH:MM:SS`, draws on display, then calls `rtc.sleep_deep()` for 10 seconds.
- On first boot (non-`CoreDeepSleep` reset reason): seeds the RTC with `INITIAL_HH/MM/SS` constants the user sets before flashing.
- Uses `Rtc::new(peripherals.LPWR)` + `TimerWakeupSource` from `esp_hal::rtc_cntl` — no new crate dependencies.
- Display is powered off after each flush; e-paper retains the image with no power.
- Set `SLEEP_SECS = 3` for faster iteration during testing.

## 2026-07-21 (2)

**Updated: `examples/ebook.rs` — two-button navigation + four-way orientation**
- BOOT (GPIO0) goes to the previous page; GPIO38 advances to the next page (confirmed on hardware).
- Both directions wrap around (page 0 back → last page; last page forward → page 0).
- Hold GPIO38 (≥500 ms) to cycle through all four orientations: 0°, 90°, 180°, 270°.
- `RotatedDisplay<'d, 'hw>` wrapper implements `DrawTarget` + `OriginDimensions` with per-orientation pixel mapping; two separate lifetimes keep the borrow scoped correctly.
- `draw_page` is generic over `DrawTarget + OriginDimensions`; selects font/margins by orientation (FONT_10X20 landscape, FONT_9X18 portrait).
- Serial monitor prints current orientation label on each change.

**Added: `examples/find_button.rs` — GPIO diagnostic**
- Polls candidate free GPIOs (excluding USB D-/D+ on GPIO19/20 and display pins) and prints which one goes low when pressed.
- Used to identify the forward button as GPIO38. Keep for future hardware debugging.

## 2026-07-21

**Fix: `examples/graphics_test.rs` — panic in triangle drawing**
- `embedded-graphics` 0.8.2 overflows `i32` in `ClosedThickSegmentIter` / `IntersectionParams::nearly_colinear_has_error` (`denominator.pow(2)`) for the triangle's large screen coordinates.
- Workaround: replaced `Triangle::new(...).into_styled(s4)` with three separate `Line` primitives (same visual result, avoids the thick-segment join code path).
- Removed unused `Triangle` import.

## 2026-07-20 (3)

**Added: `examples/battery_status.rs` — BQ27220 + BQ25896 dashboard**
- Reads all useful registers from both chips via the display's shared I2C bus and renders a two-column dashboard on the e-paper screen, refreshing every 10 s.
- Left column (BQ27220 fuel gauge): state of charge %, voltage, current + direction, remaining/full capacity, state of health, temperature.
- Right column (BQ25896 charger): USB presence, charge status, VBUS voltage, battery voltage, system voltage, charge current.
- All readings also logged to serial each cycle.

**Added: `i2c_read_u8 / i2c_read_u16 / i2c_read_i16` to `Display`**
- Generic I2C passthrough helpers on the shared bus, used by `battery_status` to reach the battery chips without opening a second I2C port.

## 2026-07-20 (2)

**Fixed: `examples/backlight.rs` — wrong GPIO**
- Was using GPIO47 (BOARD_LORA_BUSY) instead of GPIO11 (BOARD_BL_EN).
- Confirmed from official board definition in Lilygo's T5S3-4.7-e-paper-PRO repo.
- Updated README hardware table to document backlight pin, BQ25896 charger, and BQ27220 fuel gauge.

## 2026-07-20

**Added: `examples/finger_draw.rs` — touch finger-drawing demo**
- Draws a 16×16 px filled black dot at each touch position using partial refresh.
- No erasing — pixels accumulate, letting you judge the display's maximum refresh cadence.
- Each dot flush only updates the 16 dirty rows, so partial refresh completes in a fraction of a full-screen update time.
- Timing (`flush=Xms`) and dot count printed to serial for each stroke.
- Drawing area is the full screen below a thin header; header stays physically on screen without being re-sent.

**Added: `examples/backlight.rs` — frontlight PWM demo**
- New example that drives the Lilygo T5 S3 Pro frontlight (GPIO47) using the ESP32-S3 LEDC peripheral with 1 kHz 8-bit PWM.
- Draws a static label on the e-paper display, then enters a loop fading the backlight from 0% → 100% over ~2 s, holds for 1 s, then fades back to 0%.
- Prints brightness percentage to serial each step.
- Updated README examples table with the new entry.

## 2026-07-19

Rendering fixes, touch_button improvements, and waveform documentation.

**Fixed: `src/driver/display.rs` — partial refresh column bleeding**
- `draw()` was calling `self.epd.skip()` directly for non-tainted rows, bypassing `row_skip()`. This left the source drivers holding the last active row's pixel data during skip CKV pulses, causing the column range of any drawn region to bleed black top-to-bottom across the full display height over 15 waveform frames.
- Fix: use `self.row_skip()` / `self.row_write()` throughout `draw()` and reset `self.skipping = 0` at the start of each frame, so the first skip after active rows sends a blank buffer that clears the source drivers.

**Updated: `examples/touch_button.rs`**
- Shrunk button from 800×440 to 200×60 px centered on screen to measure raw partial-refresh speed.
- Removed status bar (text, constants, `update_status` helper, `Buf` struct) so only the button region is flushed.
- Replaced `clear_area()` + `BlackOnWhite` with a universal two-pass waveform approach:
  - Pass 1: `WhiteOnBlack` with all-white framebuffer drives every pixel in the button area to white for all 15 frames, establishing a known-white physical state.
  - Pass 2: `BlackOnWhite` renders the actual button content (fill or stroke+text) on that clean canvas.
  - Eliminates `clear_area()` (32 full hardware scans) for both state transitions; both filled and empty states now render correctly in both passes.

**Updated: `README.md`**
- Replaced the partial-refresh "LUT only drives toward black" note (incorrect) with a full "Waveform Engine & DrawMode" section covering: how the 15-frame LUT engine works, 2-bit waveform code meanings, DrawMode semantics, the two-pass pattern, and a latency vs quality tradeoff table.

## 2026-07-17 (gt911 byte layout fix)

Fixed GT911 touch coordinate byte offsets, inverted Y axis, and removed wrong scaling.

**Modified: `src/driver/gt911.rs`**
- Fixed `read_touch`: actual layout is Y at [0,1], X at [2,3], touch area at [4,5] (was reading X from [1,2], Y from [3,4])
- Y is physically inverted: raw y=y_max is the physical top of the screen; corrected with `y = y_max - y_raw`
- Removed incorrect 16-bit scaling (`x_raw * x_max / 65535`); the GT911 outputs coordinates directly in the configured range (0..x_max, 0..y_max) after `configure()` is called
- Removed y_raw_min/y_raw_max calibration fields and `set_y_raw_range()` — no longer needed

## 2026-07-15 (touch_button)

Added GT911 touch controller support and `examples/touch_button.rs`.

**New file: `src/driver/gt911.rs`**
- Minimal GT911 capacitive touch driver (polling, no INT pin required)
- `Gt911::new(addr)` — construct with I2C address (0x5D primary, 0x14 alternate)
- `Gt911::read_touch(i2c)` — reads status register 0x814E, returns first touch point coordinates from 0x8150, clears buffer-ready flag after each read
- `Gt911::detect(i2c)` — probes both addresses and returns the one that ACKs

**Modified: `src/driver/ed047tc1.rs`**
- Added `i2c()` method exposing `&mut I2c<'_, Blocking>` so the Display layer can pass the bus to touch reads

**Modified: `src/driver/display.rs`**
- Added `read_touch(&mut self, gt911: &mut Gt911) -> Option<(u16, u16)>` — polls GT911 via the driver's internal I2C
- Added `detect_touch_addr(&mut self) -> Option<u8>` — finds the active GT911 address at startup

**Modified: `src/driver/mod.rs`**
- Added `pub mod gt911` and re-exported `Gt911`

**New file: `examples/touch_button.rs`**
- Detects GT911 address on boot; warns if not found
- Draws a 360×160 px button centered on screen (rows 190–350)
- Toggle between outline-only and filled-black on each tap
- Uses partial refresh (only button rows flushed) for low-latency redraws
- Prints `touch at (x, y)` and `flush Nms` per tap to serial monitor
- Debounces: waits for finger-lift before accepting next tap

Flash and run: `cargo run --example touch_button`

## 2026-07-15 (graphics_test)

Added `examples/graphics_test.rs` — comprehensive 7-screen graphics test.

**New file: `examples/graphics_test.rs`**
- Screen 0: Title page listing all screens, navigation hint
- Screen 1: Shapes — 8 radiating lines, 5 concentric circles (filled + stroked), 4 stroke-width rectangles, triangle, grey-level line swatch
- Screen 2: Typography — all 9 built-in fonts (`FONT_4X6` through `FONT_10X20`), underline via `underline_with_color`, strikethrough via `strikethrough_with_color`, left/centre/right alignment demo
- Screen 3: Grayscale — 16 labelled bars (luma 0→15), 960×50 smooth gradient strip via `ImageRaw<Gray4, BigEndian>` embedded from `OUT_DIR/strip.bin`
- Screen 4: Image — 960×270 four-quadrant test card (gradient / checkerboard / solid bands / Chebyshev rings) via `ImageRaw` from `OUT_DIR/card.bin`
- Screen 5: Animation — 20-frame ball animation in a 120-row partial-refresh band; measures full-flush time (540 rows via `fill()`) and per-frame partial-flush time
- Screen 6: Timing summary — `clear_ms`, `full_flush_ms`, `partial_avg_ms` with computed speedup ratio

**Bug fix: `src/driver/display.rs`** — tainted-row dirty bitmap (`set_pixel` and `is_tainted`) divided by `TAINTED_ROWS_SIZE` (68) instead of 8, causing row-index collisions and preventing true partial refresh. Fixed to divide by 8; `1 << (row % 8)` correctly indexes the bit within each byte.

**Updated: `build.rs`** — generates two synthetic image assets at compile time for the graphics_test example:
- `OUT_DIR/card.bin` — 960×270 four-quadrant test card (129,600 bytes), 4-bit BigEndian Gray4
- `OUT_DIR/strip.bin` — 960×50 horizontal gradient (24,000 bytes), 4-bit BigEndian Gray4

Flash and run: `cargo run --example graphics_test`

## 2026-07-15 (ebook)

Added 3-page ebook demo as an example binary; `src/main.rs` is unchanged.

**New files**
- `src/lib.rs` — minimal library root (`pub mod driver`) so examples can reference the driver
- `examples/ebook.rs` — ebook page-turn demo

**Changes to `src/driver/mod.rs`**
- Re-exported `ed047tc1::PinConfig` as `driver::PinConfig` so the `pin_config!` macro works from outside the crate
- Updated macro body to use `$crate::driver::PinConfig` (was `$crate::driver::ed047tc1::PinConfig`)

**Ebook demo details**
- Three pages of text using `FONT_10X20`, ~65 chars per line, ~17 lines per page
- Chapter title + underline separator, body text, page-indicator dots (filled = current page)
- Page navigation via GPIO0 (BOOT button, active-low, pull-up with `InputConfig`): press to advance, wraps back to page 1
- `display.clear()` before every page: the waveform LUT only drives pixels toward black and leaves "white" pixels with no-drive (`0x00`), so previously-black pixels from the prior page would ghost unless the panel is unconditionally reset to white first via `push_pixels`
- Serial monitor logs `flushing...` / `flush complete` around each `flush()` call for timing observation

Flash and run: `cargo run --example ebook`

## 2026-07-14 18:50

Fixed pixel ordering in `prepare_dma_buffer` (`src/driver/display.rs`):

- The ED047TC1 panel reads the parallel bus MSB-first: bits 6–7 of each byte are the leftmost pixel in a 4-pixel group, not bits 0–1
- The LUT produced LSB-first output, causing every 4-pixel group to render right-to-left (blurry circle edges, garbled text)
- Fix: reverse the 2-bit pixel-pair order within each output byte after LUT conversion
- Uniform solid fills (0x55 / 0xAA / 0x00 / 0xFF) are palindromes under this transform, which is why `display.clear()` always worked correctly
- Verified on hardware: sharp shape edges and readable text

## 2026-07-14 18:30

Added embedded-graphics demo to `src/main.rs`:

- Added `embedded-graphics = "0.8"` dependency to `Cargo.toml`
- Draws a 6px border, filled circle, stroked rectangle, stroked triangle, and two centred text lines using `FONT_10X20`
- Uses `Gray4::BLACK` for all primitives on a white background (`display.clear()`)
- Flushes to hardware via `display.flush(DrawMode::BlackOnWhite)`
- Verified on device: serial output shows "drawing shapes... flushing... done." with no panics

## 2026-07-14

Replaced `lilygo-epd47` crate with a local `src/driver/` module forked for the T5 E-Paper S3 Pro hardware (V7 / ESP32-S3):

- **Correct GPIO wiring**: Data bus D0–D7 → GPIO5–8,15–18; CKH→GPIO4; STH→GPIO41; LEH→GPIO42; STV→GPIO45; CKV→GPIO48
- **I2C power management**: PCA9555 I/O expander (addr 0x20, SDA=GPIO39, SCL=GPIO40) for OE/MODE/PWRUP/VCOM_CTRL/WAKEUP signals; TPS65185 PMIC (addr 0x68) for voltage rail enable and VCOM=1600mV
- **Pro-specific `pin_config!` macro** wired to the correct GPIOs
- **`Display::new()` takes `peripherals.I2C0`** as an additional parameter
- Verified: display fills solid black end-to-end on hardware

## 2026-07-13

Initial project scaffold for Lilygo T5 E-Paper S3 Pro embedded Rust driver.

- Created `Cargo.toml` with `lilygo-epd47 1.1.0` as the primary display driver (ED047TC1 parallel e-paper via ESP32-S3 LCD_CAM + DMA + RMT)
- Created `.cargo/config.toml` targeting `xtensa-esp32s3-none-elf` with the `esp` toolchain
- Created `rust-toolchain.toml` pinning to the Espressif `esp` Xtensa toolchain
- Created `build.rs` linking `linkall.x` (standard ESP32 pattern)
- Created `src/main.rs` that initializes PSRAM, powers on the display, and performs a hardware clear to white
- Build compiles cleanly with `cargo build`

## 2026-07-16 (touch_button — GT911 Y axis calibration)

Fixed GT911 Y coordinate spanning only ~42 pixels instead of the full 540.

**Root cause**: The GT911 on this hardware outputs Y raw values in a hardware-specific sub-range (~1946–8240) rather than the full 0–65535 span used by the X axis. Dividing by `u16::MAX` (65535) produced only ~42 pixels of effective Y travel even across the full screen height.

**Fix**: Added `y_raw_min` and `y_raw_max` fields to `Gt911` (defaults 1946/8240, calibrated from observed tap data). `read_touch()` now clamps to this range and inverts in one step: `y = (y_raw_max - y_raw) * y_max / (y_raw_max - y_raw_min)`. Added `set_y_raw_range(min, max)` for future hardware-specific overrides.

**Derivation**: Observed y_raw≈7424 at button top (y≈70) and y_raw≈2307 at button bottom (y≈509). Extrapolated to screen edges: top y=0 → y_raw≈8240, bottom y=540 → y_raw≈1946.

**Files changed**:
- `src/driver/gt911.rs` — `Gt911` struct gains `y_raw_min`/`y_raw_max`; `new()` defaults to measured values; `read_touch()` uses calibrated Y range; added `set_y_raw_range()`

## 2026-07-16 (touch_button — GT911 coordinate scaling)

Fixed GT911 touch coordinates reporting raw 16-bit sensor values instead of display pixel coordinates.

**Root cause**: The GT911 outputs raw sensor coordinates in a 0–65535 range regardless of the `X_output_max`/`Y_output_max` config registers. The `read_touch()` function was returning the raw values directly.

**Fix**: Added `x_max`/`y_max` fields to `Gt911` struct (set by `configure()`). `read_touch()` now scales raw coordinates to display pixel space: `pixel = raw * max / 65535`.

**Files changed**:
- `src/driver/gt911.rs` — `Gt911` struct gains `x_max: u16, y_max: u16`; `configure()` saves them; `read_touch()` scales output when max fields are set

## 2026-07-16 (touch_button — button background clearing on toggle)

Fixed button not clearing when tapping a second time to return to the outline (Empty) state.

**Root cause**: The `BlackOnWhite` waveform is darken-only. `lut_default = 0x55` drives all pixels toward black; `update_lut` progressively changes entries for lighter target pixels from `01` (black-drive) to `00` (VCOM/neutral). White-target pixels get VCOM for all 15 frames — so previously-black pixels are left black, since VCOM produces no net drive on the panel.

**Fix**: Added `display.clear_area()` on the button bounds before `draw_button(Empty)`, same as the existing status-bar fix. This uses AC voltage cycles to physically drive the button interior back to white before the waveform renders the new outline.

**Files changed**:
- `examples/touch_button.rs` — added `clear_area()` call in the `ButtonState::Empty` arm of the tap handler

## 2026-07-16 (touch_button — status bar background clearing)

Fixed status bar text background not being cleared between touch events.

**Root cause**: The display waveform LUT uses only the target framebuffer value as its index. After each `flush()`, the framebuffer is reset to `0xFF` (white). When `update_status()` filled rows 0-59 with white via embedded-graphics, the framebuffer values were unchanged (already `0xFF`), so the waveform had no information about the previous display state (e.g. old black text pixels). The LUT cannot drive previously-black pixels to white without knowing they were black.

**Fix**: Added a `display.clear_area()` call at the start of `update_status()`. This uses AC voltage cycles (darken + lighten) to physically drive the status bar cells to white before the framebuffer-based `flush()` renders the new text. Kept the embedded-graphics white rectangle fill so that `flush()` taints and re-drives all 60 status rows consistently.

**Files changed**:
- `examples/touch_button.rs` — added `use epaper::driver::display::Rectangle as EpdRect`, added `display.clear_area(EpdRect { x: 0, y: 0, width: 960, height: STATUS_H as u16 })` at start of `update_status()`

## 2026-07-16 (touch debugging — GT911 config)

Debugged GT911 touch controller — IC communicates but digitizer not detected.

**Root cause found**: The GT911 config block had `version=0x00` (never programmed). With invalid/uninitialized config, the GT911 enters an "awaiting host configuration" state and does NOT start the scan engine (status register stays 0x00 indefinitely). Writing a valid 184-byte config block with correct checksum and 0x01 to the config-fresh register (0x8100) triggers the scan engine.

**Fix applied**: Added `Gt911::configure(i2c, x_max, y_max)` that writes a valid config with INT mode 1 (falling edge), touch threshold 0x01, 5-point max touch, and 5ms scan rate. Config readback confirms correct write: x_res=960, y_res=540, max_touch=5, int_mode=0x01.

**Outstanding hardware issue**: Even with the GT911 scanning (brief 0x80 burst observed at ~1.2s after config reload), the status register never shows count>0 during physical tapping. Tapping was confirmed during a 10s pure-poll diagnostic loop and a 2-minute main loop. This is consistent with the touch digitizer FPC cable not being connected to the GT911 module, or a board variant with GT911 populated but no digitizer attached. Hardware inspection of a second FPC connector on the board is needed.

**New files/methods added**:
- `Gt911::configure(i2c, x_max, y_max)` — writes full 184-byte config block
- `Gt911::read_config(i2c)` — reads 7 config bytes for diagnostics  
- `Gt911::read_status_raw(i2c)` — reads status without clearing (diagnostics)
- `Gt911::clear_status(i2c)` — write 0x00 to clear buffer-ready flag
- `Display::configure_touch(gt911, x_max, y_max)` — routes config write
- `Display::touch_read_config(gt911)` — routes config read
- `Display::touch_read_status_raw(gt911)` — routes raw status read
- `Display::touch_clear_status(gt911)` — routes status clear
- `Display::i2c_scan()` — scans all I2C addresses (diagnostic helper)

I2C scan reveals devices at: 0x20 (PCA9555), 0x51 (RTC), 0x55 (unknown), 0x68 (TPS65185), 0x6B (unknown). GT911 at 0x5D responds to write_read but not naked read (expected behavior).
