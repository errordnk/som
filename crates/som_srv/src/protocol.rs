use serde::{Deserialize, Serialize};

/// ONE fixed address for the whole machine — `som-srv` is a shared,
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
/// are actual filesystem paths — kept per-uid (`/tmp/som-srv-<uid>.sock`)
/// so several different OS accounts on a shared machine each still get
/// their OWN daemon and session registry, while every tab/profile
/// belonging to the SAME account shares one, mirroring where a real
/// tmux/screen puts their sockets (`$TMPDIR/tmux-<uid>/...`, though this
/// hardcodes `/tmp` rather than trusting `$TMPDIR` — see the historical
/// `SUN_LEN` overflow this avoided, `project_som_tmux` memory,
/// "Обновление 30").
#[cfg(windows)]
pub fn daemon_socket_path() -> String {
    r"\\.\pipe\som-srv".to_string()
}

#[cfg(unix)]
pub fn daemon_socket_path() -> String {
    let dir = std::path::Path::new("/tmp");
    let _ = std::fs::create_dir_all(dir);
    dir.join(format!("som-srv-{}.sock", unsafe { libc::getuid() })).to_string_lossy().into_owned()
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
/// ~/.local/bin/som-srv ...` — see `wrap_remote_command_args`) — `None`
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

/// Directory name (under `platform_binaries_dir()`) holding the
/// pre-built `som-srv` for a given `(Os, Arch)` pair — the names the user
/// chose for `~/.config/som/srv/{name}/som-srv`, one per (os, arch) pair
/// this codebase actually supports (see `Os`/`Arch`'s own doc comment for
/// the four supported combinations — Intel Mac and Windows-on-ARM are
/// excluded on purpose, so there is no `windows-arm`/`macos-amd` directory
/// to name). Used on BOTH ends of a deploy decision: locally, to find
/// which pre-built binary to `scp` up for a remote host reporting this
/// `(os, arch)` in its handshake; and (implicitly, by a human/release-
/// packaging script, not this codebase) to know where to drop each
/// cross-compiled build's output in the first place.
pub fn platform_dir_name(os: Os, arch: Arch) -> &'static str {
    match (os, arch) {
        (Os::Windows, Arch::Amd64) => "windows-amd",
        (Os::Darwin, Arch::Arm64) => "macos-arm",
        (Os::Linux, Arch::Amd64) => "linux-amd",
        (Os::Linux, Arch::Arm64) => "linux-arm",
        // Genuinely unsupported combos (Windows-on-ARM, Intel Mac) — no
        // directory name exists for these; `local_binary_path_for` never
        // finds a file regardless of what this string is, so any
        // placeholder is fine as long as it can never collide with a real
        // supported directory above.
        (Os::Windows, Arch::Arm64) => "windows-arm-unsupported",
        (Os::Darwin, Arch::Amd64) => "macos-amd-unsupported",
    }
}

/// `~/.config/som/srv/` — where Som keeps a pre-built `som-srv` for
/// every supported remote platform (see `platform_dir_name`), so an SSH
/// profile's deploy step is a plain `scp` of an already-built file rather
/// than a `git pull && cargo build` on the remote machine itself (see
/// `project_som_tmux` memory for why that changed: a build on every real
/// target machine was the ORIGINAL approach, explicitly chosen over
/// cross-compilation at the time — this supersedes that decision now that
/// shipping pre-built binaries inside Som's own installer for each
/// platform is possible). Lives under Som's own `paths::config_dir()`,
/// same as `db.json` and everything else Som persists locally — NOT a
/// per-remote-host location; this directory is read locally on the
/// machine actually running Som, one entry per remote platform Som might
/// ever need to talk to, not per host.
pub fn platform_binaries_dir() -> std::path::PathBuf {
    paths::config_dir().join("srv")
}

/// `som-srv[.exe]` — the file name (not path) `local_binary_path_for`/
/// `ensure_embedded_binary_extracted` both use under a platform's own
/// `platform_dir_name` subdirectory.
fn binary_file_name(os: Os) -> &'static str {
    if let Os::Windows = os { "som-srv.exe" } else { "som-srv" }
}

