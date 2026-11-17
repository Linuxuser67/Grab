#!/usr/bin/env python3
"""Generate build-aux/cargo-sources.json from Cargo.lock for offline Flatpak builds.

Same output shape as flatpak-builder-tools' flatpak-cargo-generator.py for
registry-only locks (all our deps are crates.io): per-crate archives +
.cargo-checksum.json inlines + a vendored-sources cargo config inline.
Stdlib only (tomllib), so it runs anywhere. Re-run after every Cargo.lock
change and commit the result.
Usage: ./build-aux/gen-cargo-sources.py [--check]   (run from the repo root)

--check exits nonzero when the committed file is stale instead of
rewriting it; CI runs this so a Cargo.lock change without a regenerated
vendor file fails fast instead of breaking the release Flatpak build.
"""
import argparse
import json
import sys
import tomllib

OUT = "build-aux/cargo-sources.json"
CRATES_IO = "https://static.crates.io/crates"


def render() -> tuple[str, int]:
    try:
        with open("Cargo.lock", "rb") as f:
            lock = tomllib.load(f)
    except FileNotFoundError:
        sys.exit("ERROR: Cargo.lock not found; run from the repo root")
    except tomllib.TOMLDecodeError as e:
        sys.exit(f"ERROR: cannot parse Cargo.lock: {e}")
    metadata = lock.get("metadata", {})
    sources: list = []
    n = 0
    for pkg in lock["package"]:
        source = pkg.get("source", "")
        if not source:
            # Only the root Grab workspace package is expected to be source-less.
            # Any other source-less lock entry is a path dependency; silently
            # skipping it would produce an incomplete offline vendor directory.
            if pkg["name"] == "grab":
                continue
            sys.exit(
                f"ERROR: path package {pkg['name']} {pkg['version']} has no registry source; "
                "extend this generator before adding it to Cargo.lock"
            )
        if not source.startswith("registry+"):
            # A git/path dependency cannot be vendored by this registry-only script.
            # Silent skipping would let --check pass while the offline build lacks the crate.
            sys.exit(
                f"ERROR: non-registry package {pkg['name']} has source {source}; "
                "extend this generator before adding it to Cargo.lock"
            )
        name, version = pkg["name"], pkg["version"]
        key = f"checksum {name} {version} ({source})"
        checksum = metadata.get(key, pkg.get("checksum"))
        if not checksum:
            sys.exit(f"ERROR: no checksum for {name} {version}")
        dest = f"cargo/{name}-{version}"
        sources.append(
            {
                "type": "archive",
                "archive-type": "tar-gzip",
                "url": f"{CRATES_IO}/{name}/{name}-{version}.crate",
                "sha256": checksum,
                "dest": dest,
            }
        )
        sources.append(
            {
                "type": "inline",
                "contents": json.dumps({"package": checksum, "files": {}}),
                "dest": dest,
                "dest-filename": ".cargo-checksum.json",
            }
        )
        n += 1
    config = (
        '[source.crates-io]\nreplace-with = "vendored-sources"\n'
        '[source.vendored-sources]\ndirectory = "cargo"\n'
    )
    sources.append(
        {
            "type": "inline",
            "contents": config,
            "dest": "cargo",
            "dest-filename": "config",
        }
    )
    return json.dumps(sources, indent=4) + "\n", n


def main() -> None:
    parser = argparse.ArgumentParser(description="Regenerate vendored cargo sources for offline Flatpak builds.")
    parser.add_argument(
        "--check",
        action="store_true",
        help="exit 1 when the committed file differs from a fresh render",
    )
    args = parser.parse_args()
    rendered, n = render()
    if args.check:
        try:
            with open(OUT, encoding="utf-8") as f:
                committed = f.read()
        except FileNotFoundError:
            sys.exit(f"ERROR: {OUT} missing; run {sys.argv[0]} to generate it")
        if committed != rendered:
            sys.exit(
                f"ERROR: {OUT} is stale for the current Cargo.lock ({n} crates); "
                f"run {sys.argv[0]} from the repo root and commit the result"
            )
        print(f"{OUT} in sync with Cargo.lock ({n} crates)")
        return
    with open(OUT, "w", encoding="utf-8") as f:
        f.write(rendered)
    print(f"wrote {OUT} with {n} crates")


if __name__ == "__main__":
    main()
