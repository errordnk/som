use serde::{Deserialize, Serialize};

/// ONE fixed address for the whole machine — `somsrv` is a shared,
/// host-scoped daemon (see `project_som_tmux` memory and the design notes
/// this superseded: the OLD architecture minted a distinct pipe name PER
/// PANE, since each pane got its own dedicated HOLDER process). Every
/// RELAY on this machine (regardless of which profile/pane it belongs
/// to, or even which OS account started it — see `ssh_client_id`) connects
/// to this SAME address and identifies itself via `RelayInput::Register`
/// as the connection's second message, right after `Handshake`. There is
/// no longer a per-pane name to construct, so this takes no arguments.
///
/// Platform-specific shape: Windows named pipes live in a flat global
/// namespace (`\\.\pipe\...`), no directory involved. Unix domain sockets
/// are actual filesystem paths — kept per-uid (`/tmp/somsrv-<uid>.sock`)
/// so several different OS accounts on a shared machine each still get
/// their OWN daemon and session registry, while every tab/profile
/// belonging to the SAME account shares one, mirroring where a real
/// tmux/screen puts their sockets (`$TMPDIR/tmux-<uid>/...`, though this
/// hardcodes `/tmp` rather than trusting `$TMPDIR` — see the historical
/// `SUN_LEN` overflow this avoided, `project_som_tmux` memory,
/// "Обновление 30").
#[cfg(windows)]
pub fn daemon_socket_path() -> String {
    r"\\.\pipe\somsrv".to_string()
}

#[cfg(unix)]
pub fn daemon_socket_path() -> String {
    let dir = std::path::Path::new("/tmp");
    let _ = std::fs::create_dir_all(dir);
    dir.join(format!("somsrv-{}.sock", unsafe { libc::getuid() })).to_string_lossy().into_owned()
}

/// `<remote-username>@<client-ip>` identifying BOTH the OS account that
/// SSHed into THIS host AND which machine it came from — the IP half comes
/// from sshd's own `$SSH_CLIENT`, the username half from THIS process's own
/// effective user (`whoami`-equivalent). Passed down through `--client-id`
/// on every RELAY-spawned HOLDER (see `main.rs`'s `Args`) so a HOLDER's
/// `ps` command line records who/what created it. Exists purely for orphan
/// cleanup and version-mismatch teardown: `kill_orphaned_holders`/
/// `kill_all_holders_for_redeploy` (`terminal_panel.rs`) run once per host
/// at handshake time (themselves a fresh `ssh host ...` invocation, so they
/// see their OWN `$SSH_CLIENT`/user, naturally equal to what any other
/// invocation from this same account on this same machine already got) and
/// must only ever touch HOLDERs they can actually judge "belongs to me or
/// not" — a HOLDER created by a DIFFERENT client machine, or a different
/// OS account on the SAME remote host (e.g. a shared build server with
/// several users each running their own Som), is invisible to this
/// account's own `db.json` and must never be treated as this account's to
/// kill, even though the raw `ps` listing sees it just fine. Comparing
/// `--client-id` against this very connection's own `<user>@$SSH_CLIENT`
/// needs no coordination between clients or accounts at all — sshd/the
/// remote OS already independently reports the same identity to every SSH
/// connection from the same account on the same machine.
///
/// Belt-and-suspenders on top of what Unix file/process permissions
/// already enforce (a non-privileged user's `kill` on another user's
/// process fails regardless) — this filter exists so a buggy cleanup query
/// simply never CONSIDERS another account's HOLDER a candidate in the
/// first place, rather than relying on the kill syscall to silently reject
/// it.
///
/// Only ever meaningful for an SSH `tmux: true` profile's RELAY (which
/// really does run ON the remote host, spawned via `ssh host
/// ~/.local/bin/somsrv ...` — see `wrap_remote_command_args`) — `None`
/// for a local or WSL RELAY, neither of which goes through sshd and so has
/// no `$SSH_CLIENT` to read; `kill_orphaned_holders` only ever runs for
/// `RemoteKind::Ssh` anyway, so those callers simply never pass `--client-
/// id` at all.
pub fn ssh_client_id() -> Option<String> {
    let raw = std::env::var("SSH_CLIENT").ok()?;
    let ip = raw.split_whitespace().next()?;
    let user = whoami_unix()?;
    Some(format!("{user}@{ip}"))
}

#[cfg(unix)]
fn whoami_unix() -> Option<String> {
    // SAFETY: `geteuid()` has no failure mode (always returns the calling
    // process's real effective uid) and `getpwuid` on a valid uid returns
    // either a valid pointer into thread-local/static storage libc owns
    // (never freed by us, never mutated after return) or null — both
    // branches handled below.
    unsafe {
        let passwd = libc::getpwuid(libc::geteuid());
        if passwd.is_null() {
            return None;
        }
        let name = std::ffi::CStr::from_ptr((*passwd).pw_name);
        Some(name.to_string_lossy().into_owned())
    }
}

#[cfg(not(unix))]
fn whoami_unix() -> Option<String> {
    None
}

/// Which OS a HOLDER is running on — part of the handshake (see
/// `HandshakeInfo`) so a RELAY (potentially a newer build than the HOLDER
/// it's reattaching to, e.g. after Som updated but a remote HOLDER survived
/// from before) can tell whether they're even compatible before trusting
/// anything else about the connection. Deliberately only the four
/// combinations `project_som_tmux` memory ("Обновление 21") says are
/// actually supported — Intel Mac and Windows-on-ARM are excluded on
/// purpose ("пока не поддерживается" — not supported YET, not a permanent
/// decision, just nothing to detect or build for right now).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    Windows,
    Darwin,
    Linux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Arch {
    Amd64,
    Arm64,
}

/// Detects the CURRENT process's own OS/architecture — used by both sides
/// to fill in their half of the handshake. `cfg!` rather than
/// `std::env::consts::{OS,ARCH}` string-matching, since those give runtime
/// strings ("windows", "aarch64") that would need re-parsing into this enum
/// anyway, and the actual platform is a compile-time fact for a given
/// binary.
pub fn current_platform() -> (Os, Arch) {
    let os = if cfg!(target_os = "windows") {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::Darwin
    } else {
        Os::Linux
    };
    let arch = if cfg!(target_arch = "x86_64") { Arch::Amd64 } else { Arch::Arm64 };
    (os, arch)
}

