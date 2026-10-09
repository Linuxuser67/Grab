#!/usr/bin/env python3
"""File-size ratchet for src/**/*.rs.

Files already over LIMIT may not grow past the line count recorded in
build-aux/file-size-baseline.json; every other file must stay <= LIMIT.
Shrinking a baselined file is always fine -- run with --update to lower
its recorded size (and to drop entries that fell under LIMIT).

    ./build-aux/check-file-sizes.py            # CI check
    ./build-aux/check-file-sizes.py --update   # ratchet the baseline down
"""
import json
import pathlib
import sys

LIMIT = 1000
ROOT = pathlib.Path(__file__).resolve().parent.parent
BASELINE = ROOT / "build-aux" / "file-size-baseline.json"


def sizes():
    out = {}
    for path in sorted((ROOT / "src").rglob("*.rs")):
        with path.open("rb") as fh:
            out[path.relative_to(ROOT).as_posix()] = sum(1 for _ in fh)
    return out


def main(argv):
    current = sizes()
    baseline = json.loads(BASELINE.read_text()) if BASELINE.exists() else {}

    if "--update" in argv:
        # Only files over LIMIT are tracked; never raise an existing cap.
        new = {}
        for name, lines in current.items():
            if lines <= LIMIT:
                continue
            new[name] = min(lines, baseline.get(name, lines))
        BASELINE.write_text(json.dumps(new, indent=2, sort_keys=True) + "\n")
        print(f"baseline written: {len(new)} file(s) over {LIMIT} lines")
        return 0

    failures = []
    for name, lines in current.items():
        cap = baseline.get(name, LIMIT)
        if lines > cap:
            kind = "grew past its baseline" if name in baseline else f"exceeds {LIMIT} lines"
            failures.append(f"{name}: {lines} lines, {kind} ({cap})")
    for name in baseline:
        if name not in current:
            failures.append(f"{name}: in baseline but missing; run --update")
    if failures:
        print("file-size ratchet failed:", *failures, sep="\n  ")
        print("Split the file (preferred) instead of raising the baseline.")
        return 1
    print(f"ok: {len(current)} files, {len(baseline)} baselined over {LIMIT}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
