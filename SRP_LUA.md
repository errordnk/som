# SRP + Lua: scripted content and the markdown-browser roadmap

**Status: design + first end-to-end slice landing 2026-09-02. This
document is living — update it as Lua's surface grows, don't let it
drift from the code.**

## Why Lua exists in this project at all

Som's end state is a markdown browser, not just an SSH terminal (see
memory: `project_srp_audio_and_md_browser_roadmap.md`). Static markdown
files aren't enough for that — pages need to fetch data, query a
database, react to input, and reshape themselves at request time, the
same way a real web app's backend/frontend split works. Lua is the
scripting layer for both halves of that split:

- **`som-srv` = the Lua *backend*.** It already sits on the
  network/filesystem side of the architecture (see "Where this sits in
  the existing architecture" below) — a natural place for scripts that
  query a database, hit an HTTP API, or otherwise produce content that
  becomes an SRP payload. `som-srv` is a demon process without GPUI, so
  nothing it does can touch rendering — Lua here is pure data/logic.
- **Som = the Lua *frontend*.** Som is the GPUI process — it owns
  painting, layout, and the terminal's rich-content widgets. Lua here
  interprets/shapes what arrives over SRP into what gets rendered,
  starting with the terminal's text/markdown surface (see "Phase 1"
  below) and growing from there as the `md://` browser takes shape.

This mirrors how a conventional web app splits "backend renders/
fetches data" from "frontend decides how to display it," except both
halves are Lua, and the wire between them is SRP (already used for
images/audio/video — see `SRP_PROTOCOL.md`), not HTTP.

## Where this sits in the existing architecture

Confirmed by direct code reading (2026-09-02), not assumption:

- `som-srv`'s `SrvRequest::PutChunk` is, TODAY, only ever sent by an
  external client (`somcat`, the yazi driver) that already has bytes to
  push — `handle_srv_request` in `crates/som_srv/src/server.rs` (loop
  starting ~line 305) only ever *reads* `PutChunk` off the wire and
  writes it to `SrvCache`; nothing in `som-srv` itself originates
  content. This is the gap Lua-as-backend fills: a `.lua` script run
  BY `som-srv` becomes a new, internal source of `PutChunk`s, alongside
  the existing external-client path — the wire format and the
  `SrvCache`/progress-tracking downstream of it don't change at all.
- `ContentType::Markdown` / `ContentMetadata::Markdown` already exist
  end-to-end in the protocol and cache layers — `crates/som_srv/src/
  protocol.rs:673,714`, `crates/terminal/src/rich_content_transport.rs:
  75,186` — and are already threaded through `SrvCache`/
  `RichContentCache`'s `extension_for()` (writes a `.md` file to disk)
  and `rich_content_srv_channel.rs`'s type conversion. But confirmed via
  full-codebase grep: **there is no render path for it anywhere** —
  `rich_content_player.rs:165` explicitly excludes `Markdown` from image
  decoding (`Ok(None)`), and `terminal_element.rs` has zero references
  to it. It has been a fully wired but functionally dead variant. Phase
  1 (below) is what finally gives it a consumer.