/// Directory name (under `assets`'s embedded `srv/` tree, e.g. `srv/
/// linux-amd/somsrv` — see `crates/assets/src/assets.rs`'s `#[include =
/// "srv/..."]` entries and `terminal_view::terminal_panel::stage_embedded_
/// somsrv_to_temp_file`, the one reader of this) holding the pre-built
/// `somsrv` for a given `(Os, Arch)` pair, one per (os, arch) pair this
/// codebase actually supports (see `Os`/`Arch`'s own doc comment for the
/// four supported combinations — Intel Mac and Windows-on-ARM are
/// excluded on purpose, so there is no `windows-arm`/`macos-amd` entry to
/// name). This is an asset-lookup KEY baked into `som.exe` at build time,
/// not a path anywhere on a user's disk — Som keeps no persistent local
/// cache of these binaries at all (2026-09-15: the only files Som/`som-
/// srv` ever write outside `settings.json`/`db.json`/logs are throwaway
/// temp files, extracted fresh from the embedded copy immediately before
/// an `scp`/spawn and deleted right after).
pub fn platform_dir_name(os: Os, arch: Arch) -> &'static str {
    match (os, arch) {
        (Os::Windows, Arch::Amd64) => "windows-amd",
        (Os::Darwin, Arch::Arm64) => "macos-arm",
        (Os::Linux, Arch::Amd64) => "linux-amd",
        (Os::Linux, Arch::Arm64) => "linux-arm",
        // Genuinely unsupported combos (Windows-on-ARM, Intel Mac) — no
        // asset exists for these; any placeholder is fine as long as it
        // can never collide with a real supported name above.
        (Os::Windows, Arch::Arm64) => "windows-arm-unsupported",
        (Os::Darwin, Arch::Amd64) => "macos-amd-unsupported",
    }
}

/// Exchanged first thing on every new connection, before any actual
/// terminal data — see `project_som_tmux` memory ("Обновление 19"/"21") for
/// the full policy this feeds into (always copy a newer binary over an
/// older one on disk; only ever restart a LIVE, already-running HOLDER
/// process if none of its panes have live child processes). This type only
/// carries the raw facts (version string, OS, arch) — the actual
/// version-compare/restart-or-not DECISION lives in `crate::relay`, not
/// here, since it needs additional context (e.g. "is the shell busy") this
/// protocol module has no business knowing about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeInfo {
    /// `env!("CARGO_PKG_VERSION")` at build time — compared as a plain
    /// string, not semver-parsed: any difference at all (not just a
    /// semver-incompatible one) is treated as "different build, evaluate
    /// whether to update" by the policy in `crate::relay`, since this
    /// binary doesn't follow a public semver contract with itself.
    pub version: String,
    pub os: Os,
    pub arch: Arch,
}

impl HandshakeInfo {
    pub fn current() -> Self {
        let (os, arch) = current_platform();
        Self { version: env!("CARGO_PKG_VERSION").to_string(), os, arch }
    }
}

/// The very FIRST message on any fresh connection to the shared daemon —
/// a single length-prefixed byte (via `PipeConnection::read_message`/
/// `write_message`, same framing as every other message this module
/// defines, just with a 1-byte payload) sent before either side
/// constructs a `RelayInput`/`SrvRequest`. Lets ONE `accept_on` loop
/// (`server::run`) serve two structurally different protocols on the
/// SAME fixed address (`daemon_socket_path`) — a RELAY's PTY session
/// (`RelayInput`/`HolderOutput`) and an SRP client's binary side-channel
/// or an admin tool's session-management query (`SrvRequest`/
/// `SrvResponse`) — without needing two different listening addresses or
/// guessing a connection's kind by trial-deserializing its first real
/// message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ConnectionKind {
    /// This connection is a RELAY proxying one PTY session — the daemon
    /// dispatches it to `server::handle_relay`.
    Relay = 0,
    /// This connection speaks `SrvRequest`/`SrvResponse` — the binary
    /// side-channel for rich media (today) or an admin session-management
    /// query (`ListSessions`/`KillSession`) — the daemon dispatches it to
    /// a separate handler, never touching PTY/`Session` state directly
    /// except through the same registry `handle_relay` itself uses.
    Srv = 1,
}

impl ConnectionKind {
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Relay),
            1 => Some(Self::Srv),
            _ => None,
        }
    }

    /// Reads this connection's leading `ConnectionKind` byte off `conn` —
    /// the caller (`server::run`'s accept loop) does this ONCE per fresh
    /// connection, before dispatching to either `server::handle_relay` or
    /// the `SrvRequest` handler.
    pub fn read_from(conn: &crate::pipe::PipeConnection) -> anyhow::Result<Self> {
        let message = conn.read_message()?;
        let &[byte] = message.as_slice() else {
            anyhow::bail!("expected a single-byte ConnectionKind message, got {} bytes", message.len());
        };
        Self::from_u8(byte).ok_or_else(|| anyhow::anyhow!("unknown ConnectionKind byte {byte}"))
    }

    /// Writes this connection's leading `ConnectionKind` byte to `conn` —
    /// the caller (a fresh RELAY or `SrvRequest` client) does this ONCE,
    /// before sending anything else.
    pub fn write_to(self, conn: &crate::pipe::PipeConnection) -> anyhow::Result<()> {
        conn.write_message(&[self as u8])?;
        Ok(())
    }
}

