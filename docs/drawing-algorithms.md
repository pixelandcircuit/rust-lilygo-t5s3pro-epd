# E-Paper Drawing Algorithms

This document explains how the ED047TC1 e-paper display driver works end-to-end: from
`set_pixel` through the framebuffer and dirty-row system, through the LUT waveform engine,
and out to the hardware signals on the I8080 parallel bus and RMT row clock.

---

## Table of Contents

1. [Physical Display Overview](#1-physical-display-overview)
2. [Framebuffer Layout](#2-framebuffer-layout)
3. [Tainted (Dirty) Row Tracking](#3-tainted-dirty-row-tracking)
4. [Color Space: Gray4](#4-color-space-gray4)
5. [DrawMode](#5-drawmode)
6. [The Waveform: 15-Frame Flush Pipeline](#6-the-waveform-15-frame-flush-pipeline)
7. [LUT (Look-Up Table) System](#7-lut-look-up-table-system)
8. [DMA Buffer Preparation](#8-dma-buffer-preparation)
9. [Column Clipping: flush_clip](#9-column-clipping-flush_clip)
10. [Hardware Row Protocol](#10-hardware-row-protocol)
11. [Row Skip Optimization](#11-row-skip-optimization)
12. [Hardware Clear Cycle](#12-hardware-clear-cycle)
13. [Power Sequence](#13-power-sequence)
14. [Rotation Transform](#14-rotation-transform)
15. [Embedded-Graphics Integration](#15-embedded-graphics-integration)
16. [Double-Flush Pattern](#16-double-flush-pattern)
17. [Timing Constants](#17-timing-constants)
18. [Memory Map](#18-memory-map)

---

## 1. Physical Display Overview

The **ED047TC1** is a 960×540 4-bit grayscale e-paper panel. It uses an **I8080 8-bit
parallel bus** to receive pixel data one row at a time, with a separate **RMT-generated
CKV signal** clocking each row into the panel's shift register.

### GPIO assignments

| Signal | GPIO | Peripheral | Role |
|--------|------|-----------|------|
| D0–D7  | 5,6,7,15,16,17,18,8 | I8080 data bus | Pixel data |
| CKH    | 4  | I8080 WRX | Pixel (horizontal) clock |
| STH    | 41 | I8080 DC  | Start-horizontal strobe |
| LEH    | 42 | GPIO out  | Latch-enable horizontal (loads shift register into row drivers) |
| STV    | 45 | GPIO out  | Start-vertical (marks frame boundary) |
| CKV    | 48 | RMT CH1   | Row (vertical) clock |
| SDA    | 39 | I2C0      | PCA9555 + TPS65185 + GT911 |
| SCL    | 40 | I2C0      | (same shared bus) |

### I2C peripherals

- **PCA9555** (0x20): 16-bit I/O expander. Controls OE (output-enable), MODE (panel mode),
  PWRUP, VCOM_CTRL, WAKEUP bits for the TPS65185, and reads PWRGOOD.
- **TPS65185** (0x68): PMIC that generates the panel's high-voltage rails (VPOS, VNEG, VGH,
  VGL) and VCOM. VCOM is set to 1600 mV.
- **GT911** (0x14 or 0x5D): Capacitive touch controller, polled over the same I2C bus.

---

## 2. Framebuffer Layout

```
FRAMEBUFFER_SIZE = (WIDTH / 2) * HEIGHT = 480 * 540 = 259,200 bytes
```

The framebuffer is a **4 bits-per-pixel (4bpp)** packed array stored on PSRAM via
`Box<[u8; 259_200]>`. Two pixels share each byte in the following arrangement:

```
byte[x/2 + y*(WIDTH/2)]
  bits [3:0] = pixel at even x    (x % 2 == 0)
  bits [7:4] = pixel at odd  x    (x % 2 == 1)
```

**Initialization value:** `0xFF` (both nibbles = 0xF = white). After every `flush()` or
`flush_clip()` the framebuffer is reset back to `0xFF`, so it represents a clean white
canvas for the next drawing pass.

### set_pixel

```rust
// display.rs:112
fn set_pixel(&mut self, x: u16, y: u16, color: u8) -> Result<()>
```

1. Bounds-check `x < 960`, `y < 540`, `color <= 0xF`.
2. Calculate byte index: `index = x/2 + y*480`.
3. Even x → write to low nibble: `(value & 0xF0) | (color & 0x0F)`.
4. Odd  x → write to high nibble: `(value & 0x0F) | ((color << 4) & 0xF0)`.
5. Mark row dirty (see §3).

### fill

`fill(color)` packs the 4-bit value into both nibbles of every byte
(`color << 4 | color`) and marks all rows dirty.

---

## 3. Tainted (Dirty) Row Tracking

```
TAINTED_ROWS_SIZE = HEIGHT / 8 + 1 = 68 bytes
tainted_rows: [u8; 68]
```

One bit per row. Row `y` maps to:

```
byte  = tainted_rows[y / 8]
bit   = 1 << (y % 8)
```

**Marking a row dirty** (in `set_pixel`):
```rust
tainted_rows[y as usize / 8] |= 1 << (y % 8);
```

**Checking if a row is dirty** (in `draw_inner`):
```rust
fn is_tainted(&self, row: u16) -> bool {
    tainted_rows[row as usize / 8] & (1 << (row % 8)) != 0
}
```

**After flush:** `tainted_rows.fill(0)` — all rows reset to clean.

**Effect on performance:** The 15-frame waveform only runs its full I8080 DMA transfer
on tainted rows. Non-tainted rows receive only a fast RMT skip pulse (see §11), which is
much cheaper than a DMA transfer. Updating a small region (e.g. 100×60 px) therefore
costs roughly `(60/540)×` the time of a full-screen flush.

---

## 4. Color Space: Gray4

The display and framebuffer use 4-bit grayscale:

| Value | Meaning |
|-------|---------|
| 0x0   | Black   |
| 0x7–0x8 | Mid-gray |
| 0xF   | White   |

`embedded-graphics` maps `Gray4` to the driver via `color.luma()`, which returns the
4-bit value directly (0–15).

---

## 5. DrawMode

Three modes control waveform polarity and timing:

```rust
pub enum DrawMode {
    BlackOnWhite,   // normal rendering: dark ink on light background
    WhiteOnWhite,   // intermediate mode (same timing as BlackOnWhite but black-drive LUT default)
    WhiteOnBlack,   // clearing pass: drives all pixels toward white
}
```

| Mode            | `lut_default` | `contrast_cycles`            | LUT frame direction |
|-----------------|---------------|------------------------------|---------------------|
| `BlackOnWhite`  | `0x55` (white drive) | `CONTRAST_CYCLES_4BPP` | Reverse (15→1) |
| `WhiteOnWhite`  | `0xAA` (black drive) | `CONTRAST_CYCLES_4BPP` | Reverse (15→1) |
| `WhiteOnBlack`  | `0xAA` (black drive) | `CONTRAST_CYCLES_4BPP_WHITE` | Forward (0→14) |

**`lut_default = 0x55`** means every 2-bit slot is initialized to `01` = "drive white".
**`lut_default = 0xAA`** means every slot starts as `10` = "drive black".

`update_lut` then progressively clears slots to `00` (VCOM / no-drive) as pixels reach
their target gray level. This selective stopping is how gray shades are produced.

### Usage patterns

- After `clear()` or on a white canvas: use `BlackOnWhite` alone.
- To update over existing non-white content: use `WhiteOnBlack` followed by `BlackOnWhite`
  (see §16).

---

## 6. The Waveform: 15-Frame Flush Pipeline

`flush()` / `flush_clip()` both call `draw_inner()`, which drives the 15-frame waveform:

```
draw_inner(mode, clip_x_start, clip_x_end)
  for k in 0..15:
    update_lut(&mut lut, k, mode)    // evolve LUT for this frame
    frame_start()                    // hardware: STV pulse + OE enable
    for y in 0..540:
      if !is_tainted(y):
        row_skip(contrast_cycles[k])
      else:
        buf = prepare_dma_buffer(framebuffer[row], &lut)
        if clipping:
          clip_dma_buffer(&mut buf, x_start, x_end)
        set_buffer(&buf)
        row_write(contrast_cycles[k])
    frame_end()                      // hardware: OE + MODE disable
```

After `draw_inner` returns, the caller resets both `tainted_rows` and `framebuffer` to
their initial states (`0x00` and `0xFF` respectively).

### What "a frame" means physically

Each of the 15 frames is one complete scan of the panel from top to bottom. The RMT
peripheral generates the CKV (vertical clock) pulses that advance the panel's row
pointer, and the I8080 DMA transfer loads the pixel drive codes for each row. The duration
of each CKV pulse (the `contrast_cycles[k]` value in microseconds) controls how long the
panel's pixel electrodes are energized for that frame.

---

## 7. LUT (Look-Up Table) System

The LUT is the heart of 4-bit grayscale rendering on e-paper.

### Structure

```
Vec<u8> of length 65536   (1 << 16)
```

It is indexed by a `u16` formed from **two adjacent framebuffer bytes** (= 4 pixels × 4
bits = 16 bits). Each entry is a `u8` encoding four **2-bit waveform codes**, one per
pixel.

```
lut[px3 px2 px1 px0 (packed as u16)] = [code3 code2 code1 code0 (2 bits each)]
```

### 2-bit waveform codes

| Code | Meaning |
|------|---------|
| `00` | VCOM — no drive, pixel retains current state |
| `01` | Drive white (positive voltage) |
| `10` | Drive black (negative voltage) |
| `11` | VCOM (same as 00 in practice) |

### LUT initialization

At the start of each flush, the LUT is filled with `mode.lut_default()`:
- `BlackOnWhite`: `0x55` = all four pixels get code `01` (drive white).
- `WhiteOnBlack`/`WhiteOnWhite`: `0xAA` = all four pixels get code `10` (drive black).

### LUT evolution: `update_lut`

```rust
fn update_lut(lut: &mut [u8], k: usize, mode: DrawMode)
```

Each call to `update_lut` clears certain LUT entries' 2-bit slots to `00` (VCOM). The
logic works by iterating through the index space and masking off specific nibble positions.

The effective frame counter `k` is:
- `BlackOnWhite` / `WhiteOnWhite`: `k = DRAW_IMAGE_FRAME_COUNT - frame_index` (counts
  down from 15 to 1 as frames progress).
- `WhiteOnBlack`: `k = frame_index` (counts up from 0 to 14).

For each of the four nibble positions in the 16-bit index:
- Nibble 0 (bits 3:0): entries at stride 16 with index ≥ k get their bits [1:0] cleared.
- Nibble 1 (bits 7:4): entries with nibble1 ≥ k get bits [3:2] cleared.
- Nibble 2 (bits 11:8): entries with nibble2 ≥ k get bits [5:4] cleared.
- Nibble 3 (bits 15:12): entries with nibble3 ≥ k get bits [7:6] cleared.

**Effect:** A pixel with a high gray value (near white, e.g. 0xE) stops being driven toward
white earlier (lower k threshold) than a pixel with a low gray value (near black, e.g. 0x2).
Darker pixels continue to receive drive codes until later frames. This is how the waveform
achieves 16 distinct gray shades despite e-paper's inherently binary-switching pixels.

### Why 65536 entries?

Two adjacent framebuffer bytes = 4 pixels × 4 bits = 16 bits total. Indexing by the raw
16-bit value means the LUT lookup is a single array dereference per 4 pixels, and the
entire 65536-entry table stays L1/L2-cache-warm during the tight inner loop of
`prepare_dma_buffer`.

---

## 8. DMA Buffer Preparation

```rust
fn prepare_dma_buffer(line_data: &[u8], lut: &[u8]) -> Vec<u8>
```

Converts one row of framebuffer data (480 bytes, 960 pixels) into the 240-byte DMA
buffer consumed by the I8080 peripheral.

### Step 1: Reinterpret as `u16` chunks

The 480 input bytes are read in pairs, forming 240 `u16` values. Each `u16` spans 4
pixels (two packed nibble-pairs).

### Step 2: LUT lookup in groups of four

The 240 `u16` values are processed in groups of 4 (= 16 pixels per group). Each `u16`
is looked up in the LUT to get one byte (= 4 drive codes). The 4 result bytes are
packed into a `u32`:

```
wide_epd_input[j] = lut[v1] | lut[v2]<<8 | lut[v3]<<16 | lut[v4]<<24
```

This produces 60 `u32` values covering all 960 pixels (60 × 16 = 960).

### Step 3: Flatten to bytes

The 60 `u32` values are written back as 240 bytes in little-endian order.

### Step 4: Bit reversal

The ED047TC1 panel expects **MSB-first** within each byte: bits 7–6 = leftmost pixel,
bits 5–4 = next, bits 3–2 = next, bits 1–0 = rightmost. The LUT produces the opposite
order (LSB-first). Every byte is therefore reversed at the 2-bit pair level:

```rust
*byte = ((b & 0x03) << 6)   // pair 0 (was bits 1:0) → bits 7:6
      | ((b & 0x0C) << 2)   // pair 1 → bits 5:4
      | ((b & 0x30) >> 2)   // pair 2 → bits 3:2
      | ((b & 0xC0) >> 6);  // pair 3 (was bits 7:6) → bits 1:0
```

### Output

240 bytes = 960 pixels × 2 bits/pixel. This is exactly `BYTES_PER_LINE` and fits inside
the `DMA_BUFFER_SIZE = 248` byte hardware DMA buffer.

---

## 9. Column Clipping: flush_clip

```rust
pub fn flush_clip(&mut self, mode: DrawMode, clip: Rectangle) -> Result<()>
```

`flush_clip` confines the waveform to a rectangular sub-region. The dirty-row bitmap
handles the **row axis** (vertical clipping) automatically: only rows touched by drawing
calls will be tainted. `flush_clip` adds **column axis** clipping by masking the DMA
buffer.

### clip_dma_buffer

```rust
fn clip_dma_buffer(buf: &mut [u8], x_start: u16, x_end: u16)
```

After the bit reversal in `prepare_dma_buffer`, each output byte encodes 4 pixels:

```
byte b → pixels b*4+0 (bits 7:6), b*4+1 (bits 5:4), b*4+2 (bits 3:2), b*4+3 (bits 1:0)
```

For bytes entirely outside `[x_start, x_end)`, the whole byte is zeroed (`0x00`). For
bytes that straddle a boundary, individual 2-bit pairs outside the range are masked to
`00` (VCOM — no drive).

Setting a pixel to VCOM means the panel never drives it in that frame, so pixels outside
the clip column range are left completely undisturbed.

### Why this matters

The panel always scans full rows. Even on a tainted row, pixels to the left and right of
the update region would receive drive codes and could shift, producing visible gray
"ghost bands." `flush_clip` prevents this by silencing out-of-clip pixels.

---

## 10. Hardware Row Protocol

Each row transmission follows this sequence:

### frame_start

1. PCA9555: assert `MODE` bit.
2. RMT: one CKV pulse (10 µs high, 10 µs low) — wait.
3. GPIO: `STV` low → busy-delay ~1 µs → start RMT CKV pulse → `STV` high. This STV
   low-to-high transition signals the panel to begin a new frame.
4. RMT: one CKV pulse (low only, 100 µs) — wait.
5. PCA9555: assert `OE` (output-enable) bit.
6. RMT: one CKV pulse (10/10 µs) — wait.

### output_row(output_time)

1. `latch_row()`: toggle `LEH` high then immediately low. This loads the shift-register
   contents into the row's pixel drivers.
2. Start RMT CKV pulse (`output_time` µs high, 50 µs low) — do **not** wait yet.
3. Simultaneously start I8080 DMA transfer of the 240-byte row buffer. The I8080
   peripheral generates STH + CKH signals to clock all 960 pixel values into the row's
   shift register during the CKV high phase.
4. Wait for DMA transfer to complete.
5. Wait for RMT pulse to complete.

The RMT pulse duration (`output_time` µs) is the per-frame contrast time that determines
how long the pixels are actually energized. Longer = stronger drive = needed for darker
shades or later waveform frames.

### frame_end

1. PCA9555: clear `OE` and `MODE` bits.
2. RMT: two CKV pulses (10/10 µs each) — wait.

---

## 11. Row Skip Optimization

When a row is not tainted, `row_skip` is called instead of `row_write`.

```rust
fn row_skip(&mut self, output_time: u16) -> Result<()> {
    match self.skipping {
        0 => {
            set_buffer([0u8; BYTES_PER_LINE]);  // zero data = VCOM for all pixels
            output_row(output_time);            // full DMA, but with all-zero data
            set_output_enabled(false);         // blank after pending row completes
        }
        1 => {
            output_row(10);                     // short pulse, no data change
        }
        _ => {
            skip();                             // fast RMT-only: 45µs high, 5µs low
        }
    }
    self.skipping += 1;
}
```

The `skipping` counter tracks consecutive non-tainted rows:

- **Row 0** (first skip): Sends a zero-data DMA buffer at full timing. This clears any
  residual data in the I8080/panel shift register from the previous tainted row.
- **Row 1**: A short `output_row(10)` that advances the panel row pointer with minimal
  drive.
- **Row 2+**: `skip()` — just a fast RMT CKV pulse (45/5 µs), no I8080 activity at all.
  The pulse is awaited so the RMT channel is retained for the next row.

`row_write` resets `skipping = 0` and re-enables outputs, so the transition protocol
restarts whenever the scan reaches the next tainted row. Hardware clear frames
also reset `skipping` at frame start.

These measures reduce disturbance but do not guarantee stable untouched pixels.
See [Minesweeper refresh notes](minesweeper-refresh.md) for observed fading/darkening,
the full-refresh workaround, and unresolved VCOM calibration.

---

## 12. Hardware Clear Cycle

```rust
pub fn clear(&mut self) -> Result<()>
pub fn clear_area(&mut self, area: Rectangle) -> Result<()>
```

`clear()` does **not** touch the framebuffer. It performs a hardware conditioning cycle
that physically resets all pixels on the panel to white by alternating between all-zero
and all-one pixel data.

```
clear_cycles(area, 4 cycles, 50 µs per row)
  for _ in 0..4:
    4 × push_pixels(area, 50, 0)   // drive all pixels white
    4 × push_pixels(area, 50, 1)   // drive all pixels black
```

`push_pixels` builds a row buffer with uniform 2-bit codes (either `01`=white or
`10`=black for every pixel), applies `line_buffer_reorder` (swaps 16-bit halves within
each 32-bit word to match the panel's byte-order expectation for this direct path), and
sends it for every row in the area.

The 4×8 = 32 alternating passes ensure the panel's bistable pixels are fully cycled and
any accumulated charge is neutralized, giving a clean white baseline before rendering.
This should always be called after `power_on()` and before the first `flush()`.

### line_buffer_reorder

Used only in the clear path (`push_pixels`). Swaps the two 16-bit halves of each 32-bit
word in the buffer:

```rust
val = [b0, b1, b2, b3] as u32 (little-endian)
→ [b2, b3, b0, b1]
```

This reordering corrects for the panel's shift-register bit ordering when using the
direct pixel-push path (vs. the LUT path in `prepare_dma_buffer` which handles ordering
differently via the bit-reversal step).

---

## 13. Power Sequence

### power_on

1. PCA9555 → assert `WAKEUP`.
2. PCA9555 → assert `PWRUP`.
3. PCA9555 → assert `VCOM_CTRL`.
4. Poll `PWRGOOD` bit on PCA9555 port-1 input (up to 500 retries, ~1 ms apart).
5. TPS65185 → write `0x3F` to ENABLE register (all rails on).
6. TPS65185 → write VCOM = 1600 mV (split across VCOM1/VCOM2 registers: `val = 160 = 0xA0`).
7. Poll TPS65185 PG register until bits [7:1] all high (`& 0xFA == 0xFA`).

### power_off

1. PCA9555 → set only `WAKEUP` (clear VCOM_CTRL, PWRUP, OE, MODE).
2. Wait ~1 ms.
3. PCA9555 → clear all bits (WAKEUP included).

---

## 14. Rotation Transform

`set_rotation()` stores a `DisplayRotation` variant. All drawing goes through
`translate_coord_rotation()` in the `DrawTarget` impl:

```
Rotate0:   (x, y)           → (x, y)                      (landscape, normal)
Rotate90:  (x, y)           → (WIDTH-1-y, x)               (portrait, CW)
Rotate180: (x, y)           → (WIDTH-1-x, HEIGHT-1-y)     (landscape, flipped)
Rotate270: (x, y)           → (y, HEIGHT-1-x)              (portrait, CCW)
```

`OriginDimensions::size()` returns swapped WIDTH/HEIGHT for 90° and 270° rotations, so
`embedded-graphics` primitives and text rendering work correctly against the logical
canvas size.

The physical framebuffer is always stored in the panel's native 960×540 orientation;
rotation is purely a coordinate transform at the pixel-write level.

---

## 15. Embedded-Graphics Integration

`Display` implements `embedded_graphics_core::DrawTarget`:

```rust
type Color = Gray4;   // 4-bit grayscale, values 0–15
type Error = Error;
```

`draw_iter` iterates the pixel stream, applies the rotation transform, and calls
`set_pixel`. Out-of-bounds pixels (after rotation, coordinates that fall outside
960×540) are silently discarded rather than returning an error, which matches
`embedded-graphics` convention for clipped drawing.

The `Clear` operation (`DrawTarget::clear`) maps to `Display::fill`, which fills the
entire framebuffer and marks all rows dirty.

---

## 16. Double-Flush Pattern

E-paper pixels are bistable — they hold their last driven state. When rendering new
content over an area that already has dark pixels, driving it `BlackOnWhite` alone would
leave ghost artifacts where old dark pixels resist the new white background.

The correct update sequence for content changes over non-white backgrounds:

```rust
// 1. Draw what you want to render into the framebuffer
draw_something(&mut display);

// 2. Erase pass: drive all dirty rows toward white
//    (clears old dark pixels from the panel)
display.flush(DrawMode::WhiteOnBlack).unwrap();

// flush() reset the framebuffer to 0xFF — re-draw into the clean slate
draw_something(&mut display);

// 3. Render pass: drive pixels to their final gray values
display.flush(DrawMode::BlackOnWhite).unwrap();
```

With `flush_clip`, both passes can be confined to the changed rectangle:

```rust
let clip = EpdRect { x, y, width: w, height: h };
draw_frame(&mut display, rect, fill, text_color);
display.flush_clip(DrawMode::WhiteOnBlack, clip).unwrap();
draw_frame(&mut display, rect, fill, text_color);
display.flush_clip(DrawMode::BlackOnWhite, clip).unwrap();
```

### Why two draws?

`flush()` resets `framebuffer` to `0xFF` after transmitting. The second draw is
therefore mandatory — the framebuffer after the erase pass is blank.

---

## 17. Timing Constants

### CONTRAST_CYCLES_4BPP (BlackOnWhite / WhiteOnWhite)

```
Frame:  0    1    2    3    4    5    6    7    8    9   10   11   12   13   14
µs:    30   30   20   20   30   30   30   40   40   50   50   50  100  200  300
```

The LUT counts **down** from frame 15, so frame 0 in the loop corresponds to the
strongest drive (k=15 → most entries still active), and frame 14 is the final fine-tune
(k=1 → almost all entries cleared). The longer durations in later frames (100–300 µs)
ensure the darkest pixel shades (requiring the most charge) are fully driven.

### CONTRAST_CYCLES_4BPP_WHITE (WhiteOnBlack clearing)

```
Frame:  0    1    2    3    4    5    6    7    8    9   10   11   12   13   14
µs:    10   10    8    8    8    8    8   10   10   15   15   20   20  100  300
```

Shorter overall (8–20 µs for most frames, vs 20–50 µs above) because clearing to white
requires less charge than driving to black — e-ink pixels are naturally biased toward
white and respond faster in that direction. The LUT counts **up** for `WhiteOnBlack`.

---

## 18. Memory Map

| Region | Size | Location | Contents |
|--------|------|----------|---------|
| Framebuffer | 259,200 B (~253 KB) | PSRAM heap | 4bpp pixel data, 2px/byte |
| Tainted rows | 68 B | Embedded SRAM | 1-bit dirty flag per row |
| LUT | 65,536 B (64 KB) | PSRAM heap (Vec) | Waveform codes per 4-pixel group |
| DMA buffer | 248 B | Embedded SRAM (static) | One row at 2bpp = 240 bytes used |
| LUT + DMA are reallocated each flush; all other state is persistent in `Display`. |

The framebuffer **must** live on PSRAM due to its size (253 KB exceeds on-chip SRAM).
The `psram_allocator!` macro must therefore be called before `Display::new`.
