#!/usr/bin/env bash
# Dev-only helper (NOT part of the release pipeline — that will eventually
# package these binaries inside setup.msi/dmg/tar.gz, see project_som_tmux
# memory): rebuilds som-tmux from source on the real deb and mac machines,
# then copies the three known platform binaries (windows, macos, linux-amd
# — linux-arm/pi5 deliberately excluded for now, added closer to release)
# into ~/.config/som/tmux/{platform}/ on ALL THREE machines, so any of them
# can act as a Som client and scp the right binary to whatever SSH server
# it's talking to (see som_tmux::protocol::platform_binaries_dir).
#
# Requires: git pushed to origin/main already (this pulls, doesn't push),
# and passwordless SSH to deb/mac already set up (same as Som itself needs).
set -euo pipefail

DEB=deb
MAC=mac
WIN_TMUX_DIR="$HOME/.config/som/tmux"

echo "==> building on $DEB (linux-amd)"
ssh "$DEB" "cd ~/som && git pull && (source ~/.cargo/env 2>/dev/null; cargo build --release -p som_tmux)"

echo "==> building on $MAC (macos)"
ssh "$MAC" "cd ~/som && git pull && (source ~/.cargo/env 2>/dev/null; cargo build --release -p som_tmux)"

echo "==> building locally on windows"
(cd "$(dirname "$0")/.." && cargo build --release -p som_tmux)

echo "==> collecting binaries to $WIN_TMUX_DIR"
mkdir -p "$WIN_TMUX_DIR/windows" "$WIN_TMUX_DIR/macos" "$WIN_TMUX_DIR/linux-amd"
scp "$DEB:~/som/target/release/som-tmux" "$WIN_TMUX_DIR/linux-amd/som-tmux"
scp "$MAC:~/som/target/release/som-tmux" "$WIN_TMUX_DIR/macos/som-tmux"
cp "$(dirname "$0")/../target/release/som-tmux.exe" "$WIN_TMUX_DIR/windows/som-tmux.exe"

echo "==> distributing full set to $DEB and $MAC"
for host in "$DEB" "$MAC"; do
  ssh "$host" "mkdir -p ~/.config/som/tmux/windows ~/.config/som/tmux/macos ~/.config/som/tmux/linux-amd"
  scp "$WIN_TMUX_DIR/windows/som-tmux.exe" "$host:~/.config/som/tmux/windows/som-tmux.exe"
  scp "$WIN_TMUX_DIR/macos/som-tmux" "$host:~/.config/som/tmux/macos/som-tmux"
  scp "$WIN_TMUX_DIR/linux-amd/som-tmux" "$host:~/.config/som/tmux/linux-amd/som-tmux"
  ssh "$host" "chmod +x ~/.config/som/tmux/macos/som-tmux ~/.config/som/tmux/linux-amd/som-tmux"
done

echo "==> done"