/// RELAY -> daemon: input coming from Som's own PTY (whatever the user
/// typed) gets forwarded verbatim, plus the couple of control events a
/// terminal needs to convey out-of-band from plain bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RelayInput {
    /// Always the FIRST message a RELAY sends on a fresh connection, before
    /// anything else — see `HandshakeInfo`'s doc comment. The daemon
    /// replies in kind with `HolderOutput::Handshake` before its first
    /// redraw.
    Handshake(HandshakeInfo),
    /// Always the SECOND message, right after `Handshake` — identifies
    /// which session this connection belongs to now that `somsrv` is a
    /// single shared daemon listening on ONE fixed address
    /// (`daemon_socket_path`) rather than a dedicated per-pane HOLDER
    /// process. The daemon looks `(client_id, pane_id)` up in its session
    /// registry: found → this is a reconnect, every OTHER field here is
    /// ignored (the existing session already knows its own program/args/
    /// etc from when it was first created — a stale or different value
    /// here on reconnect must never silently respawn or reconfigure a
    /// live session). Not found → spawns a brand new session using
    /// `program`/`args`/`cwd`/`cursor_shape`/`scrollback`, exactly what
    /// the old per-pane HOLDER used to do with its own argv, and inserts
    /// it into the registry under this key.
    Register {
        profile_name: String,
        pane_id: String,
        /// Mirrors the old `--client-id` argv flag — `None` for a local
        /// or WSL RELAY (no sshd involved, so no `$SSH_CLIENT` to read),
        /// `Some("<user>@<client-ip>")` for a real SSH RELAY. Part of the
        /// registry key so two different accounts (or the same account
        /// from two different client machines) on a shared remote host
        /// never see or touch each other's sessions — see
        /// `ssh_client_id`'s doc comment.
        client_id: Option<String>,
        /// This session's own `tmux: true/false` setting — decides
        /// whether the daemon tears this session down on an ungraceful
        /// disconnect (`tmux: false`, matching a plain non-persistent
        /// PTY) or keeps it running for a later reconnect same as today's
        /// HOLDER already does (`tmux: true`). Ignored on reconnect, same
        /// as every other field here — a session's `tmux` setting is
        /// fixed at creation, not changeable by a later `Register`.
        tmux: bool,
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        cursor_shape: Option<String>,
        scrollback: Option<usize>,
    },
    /// Raw bytes read from Som's side of the RELAY's own PTY — keystrokes,
    /// paste, anything the user's terminal client sends. Forwarded
    /// byte-for-byte into the real shell's PTY on the daemon side.
    Bytes(Vec<u8>),
    /// `cell_width`/`cell_height` are the REAL font cell size in pixels —
    /// `0` means "unknown" (e.g. a RELAY too old to send it, or one that
    /// hasn't extracted a real value out of Som's own resize marker yet —
    /// see `relay::PIXEL_SIZE_MARKER_PREFIX`'s doc comment for why a RELAY
    /// can't just ask Windows for this directly). A HOLDER treats `0` as
    /// "keep whatever it already had" rather than overwriting a real value
    /// with a placeholder — see `Session::force_resize`'s doc comment.
    Resize { cols: u16, rows: u16, cell_width: u16, cell_height: u16 },
    /// Explicit "tab closed via UI" — kills the real shell process for
    /// good, as opposed to the RELAY simply disconnecting (which leaves
    /// a `tmux: true` session running for a later reattach; a `tmux:
    /// false` session is torn down on ANY disconnect, graceful or not —
    /// see `Register::tmux`'s doc comment). Mirrors the detach-vs-kill
    /// semantics from the old protocol's `CloseSession`.
    Close,
}

/// HOLDER -> RELAY: the HOLDER owns a headless `alacritty_terminal::Term`
/// that mirrors the real shell's actual terminal state (fed by the real
/// PTY's output through the normal ANSI parser), and sends that STATE to
/// the RELAY rather than diffing/replaying ANSI bytes — diffing visible
/// grid content structurally cannot carry terminal MODES like DECCKM, since
/// a mode flip has no visible content of its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HolderOutput {
    /// Always the FIRST message a HOLDER sends back on a fresh connection,
    /// replying to the RELAY's own `RelayInput::Handshake` — see
    /// `HandshakeInfo`'s doc comment.
    Handshake(HandshakeInfo),
    /// A bincode-encoded `alacritty_terminal::term::serialize::TermState`
    /// (`Term::snapshot()`'s return value) — sent as the SECOND message on
    /// every connection (right after the handshake), before anything else.
    /// The RELAY deserializes it and calls `Term::restore()` on its OWN
    /// local `Term` (the same one Som's `Terminal`/`TerminalView` already
    /// render), which sets the grid, cursor, and — critically — the FULL
    /// `TermMode` bitflags directly, correct from the very first frame
    /// after a (re)connect. No ANSI replay needed to reproduce a screen a
    /// RELAY missed while disconnected; the state itself just IS correct.
    Snapshot(Vec<u8>),
    /// Raw bytes read from the real shell's PTY, forwarded so the RELAY can
    /// feed them through the SAME `ansi::Processor::advance` path any
    /// ordinary (non-tmux) Som terminal already uses on its own local
    /// `Term` — this is what keeps that `Term` (already correctly
    /// initialized by the `Snapshot` above) live and up to date afterward.
    /// These are NOT diffed/reconstructed ANSI from a `Redrawer` — they're
    /// literally what the real shell wrote, same as `RelayInput::Bytes`
    /// already is for the opposite direction.
    Bytes(Vec<u8>),
    /// The real shell process exited — the RELAY should exit too (nothing
    /// left to proxy). Distinct from a HOLDER-initiated disconnect for any
    /// other reason (which the RELAY treats as "connection lost, nothing
    /// more to do" without necessarily needing to know why).
    ShellExited,
}

