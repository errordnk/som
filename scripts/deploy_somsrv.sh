#!/usr/bin/env bash
# Dev-only helper (NOT part of the release pipeline — that will eventually
# package these binaries inside setup.msi/dmg/tar.gz, see project_som_tmux
# memory): rebuilds somsrv from source on the real deb and mac machines,
# then copies the three known platform binaries (windows-amd, macos-arm,
# linux-amd — linux-arm/pi5 deliberately excluded for now, added closer to
# release) into ~/.config/som/srv/{platform}/ on ALL THREE machines, so
# any of them can act as a Som client and scp the right binary to whatever
# SSH server it's talking to (see somsrv::protocol::platform_binaries_dir
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
ssh "$DEB" "cd ~/som && git pull && (source ~/.cargo/env 2>/dev/null; cargo build --release -p somsrv)"

echo "==> building on $MAC (macos-arm)"
ssh "$MAC" "cd ~/som && git pull && (source ~/.cargo/env 2>/dev/null; cargo build --release -p somsrv)"

echo "==> building locally on windows-amd"
(cd "$(dirname "$0")/.." && cargo build --release -p somsrv)

echo "==> collecting binaries to $WIN_SRV_DIR"
mkdir -p "$WIN_SRV_DIR/windows-amd" "$WIN_SRV_DIR/macos-arm" "$WIN_SRV_DIR/linux-amd"
scp "$DEB:~/som/target/release/somsrv" "$WIN_SRV_DIR/linux-amd/somsrv"
scp "$MAC:~/som/target/release/somsrv" "$WIN_SRV_DIR/macos-arm/somsrv"
cp "$(dirname "$0")/../target/release/somsrv.exe" "$WIN_SRV_DIR/windows-amd/somsrv.exe"

echo "==> distributing full set to $DEB and $MAC"
for host in "$DEB" "$MAC"; do
  ssh "$host" "mkdir -p ~/.config/som/srv/windows-amd ~/.config/som/srv/macos-arm ~/.config/som/srv/linux-amd"
  scp "$WIN_SRV_DIR/windows-amd/somsrv.exe" "$host:~/.config/som/srv/windows-amd/somsrv.exe"
  scp "$WIN_SRV_DIR/macos-arm/somsrv" "$host:~/.config/som/srv/macos-arm/somsrv"
  scp "$WIN_SRV_DIR/linux-amd/somsrv" "$host:~/.config/som/srv/linux-amd/somsrv"
  ssh "$host" "chmod +x ~/.config/som/srv/macos-arm/somsrv ~/.config/som/srv/linux-amd/somsrv"
done

echo "==> done"