- Som already has a real, unrelated-until-now GPUI markdown renderer:
  `crates/markdown`'s `Markdown` entity (`Markdown::new_text(source,
  cx)`, `crates/markdown/src/markdown.rs:503`) and `MarkdownElement`
  (`:984`). It's used elsewhere in Som's UI (chat-style panels, docs)
  but was never connected to the `rich_content_*` pipeline. Phase 1
  wires it in as the actual paint step for `ContentType::Markdown`
  placements.
- Neither `som-srv` nor `crates/terminal` links any Lua crate today
  (confirmed via `Cargo.toml`/`Cargo.lock` grep — zero hits for `mlua`/
  `rlua`/`lua-src`). yazi (`C:\home\dnk\yazi-fork-work`) already depends
  on `mlua 0.12` with `features = ["anyhow", "async", "error-send",
  "macros", "serde"]` and uses it as the reference pattern for both
  runtimes here — see "Runtime setup" below.

## Runtime choice: mlua

Same crate yazi already uses (`mlua`) — one Lua ecosystem across the
whole project (Som, som-srv, and the yazi fork). `som_srv`'s own
`Cargo.toml` pins `mlua = { version = "0.10", features = ["lua54",
"vendored", "send"] }` (yazi itself is on `mlua 0.12`/`lua55` — the
version gap is fine, the API this project actually uses is stable
across both).

**Correction (2026-09-02, caught by a real test, not assumption):** an
earlier draft of this document claimed `Lua::new()` already omits `io`/
`os` and that yazi's own sandboxing relies on that default. That's
wrong. Live-tested directly against `crate::lua::run_script` in
`crates/som_srv/src/lua.rs`: `mlua::Lua::new()` is defined (confirmed by
reading `mlua`'s own `state.rs`) as `Lua::new_with(StdLib::ALL_SAFE,
LuaOptions::default())`, and `StdLib::ALL_SAFE` (per mlua's own
`stdlib.rs`) is every standard library EXCEPT `DEBUG`/`FFI` — which
means `io` (and `os`, and `package`, which can `require` arbitrary
modules) ARE loaded by plain `Lua::new()`. mlua's notion of "safe" here
means "doesn't corrupt the VM's internal state," not "no access to the
host filesystem/environment" — two genuinely different meanings that
looked like the same thing until an actual `io.open(...)` call inside a
test proved otherwise (`run_script_has_no_io_library_available`, which
failed against the first draft of this code and passes now).

**Actual sandboxing (`som-srv`'s `crate::lua::phase1_stdlib()`):**
`Lua::new_with(StdLib::TABLE | StdLib::STRING | StdLib::MATH |
StdLib::UTF8 | StdLib::COROUTINE, LuaOptions::default())` — an explicit
allow-list, not a default. Excludes `IO`, `OS`, and `PACKAGE`
specifically (the three that can reach outside the VM), leaving pure
computation (string/table/math manipulation, coroutines). No Rust
functions are registered into the VM's globals at all in Phase 1
either, so there's nothing beyond that stdlib subset for a script to
call. Every later phase that grows the API surface (Phase 2/3) does so
through explicitly registered Rust functions (`db.query`, etc.) — never
by widening `StdLib` back toward `ALL_SAFE`.

## Phase 1 (landing now): minimal end-to-end slice

Scope, deliberately narrow — get the transport and both runtimes wired
and PROVEN live before growing the API surface:

1. **`som-srv` executes a `.lua` script and produces a `ContentType::
   Markdown` payload.** New `SrvRequest` variant (or an admin-style
   one-shot invocation — see open question below) that names a script
   path; `som-srv` runs it in a fresh `mlua::Lua` VM, takes its single
   string return value as the markdown source, and streams it out as
   ordinary `PutChunk`s — reusing 100% of the existing chunking/
   `SrvCache`/progress machinery images and video already go through.
   No filesystem/network/DB access exposed to the script yet — Phase 1
   scripts are pure `return "..."` functions of nothing, proving the
   plumbing before proving the sandbox boundary.
2. **Som runs its own `mlua::Lua` VM and renders the result.** The
   simplest possible frontend hook: when a `ContentType::Markdown`
   placement's bytes are fully received, Som calls into a Lua function
   (`render(source) -> source` today — identity by default, real
   scripts overriding it later) before handing the string to
   `Markdown::new_text`/`MarkdownElement` for painting in the
   terminal's rich-content widget. Phase 1 doesn't yet let scripts
   control layout/widgets beyond "here is the markdown text to show" —
   see "Phase 2" for where that grows.
3. **Verification**: a real `.lua` file, executed by a real `som-srv`
   daemon, its output landing as real rendered markdown in a real Som
   terminal window — the same live-test discipline every other SRP
   content type in this project has gone through (see `SRP_PROTOCOL.md`/
   `SRP_INTEGRATION_GUIDE.md`'s own verification sections).

### Status (2026-09-02): backend + frontend runtimes done, paint not yet wired

Landed and live-tested:
- `som-srv` backend (`SrvRequest::RunLuaScript`, `crates/som_srv/src/
  lua.rs`): runs a script, streams its markdown return value through
  `SrvCache::put_chunk` — same path/`SrvResponse::Progress` machinery
  real `PutChunk` senders use. Explicitly sandboxed (`phase1_stdlib()`
  — `TABLE | STRING | MATH | UTF8 | COROUTINE`, no `IO`/`OS`/`PACKAGE`).
- Som frontend (`crates/terminal/src/rich_content_lua_frontend.rs`):
  `render(source) -> source` identity transform, same sandboxing.
- `Terminal::rich_content_markdown_placements()` (`crates/terminal/src/
  terminal.rs`): scans the cache for `ContentType::Markdown` entries,
  reads however many contiguous bytes have arrived, runs them through
  the frontend `render()` call via `rich_content_markdown_player::
  refresh_or_create`, returns `Vec<(session_id, file_id, rendered_
  text)>` — the same shape `rich_content_audio_placements`/
  `rich_content_video_placements` already have.

**NOT yet done**: nothing calls `rich_content_markdown_placements()`
from the paint path. `terminal_element.rs`'s `paint_rich_content_
placements` has branches for image/audio/video placements but none for
markdown yet — the rendered text this phase produces has nowhere on
screen to land. Wiring that in means constructing a `crates/markdown`
`Markdown`/`MarkdownElement` (which needs a GPUI `Context`/`Window` this
crate's own `rich_content_markdown_player.rs` deliberately doesn't have
access to — see that file's own doc comment on why it returns a plain
`String`, not a GPUI entity) at the `terminal_element.rs` call site,
alongside whatever layout/sizing decision a text block needs (unlike
audio/video's fixed-size widgets, markdown's natural height depends on
its own content and the placeholder grid's reserved footprint would
need to already match it — see `ContentMetadata::Markdown`'s own doc
comment on carrying no geometric metadata, flagged as a possible Phase
2 revisit). This is the concrete next step, not yet started.

### Explicitly OUT of scope for Phase 1

- Any database/network binding (MySQL, Redis, HTTP) — the backend's
  eventual reason for existing, but not needed to prove the transport
  and both runtimes work. Tracked as Phase 3 below so the wire/runtime
  work isn't blocked on picking DB client crates.
- `md://` URL scheme / navigation / any browser-shaped UI. Phase 1 is
  "one script, one markdown blob, shown in the terminal" — not yet the
  browser itself.
