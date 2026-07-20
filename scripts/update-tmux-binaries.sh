#!/usr/bin/env bash
# Dev-only helper (NOT run automatically, no CI wiring): rebuilds som-tmux
# for the three currently-supported remote platforms and drops the result
# into assets/tmux/{platform}/ IN THE REPO, so `cargo build --release`
# embeds them straight into som.exe via RustEmbed (see crates/assets/src/
# assets.rs's `#[include = "tmux/..."]` entries) — Som ships as one
# self-contained binary per platform, no separate manual step to obtain/
# place som-tmux binaries. Replaces the old ~/.config/som/tmux/-populating
# scripts/deploy-som-tmux.sh, which is now obsolete (kept for reference/
# history, not deleted).
#
# linux-arm/pi5 is NOT built here — that platform stays permanently
# unsupported; a remote host reporting linux-arm falls back to plain
# tmux:false with a user-visible notification (see
# som_tmux::protocol::local_binary_path_for's doc comment).
#
# Run this, then `git add assets/tmux && git commit` before cutting a
# release, so the checked-in binaries stay in sync with this Som version
# (crates/som_tmux/Cargo.toml's `version`, which must be bumped in lockstep
# with crates/zed/Cargo.toml's — see HandshakeInfo's doc comment).
#
# Requires: git pushed to origin/main already (this pulls, doesn't push);
# WSL installed with its own ~/som clone; passwordless SSH to the mac build
# server (192.168.50.6) already set up (same as Som itself needs for its
# own `mac` SSH profile).
set -euo pipefail

MAC_HOST=192.168.50.6
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ASSETS_TMUX_DIR="$REPO_ROOT/assets/tmux"

echo "==> building linux-amd via WSL"
wsl -- bash -lc "cd ~/som && git pull && (source ~/.cargo/env 2>/dev/null; cargo build --release -p som_tmux)"

echo "==> building macos-arm via $MAC_HOST"
ssh "$MAC_HOST" "cd ~/som && git pull && (source ~/.cargo/env 2>/dev/null; cargo build --release -p som_tmux)"

echo "==> building windows-amd locally"
(cd "$REPO_ROOT" && cargo build --release -p som_tmux)

echo "==> collecting binaries into $ASSETS_TMUX_DIR"
mkdir -p "$ASSETS_TMUX_DIR/windows-amd" "$ASSETS_TMUX_DIR/macos-arm" "$ASSETS_TMUX_DIR/linux-amd"
wsl -- bash -lc "cat ~/som/target/release/som-tmux" > "$ASSETS_TMUX_DIR/linux-amd/som-tmux"
scp "$MAC_HOST:~/som/target/release/som-tmux" "$ASSETS_TMUX_DIR/macos-arm/som-tmux"
cp "$REPO_ROOT/target/release/som-tmux.exe" "$ASSETS_TMUX_DIR/windows-amd/som-tmux.exe"
chmod +x "$ASSETS_TMUX_DIR/linux-amd/som-tmux" "$ASSETS_TMUX_DIR/macos-arm/som-tmux"

echo "==> done — review with 'git status assets/tmux', then git add + commit"