/// `somsrp`/other SRP clients -> daemon: the binary side-channel for rich
/// media content (video/image/audio today, `md://`'s `som-lua` scripts
/// later) — a SEPARATE connection kind from `RelayInput`/`HolderOutput`
/// (which stay dedicated to PTY keystrokes/ANSI bytes), even though both
/// dial the same `daemon_socket_path()` and use the same length-prefixed
/// `PipeConnection` framing. Exists specifically so large file payloads
/// never have to travel through Som's own PTY at all — see this crate's
/// role in the wider SRP transport redesign (`rich_content_transport` in
/// `crates/terminal`, which keeps doing small control-handshake duty
/// only: `(session_id, file_id)` and metadata, not payload bytes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SrvRequest {
    /// Always the FIRST message on a fresh side-channel connection — same
    /// role as `RelayInput::Handshake`, just on this separate connection
    /// kind.
    Handshake(HandshakeInfo),
    /// One piece of a file being streamed — mirrors
    /// `rich_content_transport::Chunk`'s fields exactly (same
    /// `(session_id, file_id)` pair Som's PTY-side control handshake
    /// already established via `print_placeholder_grid`), but carries
    /// `data` RAW, with no base91/APC encoding — this channel never
    /// touches a real ConPTY-backed stdout, so there is no console
    /// codepage to defend against (see `rich_content_transport`'s own
    /// doc comment for why that encoding exists at all on the PTY path).
    /// `total_size` travels on every chunk, not just the first, for the
    /// same uniformity reason `rich_content_transport::Chunk` already
    /// does this. `content_type`/`metadata` also travel on every chunk,
    /// same reasoning: a receiver never needs to special-case "the first
    /// chunk looks different." `somsrv` stores whichever copy arrived
    /// LAST (harmless — a real sender's metadata for one `(session_id,
    /// file_id)` never actually changes mid-transfer) and reports it to
    /// Som via `SrvResponse::Progress`'s own `metadata` field, since
    /// `somsrv` has no GPUI dependency and can't decode content itself —
    /// it only relays what the sender already knows.
    PutChunk {
        session_id: u32,
        file_id: u32,
        offset: u64,
        data: Vec<u8>,
        total_size: u64,
        content_type: ContentType,
        metadata: ContentMetadata,
    },
    /// Admin query: every tmux session currently in the daemon's registry
    /// belonging to `client_id` (or every LOCAL session, if `client_id` is
    /// `None` — a local/WSL RELAY's own sessions). Replaces the OLD
    /// per-pane-HOLDER architecture's `ps`-grep-for-`--client-id`
    /// orphan-detection approach (`kill_orphaned_holders` in `terminal_
    /// panel.rs`), which read session identity off individual PROCESSES'
    /// command lines — meaningless now that one shared daemon process
    /// holds every session, with no per-session process/argv to grep at
    /// all. Answered with `SrvResponse::Sessions`.
    ListSessions { client_id: Option<String> },
    /// Admin command: tear down one specific session immediately,
    /// regardless of its `tmux` setting or whether a RELAY is currently
    /// connected to it — the direct replacement for the old design's "SSH
    /// in and `kill <pid>` a specific orphaned HOLDER process." Used by
    /// `kill_orphaned_holders`'s replacement (a session found via
    /// `ListSessions` whose `pane_id` isn't in the caller's own
    /// `db.json`) and by `kill_all_holders_for_redeploy`'s replacement
    /// (every session for a given `client_id`, unconditionally, ahead of
    /// deploying a newer `somsrv` build). No-op (not an error) if
    /// `(client_id, pane_id)` isn't in the registry — same "already
    /// gone, nothing to do" tolerance the old `kill <pid>`-based approach
    /// had for a PID that had already exited on its own.
    KillSession { client_id: Option<String>, pane_id: String },
    /// Sent by Som (never by `somsrp`) right after `Handshake`, on a
    /// SECOND long-lived `Srv`-kind connection separate from whichever
    /// one (if any) is sending `PutChunk`s for this same
    /// `(session_id, file_id)` — subscribes this connection to
    /// `SrvResponse::Progress` pushes as they land, so Som can track
    /// `contiguous_len` accurately (gap-tolerant, same semantics
    /// `RichContentCache::apply_chunk` already has today) without racing
    /// a raw on-disk file-size poll against out-of-order chunk arrival.
    SubscribeProgress { session_id: u32, file_id: u32 },
    /// Sent by Som (never by `somsrp`) on the SAME connection as
    /// `SubscribeProgress` above, when this placement's in-memory state
    /// (`crate::terminal::rich_content_srv_channel::SrvProgressState` and
    /// friends) has just been evicted because its placeholder cells are
    /// no longer anywhere in the grid (scrolled out of `scrollback`'s own
    /// bounded history, or wiped by a real `clear` — see `Terminal::
    /// evict_vanished_image_gif_markdown_placements`'s own doc comment).
    /// `somsrv` answers with `SrvResponse::Unsubscribed` pushed back on
    /// THIS SAME connection — the one, and only, reliable way to
    /// unblock that connection's own blocking `read_message()` call from
    /// the outside: this daemon has no other mechanism to forcibly close
    /// a specific client connection from a DIFFERENT thread, so instead
    /// it wakes the blocked reader with a real message it's built to
    /// recognize as "stop reading, this connection is done" (see
    /// `Unsubscribed`'s own doc comment). For an image/GIF/markdown
    /// placement (the only content types this is used for today), the
    /// transfer has virtually always already finished by the time
    /// eviction fires, so there is nothing lost by no longer listening
    /// for further `Progress` pushes on this key.
    UnsubscribeProgress { session_id: u32, file_id: u32 },
    /// Sent by Som (never by `somsrp`) on the SAME connection as
    /// `SubscribeProgress` above, whenever it needs bytes further into a
    /// file than the sequential `PutChunk` stream has reached yet (e.g.
    /// seeking forward in audio/video playback past what's currently
    /// cached) — the direct replacement for the old PTY-based `Query::
    /// AudioByteRange` mechanism (`crates/terminal/src/
    /// rich_content_transport.rs`, now deleted). The daemon looks up
    /// `(session_id, file_id)` in its sender-routing table (populated by
    /// the first `PutChunk` seen for that key) and forwards this same
    /// message, verbatim, down THAT connection — the client that's
    /// actually holding the file (`somsrp` or equivalent) answers by
    /// sending ordinary `PutChunk`s covering `[offset, offset+len)` back
    /// on its own connection, same as it would for any other part of the
    /// file; there is no separate "range response" message shape needed,
    /// mirroring how `Query::AudioByteRange`'s answer was always just
    /// ordinary chunk envelopes at arbitrary offsets. Silently
    /// undeliverable (no-op) if the sender connection has already closed
    /// — see this plan's own doc comment for why that's an accepted gap,
    /// not a new failure mode.
    RequestByteRange { session_id: u32, file_id: u32, offset: u64, len: u64 },
    /// Sent by `somsrp` (never by Som) on a SECOND connection, separate
    /// from whichever one is sending the sequential `PutChunk` stream for
    /// this `(session_id, file_id)` — registers THIS connection as the
    /// one `RequestByteRange` gets forwarded to, instead of the
    /// sequential sender's own connection. Exists because the sequential
    /// sender for a large file can be mid-flight for minutes, holding its
    /// own connection saturated with a steady stream of outgoing
    /// `PutChunk`s the whole time — a `RequestByteRange` reply sharing
    /// that SAME connection (and the sender-side `write_lock` guarding
    /// it) has to win a mutex race against every one of those chunks to
    /// get a word in, and a plain (non-fair) `Mutex` has no obligation to
    /// let a rarely-contending thread win against one re-acquiring the
    /// lock in a tight loop — confirmed live as multi-second-to-minutes
    /// seek latency that scaled with file size (more chunks in flight for
    /// longer = more mutex contention to lose to), even though the
    /// `avformat_seek_file`/disk-read work a seek actually needs is
    /// itself sub-millisecond. Routing range responses through a
    /// dedicated connection removes the contention entirely rather than
    /// trying to arbitrate it. `somsrv` keeps BOTH the implicit
    /// PutChunk-derived route and this explicit one; `route_byte_range_
    /// request` prefers this one when present (see `SrvCache`'s own doc
    /// comment).
    RegisterRangeResponder { session_id: u32, file_id: u32 },
    /// Sent by `somsrp` on a fresh, one-shot connection when starting a
    /// live markdown placement's grower thread — registers THAT
    /// connection as the target for `SrvRequest::GrowMarkdownRows`
    /// forwarding. DELIBERATELY NOT `RegisterRangeResponder`, even though
    /// both are "somsrp says where to send me things for this id" —
    /// reusing that registration made `SrvCache::route_byte_range_request`
    /// treat a successful SEND to the markdown grower (which doesn't
    /// understand `RequestByteRange` at all) as "handled," silently
    /// swallowing a late-subscriber catch-up request that should have
    /// fallen through to `SrvCache::serve_from_recent_bytes` instead —
    /// confirmed live as the root cause of a markdown placement never
    /// rendering at all once its `somsrp` process started registering a
    /// range responder for the live-process model. Routed via its own
    /// dedicated `SrvCache::markdown_grower_routes` table, entirely
    /// separate from `range_response_routes`.
    RegisterMarkdownGrower { session_id: u32, file_id: u32 },
    /// Sent by Som (never by `somsrp`) on the SAME connection as
    /// `SubscribeProgress`/`RequestByteRange`, telling whichever client
    /// registered itself as `(session_id, file_id)`'s range responder
    /// (`RegisterRangeResponder`) that playback has definitively ended —
    /// natural end-of-content (decode reached EOF) or the user pressing
    /// the widget's own stop icon / closing the placement. Routed by the
    /// daemon exactly like `RequestByteRange` (`SrvCache::route_byte_
    /// range_request`'s same `range_response_routes` lookup, see that
    /// method's own doc comment) straight to the registered responder
    /// connection — `somsrp`'s reader loop treats this as its cue to stop
    /// answering `RequestByteRange` and exit, letting the shell that
    /// launched it print its next prompt. This is the reverse direction
    /// of the existing `StopPlayback` variant below (that one is sent BY
    /// an SRP client TO Som, e.g. yazi's preview cursor moving away) —
    /// the two together cover both "the source wants the viewer to stop"
    /// and "the viewer wants the source to stop" without conflating them
    /// into one ambiguous message. Silently undeliverable (no-op) if the
    /// responder connection has already closed — same tolerance every
    /// other best-effort message in this protocol already has (e.g. the
    /// Ctrl+C case: `somsrp` is already gone by the time this would
    /// arrive, so there's nothing left to tell — the daemon separately
    /// notices that disconnect on its own and pushes `StopPlayback` to
    /// subscribers, see `handle_srv_request`'s own doc comment).
    EndPlayback { session_id: u32, file_id: u32 },
    /// Sent by Som on a fresh, one-shot connection whenever a live
    /// markdown placement's real laid-out row count (`layout_markdown`,
    /// which alone knows the real word-wrap width) exceeds however many
    /// placeholder-grid rows `somsrp` has printed so far. Routed by the
    /// daemon exactly like `EndPlayback` (`SrvCache::route_grow_markdown_
    /// rows`'s same `range_response_routes` lookup — `somsrp` registers
    /// via the EXISTING `RegisterRangeResponder`, no separate
    /// registration message needed for this) straight to the registered
    /// responder connection. Unlike `RequestByteRange`/`EndPlayback`,
    /// receiving this does NOT end `somsrp`'s reader loop — a live
    /// markdown placement can grow many times over its lifetime as more
    /// of the document streams in and gets laid out, each growth simply
    /// printing `additional_rows` more placeholder cells (continuing the
    /// SAME placement's row numbering, never restarting from row 0) and
    /// looping back to keep listening. Exists specifically because
    /// `somsrp` (a plain CLI with no GPUI/font-shaping dependency) cannot
    /// predict the real wrapped row count up front the way it can for
    /// audio/video's own fixed pixel-derived footprint — see
    /// `markdown_line_count`'s removal (this variant is what replaced
    /// it) for the full "why a lightweight prediction wasn't good enough"
    /// reasoning. Silently undeliverable (no-op) if the responder
    /// connection has already closed, same tolerance every other best-
    /// effort message in this protocol already has.
    GrowMarkdownRows { session_id: u32, file_id: u32, additional_rows: u32 },
    /// Runs `script_source` as a fresh, explicitly-sandboxed `mlua::Lua`
    /// VM (see `crate::lua::phase1_stdlib`'s own doc comment — NOT
    /// `mlua::Lua::new()`'s default, which turned out live-confirmed to
    /// still include `io`, since mlua's own `StdLib::ALL_SAFE` classifies
    /// `io` as "safe" in the sense of "doesn't corrupt the VM," not "no
    /// filesystem access") INSIDE `somsrv` itself — the first case where the
    /// daemon originates `PutChunk`s on its own, rather than only relaying
    /// ones an external client (`somsrp`, the yazi driver) already sent.
    /// The script's single string return value becomes the markdown
    /// source, chunked and pushed through the exact same `SrvCache::
    /// put_chunk` path (and thus the exact same `SrvResponse::Progress`
    /// subscriber-notification machinery) a real `PutChunk` sender already
    /// uses — `(session_id, file_id)` here plays the identical role it
    /// does in `PutChunk` itself: the placeholder-grid id a client printed
    /// on the PTY BEFORE sending this request, so Som already has
    /// somewhere to paint the result once it arrives. Phase 1 only
    /// (`SRP_LUA.md`): no filesystem/network/DB access exposed to the
    /// script yet, and no persistent VM across calls — each request gets
    /// its own fresh `Lua::new()`, run synchronously to completion before
    /// this variant's handler returns.
    RunLuaScript { session_id: u32, file_id: u32, script_source: String },
    /// Resolves `target` (an `http://`/`https://` URL, or a filesystem
    /// path — relative paths are joined against `base_dir`, absolute
    /// paths used as-is) and streams its bytes into Som through the SAME
    /// `SrvCache::put_chunk` path/`SrvResponse::Progress` push machinery
    /// `PutChunk`/`RunLuaScript` already use — a second case (alongside
    /// `RunLuaScript`) where `somsrv` itself originates `PutChunk`s
    /// rather than only relaying ones an external client already sent.
    /// `(session_id, file_id)` plays its usual role: the placeholder-grid
    /// id a client printed on the PTY BEFORE sending this request. Built
    /// for a future markdown-embedded-media feature (an `![alt](target)`
    /// link inside a markdown document Som is displaying) — `target` is
    /// whatever raw string that link pointed at, `base_dir` is the
    /// sending client's own working directory at the time (`somsrp`'s
    /// `std::env::current_dir()`), needed to resolve a *relative*
    /// `target` the same way a shell would; ignored for an absolute path
    /// or a `http(s)://` URL. No markdown parsing exists yet to actually
    /// send this in practice — this variant exists so the transport is
    /// ready ahead of that feature landing.
    ///
    /// Deliberately NO path sandboxing beyond whatever the OS's own file
    /// permissions already enforce for the user `somsrv` runs as: reaching
    /// `somsrv` at all already implies the caller is the authenticated
    /// owner of this session (same trust boundary as having an
    /// interactive shell on this host), so a local/absolute `target`
    /// resolves exactly as if that same user ran `cat` on it themselves
    /// — no canonicalize-and-verify-prefix jail is built on top of that.
    ///
    /// On failure (DNS/connect/TLS/non-2xx for a URL; not-found/
    /// permission-denied/unreadable for a local path; an unrecognized
    /// `Content-Type`/extension), replies with `SrvResponse::FetchFailed`
    /// — broadcast to every current `SubscribeProgress` subscriber of
    /// this `(session_id, file_id)` (see `SrvCache::notify_fetch_failed`,
    /// mirroring how `StopPlayback` already broadcasts via `notify_stop_
    /// playback`), AND written back directly on this same connection.
    /// The broadcast is the one that actually matters: the sender of
    /// `FetchResource` is expected to be a fire-and-forget one-shot
    /// connection it closes immediately (see `rich_content_srv_channel::
    /// request_fetch_resource`'s own doc comment), so a reply on that
    /// SAME connection would go unread — unlike `RunLuaScript`'s failure
    /// path (logged server-side only, caller never told), a fetch
    /// failure needs a real signal back to whoever is actually listening
    /// (the requester's own `SubscribeProgress` connection) so a markdown
    /// widget can show "failed to load" instead of hanging forever.
    FetchResource { session_id: u32, file_id: u32, target: String, base_dir: Option<String> },
    /// Sent by an SRP client (`somsrp`, the yazi driver — never by Som
    /// itself) on a fresh, one-shot connection, exactly mirroring
    /// `RequestByteRange`'s own connection shape — the mechanism a
    /// PREVIEW-style client uses to tell Som "stop decoding/playing
    /// `(session_id, file_id)`'s audio/video, its placeholder cells are
    /// about to be overwritten by a different placement." Exists
    /// specifically for yazi's own preview pane: switching the cursor to
    /// a different file overwrites the PTY-side placeholder cells
    /// belonging to whatever audio/video was previously playing, but
    /// (unlike a real `clear` escape sequence, which `Terminal::process_
    /// event`'s `AlacTermEvent::ClearScreen` arm already handles) nothing
    /// on the wire told Som the OLD placement was actually abandoned —
    /// its `cpal`/FFmpeg decode thread just kept running fully audible
    /// with no widget left anywhere to reach it. `somsrv` forwards this,
    /// verbatim, to every current `SubscribeProgress` subscriber for the
    /// same key (see `SrvCache::notify_stop_playback`) as `SrvResponse::
    /// StopPlayback` — in practice that's Som's own long-lived progress-
    /// listener connection, the same one `SrvResponse::Progress` already
    /// arrives on. Silently a no-op if nobody is subscribed (e.g. Som
    /// already tore the player down itself, or never opened one for this
    /// id at all) — same "already gone, nothing to do" tolerance every
    /// other best-effort message in this protocol already has.
    StopPlayback { session_id: u32, file_id: u32 },
}

