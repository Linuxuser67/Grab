#!/usr/bin/env python3
"""Fail CI when Rust gettext sources are missing from po/POTFILES.in."""

from __future__ import annotations

import re
import sys
from pathlib import Path

GETTEXT_CALL = re.compile(r"\b(?:gettext|ngettext|pgettext|npgettext)\s*\(")
REQUIRED_DATA_SOURCES = {
    "data/io.github.linuxuser67.Grab.desktop.in",
    "data/io.github.linuxuser67.Grab.metainfo.xml.in",
}


def main() -> int:
    potfiles_path = Path("po/POTFILES.in")
    entries = {
        line.strip()
        for line in potfiles_path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    }

    missing: list[str] = []
    for source in sorted(Path("src").rglob("*.rs")):
        if source.name.endswith("_tests.rs"):
            continue
        content = source.read_text(encoding="utf-8")
        if GETTEXT_CALL.search(content):
            relative = source.as_posix()
            if relative not in entries:
                missing.append(relative)

    missing.extend(sorted(REQUIRED_DATA_SOURCES - entries))
    missing = sorted(set(missing))

    nonexistent = sorted(entry for entry in entries if not Path(entry).is_file())
    if missing or nonexistent:
        if missing:
            print("Missing from po/POTFILES.in:")
            for entry in missing:
                print(f"  {entry}")
        if nonexistent:
            print("POTFILES.in entries that do not exist:")
            for entry in nonexistent:
                print(f"  {entry}")
        return 1

    print("All Rust gettext sources and translatable data templates are listed in po/POTFILES.in.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
