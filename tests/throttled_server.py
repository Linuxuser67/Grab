"""Throttled single-file HTTP server for Grab's lifecycle test.

Serves one file slowly (~160KB/s) so pause() always lands mid-transfer.
Honors Range requests with 206 (exercises real resume); logs each
request's range (or "full") for assertions.
Supports If-Range: logs the header, sends ETag, and in "if-range-strict"
mode returns 200 if If-Range doesn't match (simulating a changed file).
Usage: throttled_server.py PORT FILE RANGELOG [SLEEP] [DISPOSITION] [MODE] [ETAG]
"""
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

with open(sys.argv[2], "rb") as f:
    DATA = f.read()
RANGELOG = sys.argv[3]
SLEEP = float(sys.argv[4]) if len(sys.argv) > 4 else 0.05
DISPOSITION = sys.argv[5] if len(sys.argv) > 5 else None
# "throttle-ranges": 206 only for the bytes=0-0 probe, 403 for every other
# Range request (simulates per-IP connection limits on file hosts).
# "if-range-strict": if If-Range is present and doesn't match ETAG, return 200
# (simulates a changed file); otherwise honor ranges normally.
MODE = sys.argv[6] if len(sys.argv) > 6 else "normal"
# ETag to send; defaults to a fixed value for If-Range testing.
ETAG = sys.argv[7] if len(sys.argv) > 7 else '"test-etag-123"'


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        requested = self.headers.get("Range")
        if_range = self.headers.get("If-Range")
        with open(RANGELOG, "a") as log:
            # Log format: "range=<range>|if-range=<value>" for assertions.
            # "full" if no Range header.
            range_part = requested or "full"
            if_range_part = if_range or "none"
            log.write(f"range={range_part}|if-range={if_range_part}\n")
        start, end, is_range = 0, len(DATA) - 1, False
        if requested and requested.startswith("bytes="):
            is_range = True
            spec = requested[len("bytes="):]
            first, _, last = spec.partition("-")
            try:
                start = int(first) if first else 0
            except ValueError:
                start = 0
            try:
                end = int(last) if last else len(DATA) - 1
            except ValueError:
                end = len(DATA) - 1
        if start >= len(DATA):
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{len(DATA)}")
            self.end_headers()
            return
        if MODE == "throttle-ranges" and is_range and not (start == 0 and end == 0):
            self.send_response(403)
            self.end_headers()
            return
        # If-Range strict mode: mismatch means the file changed → return 200
        # with full body (client must detect Changed).
        if MODE == "if-range-strict" and if_range and if_range != ETAG:
            self.send_response(200)
            self.send_header("Content-Length", str(len(DATA)))
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("ETag", ETAG)
            self.end_headers()
            try:
                self.wfile.write(DATA)
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        body = DATA[start : end + 1]
        if is_range:
            self.send_response(206)
            self.send_header("Content-Range", f"bytes {start}-{end}/{len(DATA)}")
        else:
            self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("ETag", ETAG)
        if DISPOSITION:
            self.send_header("Content-Disposition", DISPOSITION)
        self.end_headers()
        try:
            for i in range(0, len(body), 8192):
                self.wfile.write(body[i : i + 8192])
                self.wfile.flush()
                time.sleep(SLEEP)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, *a):
        pass


HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
