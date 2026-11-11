#!/usr/bin/env python3
"""Install pinned, checksum-verified gallery-dl for reproducible CI tests."""

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

PINNED_VERSION = "v1.32.17"
PINNED_VERSION_NUMBER = PINNED_VERSION.removeprefix("v")
# Preferred standalone binary from the Codeberg release.
PINNED_BIN_SHA256 = (
    "ccf4d304408a0d57ef78d4ad9221aaad964e36b57466109b48595a52377dcdcc"
)
API = (
    "https://codeberg.org/api/v1/repos/mikf/gallery-dl/"
    f"releases/tags/{PINNED_VERSION}"
)
BASE = "https://codeberg.org/mikf/gallery-dl/releases/download"

# Codeberg is the preferred release source. If it is temporarily unavailable,
# use the maintainer-published PyPI wheel for the same exact version. Pin both
# its filename and SHA-256; never resolve a floating/latest package.
PYPI_API = f"https://pypi.org/pypi/gallery-dl/{PINNED_VERSION_NUMBER}/json"
PINNED_PYPI_WHEEL = f"gallery_dl-{PINNED_VERSION_NUMBER}-py3-none-any.whl"
PINNED_PYPI_WHEEL_SHA256 = (
    "f88ddeb788506f33385e541dbbfba57376ed04bbf6f3f635249a9dd3a78930ed"
)

MAX_METADATA = 2 * 1024 * 1024
MAX_CHECKSUMS = 1024 * 1024
MAX_BINARY = 128 * 1024 * 1024
MAX_WHEEL = 16 * 1024 * 1024
MAX_HTTP_ATTEMPTS = 4
RETRYABLE_HTTP_STATUSES = frozenset({408, 429, 500, 502, 503, 504})
TARGET = Path("/usr/local/bin/gallery-dl")


def request(url, accept="*/*"):
    return urllib.request.Request(
        url, headers={"User-Agent": "Grab-CI", "Accept": accept}
    )


def open_with_retries(url, timeout):
    """Open a public URL, retrying only transient HTTP/network errors."""
    for attempt in range(MAX_HTTP_ATTEMPTS):
        try:
            return urllib.request.urlopen(url, timeout=timeout)
        except urllib.error.HTTPError as error:
            if (
                error.code not in RETRYABLE_HTTP_STATUSES
                or attempt + 1 == MAX_HTTP_ATTEMPTS
            ):
                raise
            retry_after = error.headers.get("Retry-After") if error.headers else None
            error.close()
            try:
                delay = min(8.0, max(0.0, float(retry_after)))
            except (TypeError, ValueError):
                delay = min(8.0, 2.0**attempt)
        except (urllib.error.URLError, TimeoutError):
            if attempt + 1 == MAX_HTTP_ATTEMPTS:
                raise
            delay = min(8.0, 2.0**attempt)
        time.sleep(delay)
    raise RuntimeError("HTTP retry loop exited unexpectedly")


def read_bounded(url, limit, *, accept="*/*"):
    with open_with_retries(request(url, accept=accept), timeout=30) as response:
        length = response.headers.get("Content-Length")
        if length and int(length) > limit:
            raise RuntimeError(f"response exceeds {limit} bytes: {url}")
        data = response.read(limit + 1)
    if len(data) > limit:
        raise RuntimeError(f"response exceeds {limit} bytes: {url}")
    return data


def verify_installed_version():
    result = subprocess.run(
        [str(TARGET), "--version"], check=True, text=True, capture_output=True
    )
    version_output = result.stdout.strip() or result.stderr.strip()
    if PINNED_VERSION_NUMBER not in version_output:
        raise RuntimeError(
            f"expected gallery-dl {PINNED_VERSION_NUMBER}, received {version_output!r}"
        )
    print(f"Installed pinned stable gallery-dl {version_output}")


def install_codeberg_binary():
    release = json.loads(read_bounded(API, MAX_METADATA, accept="application/json"))
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
                raise RuntimeError(
                    "gallery-dl.bin checksum entry is malformed or duplicated"
                )
            digest = fields[0].lower()
    if digest is None:
        raise RuntimeError("SHA256SUMS has no unique gallery-dl.bin digest")
    if digest != PINNED_BIN_SHA256:
        raise RuntimeError(
            f"gallery-dl.bin digest {digest} does not match the pinned digest"
        )

    fd, temporary = tempfile.mkstemp(prefix="gallery-dl.", dir="/usr/local/bin")
    actual = hashlib.sha256()
    size = 0
    try:
        with os.fdopen(fd, "wb") as output, open_with_retries(
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
        os.replace(temporary, TARGET)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)

    verify_installed_version()
    print(f"Codeberg binary SHA-256 verified: {PINNED_BIN_SHA256}")