/// daemon -> `somsrp`/other SRP clients, answering a `SrvRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SrvResponse {
    /// Always the FIRST message, replying to `SrvRequest::Handshake`.
    Handshake(HandshakeInfo),
    /// Answers `SrvRequest::ListSessions`.
    Sessions(Vec<SessionInfo>),
    /// Answers `SrvRequest::KillSession` — always sent, whether or not a
    /// matching session was actually found (see `KillSession`'s own doc
    /// comment on why a miss is a no-op, not an error).
    Killed,
    /// Pushed (unsolicited, not a direct reply to any single request) on
    /// a connection that sent `SrvRequest::SubscribeProgress`, once per
    /// `PutChunk` that advances this `(session_id, file_id)`'s
    /// contiguous-length watermark — mirrors what `VideoTransferProgress`/
    /// `AudioTransferProgress`'s atomics on the Som side already track,
    /// just pushed over the wire instead of updated from a local
    /// `apply_chunk` call (which `somsrv`, having no GPUI dependency,
    /// cannot make on Som's behalf). `content_type`/`metadata` are
    /// whatever the sender's most recent `PutChunk` carried — Som uses
    /// this the same way it used to read `Chunk::content_type`/
    /// `Chunk::metadata` off a parsed PTY envelope, just delivered here
    /// instead.
    Progress {
        session_id: u32,
        file_id: u32,
        contiguous_len: u64,
        /// The lowest offset such that every byte from here through
        /// `total_size` has arrived — starts at `total_size` (nothing
        /// confirmed) and shrinks toward 0 as tail bytes land. Lets a
        /// consumer like `GrowingFileStream` (video decode's custom
        /// `Seek`/`Read`, `crates/terminal/src/rich_content_video_player.rs`)
        /// serve a `SeekFrom::End`-derived read from the FILE'S TAIL
        /// once that region specifically has arrived, without waiting
        /// for `contiguous_len` to grow all the way there from 0 —
        /// `contiguous_len` alone can't express "the tail arrived early
        /// out of order," since it only ever advances from the front.
        /// Added after a live-confirmed bug: an MKV whose Cues (seek
        /// index) sit near the end took ~20 minutes to start playing on
        /// a 16GB file, because the existing speculative tail-fetch
        /// (`somsrp`'s `stream_file_from_disk`) wrote the tail bytes to
        /// disk successfully, but `GrowingFileStream::read` had no way
        /// to know they were there — it only trusted `contiguous_len`,
        /// which doesn't move until the SEQUENTIAL send reaches that
        /// offset.
        tail_available_from: u64,
        /// Out-of-order byte ranges that have arrived but aren't yet
        /// folded into `contiguous_len` (still growing from the front)
        /// or `tail_available_from` (still shrinking from the back) —
        /// e.g. the response to a SEEK into the middle of a still-
        /// downloading file, which lands nowhere near either the front
        /// or the tail. Without this, `GrowingFileStream::read`
        /// (`crates/terminal/src/rich_content_video_player.rs`) had no
        /// way to know a mid-file seek target had actually arrived, and
        /// fell back to waiting for the ORDINARY sequential download to
        /// reach that offset naturally — confirmed live as a real bug:
        /// seeking became "eventually works, but the wait is
        /// proportional to how far ahead the seek target is," exactly
        /// as if the seek's own targeted byte-range fetch had no effect
        /// at all. Mirrors `SrvCache`'s own internal `pending_ranges`
        /// field (`crates/somsrv/src/srv_cache.rs`) — same "expected to
        /// stay small" assumption (a real sender streams mostly in
        /// order; the only source of entries here is a small number of
        /// deliberate out-of-order seek/tail fetches, not routine
        /// reordering).
        pending_ranges: Vec<(u64, u64)>,
        total_size: u64,
        content_type: ContentType,
        metadata: ContentMetadata,
        /// The exact `(offset, data)` this push's own triggering
        /// `PutChunk` carried — the actual byte-delivery mechanism now
        /// that `somsrv` no longer persists chunks to disk (see
        /// `srv_cache::SrvCache::put_chunk`'s own doc comment for why):
        /// a subscriber (Som's `SrvProgressState`) appends `chunk_data`
        /// to its own in-memory forward-only buffer at `chunk_offset`
        /// instead of reading it back off a file `somsrv` would
        /// otherwise have written. Travels on every push, not just
        /// pushes that advance `contiguous_len` from the front — an
        /// out-of-order/tail/`RequestByteRange`-answer chunk still needs
        /// its own bytes delivered even though it doesn't move that
        /// particular watermark.
        chunk_offset: u64,
        chunk_data: Vec<u8>,
    },
    /// Pushed (unsolicited) on a `SubscribeProgress` connection, answering
    /// a DIFFERENT client's `SrvRequest::StopPlayback` for the same
    /// `(session_id, file_id)` — see that variant's own doc comment.
    StopPlayback { session_id: u32, file_id: u32 },
    /// Pushed on a `SubscribeProgress` connection in direct reply to that
    /// SAME connection's own `SrvRequest::UnsubscribeProgress` — see that
    /// variant's own doc comment for why this exists (unblocking a
    /// blocking `read_message()` call from the outside). The receiving
    /// side's own subscription loop treats this as its cue to return
    /// immediately, same as if its own `stop` flag had fired.
    Unsubscribed { session_id: u32, file_id: u32 },
    /// Pushed to every `SubscribeProgress` subscriber of `(session_id,
    /// file_id)` whose `SrvRequest::FetchResource` fetch/read did not
    /// succeed (see that variant's own doc comment for the full list of
    /// failure causes and why this is broadcast rather than only replied
    /// to the requesting connection). `reason` is a human-readable
    /// message (from the underlying `ureq`/`std::io::Error`, or
    /// "unrecognized content type"/similar) suitable for direct display,
    /// not a machine-parseable error code — there is exactly one caller
    /// of this today (a markdown widget showing "failed to load"), which
    /// has no need to branch on failure kind.
    FetchFailed { session_id: u32, file_id: u32, reason: String },
}