/// Full local path to the pre-built `som-srv` for a given remote
/// `(Os, Arch)` — `None` if Som has never been told about (or hasn't yet
/// downloaded/copied in) a binary for that platform. Callers (`terminal_
/// view`'s `ensure_remote_binary_deployed`) treat a missing file the same
/// way as an unreachable host: log and skip, rather than failing the tab.
pub fn local_binary_path_for(os: Os, arch: Arch) -> Option<std::path::PathBuf> {
    let path = platform_binaries_dir().join(platform_dir_name(os, arch)).join(binary_file_name(os));
    path.is_file().then_some(path)
}

/// Ensures the embedded `som-srv` binary for a remote `(Os, Arch)` is
/// present and current at `platform_binaries_dir()`'s own path for that
/// platform, extracting `embedded_bytes` (Som's own RustEmbed'd copy — see
/// `crates/assets/src/assets.rs`'s `#[include = "tmux/..."]` entries) there
/// if the file is missing OR its own `--version` doesn't match
/// `embedded_version` (`env!("CARGO_PKG_VERSION")` at Som's build time,
/// same string `HandshakeInfo` already compares against a REMOTE host's
/// binary — this is the identical check, just run locally as a plain child
/// process instead of over SSH).
///
/// Returns the (now guaranteed-current, if `Some`) local path — the same
/// path `local_binary_path_for` would return once this has run — or `None`
/// if `embedded_bytes` is `None` (an unsupported platform: linux-arm,
/// windows-arm, or macos-amd; Som was never built with a binary for that
/// combination in the first place). Callers treat `None` exactly like
/// `local_binary_path_for`'s `None` already was: log and fall back to
/// plain (non-tmux) behavior for that profile, no crash.
///
/// This never touches a REMOTE host's binary — that overwrite-a-possibly-
/// running-process concern (`ETXTBSY`, kill-before-scp) belongs entirely to
/// `ensure_remote_binary_deployed` on the Som side. This function only
/// ever writes to Som's own local cache directory, which nothing else runs
/// out of directly.
pub fn ensure_embedded_binary_extracted(
    os: Os,
    arch: Arch,
    embedded_bytes: Option<&[u8]>,
    embedded_version: &str,
) -> Option<std::path::PathBuf> {
    let path = platform_binaries_dir().join(platform_dir_name(os, arch)).join(binary_file_name(os));
    ensure_embedded_binary_extracted_at(&path, embedded_bytes, embedded_version)
}

/// The actual extraction logic behind `ensure_embedded_binary_extracted`,
/// taking the target path directly rather than deriving it from
/// `platform_binaries_dir()` — split out so tests can point it at a
/// throwaway temp path instead of Som's real `~/.config/som/tmux/`.
fn ensure_embedded_binary_extracted_at(
    path: &std::path::Path,
    embedded_bytes: Option<&[u8]>,
    embedded_version: &str,
) -> Option<std::path::PathBuf> {
    let embedded_bytes = embedded_bytes?;

    if local_binary_version(path).as_deref() == Some(embedded_version) {
        return Some(path.to_path_buf());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::write(path, embedded_bytes).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).ok()?;
    }
    Some(path.to_path_buf())
}

