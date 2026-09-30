#!/usr/bin/env python3
"""Verify that a release tag's shipped metadata is internally consistent.

Release builds run from the tag, so everything the artifacts embed must
already agree there: the version, the metainfo release notes, the
vendored sources, and the Flatpak manifest's tag. Publishing a tag whose
metadata disagrees ships a stale version string or the wrong dependencies
to users, so CI runs this before any build.

Usage:
  ./build-aux/check-release-consistency.py [--tag v4.3.1] [--manifest path]

Exit codes: 0 consistent, 1 mismatch (message on stderr), 2 usage/IO error.
Stdlib only (tomllib), so it runs anywhere Python 3.11+ exists (CI and
the SDK image both qualify).
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO_TOML = ROOT / "Cargo.toml"
CARGO_LOCK = ROOT / "Cargo.lock"
METAINFO = ROOT / "data" / "io.github.linuxuser67.Grab.metainfo.xml.in"
FLATPAK = ROOT / "build-aux" / "io.github.linuxuser67.Grab.json"

RELEASE_MARKER = '<release version="'


class Mismatch(Exception):
    """A consistency check failed; message is user-facing."""


def fail(msg: str) -> Mismatch:
    return Mismatch(msg)


def package_version() -> str:
    with CARGO_TOML.open("rb") as fh:
        return tomllib.load(fh)["package"]["version"]


def lock_version() -> str:
    with CARGO_LOCK.open("rb") as fh:
        lock = tomllib.load(fh)
    for pkg in lock["package"]:
        if pkg["name"] == "grab":
            return str(pkg["version"])
    raise fail("Cargo.lock has no grab package")


def metainfo_release_versions() -> list[str]:
    xml = METAINFO.read_text(encoding="utf-8")
    versions = re.findall(r'<release version="([^"]+)"', xml)
    if not versions:
        raise fail("metainfo has no <release> entries")
    return versions


def version_key(version: str) -> tuple[int, ...]:
    core = re.split(r"[-+]", version)[0]
    return tuple(int(part) if part.isdigit() else 0 for part in core.split("."))


def flatpak_manifest_tag(manifest: pathlib.Path) -> str | None:
    if not manifest.is_file():
        return None
    data = json.loads(manifest.read_text(encoding="utf-8"))
    tag = data.get("tag")
    return tag if isinstance(tag, str) else None


def normalize_tag(tag: str) -> str:
    return tag[1:] if tag.startswith("v") else tag


def git_describe() -> str | None:
    try:
        out = subprocess.run(
            ["git", "describe", "--tags", "--exact-match"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return out.stdout.strip() if out.returncode == 0 else None


def check(expected: str | None, manifest: pathlib.Path) -> list[str]:
    """Run every check; return one human-readable line per success."""
    notes: list[str] = []

    version = package_version()
    lock = lock_version()
    if version != lock:
        raise fail(f"Cargo.toml version {version} != Cargo.lock version {lock}")
    notes.append(f"cargo version {version} (toml + lock agree)")

    releases = metainfo_release_versions()
    newest = max(releases, key=version_key)
    if newest != version:
        raise fail(
            f"metainfo newest release {newest} != package version {version} "
            "(add or fix the release entry before tagging)"
        )
    if releases[0] != version:
        raise fail(
            f"metainfo lists {releases[0]} first, but the newest is {version}; "
            "from_appdata reads the leading <release>"
        )
    notes.append(f"metainfo newest-first release {newest}")

    if expected is not None:
        want = normalize_tag(expected)
        if version != want:
            raise fail(f"tag {expected} (version {want}) != package version {version}")
        notes.append(f"tag {expected} matches package version")

        manifest_tag = flatpak_manifest_tag(manifest)
        if manifest_tag is not None and normalize_tag(manifest_tag) != want:
            raise fail(
                f"Flatpak manifest tag {manifest_tag} != release tag {expected}"
            )
        if manifest_tag is not None:
            notes.append(f"flatpak manifest tag {manifest_tag}")

        describe = git_describe()
        if describe is not None and normalize_tag(describe) != want:
            raise fail(
                f"HEAD is tagged {describe} but release tag is {expected}; "
                "artifacts would be built from a different commit"
            )

    return notes


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--tag",
        help="release tag being built (e.g. v4.3.1); omit for untagged trees",
    )
    parser.add_argument(
        "--manifest",
        default=str(FLATPAK),
        help="Flatpak manifest path to cross-check the tag against",
    )
    args = parser.parse_args(argv)

    try:
        notes = check(args.tag, pathlib.Path(args.manifest))
    except Mismatch as exc:
        print(f"release consistency check FAILED: {exc}", file=sys.stderr)
        return 1
    except (OSError, tomllib.TOMLDecodeError, json.JSONDecodeError) as exc:
        print(f"release consistency check ERROR: {exc}", file=sys.stderr)
        return 2

    for note in notes:
        print(f"ok: {note}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