- Sandboxing beyond mlua's own stdlib-omission default. Real, audited
  resource limits (execution timeouts, memory caps, filesystem
  jailing) are a Phase 3+ concern once scripts can actually reach
  outside their own VM.

## Phase 2 (next): frontend scripting grows

Once Phase 1's plumbing is proven, Lua in Som stops being "just
markdown-through" and starts controlling more of the rich-content
surface:

- Scripts choose HOW to render a payload, not just what markdown to
  show — e.g. deciding between a markdown view and a raw/table view of
  the same backend response, the same kind of decision a real web
  frontend framework makes.
- A registered-function surface on the Som side analogous to yazi's
  `ya.*`/`fs.*` tables (see `standard.rs`'s `Composer`-based lazy
  registration pattern) — grown incrementally, one binding at a time,
  each documented here when it lands.
- Revisit whether `ContentMetadata::Markdown`'s "carries no geometric/
  format metadata at all" design (see that variant's own doc comment
  in `rich_content_transport.rs`) still holds once scripts want to
  hint layout/sizing.

## Phase 3 (backend data access): databases and network

This is the part that makes `som-srv` a real backend, not just a
text-templating engine:

- **Design direction (2026-09-02): Rust crates as bindings, not raw
  socket/FFI access from Lua.** `som-srv` links real Rust DB client
  crates (e.g. `sqlx`/`mysql_async` for MySQL, `redis` for Redis) and
  exposes a thin Lua API (`db.query(...)`, `redis.get(...)`/`redis.set(
  ...)`) — connection pooling, protocol handling, and TLS all live in
  Rust; Lua only ever calls already-safe, already-typed functions.
  Chosen over letting Lua reach raw TCP sockets and speak DB wire
  protocols itself (rejected: far more per-script surface to get
  wrong, no shared pooling/safety, and each new DB backend would need
  bespoke Lua-side protocol code instead of one Rust crate integration
  reused by every script).