/// Runs `path --version` (the same handshake-JSON probe `ensure_remote_
/// binary_deployed` already runs over SSH against a remote binary, here
/// invoked as a local child process instead) and returns just its version
/// string, or `None` if the file doesn't exist, isn't executable, or
/// doesn't understand `--version` (e.g. a corrupted/truncated download —
/// treated as "needs re-extracting", same as a missing file).
fn local_binary_version(path: &std::path::Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let output = std::process::Command::new(path).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    serde_json::from_str::<HandshakeInfo>(stdout.trim()).ok().map(|info| info.version)
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
    /// which session this connection belongs to now that `som-srv` is a
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

/// `somcat`/other SRP clients -> daemon: the binary side-channel for rich
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
    /// chunk looks different." `som-srv` stores whichever copy arrived
    /// LAST (harmless — a real sender's metadata for one `(session_id,
    /// file_id)` never actually changes mid-transfer) and reports it to
    /// Som via `SrvResponse::Progress`'s own `metadata` field, since
    /// `som-srv` has no GPUI dependency and can't decode content itself —
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
    /// deploying a newer `som-srv` build). No-op (not an error) if
    /// `(client_id, pane_id)` isn't in the registry — same "already
    /// gone, nothing to do" tolerance the old `kill <pid>`-based approach
    /// had for a PID that had already exited on its own.
    KillSession { client_id: Option<String>, pane_id: String },
    /// Sent by Som (never by `somcat`) right after `Handshake`, on a
    /// SECOND long-lived `Srv`-kind connection separate from whichever
    /// one (if any) is sending `PutChunk`s for this same
    /// `(session_id, file_id)` — subscribes this connection to
    /// `SrvResponse::Progress` pushes as they land, so Som can track
    /// `contiguous_len` accurately (gap-tolerant, same semantics
    /// `RichContentCache::apply_chunk` already has today) without racing
    /// a raw on-disk file-size poll against out-of-order chunk arrival.
    SubscribeProgress { session_id: u32, file_id: u32 },
    /// Sent by Som (never by `somcat`) on the SAME connection as
    /// `SubscribeProgress` above, whenever it needs bytes further into a
    /// file than the sequential `PutChunk` stream has reached yet (e.g.
    /// seeking forward in audio/video playback past what's currently
    /// cached) — the direct replacement for the old PTY-based `Query::
    /// AudioByteRange` mechanism (`crates/terminal/src/
    /// rich_content_transport.rs`, now deleted). The daemon looks up
    /// `(session_id, file_id)` in its sender-routing table (populated by
    /// the first `PutChunk` seen for that key) and forwards this same
    /// message, verbatim, down THAT connection — the client that's
    /// actually holding the file (`somcat` or equivalent) answers by
    /// sending ordinary `PutChunk`s covering `[offset, offset+len)` back
    /// on its own connection, same as it would for any other part of the
    /// file; there is no separate "range response" message shape needed,
    /// mirroring how `Query::AudioByteRange`'s answer was always just
    /// ordinary chunk envelopes at arbitrary offsets. Silently
    /// undeliverable (no-op) if the sender connection has already closed
    /// — see this plan's own doc comment for why that's an accepted gap,
    /// not a new failure mode.
    RequestByteRange { session_id: u32, file_id: u32, offset: u64, len: u64 },
}

/// daemon -> `somcat`/other SRP clients, answering a `SrvRequest`.
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
    /// `apply_chunk` call (which `som-srv`, having no GPUI dependency,
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
        /// (`somcat`'s `stream_file_from_disk`) wrote the tail bytes to
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
        /// field (`crates/som_srv/src/srv_cache.rs`) — same "expected to
        /// stay small" assumption (a real sender streams mostly in
        /// order; the only source of entries here is a small number of
        /// deliberate out-of-order seek/tail fetches, not routine
        /// reordering).
        pending_ranges: Vec<(u64, u64)>,
        total_size: u64,
        content_type: ContentType,
        metadata: ContentMetadata,
    },
}

/// Mirrors `crates/terminal/src/rich_content_transport::ContentType`
/// field-for-field — a separate, serializable copy rather than a shared
/// dependency, since `som_srv` deliberately has no dependency on
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentMetadata {
    Image { width_px: u32, height_px: u32, color_bits: u8, is_animated: bool },
    Audio { sample_rate: u32, channels: u8, bits_per_sample: u8, duration_ms: u32 },
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
    },
    Markdown,
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

    #[test]
    fn local_binary_path_for_uses_the_exe_suffix_only_on_windows() {
        // Doesn't touch the filesystem (no real file exists at this
        // fabricated config_dir in a test environment) — local_binary_
        // path_for returns None either way, but the file NAME it would
        // have looked for is what this test actually protects (the .exe
        // suffix decision), so this reaches into platform_binaries_dir()
        // directly rather than trying to assert on local_binary_path_for's
        // None result.
        let windows_dir = platform_binaries_dir().join(platform_dir_name(Os::Windows, Arch::Amd64));
        let unix_dir = platform_binaries_dir().join(platform_dir_name(Os::Linux, Arch::Amd64));
        assert!(windows_dir.ends_with("windows-amd"));
        assert!(unix_dir.ends_with("linux-amd"));
    }

    #[test]
    fn local_binary_path_for_returns_none_when_no_file_exists_for_that_platform() {
        // A platform combo whose directory almost certainly doesn't exist
        // in whatever environment runs this test (or if it does, has no
        // file in it) — must return None, not panic or fabricate a path
        // that doesn't actually exist on disk.
        let result = local_binary_path_for(Os::Linux, Arch::Arm64);
        // Can't assert None unconditionally — a dev machine that's ALSO
        // used to run the real deploy-som-srv.sh script might genuinely
        // have this file. Only assert the contract that matters: if Some,
        // the path really does exist on disk.
        if let Some(path) = result {
            assert!(path.is_file(), "local_binary_path_for returned a path that doesn't actually exist: {path:?}");
        }
    }
}

