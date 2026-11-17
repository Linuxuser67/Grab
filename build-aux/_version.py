"""Shared AppStream/Cargo version ordering helpers."""

from __future__ import annotations


def _identifier(part: str) -> tuple[int, int | str]:
    """Sort numeric identifiers before text without comparing unlike types."""
    return (0, int(part)) if part.isdigit() else (1, part)


def version_key(version: str) -> tuple:
    """Return an AppStream-compatible ordering key for a release version.

    AppStream uses '~' for prereleases while Cargo tags commonly use '-'.
    """
    version = (
        version.replace("-beta.", "~beta.")
        .replace("-alpha.", "~alpha.")
        .replace("-rc.", "~rc.")
    )
    if "~" in version:
        core, pre = version.split("~", 1)
        core_tuple = tuple(_identifier(part) for part in core.split("."))
        pre_parts = tuple(_identifier(part) for part in pre.split("."))
        return (core_tuple, 0, pre_parts)
    core_tuple = tuple(_identifier(part) for part in version.split("."))
    return (core_tuple, 1, ())
