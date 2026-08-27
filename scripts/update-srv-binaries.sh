#!/usr/bin/env bash
# Dev-only helper (NOT run automatically, no CI wiring): rebuilds som-srv
# for the three currently-supported remote platforms and drops the result
# into assets/srv/{platform}/ IN THE REPO, so `cargo build --release`
# embeds them straight into som.exe via RustEmbed (see crates/assets/src/
# assets.rs's `#[include = "srv/..."]` entries) — Som ships as one
# self-contained binary per platform, no separate manual step to obtain/
# place som-srv binaries. Replaces the old ~/.config/som/tmux/-populating
# scripts/deploy-som-srv.sh, which is now obsolete (kept for reference/
# history, not deleted).
#
# linux-arm/pi5 is NOT built here — that platform stays permanently
# unsupported; a remote host reporting linux-arm falls back to plain
# tmux:false with a user-visible notification (see
# som_srv::protocol::local_binary_path_for's doc comment).
#
# Run this, then `git add assets/srv && git commit` before cutting a
# release, so the checked-in binaries stay in sync with this Som version
# (crates/som_srv/Cargo.toml's `version`, which must be bumped in lockstep
# with crates/zed/Cargo.toml's — see HandshakeInfo's doc comment).
#
# Requires: git pushed to origin/main already (this pulls, doesn't push);
# WSL installed with its own ~/som clone; passwordless SSH to the mac build
# server (192.168.50.6) already set up (same as Som itself needs for its
# own `mac` SSH profile).
set -euo pipefail

MAC_HOST=192.168.50.6
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ASSETS_SRV_DIR="$REPO_ROOT/assets/srv"

echo "==> building linux-amd via WSL"
wsl -- bash -lc "cd ~/som && git pull && (source ~/.cargo/env 2>/dev/null; cargo build --release -p som_srv)"

echo "==> building macos-arm via $MAC_HOST"
ssh "$MAC_HOST" "cd ~/som && git pull && (source ~/.cargo/env 2>/dev/null; cargo build --release -p som_srv)"

echo "==> building windows-amd locally"
(cd "$REPO_ROOT" && cargo build --release -p som_srv)

echo "==> collecting binaries into $ASSETS_SRV_DIR"
mkdir -p "$ASSETS_SRV_DIR/windows-amd" "$ASSETS_SRV_DIR/macos-arm" "$ASSETS_SRV_DIR/linux-amd"
wsl -- bash -lc "cat ~/som/target/release/som-srv" > "$ASSETS_SRV_DIR/linux-amd/som-srv"
scp "$MAC_HOST:~/som/target/release/som-srv" "$ASSETS_SRV_DIR/macos-arm/som-srv"
cp "$REPO_ROOT/target/release/som-srv.exe" "$ASSETS_SRV_DIR/windows-amd/som-srv.exe"
chmod +x "$ASSETS_SRV_DIR/linux-amd/som-srv" "$ASSETS_SRV_DIR/macos-arm/som-srv"

echo "==> done — review with 'git status assets/srv', then git add + commit"