/// Mirrors `crates/terminal/src/rich_content_transport::ContentType`
/// field-for-field — a separate, serializable copy rather than a shared
/// dependency, since `somsrv` deliberately has no dependency on
/// `crates/terminal` (which pulls in GPUI, entirely unwanted in a small
/// standalone daemon binary). Keep the two in sync by hand if either
/// ever gains/removes a variant — there is no automated check for this
/// today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentType {
    Gif,
    Audio,
    Markdown,
    Video,
    Jpeg,
    Png,
}

/// Mirrors `crates/terminal/src/rich_content_transport::VideoCodec`
/// field-for-field — see `ContentType`'s own doc comment for why this is
/// a separate copy, not a shared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    Unknown,
    H264,
    H265,
    Vp9,
    Av1,
    Mpeg4,
}

/// Mirrors `crates/terminal/src/rich_content_transport::ContentMetadata`
/// field-for-field — see `ContentType`'s own doc comment for why this is
/// a separate copy, not a shared type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentMetadata {
    Image { width_px: u32, height_px: u32, color_bits: u8, is_animated: bool },
    Audio {
        sample_rate: u32,
        channels: u8,
        bits_per_sample: u8,
        duration_ms: u32,
        /// Mirrors `crates/terminal/src/rich_content_transport::
        /// ContentMetadata::Audio::extension` field-for-field — see that
        /// field's own doc comment for why this exists now that `somsrv`
        /// no longer persists chunks to a real on-disk file (`SrvCache`'s
        /// own doc comment) whose extension a decoder's probe could
        /// otherwise infer from a `Path`.
        extension: String,
    },
    Video {
        width_px: u32,
        height_px: u32,
        fps_numerator: u32,
        fps_denominator: u32,
        codec: VideoCodec,
        /// Mirrors `crates/terminal/src/rich_content_transport::
        /// ContentMetadata::Video::audio_stream_index` field-for-field —
        /// see that field's own doc comment.
        audio_stream_index: Option<u32>,
        /// Mirrors `crates/terminal/src/rich_content_transport::
        /// ContentMetadata::Video::subtitle_stream_index` field-for-field
        /// — see that field's own doc comment.
        subtitle_stream_index: Option<u32>,
        /// Mirrors `crates/terminal/src/rich_content_transport::
        /// ContentMetadata::Video::extension` field-for-field — see that
        /// field's own doc comment.
        extension: String,
    },
    Markdown {
        /// The source `.md` file's own directory (`somsrp`'s own
        /// `std::env::current_dir()` at the moment it streamed this
        /// file) — the only way Som can later resolve a RELATIVE
        /// `![alt](./img.png)` link inside this document back to a real
        /// path, now that no on-disk file/`Path` reaches the receiving
        /// side at all (`SrvCache`'s own doc comment: chunks are never
        /// persisted). Threaded back out as `SrvRequest::FetchResource::
        /// base_dir` when Som later fetches an embedded link. Empty
        /// string means unknown — the same "empty means unknown"
        /// convention `Video`/`Audio`'s own `extension` fields already
        /// use for an identical "receiver has no path anymore" problem.
        base_dir: String,
    },
}

