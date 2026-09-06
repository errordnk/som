#!/usr/bin/env python3
"""Repair a vcpkg tree whose `installed/<triplet>/vcpkg/info/*.list` manifests
were pruned (e.g. by a `buildtrees`/`packages` cleanup that also took the
`info/` dir).

`ffmpeg-sys-next`'s build script probes for FFmpeg via the `vcpkg` crate, which
walks *every* package in `installed/vcpkg/status` whose `Architecture` matches
the target triplet and `Status` ends with " installed", and calls
`load_port_manifest` for each. A single missing `.list` file makes the whole
probe bail out with "Could not open port manifest file ..." and the build then
falls through to pkg-config (absent on Windows) and fails.

The packages that lose their manifests here are vcpkg's own host build helpers
(`vcpkg-cmake`, `pkgconf`, `vcpkg-tool-meson`, ...) plus `ffmpeg-bin2c` — none
of which install any files into the target triplet, so a stub manifest listing
just the triplet root dir is faithful and lets the probe succeed.

Idempotent: only writes manifests that are actually missing.

Usage:
    python scripts/fix-vcpkg-manifests.py [VCPKG_ROOT]

VCPKG_ROOT defaults to $VCPKG_ROOT, then to
%LOCALAPPDATA%\\Temp\\vcpkg-root (the path this project has used).
"""
import os
import sys


def main() -> int:
    root = (
        (sys.argv[1] if len(sys.argv) > 1 else None)
        or os.environ.get("VCPKG_ROOT")
        or os.path.expanduser(r"~\AppData\Local\Temp\vcpkg-root")
    )
    root = os.path.abspath(root)

    marker = os.path.join(root, ".vcpkg-root")
    if not os.path.isfile(marker):
        # The `vcpkg` crate refuses a tree without this sentinel.
        open(marker, "a").close()
        print(f"created missing marker {marker}")

    status_dir = os.path.join(root, "installed", "vcpkg")
    status_path = os.path.join(status_dir, "status")
    info_dir = os.path.join(status_dir, "info")
    if not os.path.isfile(status_path):
        print(f"no status file at {status_path} - nothing to do", file=sys.stderr)
        return 1
    os.makedirs(info_dir, exist_ok=True)
    have = set(os.listdir(info_dir))

    stanzas = [s for s in open(status_path, encoding="utf-8").read().split("\n\n") if s.strip()]
    created = 0
    for st in stanzas:
        d = {}
        for line in st.splitlines():
            if ": " in line:
                k, v = line.split(": ", 1)
                d[k] = v
        pkg, arch, ver = d.get("Package"), d.get("Architecture"), d.get("Version")
        if not (pkg and arch and ver):
            continue
        if d.get("Feature"):  # feature rows carry no manifest of their own
            continue
        if not d.get("Status", "").endswith("installed"):
            continue
        fn = f"{pkg}_{ver}_{arch}.list"
        if fn in have:
            continue
        with open(os.path.join(info_dir, fn), "w", encoding="utf-8", newline="\n") as f:
            f.write(f"{arch}/\n")
        created += 1
        print(f"wrote stub manifest {fn}")

    print(f"done - {created} manifest(s) created" if created else "done - all manifests present")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
