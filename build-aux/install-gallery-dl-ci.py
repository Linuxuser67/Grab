#!/usr/bin/env python3
"""Install the pinned, checksum-verified gallery-dl binary used by CI."""

import hashlib
import json
import os
import re
import subprocess
import tempfile
import urllib.request
from pathlib import Path

PINNED_VERSION = "v1.32.17"
# The release's SHA256SUMS is fetched only as a cross-check; the installed
# binary must match this committed digest, not whatever the host serves today.
PINNED_BIN_SHA256 = (
    "ccf4d304408a0d57ef78d4ad9221aaad964e36b57466109b48595a52377dcdcc"
)
API = (
    "https://codeberg.org/api/v1/repos/mikf/gallery-dl/"
    f"releases/tags/{PINNED_VERSION}"
)
BASE = "https://codeberg.org/mikf/gallery-dl/releases/download"
MAX_METADATA = 2 * 1024 * 1024
MAX_CHECKSUMS = 1024 * 1024
MAX_BINARY = 128 * 1024 * 1024


def request(url):
    return urllib.request.Request(
        url, headers={"User-Agent": "Grab-CI", "Accept": "application/json"}
    )


def read_bounded(url, limit):
    with urllib.request.urlopen(request(url), timeout=30) as response:
        length = response.headers.get("Content-Length")
        if length and int(length) > limit:
            raise RuntimeError(f"response exceeds {limit} bytes: {url}")
        data = response.read(limit + 1)
    if len(data) > limit:
        raise RuntimeError(f"response exceeds {limit} bytes: {url}")
    return data


release = json.loads(read_bounded(API, MAX_METADATA))
tag = release.get("tag_name")
if tag != PINNED_VERSION:
    raise RuntimeError(f"expected gallery-dl {PINNED_VERSION}, received {tag!r}")
if release.get("draft") or release.get("prerelease"):
    raise RuntimeError(f"gallery-dl {PINNED_VERSION} is not a stable release")
if not isinstance(tag, str) or not re.fullmatch(
    r"[A-Za-z0-9][A-Za-z0-9.+_-]*", tag
):
    raise RuntimeError(f"invalid gallery-dl release tag: {tag!r}")

assets = release.get("assets")
if not isinstance(assets, list):
    raise RuntimeError("release has no assets list")


def exact_asset(name):
    matches = [asset for asset in assets if asset.get("name") == name]
    if len(matches) != 1:
        raise RuntimeError(f"expected exactly one {name} asset")
    url = matches[0].get("browser_download_url")
    expected = f"{BASE}/{tag}/{name}"
    if url != expected:
        raise RuntimeError(f"unexpected {name} asset URL: {url!r}")
    return url


binary_url = exact_asset("gallery-dl.bin")
checksums_url = exact_asset("SHA256SUMS")
checksums = read_bounded(checksums_url, MAX_CHECKSUMS).decode("utf-8")
digest = None
for line in checksums.splitlines():
    fields = line.split()
    if len(fields) >= 2 and fields[1].removeprefix("*") == "gallery-dl.bin":
        if (
            len(fields) != 2
            or not re.fullmatch(r"[0-9a-fA-F]{64}", fields[0])
            or digest is not None
        ):
            raise RuntimeError("gallery-dl.bin checksum entry is malformed or duplicated")
        digest = fields[0].lower()
if digest is None:
    raise RuntimeError("SHA256SUMS has no unique gallery-dl.bin digest")
if digest != PINNED_BIN_SHA256:
    raise RuntimeError(
        f"gallery-dl.bin digest {digest} does not match the pinned digest"
    )

target = Path("/usr/local/bin/gallery-dl")
fd, temporary = tempfile.mkstemp(prefix="gallery-dl.", dir="/usr/local/bin")
actual = hashlib.sha256()
size = 0
try:
    with os.fdopen(fd, "wb") as output, urllib.request.urlopen(
        request(binary_url), timeout=60
    ) as response:
        length = response.headers.get("Content-Length")
        if length and int(length) > MAX_BINARY:
            raise RuntimeError("gallery-dl.bin exceeds the 128 MiB ceiling")
        while True:
            chunk = response.read(1024 * 1024)
            if not chunk:
                break
            size += len(chunk)
            if size > MAX_BINARY:
                raise RuntimeError("gallery-dl.bin exceeds the 128 MiB ceiling")
            actual.update(chunk)
            output.write(chunk)
    if actual.hexdigest() != PINNED_BIN_SHA256:
        raise RuntimeError("gallery-dl.bin SHA-256 mismatch")
    os.chmod(temporary, 0o755)
    os.replace(temporary, target)
finally:
    if os.path.exists(temporary):
        os.unlink(temporary)

result = subprocess.run(
    [str(target), "--version"], check=True, text=True, capture_output=True
)
print(
    f"Installed pinned stable gallery-dl {tag} "
    f"({result.stdout.strip() or result.stderr.strip()}); SHA-256 verified"
)