/// One entry in a `SrvResponse::Sessions` answer — just enough for a
/// caller to decide "is this one of mine, and is it still wanted"
/// (`kill_orphaned_holders`'s replacement compares `pane_id` against its
/// own `db.json`), without exposing the daemon's internal `Session`/
/// `alacritty_terminal::Term` types across the wire at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub profile_name: String,
    pub pane_id: String,
    pub client_id: Option<String>,
}

#[cfg(test)]
mod platform_dir_tests {
    use super::*;

    /// Regression coverage for a real naming ambiguity: `platform_dir_name`
    /// used to map EVERY `Os::Windows` to a bare `"windows"` and every
    /// `Os::Darwin` to a bare `"macos"`, ignoring `Arch` entirely — fine by
    /// accident today (only one arch per OS is actually supported, see
    /// `Os`/`Arch`'s own doc comment), but the name alone gave no way to
    /// tell whether a given directory held an amd64 or arm64 build.
    /// Renamed to always carry the arch suffix, matching `linux-amd`/
    /// `linux-arm`'s existing convention, so `~/.config/som/tmux/`'s
    /// directory names are self-describing regardless of which two of the
    /// four supported combos anyone's actually using.
    #[test]
    fn windows_amd64_maps_to_windows_amd() {
        assert_eq!(platform_dir_name(Os::Windows, Arch::Amd64), "windows-amd");
    }