- Each new DB/service integration is its own follow-up — this section
  should grow one subsection per binding as they land (e.g. "### MySQL
  binding", "### Redis binding"), with the Rust crate chosen, the Lua
  API shape, and the connection-lifecycle model (per-script connection?
  a shared pool `som-srv` owns across all scripts?) documented at that
  point, not speculated here ahead of implementation.

## Open questions (track here, resolve before the relevant phase lands)

- **How does a client ask `som-srv` to run a specific script?** A new
  `SrvRequest` variant naming a script path/id (mirrors `PutChunk`'s
  own shape), vs. an admin-style one-shot command (mirrors `--list-
  sessions`/`--kill-session`'s pattern in `main.rs`) — not yet decided,
  resolve when Phase 1's script-invocation entry point is actually
  implemented.
- **Script discovery/storage**: a fixed directory `som-srv` watches
  (mirrors yazi's `preset!()`-embedded `.lua` files), vs. scripts
  submitted inline over the wire, vs. both. Not needed for Phase 1's
  single hardcoded verification script; resolve before Phase 2 needs
  more than one script to exist.
- **Lifecycle of the Lua VM**: fresh `mlua::Lua` per invocation
  (simplest, matches Phase 1's stateless `return "..."` scripts) vs. a
  persistent VM per session/script (needed once scripts hold open DB
  connections in Phase 3) — Phase 1 uses fresh-per-call; revisit when
  Phase 3's connection-lifecycle question above is resolved, since the
  two are the same underlying decision.

## Changelog

- **2026-09-02**: Document created. Phase 1 scoped and design frozen
  (mlua on both sides, reuse `ContentType::Markdown`'s already-wired-
  but-dead transport path, `crates/markdown`'s `Markdown`/
  `MarkdownElement` as the actual paint step). Phase 3's DB-access shape
  (Rust-crate bindings, not raw Lua socket access) decided ahead of
  implementation per explicit user requirement that som-srv's Lua
  backend must be able to write to MySQL/Redis/etc.
- **2026-09-02**: `som-srv`'s backend runtime landed
  (`SrvRequest::RunLuaScript`, `crates/som_srv/src/lua.rs`) — corrected
  the "Runtime choice" section's sandboxing claim after a real test
  (`io.open(...)` inside a script) proved `Lua::new()`'s default DOES
  load `io`; switched to an explicit `StdLib` allow-list
  (`phase1_stdlib()`) instead. Verified live: a Lua script's markdown
  return value streams through the exact same `SrvCache::put_chunk`
  path/`SrvResponse::Progress` machinery real `PutChunk` senders use.
- **2026-09-02**: Som's frontend runtime landed (`crates/terminal/src/
  rich_content_lua_frontend.rs`'s `render()`, `rich_content_markdown_
  player.rs`, `Terminal::rich_content_markdown_placements()`) — same
  sandboxing correction applied on this side too. Transport + both Lua
  runtimes are now proven end-to-end by unit tests (backend script
  execution + sandbox, frontend identity render + sandbox, markdown
  player refresh/cache-skip/empty-cache behavior). Paint integration
  (`terminal_element.rs` actually drawing the rendered text via
  `crates/markdown`'s `Markdown`/`MarkdownElement`) is the next
  concrete step — see "Status" under Phase 1 above for exactly what's
  missing.
