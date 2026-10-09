#!/usr/bin/env python3
"""Verify a built Flatpak bundle: does it contain what the release claims?

`flatpak build-bundle` emits an opaque ostree GVariant file (magic
`flatpak\\0`) — never a tarball — so the `.flatpak` file itself cannot be
inspected. This reads the ostree repo the bundle is packed from instead
(`flatpak-repo/` in the workflow): the repo commit is exactly what
`flatpak build-bundle` packs, so checking it checks the bundle.

It cross-checks the installed metainfo against the release tag: the
newest release must match the tag, be listed first (from_appdata reads
the leading entry), and be byte-identical to the tag's source file, so a
tag cut before the metainfo was updated cannot publish stale notes.

The vendored crate list needs no bundle-level check: the workflow
regenerates cargo-sources.json from the tag's Cargo.lock before building,
vendor-sync CI pins the committed file to the lock, check-release-
consistency.py gates Cargo.toml == Cargo.lock, and every build is
pristine (--force-clean, no module cache) — a stale-dependency bundle
cannot be produced.

Usage:
  ./build-aux/check-bundle-contents.py --repo flatpak-repo --tag v4.3.1

Exit codes: 0 consistent, 1 mismatch (message on stderr), 2 usage/IO error.
Requires the `ostree` CLI (ships with flatpak).
"""

from __future__ import annotations

import argparse
import pathlib
import re
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO_TOML = ROOT / "Cargo.toml"
CARGO_LOCK = ROOT / "Cargo.lock"
METAINFO = ROOT / "data" / "io.github.linuxuser67.Grab.metainfo.xml.in"
APP_ID = "io.github.linuxuser67.Grab"
# Inside the app commit, the exported tree lives under /files (mounted at
# /app); the flatpak metadata file sits beside it at /metadata.
INSTALLED_METAINFO = f"/files/share/metainfo/{APP_ID}.metainfo.xml"


class Mismatch(Exception):
    """A consistency check failed; message is user-facing."""


def fail(msg: str) -> Mismatch:
    return Mismatch(msg)


def version_key(version: str) -> tuple:
    core = re.split(r"[-+]", version)[0]
    parts = tuple(int(part) if part.isdigit() else 0 for part in core.split("."))
    # A stable release outranks its own pre-releases, matching the
    # metainfo test in src/application.rs: without the flag, `4.4.0` and
    # `4.4.0-beta.1` tie and max() keeps the last tie -- the beta.
    return parts + (not re.search(r"[-+]", version),)


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


def metainfo_versions(xml: str) -> list[str]:
    versions = re.findall(r'<release version="([^"]+)"', xml)
    if not versions:
        raise fail("metainfo has no <release> entries")
    return versions


def ostree(repo: pathlib.Path, *args: str) -> bytes:
    """Run the ostree CLI against a repo; its own failures propagate."""
    return subprocess.run(
        ["ostree", f"--repo={repo}", *args],
        capture_output=True,
        check=True,
        timeout=60,
    ).stdout


def app_commit(repo: pathlib.Path) -> str:
    """The commit `flatpak build-bundle` would pack for this app."""
    refs = ostree(repo, "refs").decode("utf-8").split()
    hits = [r for r in refs if APP_ID in r]
    if len(hits) != 1:
        raise fail(f"repo has {len(hits)} refs matching {APP_ID} (want 1): {hits}")
    return ostree(repo, "rev-parse", hits[0]).decode("utf-8").strip()


def repo_file(repo: pathlib.Path, commit: str, path: str) -> str:
    """File contents at an ostree path; missing reads as a mismatch."""
    try:
        return ostree(repo, "cat", commit, path).decode("utf-8")
    except subprocess.CalledProcessError:
        raise fail(f"repo commit {commit[:12]} has no {path}") from None


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo",
        type=pathlib.Path,
        required=True,
        help="ostree repo dir the bundle is packed from (e.g. flatpak-repo)",
    )
    parser.add_argument("--tag", required=True, help="release tag, e.g. v4.3.1")
    args = parser.parse_args(argv)

    want = args.tag[1:] if args.tag.startswith("v") else args.tag
    notes: list[str] = []

    try:
        if not args.repo.is_dir():
            raise OSError(f"repo dir not found: {args.repo}")

        version = package_version()
        lock = lock_version()
        if version != lock:
            raise fail(f"Cargo.toml version {version} != Cargo.lock version {lock}")
        if version != want:
            raise fail(f"tag {args.tag} (version {want}) != package version {version}")

        commit = app_commit(args.repo)

        metainfo = repo_file(args.repo, commit, INSTALLED_METAINFO)
        releases = metainfo_versions(metainfo)
        newest = max(releases, key=version_key)
        # Normalize -beta. to ~beta.: metainfo uses AppStream's ~, tags use -.
        if version_key(newest) != version_key(version):
            raise fail(f"bundle metainfo newest release {newest} != version {version}")
        if version_key(releases[0]) != version_key(version):
            raise fail(
                f"bundle metainfo lists {releases[0]} first, newest is {version}"
            )
        source = METAINFO.read_text(encoding="utf-8")
        if source != metainfo:
            raise fail(
                "bundle metainfo differs from the tag source: the release was "
                "tagged before the metainfo was updated"
            )
        notes.append(
            f"bundle metainfo newest-first release {newest}, matches tag source"
        )

    except Mismatch as exc:
        print(f"bundle consistency check FAILED: {exc}", file=sys.stderr)
        return 1
    except (OSError, subprocess.SubprocessError, UnicodeDecodeError, tomllib.TOMLDecodeError) as exc:
        print(f"bundle consistency check ERROR: {exc}", file=sys.stderr)
        return 2

    for note in notes:
        print(f"ok: {note}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