    #[test]
    fn macos_arm64_maps_to_macos_arm() {
        // The real Mac this codebase talks to (see project_som_tmux
        // memory) is Apple Silicon (arm64) — Intel Mac is explicitly
        // unsupported (Os/Arch's own doc comment), so this is the only
        // Darwin combo that should ever resolve to a real directory.
        assert_eq!(platform_dir_name(Os::Darwin, Arch::Arm64), "macos-arm");
    }

    #[test]
    fn linux_amd64_maps_to_linux_amd() {
        assert_eq!(platform_dir_name(Os::Linux, Arch::Amd64), "linux-amd");
    }

    #[test]
    fn linux_arm64_maps_to_linux_arm() {
        assert_eq!(platform_dir_name(Os::Linux, Arch::Arm64), "linux-arm");
    }

    #[test]
    fn every_supported_combo_maps_to_a_distinct_directory_name() {
        // The four combos project_som_tmux memory ("Обновление 21") says
        // are actually supported — Intel Mac and Windows-on-ARM excluded
        // on purpose, deliberately not asserted here at all.
        let supported = [
            (Os::Windows, Arch::Amd64),
            (Os::Darwin, Arch::Arm64),
            (Os::Linux, Arch::Amd64),
            (Os::Linux, Arch::Arm64),
        ];
        let names: Vec<&str> = supported.iter().map(|&(os, arch)| platform_dir_name(os, arch)).collect();
        for (i, a) in names.iter().enumerate() {
            for (j, b) in names.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "two supported (os, arch) combos must never share a directory name: {supported:?}");
                }
            }
        }
    }

    #[test]
    fn current_platform_always_resolves_to_a_supported_combo() {
        // Whatever machine actually runs this test, current_platform()'s
        // own (os, arch) result must itself be one of the four supported
        // combos — otherwise Som's own build platform wouldn't even be
        // able to name its own directory under ~/.config/som/tmux/.
        let (os, arch) = current_platform();
        let supported = matches!(
            (os, arch),
            (Os::Windows, Arch::Amd64) | (Os::Darwin, Arch::Arm64) | (Os::Linux, Arch::Amd64) | (Os::Linux, Arch::Arm64)
        );
        assert!(supported, "current_platform() returned an unsupported combo: {os:?}/{arch:?}");
    }

}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// `sockaddr_un::sun_path` is 104 bytes on macOS, 108 on Linux — this
    /// asserts against the SMALLER (macOS) limit. No longer a meaningful
    /// risk with a fixed daemon address (unlike the old per-pane pipe name,
    /// which concatenated a profile name and a full UUID pane_id and used
    /// to overflow this easily — see `project_som_tmux` memory,
    /// "Обновление 30"), but kept as a guard against ever reintroducing
    /// unbounded input into this path.
    #[test]
    fn daemon_socket_path_stays_within_sun_len() {
        const MACOS_SUN_LEN: usize = 104;
        let path = daemon_socket_path();

        assert!(
            path.len() < MACOS_SUN_LEN,
            "daemon_socket_path produced a {}-byte path (>= the {MACOS_SUN_LEN}-byte SUN_LEN limit): {path:?}",
            path.len()
        );
    }

    #[test]
    fn daemon_socket_path_is_stable_across_calls() {
        assert_eq!(daemon_socket_path(), daemon_socket_path(), "the fixed daemon address must not vary between calls");
    }
}
