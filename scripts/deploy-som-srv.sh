#!/usr/bin/env bash
# Dev-only helper (NOT part of the release pipeline — that will eventually
# package these binaries inside setup.msi/dmg/tar.gz, see project_som_tmux
# memory): rebuilds som-srv from source on the real deb and mac machines,
# then copies the three known platform binaries (windows-amd, macos-arm,
# linux-amd — linux-arm/pi5 deliberately excluded for now, added closer to
# release) into ~/.config/som/srv/{platform}/ on ALL THREE machines, so
# any of them can act as a Som client and scp the right binary to whatever
# SSH server it's talking to (see som_srv::protocol::platform_binaries_dir
# / platform_dir_name for the naming — always <os>-<arch>, even where only
# one arch per OS is actually supported today).
#
# Requires: git pushed to origin/main already (this pulls, doesn't push),
# and passwordless SSH to deb/mac already set up (same as Som itself needs).
set -euo pipefail

DEB=deb
MAC=mac
WIN_SRV_DIR="$HOME/.config/som/srv"

echo "==> building on $DEB (linux-amd)"
ssh "$DEB" "cd ~/som && git pull && (source ~/.cargo/env 2>/dev/null; cargo build --release -p som_srv)"

echo "==> building on $MAC (macos-arm)"
ssh "$MAC" "cd ~/som && git pull && (source ~/.cargo/env 2>/dev/null; cargo build --release -p som_srv)"

echo "==> building locally on windows-amd"
(cd "$(dirname "$0")/.." && cargo build --release -p som_srv)

echo "==> collecting binaries to $WIN_SRV_DIR"
mkdir -p "$WIN_SRV_DIR/windows-amd" "$WIN_SRV_DIR/macos-arm" "$WIN_SRV_DIR/linux-amd"
scp "$DEB:~/som/target/release/som-srv" "$WIN_SRV_DIR/linux-amd/som-srv"
scp "$MAC:~/som/target/release/som-srv" "$WIN_SRV_DIR/macos-arm/som-srv"
cp "$(dirname "$0")/../target/release/som-srv.exe" "$WIN_SRV_DIR/windows-amd/som-srv.exe"

echo "==> distributing full set to $DEB and $MAC"
for host in "$DEB" "$MAC"; do
  ssh "$host" "mkdir -p ~/.config/som/srv/windows-amd ~/.config/som/srv/macos-arm ~/.config/som/srv/linux-amd"
  scp "$WIN_SRV_DIR/windows-amd/som-srv.exe" "$host:~/.config/som/srv/windows-amd/som-srv.exe"
  scp "$WIN_SRV_DIR/macos-arm/som-srv" "$host:~/.config/som/srv/macos-arm/som-srv"
  scp "$WIN_SRV_DIR/linux-amd/som-srv" "$host:~/.config/som/srv/linux-amd/som-srv"
  ssh "$host" "chmod +x ~/.config/som/srv/macos-arm/som-srv ~/.config/som/srv/linux-amd/som-srv"
done

echo "==> done"