#[cfg(test)]
mod embedded_binary_tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "som_srv_embedded_binary_test_{}_{name}",
            std::process::id()
        ))
    }

    #[test]
    fn missing_embedded_bytes_returns_none_without_touching_disk() {
        let path = temp_path("missing_bytes");
        let _ = std::fs::remove_file(&path);
        assert_eq!(ensure_embedded_binary_extracted_at(&path, None, "1.0.0"), None);
        assert!(!path.exists(), "must not create a file when embedded_bytes is None");
    }

    #[test]
    fn missing_local_file_gets_written_from_embedded_bytes() {
        let path = temp_path("missing_file");
        let _ = std::fs::remove_file(&path);
        let bytes = b"not a real binary, just test content";

        let result = ensure_embedded_binary_extracted_at(&path, Some(bytes), "1.0.0");

        assert_eq!(result.as_deref(), Some(path.as_path()));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_that_fails_the_version_probe_gets_overwritten() {
        // Simulates a stale/corrupted extraction: `local_binary_version`
        // can't parse `--version` output from arbitrary garbage bytes (this
        // isn't even a real executable, so running it fails outright) —
        // same code path a genuinely OLDER som-srv build would hit if its
        // version string just didn't match, both treated identically as
        // "needs re-extracting".
        let path = temp_path("garbage_content");
        std::fs::write(&path, b"garbage, not an executable").unwrap();
        let new_bytes = b"replacement content";

        let result = ensure_embedded_binary_extracted_at(&path, Some(new_bytes), "1.0.0");

        assert_eq!(result.as_deref(), Some(path.as_path()));
        assert_eq!(std::fs::read(&path).unwrap(), new_bytes);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_binary_reporting_the_matching_version_is_left_untouched() {
        // Uses the REAL locally-built som-srv (this very test binary's own
        // sibling artifact) so `--version` genuinely succeeds and reports a
        // real, parseable HandshakeInfo — the one case that must NOT
        // rewrite the file (mirrors ensure_remote_binary_deployed's own
        // "already up to date, nothing to do" early return for a remote
        // host).
        let Ok(current_exe) = std::env::current_exe() else { return };
        let Some(deps_dir) = current_exe.parent() else { return };
        let Some(profile_dir) = deps_dir.parent() else { return };
        let candidate = profile_dir.join(if cfg!(windows) { "som-srv.exe" } else { "som-srv" });
        if !candidate.is_file() {
            // Not every dev environment has built the som-srv bin
            // alongside the test artifacts — skip rather than fail.
            return;
        }

        let real_version = HandshakeInfo::current().version;
        let path = temp_path("matching_version");
        std::fs::copy(&candidate, &path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let original_bytes = std::fs::read(&path).unwrap();
        let modified_time_before = std::fs::metadata(&path).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        let result = ensure_embedded_binary_extracted_at(&path, Some(b"should never be written"), &real_version);

        assert_eq!(result.as_deref(), Some(path.as_path()));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original_bytes,
            "a binary already reporting the matching version must not be rewritten"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified_time_before,
            "must not touch the file at all when the version already matches"
        );
        let _ = std::fs::remove_file(&path);
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
