# Adding Som Rich Protocol (SRP) support to a third-party TUI

This is a spec-and-implementation guide for developers who want their own
terminal application (a file manager, a pager, a chat client — anything
running inside a PTY) to display images/audio/video through Som's own
graphics protocol, the same way it might already support the Kitty
graphics protocol or Sixel. If you're looking for Som's own internal
design history and decision log instead, see `SRP_PROTOCOL.md` in this
repository — this document is the external-facing spec extracted from
that journal, aimed at someone who has never seen Som's source before.

**BREAKING CHANGE (2026-08-27): if you integrated against an earlier
version of this guide, read "Migrating from the old PTY/base91
transport" near the end before anything else** — the payload transport
this guide used to describe (base91-encoded APC envelopes carrying file
bytes over the same PTY as keystrokes) has been removed from Som
entirely. Placement geometry (the Unicode-placeholder grid) is unchanged
— only how file bytes get from your client to Som changed.

The reference implementation this guide walks through lives in
[`errordnk/yazi`](https://github.com/errordnk/yazi), a fork of the
[yazi](https://github.com/sxyazi/yazi) terminal file manager, specifically
`yazi-adapter/src/drivers/srp.rs`. **As of this writing, that reference
implementation still speaks the OLD (removed) transport and needs to be
migrated** — read this guide as the target to migrate it to, not as an
accurate description of its current state.

## Why a protocol integration, not just "print an image"

If you already support Kitty's graphics protocol, you might reasonably
ask why SRP needs separate code instead of just detecting Som and
speaking Kitty to it. Two real, unavoidable reasons:

1. **Kitty's protocol is base64-only over the wire**, which on a large
   animated GIF (or, worse, a video file) produces multi-megabyte
   payloads and multi-second display latency — acceptable for a single
   screenshot, not for smooth animation. SRP streams the source file's
   raw bytes over a dedicated binary channel (no text-safe re-encoding
   overhead at all — see "Transport: the som-srv binary side-channel"
   below), progressively, rather than re-encoding every frame to PNG
   first.
2. **Windows ConPTY is not a transparent byte pipe for a PTY child
   process's stdout.** Any protocol that puts raw bytes `>= 0x80` on a
   PTY's wire gets silently corrupted by the active console codepage
   before the reading terminal ever sees them (confirmed experimentally
   during SRP's own development — see `SRP_PROTOCOL.md`'s "ConPTY
   реинтерпретирует сырые байты" section for the full story). This is
   exactly why SRP does NOT put file payload on the PTY at all anymore
   — see the transport section below. The PTY still carries a small
   amount of real Unicode TEXT (the placeholder grid), which survives
   fine because Som sets the console output codepage to UTF-8 before
   printing it (see "Windows-specific pitfalls" below) — but no binary
   payload ever touches the PTY.

If your application only needs to work on Unix and only ever displays
small images, you could in principle implement Kitty's protocol instead
and get a similar visual result for that narrow case. SRP exists because
Som itself needed a protocol that (a) never re-encodes frames, for
animation/video performance, and (b) never puts payload where Windows
ConPTY can corrupt it. If you're implementing SRP specifically (not just
"some graphics protocol"), the format below is what Som's receiving side
actually expects — matching it exactly is not optional.

## Detecting that you're running inside Som

Som sets `SOM_WINDOW_ID` (any local terminal spawn, and any remote
session reached through `som-srv`) to the PID of the Som process, the
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
isn't set, don't attempt an SRP transfer; Som isn't there to receive it.

## Two channels: PTY (control) and som-srv (payload)

SRP now has two genuinely separate transports, each carrying a different
kind of data:

1. **The PTY** — the SAME stdout your client already writes ordinary
   output through. Carries ONLY the placeholder-grid control text (see
   "Placement: the Unicode-placeholder grid technique" below) — real
   Unicode text, printed exactly the way any other program's output
   would be. No file bytes, ever.
2. **The `som-srv` binary side-channel** — a separate connection (named
   pipe on Windows, Unix domain socket elsewhere) to a small daemon,
   `som-srv`, that Som deploys and runs on whichever machine your client
   process is actually running on (the SAME machine — if your client is
   reached over SSH, this is a LOCAL socket on the remote end, not a
   connection back to wherever Som's own window is; see "Where is
   `som-srv`" below). Carries the raw file payload, plus a small
   typed request/response protocol for the one direction that flows
   Som → client (byte-range seek requests — see "Answering byte-range
   requests" below).

Both channels are required for a working integration: without the PTY
grid, Som never learns your placement exists at all; without the
`som-srv` connection, there's no payload to show.

## Transport: the som-srv binary side-channel

### Where is `som-srv`

Som deploys a small daemon binary, `som-srv`, next to itself: locally,
in the same directory as Som's own executable; on a remote host reached
over SSH, in `~/.local/bin/som-srv` (deployed automatically the first
time a `tmux: true` — or now, any — profile on that host needs it). This
is the SAME daemon that also implements Som's tmux-style persistent-session
functionality — the binary side-channel is just one of its jobs, not a
separate process you need to find or start yourself.

The daemon listens on a fixed, well-known local address:

- Windows: named pipe `\\.\pipe\som-srv`
- Unix (macOS/Linux, including the remote end of an SSH session): Unix
  domain socket `/tmp/som-srv-<uid>.sock` (per-uid, so multiple users on
  a shared machine each get their own daemon and session registry)

**Your client connects to this LOCAL address on whatever machine it's
actually running on** — never to Som directly, never over the network
itself. If Som is local, that's the same machine. If Som reached your
client over SSH, `som-srv` is running on the remote end (deployed there
the same way), and your client connects to the socket on THAT machine —
`som-srv` itself bridges back to Som over the SSH connection Som
already has open, entirely transparently to your client.

If the daemon isn't running yet when your client tries to connect,
spawn it yourself (detached, so it outlives your own process) and
retry — this is exactly what Som's own clients do (`som_srv::daemon::
connect_or_spawn`). Find the `som-srv(.exe)` binary next to your own
client's executable (the deploy convention every Som-side client
follows) or, if you can't find it there, treat SRP support as
unavailable for this run rather than trying to embed/fetch a copy
yourself.

### Wire protocol

Length-prefixed JSON frames over the pipe/socket connection (no text-safe
encoding needed at all — this is a raw local IPC channel, not a PTY, so
none of the ConPTY-codepage concerns from the old transport apply here).
The two message enums:

```rust
enum SrvRequest {
    Handshake(HandshakeInfo),
    PutChunk {
        session_id: u32,
        file_id: u32,
        offset: u64,
        data: Vec<u8>,
        total_size: u64,
        content_type: ContentType,   // 0=Gif, 1=Audio, 2=Markdown, 3=Video, 4=Jpeg, 5=Png
        metadata: ContentMetadata,   // see below
    },
    SubscribeProgress { session_id: u32, file_id: u32 },   // Som sends this, not your client
    RequestByteRange { session_id: u32, file_id: u32, offset: u64, len: u64 }, // Som -> client
}
enum SrvResponse {
    Handshake(HandshakeInfo),
    Progress { session_id: u32, file_id: u32, contiguous_len: u64, total_size: u64, content_type: ContentType, metadata: ContentMetadata },
}
```

Your client only ever needs to construct `SrvRequest::Handshake` and
`SrvRequest::PutChunk`, and (for audio/video) read `SrvRequest::
RequestByteRange` arriving unsolicited on the same connection it used to
send `PutChunk`s. `SubscribeProgress`/`Progress` are how Som's OWN
receiving side tracks transfer progress internally — you never send or
need to parse `Progress` yourself.

**Connection lifecycle**:
1. Connect to the local socket.
2. Send `SrvRequest::Handshake(HandshakeInfo::current())` — a small
   struct identifying your client's version/OS/arch (used for daemon
   version-mismatch diagnostics on Som's side, not a capability
   negotiation your client needs to branch on).
3. Read the daemon's own `SrvResponse::Handshake` reply.
4. Send `PutChunk` messages, one per chunk, sequentially, `offset`
   tracking the running byte position — the same "just stream the raw
   file bytes, unmodified, chunk by chunk" model the old transport used,
   minus the base91 encoding step.
5. For content with no seek concept (a static image, an unanimated
   GIF... actually GIF/JPEG/PNG in general): close the connection once
   every chunk is sent, exit.
6. For audio/video (seekable): keep the connection open, and in a
   background thread/task, keep reading from it — any `SrvRequest::
   RequestByteRange` that arrives is Som asking for more of the file
   (see "Answering byte-range requests" below). This is unsolicited: the
   daemon forwards it to you on the SAME connection you used to send
   `PutChunk`s, not as an `SrvResponse` variant — a reader on this
   connection needs to be prepared to decode EITHER a `SrvResponse` or a
   raw `SrvRequest` off the wire (Som's own client-side library,
   `somcat::srv_channel::Incoming`, models this as a two-variant enum
   and tries `SrvResponse` first, `SrvRequest` second — do the
   equivalent in your own language).

`content_type`/`metadata` travel on **every** `PutChunk` (not just the
first) — this keeps the message shape uniform and lets the daemon
determine the on-disk cache file's extension from the very first chunk
it ever sees for a given `(session_id, file_id)`, without a special case
for "the first message looks different."

### `ContentMetadata`

Same fields, same meaning as the pre-2026-08-27 transport — only the
wire encoding changed (JSON struct field instead of a fixed-width binary
layout):

```rust
enum ContentMetadata {
    Image { width_px: u32, height_px: u32, color_bits: u8, is_animated: bool },
    Audio { sample_rate: u32, channels: u8, bits_per_sample: u8, duration_ms: u32 },
    Video { width_px: u32, height_px: u32, fps_numerator: u32, fps_denominator: u32, codec: VideoCodec },
    Markdown,
}
```

`width_px`/`height_px` are the image/video's real decoded pixel
dimensions (from the file's own format header — not anything about
compressed file size). `color_bits` should be `32` if you're sending
RGBA-equivalent data. `is_animated` should be `true` if the source GIF
has more than one frame. `duration_ms` is the whole audio track's real
duration in milliseconds, read straight from the source file's own
header (an MP3's Xing/VBRI VBR tag, or a size/bitrate-based estimate; a
FLAC's STREAMINFO block) — not derived from how many bytes have streamed
in so far. Every format-probing library capable of reading container
metadata exposes this without decoding samples (Rust's `symphonia`:
`CodecParameters::n_frames`/`time_base`). This is what lets Som show an
accurate duration and correctly-sized seek bar the moment the first
chunk arrives, even for a multi-gigabyte file whose transfer will take a
long time to finish.

### Session and file ids

Unchanged from the old transport: `session_id`/`file_id` are both
sender-assigned 32-bit values, masked to 24 bits (`& 0xFF_FFFF`, clamped
to a minimum of 1 — 0 is reserved as "no id"). The reference
implementation derives both from the current Unix timestamp in
milliseconds, with `file_id` further scrambled by a cheap multiplicative
hash to decorrelate it from `session_id`:

```rust
let now_ms = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_millis() as u32)
    .unwrap_or(1);
let session_id = (now_ms & 0xFF_FFFF).max(1);
let file_id = (now_ms.wrapping_mul(2_654_435_761) & 0xFF_FFFF).max(1);
```

The 24-bit mask still matters for the same reason it always did: both
ids get painted directly into the placeholder grid's cell colors (see
below), which only have 24 bits of RGB to work with per channel.

## Order matters: placeholder grid FIRST, then stream

**Print the placeholder grid before sending a single `PutChunk`.** This
is not just a latency nicety anymore — it's a correctness requirement.
Som only subscribes to a given `(session_id, file_id)`'s progress once
it has seen that id in the placeholder grid; if your client streams an
entire small file and closes its `som-srv` connection before Som has had
a chance to print/see the grid and subscribe, Som may never learn the
transfer happened at all (the daemon does replay the current watermark
to a late subscriber as a best-effort mitigation, but relying on that
race resolving correctly is strictly worse than just getting the order
right). This bit Som's own `somcat` client during this exact migration —
its image/GIF branch streamed first and printed the grid last, which
worked fine under the old always-on-PTY transport but broke silently
under the new one; reordering it (grid first) fixed it.

For audio/video specifically, Som's own clients ALSO keep the `som-srv`
connection open after the initial sequential stream (see "Answering
byte-range requests" below) — for images/GIF with no seek concept, close
the connection once the last chunk is sent.

## Answering byte-range requests

This is the one part of SRP that flows in the opposite direction — Som
asking your client for something, not your client pushing data to Som.
Everything else this guide describes so far is client → Som; this
section covers Som → client, and how your client should answer.

### Why this exists, and why it isn't audio-specific

The concrete, shipped use case today is a byte-range seek: Som needs
bytes from further into a file than the sequential stream has reached
(a user dragging an audio/video seek bar past the currently-cached
prefix), and the only way to get them without waiting is to ask the
client directly, since the client is the one holding the file open
(possibly on a completely different machine, reached over SSH). The
message itself is deliberately general (a typed request in the same
`SrvRequest` enum everything else uses, not a hardcoded "give me these
bytes" one-off shape) — a second, unrelated planned use needs an
identical request/response round trip: a future `.md` document embedding
a Lua script that executes wherever the document physically lives, with
a form submission needing to reach that script and bring an answer back.
Audio/video byte ranges are the FIRST concrete consumer of this general
mechanism, not the whole design surface.

### How to answer

**You don't need a new response shape.** `SrvRequest::RequestByteRange
{ session_id, file_id, offset, len }` arrives unsolicited on your
client's already-open `som-srv` connection (the same one it used to send
`PutChunk`s — see "Connection lifecycle" above). Answer it with ordinary
`PutChunk` message(s) covering `[offset, offset + len)` of the file,
`offset` set to the real file offset the range starts at (not
offset-from-zero the way the initial stream's first chunk was). `som-srv`'s
receiving cache already tolerates a chunk landing at an arbitrary,
out-of-sequence offset (needed for ordinary out-of-order delivery
robustness regardless of byte-range queries) — no new receiving-side
machinery had to be built for this to work.

Concretely: keep the file open (or reopen it) after the initial
sequential stream finishes, read `[offset, offset + len)` from it
directly, and send that slice through the exact same `PutChunk`-sending
code path you already have — just parameterized with a non-zero
starting offset instead of 0.

### Your process needs to still be alive when a seek happens

If your client closes its `som-srv` connection immediately after the
initial sequential stream finishes (the simplest, and for small files
entirely correct, shape), there's nothing left to answer a byte-range
request with — the daemon has nowhere to forward it to, and Som simply
won't get a response (surfacing as "no more progress for that range,"
the same silent gap the old transport's unanswered `Query` also
tolerated — not a new failure mode). For a client that wants to support
seeking into a large, still-transferring (or even fully-transferred but
since exited) file, keep the process running and the connection open
for as long as you're willing to keep answering. Som's own clients keep
the connection open for the whole process lifetime for audio/video
specifically, and close it immediately (no keep-alive) for images/GIF,
which have no seek concept at all.

## Don't block your client's own UI on the transfer

This matters for any client embedded in a larger interactive application
(a file manager's preview pane, a chat client rendering an inline
attachment) — not for a standalone one-shot tool, which has nothing else
running to block.

Som's receiving side is already designed to start decoding/playing
before a transfer finishes (see `SRP_PROTOCOL.md`'s own benchmark
proving the first partial GIF decode happens before the full file has
arrived). A client throws this property away if it makes its own caller
wait for the entire `PutChunk` loop to finish before doing anything else
— the file transfer becomes the bottleneck for the client's OWN
responsiveness, independent of how fast Som itself can decode. The
general shape: resolve geometry synchronously, print the placeholder
synchronously, hand the byte-pushing loop to a background task/thread,
return control to your own caller immediately after the placeholder is
on screen — not after the transfer finishes.

This same principle applies to audio, with one further difference:
**your client does not need to build any playback UI at all.**
`ContentType::Audio`/`ContentType::Video` are fully implemented on Som's
receiving end — decoding and playback both happen inside Som itself,
along with an inline play/pause/seek widget Som paints and handles
clicks for directly. Your client's only job is to probe the file's
header for metadata (a cheap format-reader probe, not a full decode —
see `somcat`'s own `audio_metadata()` for a worked example) and stream
the raw bytes, then either exit (image/GIF) or keep answering byte-range
requests (audio/video — see above). No placeholder-grid pixel-size math
for audio either: audio has no pixel dimensions, so Som's widget uses a
fixed cell footprint instead. This is the OPPOSITE of the "client owns
rendering" pattern that's true for images/GIF: the reason is
architectural, not a style choice — your client process might be running
on a different machine than the one whose speakers should produce sound
(e.g. a client running over SSH), while Som is guaranteed to be local to
whoever's actually listening.

## Placement: the Unicode-placeholder grid technique

**Unchanged from the old transport** — this always lived on the PTY, and
still does. This is the part that makes SRP (and Kitty's own "Unicode
placeholders" extension, which SRP reuses the encoding scheme from)
genuinely different from most terminal graphics protocols: **the image
isn't anchored to a pixel or cell position tracked out-of-band from the
terminal's own text model.** Instead, the image's position and size are
encoded directly into ordinary grid cells, printed as real text through
the same stdout the rest of your program's output goes through.

Concretely: after streaming a file (or, per "Order matters" above,
BEFORE streaming it — print the grid first), print an N×M block of
cells (N = columns, M = rows) where every cell contains:

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
own `U=1` unicode-placeholder mode uses.

### Why this works: the placement IS text, not an overlay

A block of cells printed this way goes through the terminal's completely
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
stretch it edge-to-edge, distorting the aspect ratio. Som's own resize
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
DEC private-mode queries the same way any modern terminal emulator does
(these queries are unchanged, still travel on the PTY, exactly as
before):

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
printing the image inline. This mirrors exactly what Som's own `somcat`
client and `Terminal::resync_rich_content_placements` (Som's resize
handler) both do — see `SRP_PROTOCOL.md`'s "Единицы измерения" and
"Пересчёт placement'ов при ресайзе" sections for the full history of why
this specific clamp exists.

If you already have terminal dimensions available some other way (a file
manager or multiplexer typically does — yazi's reference implementation
uses its own existing `Rect`/layout system instead of querying Som
directly, since yazi already knows exactly how many cells its preview
pane occupies), use that instead of the CSI queries above.

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
  `false` for these two formats.
- **Audio (mp3/flac)**: probe with a format-reader library (Rust:
  `symphonia`'s `probe`/`FormatReader`, no full decode) for `sample_rate`/
  `channels`/`bits_per_sample`/`duration_ms`.
- **Video**: Som's own `somcat` client does NOT probe real video
  dimensions/fps/codec client-side (`ContentMetadata::Video` is sent
  with all-zero/`Unknown` placeholder values) — Som's paint path scales
  whatever it decodes to fit the placeholder grid's footprint regardless,
  so an inaccurate footprint only affects initial aspect ratio until the
  user resizes. You can do better if your client has a cheap way to
  probe real video metadata, but it isn't required for a working
  integration.

## Windows-specific pitfalls if your client runs there

Two Windows-only issues affect any Rust (or similarly buffered-stdout)
client, both already solved in Som's own `somcat` reference client
(`crates/somcat/src/raw_mode.rs`/`main.rs`'s `write_raw_stdout`) — worth
knowing about even if you're implementing in a different language, since
the underlying causes are platform behavior, not Rust-specific. Both are
about the PLACEHOLDER GRID text now (the only thing still on the PTY) —
neither applies to the `som-srv` connection, which is a plain local
socket/pipe with no console/codepage involvement at all.

1. **`std::io::Stdout::flush()` can hang indefinitely.** Rust's
   `std::io::Stdout` on Windows wraps its handle in a `LineWriter`, which
   only flushes its internal buffer up to the last `\n` (`0x0A`) byte
   seen. The placeholder grid's own output does contain real `\n`s (one
   per row, via `\r\n`), so this specific hang is less likely to bite the
   grid print itself than it was for the old binary payload — but Som's
   own reference client bypasses `std::io::Stdout` for ALL of its output
   anyway (writes directly to `STD_OUTPUT_HANDLE` via the raw `WriteFile`
   Win32 call), as the simplest way to sidestep this class of issue
   entirely rather than reasoning about which specific writes are safe.
2. **Console output codepage reinterprets multi-byte UTF-8.** The
   placeholder grid's `U+10EEEE` character plus combining diacritics is
   real multi-byte UTF-8 text, and a non-UTF-8 active console codepage
   (e.g. CP866) will transliterate those bytes into visible garbage
   instead of an invisible placeholder block with the image painted over
   it. Call `SetConsoleOutputCP(65001)` before printing the placeholder
   grid (and set `ENABLE_VIRTUAL_TERMINAL_PROCESSING` on the output
   handle, which many legacy Windows console configurations don't have
   on by default).

## Migrating from the old PTY/base91 transport

If your integration currently implements the transport this guide used
to describe (before 2026-08-27) — base91-encoded `ESC _ S ... ESC \`
chunk envelopes carrying file payload over the PTY, and `ESC _ Q ...
ESC \` query envelopes for byte-range seeks — here's what changed and
what to do about it:

**Delete entirely, no longer needed:**
- base91 encode/decode.
- The chunk-envelope header/marker/separator format (`ESC _ S ...`).
- The query-envelope format (`ESC _ Q ...`) and its background
  stdin-reading thread.
- The `STDOUT_WRITE_LOCK`-style synchronization your client needed once
  it had two threads (main stream + query responder) both writing to
  stdout — the `som-srv` connection is a separate channel from stdout
  now, so this concurrency concern moves there instead (see below), but
  the stdout-specific lock itself is gone.

**Add:**
- A `som-srv` client: connect to the local daemon (spawn it if not
  running — see "Where is `som-srv`" above), send `Handshake`, then
  `PutChunk` per chunk instead of building/writing envelopes.
- For audio/video: keep the `som-srv` connection open, read it in a
  background thread/task for `RequestByteRange`, answer with more
  `PutChunk`s — same overall shape as the old query-responder thread,
  just reading a different connection instead of stdin.

**Unchanged, keep exactly as-is:**
- Placeholder-grid printing (still real PTY text, still needs to happen
  BEFORE the transfer now more than ever — see "Order matters" above).
- `CSI 16 t`/`CSI 18 t` cell-geometry queries, if your client uses them.
- Metadata extraction (format-header probing).
- Windows `SetConsoleOutputCP`/raw-`WriteFile` handling for the grid text.
- Session/file id derivation.

## Worked example: the yazi driver

The reference implementation, `yazi-adapter/src/drivers/srp/` in
[`errordnk/yazi`](https://github.com/errordnk/yazi), has been migrated
to the current `som-srv` transport (2026-09-02) and now has full parity
with `somcat`, not just images/GIF:

- **Transport**: `srp/protocol.rs`, `srp/pipe.rs`, `srp/daemon.rs`, and
  `srp/srv_channel.rs` are a hand-kept, client-only port of `som_srv::
  protocol`/`som_srv::pipe`/`som_srv::daemon`/`somcat`'s own `srv_
  channel.rs` — see `protocol.rs`'s own doc comment for why this is a
  port rather than a dependency on the `som_srv` crate (it pulls in
  `alacritty_terminal`/`smol`/`sysinfo`/`zlog`, all Som-internal and
  unwanted in a general-purpose file manager's dependency tree). Unlike
  `somcat` (which finds `som-srv` next to its own executable, since the
  two are built and deployed together), this driver has no such
  relationship to `som-srv` at all — it looks for it at the fixed path
  `~/.local/bin/som-srv[.exe]` instead (`daemon.rs`), spawning it
  detached if not already running, same retry-then-give-up shape as
  `som_srv::daemon::connect_or_spawn`.
- **Images/GIF** (`srp/mod.rs`'s `show_image`): unchanged in spirit from
  before the migration — reads the whole (typically small) file into
  memory, probes its header (`metadata.rs`), prints the placeholder
  grid, then sends the buffer as `PutChunk`s over a fresh `SrvChannel`.
- **Video** (`stream_video`): streamed off disk in bounded chunks (never
  the whole file into memory), with a dedicated byte-range-responder
  connection (`RegisterRangeResponder`) so a seek on Som's side gets
  answered promptly instead of queueing behind a large in-flight
  sequential transfer — mirrors `somcat::stream_file_from_disk`/`spawn_
  byte_range_responder_from_disk`/`send_range_chunks_from_disk_
  interruptible` field-for-field, including the seek-signal compare-and-
  clear pattern (see `mod.rs`'s own doc comments for the bugs that
  pattern fixes). Does NOT do real ffprobe-style metadata probing — see
  `metadata.rs`'s own doc comment for why (FFmpeg is a large, platform-
  specific dependency this driver deliberately avoids) — so it falls
  back to a fixed placeholder footprint (same numbers `somcat` itself
  falls back to when its own real probe fails); Som decodes the real
  file and learns its true dimensions once playback actually starts
  regardless.
- **Audio** (`audio_show`, `pub` rather than `pub(super)`): same disk-
  streaming path as video, but WITH real header metadata (sample_rate/
  channels/bits_per_sample/duration_ms) via `symphonia` — unlike video's
  FFmpeg probe, this is cheap enough (a small header read, not a multi-
  gigabyte container probe) to always attempt (`metadata::audio_
  metadata`). Called directly by `Adapter::audio_show`
  (`yazi-adapter/src/adapter.rs`) rather than going through `Driver::
  image_show`'s per-driver dispatch — audio has no equivalent concept on
  any OTHER driver in this codebase (they all exist to place pixels, not
  play sound), so there's no dispatch table entry to route through. Has
  a companion Lua binding (`ya.audio_show`, `yazi-plugin/src/utils/
  image.rs`) and previewer (`yazi-plugin/preset/plugins/audio.lua`,
  registered for `mime = "audio/*"` in `yazi-default.toml`), which fall
  back to `file.lua`'s plain classification preview outside Som (checked
  via `SOM_WINDOW_ID`) since no other driver has any audio concept to
  fall back to. The widget's fixed footprint (`AUDIO_WIDGET_COLUMNS`/
  `ROWS`, 40x1) is centered both horizontally and vertically within the
  preview pane, not anchored to a corner.
- **Placeholder-grid printing, terminal-cell-size sizing** (via yazi's
  own `Rect`/`Image::pixel_area`), and **`Brand::Som` detection**
  (`yazi-emulator/src/brand.rs`, via `SOM_WINDOW_ID`) are unchanged from
  before the migration.

If you're integrating SRP into a different application, the yazi driver
is a useful reference for both the transport port (`srp/protocol.rs`
through `srp/srv_channel.rs`) and the geometry/detection pieces — follow
this guide's `som-srv` sections for the wire protocol itself, and treat
`srp/mod.rs` as a worked example of wiring video/audio through it
end-to-end, not just images.
