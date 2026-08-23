# Adding Som Rich Protocol (SRP) support to a third-party TUI

This is a spec-and-implementation guide for developers who want their own
terminal application (a file manager, a pager, a chat client — anything
running inside a PTY) to display images through Som's own graphics
protocol, the same way it might already support the Kitty graphics
protocol or Sixel. If you're looking for Som's own internal design
history and decision log instead, see `SRP_PROTOCOL.md` in this
repository — this document is the external-facing spec extracted from
that journal, aimed at someone who has never seen Som's source before.

The reference implementation this guide walks through lives in
[`errordnk/yazi`](https://github.com/errordnk/yazi), a fork of the
[yazi](https://github.com/sxyazi/yazi) terminal file manager, specifically
`yazi-adapter/src/drivers/srp.rs`. Read that file alongside this guide —
every section below names the exact function that implements it.

## Why a protocol integration, not just "print an image"

If you already support Kitty's graphics protocol, you might reasonably
ask why SRP needs separate code instead of just detecting Som and
speaking Kitty to it. Two real, unavoidable reasons:

1. **Kitty's protocol is base64-only over the wire**, which on a large
   animated GIF produces multi-megabyte payloads and multi-second display
   latency — acceptable for a single screenshot, not for smooth
   animation. SRP uses base91 encoding instead (~1.23x overhead vs
   base64's ~1.33x) and, more importantly, streams the source file
   progressively rather than re-encoding every frame to PNG first.
2. **Windows ConPTY is not a transparent byte pipe.** Any protocol that
   puts raw bytes `>= 0x80` on the wire gets silently corrupted by the
   active console codepage before this data ever reaches the reading
   terminal (see "Why every wire byte must be base91" below) —
   independent of any particular parser's own byte-range restrictions.
   Kitty's protocol survives this because it already commits to base64;
   SRP was designed the same way from the start, with a cheaper alphabet.

If your application only needs to work on Unix, you could in principle
implement Kitty's protocol instead and get the same visual result. SRP
exists because Som itself needed a protocol that (a) never re-encodes
frames, for animation performance, and (b) survives Windows ConPTY
byte-for-byte. If you're implementing SRP specifically (not just "some
graphics protocol"), the format below is what Som's receiving side
actually parses — matching it exactly is not optional.

## Detecting that you're running inside Som

Som sets `SOM_WINDOW_ID` (any local terminal spawn, and any remote
session reached through `som-tmux`) to the PID of the Som process, the
same role `KITTY_WINDOW_ID` plays for Kitty. Presence of the variable —
not its specific value — is the capability signal:

```rust
let inside_som = std::env::var_os("SOM_WINDOW_ID").is_some();
```

Som also sets `TERM_PROGRAM=zed` (a historical artifact of Som's Zed
fork lineage, not something SRP-aware code should rely on) and
`TERM=xterm-256color`. Detect SRP support via `SOM_WINDOW_ID`
specifically, not `TERM_PROGRAM` — the latter is shared with actual Zed
and carries no SRP capability information.

There is no query/response capability negotiation (no equivalent of
Kitty's "does the terminal support the graphics protocol" APC probe) —
SRP has exactly one receiving implementation (Som itself), so the
environment variable is the whole detection story. If `SOM_WINDOW_ID`
isn't set, don't send SRP envelopes; Som isn't there to receive them and
no other terminal understands this wire format.

## Envelope format

Every SRP transmission is one or more envelopes, each wrapped in a
standard terminal APC (Application Program Command) string:

```
ESC _ S <header_b91> <SEPARATOR> <payload_b91> ESC \
```

- `ESC _` (`0x1B 0x5F`) opens an APC string — this is standard
  ECMA-48/ANSI terminal syntax, the same opener Kitty's own protocol
  uses (which instead follows it with `G`).
- The leading `S` (`0x53`) right after `ESC _` is SRP's marker byte —
  what a receiver checks first to route the string to this parser instead
  of Kitty's (`G`) or anything else. The two are mutually exclusive by
  construction: if the byte isn't `S`, this isn't an SRP envelope.
- `header_b91` — the fixed-length header (see below), base91-encoded.
- `<SEPARATOR>` — a single literal `0x20` (space) byte, NOT base91-encoded.
  See "Why a separator byte, not a fixed offset" below for why this can't
  be a computed byte offset.
- `payload_b91` — the chunk's raw file bytes, base91-encoded.
- `ESC \` (`0x1B 0x5C`) closes the APC string — the standard ECMA-48
  string terminator (ST).

### Header fields (before base91 encoding)

| Field | Size | Notes |
|---|---|---|
| `version` | 1 byte | Currently always `1`. A receiver seeing any other value should reject the envelope. |
| `content_type` | 1 byte | `0`=Gif, `1`=Audio (reserved), `2`=Markdown (reserved), `3`=Video (reserved), `4`=Jpeg, `5`=Png |
| `session_id` | 4 bytes, little-endian | See "Session and file ids" below |
| `file_id` | 4 bytes, little-endian | See "Session and file ids" below |
| `chunk_offset` | 8 bytes, little-endian | Byte offset of this chunk's payload within the full file |
| `chunk_len` | 4 bytes, little-endian | Length of the RAW (pre-base91) payload, not its encoded wire size |
| `total_size` | 8 bytes, little-endian | Total file size if known upfront; `0` means unknown |
| `metadata` | 18 bytes | Encoded `ContentMetadata` — see below |
| `crc32` | 4 bytes, little-endian | CRC32 (IEEE 802.3 polynomial) over the raw payload bytes |

Total header size: 1+1+4+4+8+4+8+18+4 = **52 bytes**, before base91
encoding.

`chunk_len` and `crc32` always describe the raw file bytes, never the
base91-encoded wire size — a receiving implementation's downstream
decoder (GIF/JPEG/PNG parser) works exclusively in terms of real file
bytes and has no reason to know a wire encoding exists at all.

### `ContentMetadata` — 18 bytes, fixed length regardless of variant

`ContentMetadata` is a tagged union: 1 discriminant byte, followed by 17
bytes of fields, zero-padded to that fixed length for any variant that
doesn't use all 17 (only the `Video` variant, reserved and not yet wired
up on Som's receiving end, uses the full 17). If you're only sending
still/animated images (the only content type any current client speaks),
you only ever need the `Image` variant:

| Discriminant | Variant | Fields (in order, all little-endian) |
|---|---|---|
| `0` | `Image` | `width_px: u32`, `height_px: u32`, `color_bits: u8`, `is_animated: u8` (0 or 1) — 10 bytes used, 7 bytes zero-padding |
| `1` | `Audio` (reserved) | `sample_rate: u32`, `channels: u8`, `bits_per_sample: u8` |
| `2` | `Video` (reserved) | `width_px: u32`, `height_px: u32`, `fps_numerator: u32`, `fps_denominator: u32`, `codec: u8` |
| `3` | `Markdown` (reserved) | no fields |

`width_px`/`height_px` are the image's real decoded pixel dimensions
(from the file's own format header — GIF's logical screen descriptor,
JPEG/PNG's own metadata — not anything about compressed file size).
`color_bits` should be `32` if you're sending RGBA-equivalent data (every
pixel SRP transmits ends up as RGBA/BGRA on the receiving end regardless
of the source file's own on-disk color depth). `is_animated` should be
`1` if the source file has more than one frame.

These fields travel on **every** chunk of a given transmission, not just
the first — this keeps the envelope format uniform (a receiver never
special-cases "the first chunk looks different") at the cost of a few
redundant bytes per chunk, which base91's own per-byte overhead already
dwarfs.

### Why every wire byte must be base91, not raw binary

This is the single most important thing to get right, and the reason
this protocol exists in the form it does. **Windows ConPTY is not a
transparent byte pipe for a child process's stdout.** `conhost.exe`/
`OpenConsole.exe` on the other end interprets a child process's raw
output through the **active console output codepage** (a real
per-character-cell text buffer, not byte-oriented) and re-encodes it to
UTF-8 before the reading terminal's PTY reader ever sees it. Any byte
`>= 0x80` gets silently reinterpreted as whatever character that
codepage maps it to, then re-emitted as a multi-byte UTF-8 sequence —
turning one source byte into two or three wire bytes, with no way to
detect after the fact where the damage happened.

This was confirmed experimentally, not assumed: a raw payload sent
through a real Windows child process over a real ConPTY pseudo-console
consistently arrived larger than sent, with predictable corruption tied
to the active codepage (e.g. CP866 at diagnosis time). Switching the
child's console output codepage to UTF-8 (`SetConsoleOutputCP(65001)`)
does **not** fix this — it only changes the failure mode, since arbitrary
binary data usually isn't valid UTF-8 either.

Every SRP wire byte therefore comes from a 91-symbol alphabet: every
printable ASCII byte `0x21..=0x7E` (94 values) except `"`, `'`, and `\`
(91 symbols total). This range is safe specifically because it's
ASCII-compatible across every common single-byte Windows codepage — a
byte in this range maps to the identical character under any codepage
reinterpretation, so nothing gets mangled. `\` is excluded to keep it
visually/logically away from this protocol's own `ESC \` terminator
(though the parser only ever looks for the literal two-byte pair, not a
bare `\`); `"`/`'` are excluded as a general precaution around common
shell/JSON quoting, not because they're independently unsafe on this
wire.

If you're implementing a **receiver** for a different terminal, base91 is
non-negotiable for the same reason — any wire encoding that permits bytes
`>= 0x80` will get corrupted the identical way over ConPTY. If you're
implementing a **sender** targeting Som specifically (Som's receiver only
speaks base91), you have no choice regardless of what platform you're
running on: match the format below exactly.

### Base91 encoding algorithm

Standard basE91 bit-packing: bits are packed LSB-first into a growing
buffer, and every time the buffer holds more than 13 bits, two output
symbols are emitted, consuming either 13 or 14 bits depending on whether
the low 13 bits alone exceed 88 (`13 bits -> 8192` max value vs
`91*91-1 = 8280`, so most 13-bit values fit two digits cleanly; values
above 88 in the low 13 bits borrow a 14th bit instead — the same
trade-off the reference basE91 implementation uses).

```rust
fn base91_encode(data: &[u8], alphabet: &[u8; 91]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut bit_buf: u32 = 0;
    let mut bit_count: u32 = 0;
    for &byte in data {
        bit_buf |= (byte as u32) << bit_count;
        bit_count += 8;
        if bit_count > 13 {
            let mut v = bit_buf & 8191; // low 13 bits
            if v > 88 {
                bit_buf >>= 13;
                bit_count -= 13;
            } else {
                v = bit_buf & 16383; // low 14 bits
                bit_buf >>= 14;
                bit_count -= 14;
            }
            out.push(alphabet[(v % 91) as usize]);
            out.push(alphabet[(v / 91) as usize]);
        }
    }
    if bit_count > 0 {
        out.push(alphabet[(bit_buf % 91) as usize]);
        if bit_count > 7 || bit_buf > 90 {
            out.push(alphabet[(bit_buf / 91) as usize]);
        }
    }
    out
}
```

You only need the encoder if you're only ever sending to Som (the common
case — a decoder is only needed if you're also parsing SRP envelopes
coming FROM Som, which no known client currently does).

### Why a separator byte, not a fixed offset

basE91's bit-packing means the ENCODED length of a fixed-size input
varies with the actual bit pattern of the data (13 or 14 bits consumed
per output symbol pair, decided by the data itself) — there is no fixed
byte offset a receiver could slice the header out at, unlike a
hex-encoded format would allow. A literal `0x20` (space) separates
`header_b91` from `payload_b91` because it sits directly below the
base91 alphabet's lower bound (`0x21`) and can never appear inside either
base91-encoded region — a receiver scans for the first `0x20` after the
marker byte and splits there unambiguously.

## Session and file ids

`session_id`/`file_id` are both sender-assigned 32-bit values, masked to
24 bits (`& 0xFF_FFFF`, and clamped to a minimum of 1 — 0 is reserved as
"no id"). The reference implementation derives both from the current
Unix timestamp in milliseconds, with `file_id` further scrambled by a
cheap multiplicative hash to decorrelate it from `session_id`:

```rust
let now_ms = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_millis() as u32)
    .unwrap_or(1);
let session_id = (now_ms & 0xFF_FFFF).max(1);
let file_id = (now_ms.wrapping_mul(2_654_435_761) & 0xFF_FFFF).max(1);
```

The 24-bit mask matters for a concrete reason, not just tidiness: both
ids get painted directly into the placeholder grid's cell colors (see
below), which only have 24 bits of RGB to work with. An unmasked id would
silently lose its high byte on the round trip through the grid, so the
id painted next to the image would never match the id the sender actually
used — Som's receiving side would then fail to find the cached image data
for what LOOKS like a valid placement.

Two separate invocations of your program should very rarely collide on
`(session_id, file_id)` — millisecond-timestamp derivation is good enough
for this (not cryptographically unique, just "unique enough in practice
that a real collision is not something users will hit").

## Streaming a file

Split the raw file bytes into fixed-size chunks (the reference
implementation uses 3800 bytes per chunk — chosen empirically as a size
that keeps individual APC strings comfortably below any terminal's likely
internal buffer limits, not a value the wire format itself requires) and
send one envelope per chunk, in order, with `chunk_offset` tracking the
running byte offset:

```rust
const CHUNK_SIZE: usize = 3800;
let mut offset = 0u64;
for payload in file_bytes.chunks(CHUNK_SIZE) {
    let envelope = build_envelope(content_type, session_id, file_id,
        offset, file_bytes.len() as u64, width_px, height_px,
        is_animated, payload);
    write_apc_string(&envelope); // ESC _ <envelope> ESC \
    offset += payload.len() as u64;
}
```

No frame re-encoding happens anywhere in this path — the exact bytes of
the source GIF/JPEG/PNG file go on the wire, chunk by chunk, unmodified
except for the base91 wire encoding. Som's receiving side writes each
chunk to a local cache file and, for GIF specifically, can begin decoding
before the whole file has arrived (JPEG/PNG have no progressive-prefix
decode story, so Som's receiver waits for the complete file before
decoding either).

### Don't block your client's own UI on the transfer

This matters for any client embedded in a larger interactive
application (a file manager's preview pane, a chat client rendering an
inline attachment) — not for a standalone one-shot tool like `somcat`,
which has nothing else running to block.

Som's receiving side is already designed to start decoding/playing
before a transfer finishes (see the previous paragraph, and
`SRP_PROTOCOL.md`'s own benchmark proving the first partial GIF decode
happens before the full file has arrived). A client throws this
property away if it makes its own caller wait for the entire chunk loop
to finish before doing anything else — the file transfer becomes the
bottleneck for the client's OWN responsiveness, independent of how fast
Som itself can decode.

The reference yazi driver's `image_show` learned this the hard way:
running the full base91-encode-and-write loop synchronously inside the
same `async fn` yazi's preview machinery awaits stalled yazi's entire UI
redraw loop for the whole transfer (both are on the same shared tokio
runtime) — a ~1MB GIF took upward of two seconds to even start
appearing, even though Som itself could have shown the first frame in a
fraction of that. The fix has two parts, and both matter independently:

1. **Print the placeholder grid before sending a single byte of the
   file**, not after the transfer completes. Its footprint only depends
   on `width_px`/`height_px` — already known from the format header
   probe, before any chunk has gone out — so there's no reason to wait.
   This is also what Som is watching: it starts producing decoded output
   into the cache file as chunks arrive, independent of when the
   placeholder text appeared on screen. Whichever side (client-code-
   returning vs. Som-decoding) is slower for a given file, they're no
   longer serialized behind each other.
2. **Send the chunk loop from a detached/background task** (Rust:
   `tokio::spawn`, not `tokio::task::spawn_blocking` awaited inline —
   `spawn_blocking` alone fixes CPU-starvation of other async tasks but
   still makes the caller wait for the `.await` to resolve; the two
   together are what actually decouples "is this file done sending" from
   "did my UI just get its area back"). A transfer failure in a detached
   task has nowhere to report to except a log line — that's an accepted
   tradeoff, not an oversight: the alternative (blocking on the result to
   propagate a `Result`) reintroduces the exact stall this is meant to
   avoid, and a failed transfer is already visible to the user as "the
   placeholder never filled in."

The general shape, once a language/framework's client has its own
concept of "detached background task" and "CPU-bound work off the UI
thread" (most async runtimes and most GUI event loops do, under some
name): resolve geometry synchronously, print the placeholder
synchronously, hand the byte-pushing loop to whatever your framework's
equivalent of a background worker is, and return control to your own
caller immediately after the placeholder is on screen — not after the
worker finishes.

This same shape is what audio/video support will need once
`ContentType::Audio`/`ContentType::Video` are wired up on Som's
receiving end (see `SRP_PROTOCOL.md`'s "Следующие нереализованные
этапы" section) — starting playback from the first buffered samples/
frames rather than waiting for a whole audio/video file to finish
transferring is the same "don't block on the full transfer" principle
this section describes for images, just with a decoder that consumes
the cache file continuously instead of one that decodes it once.

## Placement: the Unicode-placeholder grid technique

This is the part that makes SRP (and Kitty's own "Unicode placeholders"
extension, which SRP reuses the encoding scheme from) genuinely different
from most terminal graphics protocols: **the image isn't anchored to a
pixel or cell position tracked out-of-band from the terminal's own text
model.** Instead, the image's position and size are encoded directly into
ordinary grid cells, printed as real text through the same stdout the
rest of your program's output goes through.

Concretely: after streaming a file, print an N×M block of cells (N =
columns, M = rows) where every cell contains:

1. The placeholder character `U+10EEEE` (a Private Use Area codepoint).
2. Two combining diacritics from a fixed 297-entry table (see below),
   encoding this cell's `(row, column)` offset within the placement,
   relative to the block's own top-left corner.
3. A foreground color whose 24-bit RGB value equals `session_id`.
4. An underline color whose 24-bit RGB value equals `file_id`.

```rust
// Foreground carries session_id, underline color carries file_id.
fn id_to_rgb(id: u32) -> (u8, u8, u8) {
    (((id >> 16) & 0xFF) as u8, ((id >> 8) & 0xFF) as u8, (id & 0xFF) as u8)
}

let (sr, sg, sb) = id_to_rgb(session_id);
let (fr, fg, fb) = id_to_rgb(file_id);
print!("\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m");
for row in 0..rows {
    for col in 0..columns {
        print!("\u{10EEEE}");
        print!("{}", DIACRITICS[row as usize]); // row diacritic first
        print!("{}", DIACRITICS[col as usize]); // then column diacritic
    }
    if row + 1 < rows { print!("\r\n"); } // hard newline, not soft wrap — see below
}
print!("\x1b[0m\r\n");
```

The 297-entry `DIACRITICS` table is Kitty's own published reference
table
(<https://sw.kovidgoyal.net/kitty/_downloads/f0a0de9ec8d9ff4456206db8e0814937/rowcolumn-diacritics.txt>).
It's reused verbatim, not reinvented — this is the same encoding Kitty's
own `U=1` unicode-placeholder mode uses, and the reference yazi driver
duplicates it identically to the existing Kitty driver in the same
codebase (`yazi-adapter/src/drivers/diacritics.rs` in the fork).

### Why this works: the placement IS text, not an overlay

The entire reason this technique exists, and the reason SRP switched to
it after an earlier cursor-anchored design (documented in
`SRP_PROTOCOL.md`'s "Paint-path v1" section) was explicitly rejected: a
block of cells printed this way goes through the terminal's completely
ordinary text-handling machinery. Scroll, resize, and `clear` all work
correctly for free, because from the terminal's point of view, this is
just text someone printed — there's no separate anchor/position tracking
to keep in sync with the terminal's own state:

- `clear` hides the image simply because it's part of the cleared text.
- The cursor naturally lands after the printed block, because that's
  where any program's cursor lands after printing N lines of text.
- Scrolling moves the image together with surrounding text, because it
  IS surrounded text, not an overlay painted at a screen coordinate.

**Use a hard `\r\n` between rows, not the terminal's own soft-wrap.** A
terminal's line-wrap always reflows to the terminal's CURRENT width, not
the image's — if the window is wider than the placement, soft-wrap would
stretch it edge-to-edge, distorting the aspect ratio. Om's own resize
handling (`Terminal::resync_rich_content_placements`) actively re-derives
and rewrites this grid's cells on every resize instead of relying on
reflow; a client-side implementation on a receiving terminal without that
active-resize machinery would need equivalent logic, or would simply
leave a placement at its originally-printed size until the terminal is
cleared and the image reprinted.

## Sizing the placement: real terminal cell metrics, not assumptions

`width_px`/`height_px` (from the source file's format header) are real
physical file pixels. A terminal's cell size in pixels is a completely
separate, DPI-dependent quantity you must query, not assume — dividing
one by the other using an assumed cell size will produce a `columns`
count that's wrong on any display where DPI scale isn't exactly 1.0, and
a too-wide placement gets silently wrapped mid-row by the real terminal,
scrambling every cell's encoded row/column.

If your program (like yazi) already has its own terminal-cell-size
detection for other purposes, use that. If not, Som answers two standard
DEC private-mode queries the same way any modern terminal emulator does:

- `CSI 16 t` ("report cell size in pixels") → reply `ESC [ 6 ; height ;
  width t`
- `CSI 18 t` ("report text area size in characters") → reply `ESC [ 8 ;
  lines ; cols t`

```
columns = ceil(width_px / cell_width_px)
rows    = ceil(height_px / cell_height_px)
```

Then clamp against the terminal's own reported character grid size
(queried via `CSI 18 t`), not just the pixel math above — this is what
actually prevents the wrap-scrambling problem, independent of whatever
DPI mismatch caused the pixel math to be wrong in the first place:

```rust
if columns > terminal_columns {
    let scale = terminal_columns as f64 / columns as f64;
    columns = terminal_columns.max(1);
    rows = ((rows as f64 * scale).floor() as u32).max(1);
}
let max_rows = terminal_rows.saturating_sub(1).max(1);
if rows > max_rows {
    let scale = max_rows as f64 / rows as f64;
    rows = max_rows;
    columns = ((columns as f64 * scale).floor() as u32).max(1);
}
```

Clamp height to `terminal_rows - 1`, not `terminal_rows` — a placement
that fills the screen edge-to-edge pushes the next prompt line
completely out of view until the user scrolls, defeating the point of
printing the image inline. This mirrors exactly what Som's own
`somcat` client and `Terminal::resync_rich_content_placements` (Som's
resize handler) both do — see `SRP_PROTOCOL.md`'s "Единицы измерения"
and "Пересчёт placement'ов при ресайзе" sections for the full history of
why this specific clamp exists.

If you already have terminal dimensions available some other way (a file
manager or multiplexer typically does — yazi's reference implementation
uses its own existing `Rect`/layout system instead of querying Som
directly, since yazi already knows exactly how many cells its preview
pane occupies), use that instead of the CSI queries above. The queries
exist for standalone clients like `somcat` that have no other source of
truth for terminal geometry.

## Client-side metadata extraction

Extract `width_px`/`height_px`/`is_animated` by reading only the file's
format header, not by decoding pixel data — this should be fast even on
a large file:

- **GIF**: read the Logical Screen Descriptor for width/height (any GIF
  decoder's `dimensions()` call does this without decoding a frame).
  Animation detection: scan the raw bytes for a second Image Descriptor
  block (`0x2C`) after the header — not a strict GIF parser (doesn't walk
  extension blocks byte-for-byte), but `0x2C` can't legitimately appear
  as data before the first real Image Descriptor in realistic GIF files,
  so this is a reliable heuristic without needing to decode any frame
  just to count them.
- **JPEG/PNG**: most image-handling libraries expose a
  "read dimensions without decoding" call (e.g. Rust's `image` crate's
  `ImageReader::into_dimensions()`) — use that. `is_animated` is always
  `false` for these two formats (neither has animation semantics SRP
  currently transmits).

## Windows-specific pitfalls if your client runs there

Two Windows-only issues affect any Rust (or similarly buffered-stdout)
client, both already solved in Som's own `somcat` reference client
(`crates/somcat/src/raw_mode.rs`/`main.rs`'s `write_raw_stdout`) — worth
knowing about even if you're implementing in a different language, since
the underlying causes are platform behavior, not Rust-specific:

1. **`std::io::Stdout::flush()` can hang indefinitely.** Rust's
   `std::io::Stdout` on Windows wraps its handle in a `LineWriter`, which
   only flushes its internal buffer up to the last `\n` (`0x0A`) byte
   seen. SRP's binary/base91 envelopes essentially never contain a real
   `\n`, so `write_all()` alone leaves most of what was "written" sitting
   in an unflushed userspace buffer — and an explicit `.flush()`
   afterward to force it out can hang forever rather than erroring. The
   fix is to bypass `std::io::Stdout` and write directly to the
   `STD_OUTPUT_HANDLE` via the raw `WriteFile` Win32 call.
2. **Console output codepage reinterprets multi-byte UTF-8 too**, not
   just the base91 payload it was designed around — the placeholder
   grid's `U+10EEEE` character plus combining diacritics is real
   multi-byte UTF-8 text, and a non-UTF-8 active console codepage (e.g.
   CP866) will transliterate those bytes into visible garbage instead of
   an invisible placeholder block with the image painted over it. Call
   `SetConsoleOutputCP(65001)` before printing the placeholder grid (and
   set `ENABLE_VIRTUAL_TERMINAL_PROCESSING` on the output handle, which
   many legacy Windows console configurations don't have on by default).

## Worked example: the yazi driver

The full reference implementation is
`yazi-adapter/src/drivers/srp.rs` in
[`errordnk/yazi`](https://github.com/errordnk/yazi). Its `image_show`
function does, in order, exactly what this guide describes:

1. Reads the file, determines `content_type` from its extension.
2. Probes format-header dimensions (`probe_dimensions`) without decoding
   pixel data.
3. Derives `session_id`/`file_id` from the current timestamp.
4. Streams the file in 3800-byte chunks, each wrapped in a base91-encoded
   SRP envelope (`build_envelope`).
5. Computes the placement's cell footprint from yazi's own already-known
   `Rect` (its preview pane's cell dimensions — no CSI query needed here,
   since yazi's layout engine already has this) via the existing
   `Image::pixel_area` helper shared with yazi's Kitty/Sixel drivers.
6. Prints the Unicode-placeholder grid (`place`), reusing the same
   297-entry diacritic table the fork's Kitty driver already had, moved
   into a small shared module (`diacritics.rs`) once a second driver
   needed it.

Terminal detection lives in `yazi-emulator/src/brand.rs`: a `Brand::Som`
variant, detected via the `SOM_WINDOW_ID` environment variable in
`Brand::from_env()`, wired into `Drivers::matches` (`drivers.rs`) so that
running yazi inside Som automatically selects the SRP driver over
Kitty/Sixel/chafa fallbacks, the same way running yazi inside Kitty
itself automatically selects the Kgp driver.

If you're integrating SRP into a different application, the yazi driver
is meant to be read top-to-bottom as a template — the pieces (envelope
building, base91 encoding, placeholder-grid printing, terminal
detection) are independent enough to copy in whatever order your own
codebase's structure calls for.
