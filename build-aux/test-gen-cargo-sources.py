#!/usr/bin/env python3
"""Regression tests for the registry-source assumptions in gen-cargo-sources.py."""

from __future__ import annotations

import importlib.util
from pathlib import Path


SCRIPT = Path(__file__).with_name("gen-cargo-sources.py")
SPEC = importlib.util.spec_from_file_location("gen_cargo_sources", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise SystemExit("could not load gen-cargo-sources.py")
GENERATOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GENERATOR)


def main() -> None:
    supported = GENERATOR.CRATES_IO_SOURCE
    assert GENERATOR.is_supported_registry_source(supported)
    for source in (
        "registry+https://packages.example.test/index",
        "git+https://github.com/example/dependency",
        "",
    ):
        assert not GENERATOR.is_supported_registry_source(source), source
    print("gen-cargo-sources registry guards passed")


if __name__ == "__main__":
    main()