def install_pypi_wheel():
    release = json.loads(
        read_bounded(PYPI_API, MAX_METADATA, accept="application/json")
    )
    info = release.get("info")
    if not isinstance(info, dict):
        raise RuntimeError("PyPI release metadata is malformed")
    if info.get("name", "").lower() != "gallery-dl":
        raise RuntimeError(f"unexpected PyPI project name: {info.get('name')!r}")
    if info.get("version") != PINNED_VERSION_NUMBER:
        raise RuntimeError(
            f"expected gallery-dl {PINNED_VERSION_NUMBER}, "
            f"received {info.get('version')!r}"
        )

    urls = release.get("urls")
    if not isinstance(urls, list):
        raise RuntimeError("PyPI release has no files list")
    matches = [item for item in urls if item.get("filename") == PINNED_PYPI_WHEEL]
    if len(matches) != 1:
        raise RuntimeError(f"expected exactly one {PINNED_PYPI_WHEEL} wheel")
    wheel = matches[0]
    if wheel.get("packagetype") != "bdist_wheel" or wheel.get("yanked"):
        raise RuntimeError("the pinned PyPI wheel is not an active wheel release")
    if wheel.get("digests", {}).get("sha256") != PINNED_PYPI_WHEEL_SHA256:
        raise RuntimeError("PyPI wheel metadata does not match the pinned SHA-256")

    wheel_url = wheel.get("url")
    parsed_url = urllib.parse.urlparse(wheel_url or "")
    if (
        parsed_url.scheme != "https"
        or parsed_url.hostname != "files.pythonhosted.org"
        or not parsed_url.path.startswith("/packages/")
        or parsed_url.query
        or parsed_url.fragment
    ):
        raise RuntimeError(f"unexpected PyPI wheel URL: {wheel_url!r}")
    size = wheel.get("size")
    if not isinstance(size, int) or not 0 < size <= MAX_WHEEL:
        raise RuntimeError(f"invalid PyPI wheel size: {size!r}")

    wheel_bytes = read_bounded(wheel_url, MAX_WHEEL)
    actual = hashlib.sha256(wheel_bytes).hexdigest()
    if actual != PINNED_PYPI_WHEEL_SHA256:
        raise RuntimeError(
            f"PyPI wheel SHA-256 {actual} does not match the pinned digest"
        )

    # pip only unpacks the already downloaded and hash-verified local wheel.
    # requests is installed explicitly by the Fedora CI dependency step.
    install_dir = Path("/usr/local/lib/grab-ci-gallery-dl")
    if install_dir.exists():
        shutil.rmtree(install_dir)
    install_dir.mkdir(parents=True, exist_ok=True)
    # pip validates wheel filenames against the distribution's PEP 427
    # name-version-build-python-abi-platform structure. A random tempfile
    # basename is not a valid wheel name even when its contents are valid.
    # Keep the exact pinned filename inside a unique temporary directory.
    with tempfile.TemporaryDirectory(prefix="grab-ci-gallery-dl-") as temp_dir:
        wheel_path = Path(temp_dir) / PINNED_PYPI_WHEEL
        wheel_path.write_bytes(wheel_bytes)
        subprocess.run(
            [
                sys.executable,
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                "--no-index",
                "--no-deps",
                "--no-compile",
                "--target",
                str(install_dir),
                str(wheel_path),
            ],
            check=True,
        )

    # The wheel's console entry point is gallery_dl:main. Keep the expected
    # command name while loading only the unpacked, checksum-verified package.
    fd, temporary = tempfile.mkstemp(prefix="gallery-dl.", dir="/usr/local/bin")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            output.write(
                f"#!{sys.executable}\n"
                "import sys\n"
                f"sys.path.insert(0, {str(install_dir)!r})\n"
                "from gallery_dl import main\n"
                "raise SystemExit(main())\n"
            )
        os.chmod(temporary, 0o755)
        os.replace(temporary, TARGET)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)

    verify_installed_version()
    print(f"PyPI wheel SHA-256 verified: {PINNED_PYPI_WHEEL_SHA256}")


try:
    install_codeberg_binary()
except urllib.error.HTTPError as error:
    if error.code not in RETRYABLE_HTTP_STATUSES:
        raise
    print(
        f"Codeberg returned HTTP {error.code} after bounded retries; "
        "falling back to the pinned PyPI wheel",
        file=sys.stderr,
    )
    install_pypi_wheel()
except (urllib.error.URLError, TimeoutError) as error:
    print(
        f"Codeberg release unavailable after bounded retries ({error}); "
        "falling back to the pinned PyPI wheel",
        file=sys.stderr,
    )
    install_pypi_wheel()
