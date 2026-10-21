#!/usr/bin/env python3
"""Disposable real-rclone video ranges, public links and plaintext response cache."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import http.client
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

from test_external_storage import Harness, REPO, wait_port


def endpoint(path):
    return "/file/" + urllib.parse.quote(path, safe="")


def cache_files(app):
    root = app.appdir / "temp" / "file-cache"
    return {p for p in root.rglob("*") if p.is_file()} if root.exists() else set()


def downloads(app, start):
    return [c for c in app.adapter_calls()[start:] if c[0] == "copyto" and c[1].startswith("oasis:")]


def run(app):
    app.start_oasis()
    providers, roots = [], {}
    for protocol in app.args.protocols.split(","):
        encrypted, root = app.provider(protocol)
        plain = dict(encrypted, id=protocol + "-plain", name=protocol.upper() + " plain", encryption=False,
                     root=encrypted["root"].replace("encrypted", "plain"))
        providers.extend([encrypted, plain])
        roots[encrypted["name"]] = root
        roots[plain["name"]] = root.parent / "plain"
    config = dict(enabled=True, mode="mount", main_provider=providers[0]["id"], providers=providers)
    app.json("PUT", "/sys/storage", config)
    for provider in providers:
        drive = provider["name"]
        for size in [0, 65536, 262267]:
            data = bytes(i % 251 for i in range(size))
            name = "video-%s.MP4" % size
            path = drive + "/" + name
            app.upload(drive, name, data)
            before = len(app.adapter_calls())
            head, headers = app.api("HEAD", endpoint(path))
            assert not head and int(headers["Content-Length"]) == size
            assert not any(c[0] in ("cat", "copyto") for c in app.adapter_calls()[before:]), "HEAD fetched video bytes"
            cache_before = cache_files(app)
            before = len(app.adapter_calls())
            payload, headers = app.file(path)
            assert payload == data and headers["Accept-Ranges"] == "bytes"
            assert headers["Content-Type"].startswith("video/mp4")
            if size:
                for value, start, stop in [("bytes=0-0", 0, 1), ("bytes=-17", max(0, size - 17), size),
                                            ("bytes=65530-65570", min(65530, size), min(65571, size)),
                                            ("bytes=65000-", min(65000, size), size)]:
                    actual, hdr = app.file(path, headers={"Range": value}, status=206)
                    assert actual == data[start:stop], (drive, size, value, len(actual))
                    assert hdr["Content-Range"] == "bytes %s-%s/%s" % (start, stop - 1, size)
            _, hdr = app.file(path, headers={"Range": "bytes=%s-" % size}, status=416)
            assert hdr["Content-Range"] == "bytes */%s" % size
            app.file(path, headers={"Range": "bytes=0-1,4-5"}, status=416)
            assert not downloads(app, before), "Video was staged to disk"
            assert cache_files(app) == cache_before, "Video entered the file cache"
        share = app.api("POST", "/file/share", {"path": urllib.parse.quote(path, safe=""), "expire": int(time.time()) + 3600})[0].decode()
        public = "/file/share?" + share
        assert app.api("GET", public, headers={"Range": "bytes=65530-65570"}, status=206, anonymous=True)[0] == data[65530:65571]
        assert not app.api("HEAD", public, anonymous=True)[0]
        app.api("GET", public.replace("hash=", "hash=bad"), status=403, anonymous=True)
        app.api("GET", public.replace("expire=", "expire=1&unused="), status=403, anonymous=True)
        app.api("GET", endpoint(path), status=401, anonymous=True)
        print("PASS %s: full/HEAD/ranges/EOF, authenticated and public video, no disk staging" % drive, flush=True)

        # Concurrent anonymous requests use a valid signed link and share one fill.
        content = b"plaintext cache contents " * 4000
        app.upload(drive, "document.bin", content)
        document = drive + "/document.bin"
        share = app.api("POST", "/file/share", {"path": urllib.parse.quote(document, safe=""), "expire": int(time.time()) + 3600})[0].decode()
        public_document = "/file/share?" + share
        before = len(app.adapter_calls())
        with ThreadPoolExecutor(max_workers=4) as pool:
            results = list(pool.map(lambda _: app.api("GET", public_document, anonymous=True)[0], range(4)))
        assert all(result == content for result in results)
        assert len(downloads(app, before)) == 1, "Concurrent fills were not deduplicated"
        assert any(p.read_bytes() == content for p in cache_files(app)), "Plaintext cache missing"
        before = len(app.adapter_calls())
        assert app.file(document)[0] == content
        assert not downloads(app, before), "Cache hit fetched the full file"
        assert any(c[0] == "lsjson" and "--stat" in c for c in app.adapter_calls()[before:]), "Cache hit skipped remote validation"
        app.api("GET", public_document.replace("hash=", "hash=bad"), status=403, anonymous=True)

        replacement = content + b"changed"
        app.upload(drive, "document.bin", replacement)
        assert app.file(document)[0] == replacement, "Oasis overwrite served stale bytes"
        if not provider["encryption"] and provider["protocol"] != "smb":
            (roots[drive] / "document.bin").write_bytes(b"provider-side edit")
            assert app.file(document)[0] == b"provider-side edit", "Out-of-band edit served stale bytes"
            if provider["protocol"] == "b2":
                # The fixture stores B2 versions in memory; removing its disk
                # mirror alone does not remove a native B2 object.
                app.api("DELETE", endpoint(document))
            else:
                (roots[drive] / "document.bin").unlink()
            app.file(document, status=500)
        else:
            app.api("DELETE", endpoint(document))
            app.file(document, status=500)
        print("PASS %s: concurrent plaintext cache, revalidation, overwrite/deletion and auth" % drive, flush=True)

    # Ciphertext corruption must fail before even the first plaintext response.
    provider = providers[0]
    drive = provider["name"]
    app.upload(drive, "tampered.mp4", b"never release corrupt plaintext" * 4000)
    candidates = [p for p in roots[drive].iterdir() if p.is_file() and p.name.startswith("o1-")]
    for candidate in candidates:
        data = candidate.read_bytes()
        if int.from_bytes(data[40:48], "big") == len(b"never release corrupt plaintext" * 4000):
            candidate.write_bytes(data[:48] + bytes([data[48] ^ 1]) + data[49:])
            break
    else:
        raise AssertionError("Encrypted corruption fixture not found")
    app.file(drive + "/tampered.mp4", status=500)

    # Later corruption must abort a started response, while an unrelated range
    # remains readable. Exact multiples must authenticate the empty EOF chunk.
    for name, size, corrupt_at in [("late.mp4", 196731, 48 + 65552 + 7), ("terminal.mp4", 65536, -1)]:
        existing = set(roots[drive].iterdir())
        app.upload(drive, name, b"x" * size)
        for candidate in set(roots[drive].iterdir()) - existing:
            if candidate.is_file() and candidate.name.startswith("o1-"):
                data = candidate.read_bytes()
                if int.from_bytes(data[40:48], "big") == size:
                    damaged = bytearray(data)
                    damaged[corrupt_at] ^= 1
                    candidate.write_bytes(damaged)
                    break
        else:
            raise AssertionError("Corruption fixture not found")
        if name == "late.mp4":
            assert app.file(drive + "/" + name, headers={"Range": "bytes=0-100"}, status=206)[0] == b"x" * 101
            try:
                app.file(drive + "/" + name)
            except http.client.IncompleteRead as error:
                assert len(error.partial) < size
            else:
                raise AssertionError("Late authentication failure returned a complete video")
        else:
            app.file(drive + "/" + name, status=500)

    app.upload(drive, "unavailable.bin", b"must not serve stale cache")
    assert app.file(drive + "/unavailable.bin")[0] == b"must not serve stale cache"
    fault = app.root / "verification-fault.json"
    try:
        fault.write_text(json.dumps({"mode": "request-error"}))
        app.file(drive + "/unavailable.bin", status=500)
    finally:
        fault.unlink(missing_ok=True)
    before = len(app.adapter_calls())
    app.file(drive + "/unavailable.bin")
    assert len(downloads(app, before)) == 1

    # Failure to create cache storage must not publish a partial entry.
    app.json("PUT", "/sys/storage", config)
    cache = app.appdir / "temp" / "file-cache"
    cache.rmdir()
    cache.write_bytes(b"deliberate cache path collision")
    try:
        app.file(drive + "/unavailable.bin", status=500)
    finally:
        cache.unlink()
    assert app.file(drive + "/unavailable.bin")[0] == b"must not serve stale cache"
    print("PASS early/late/terminal authentication failures, unavailable-provider and cache-write failures", flush=True)

    native = next((p for p in providers if p["protocol"] == "b2" and p["encryption"]), None)
    if native:
        app.upload(native["name"], "cancel.mp4", b"v" * (8 * 1024 * 1024))
        fault = roots[native["name"]].parent.parent / "b2-fault.json"
        try:
            fault.write_text(json.dumps({"slow_download": True}))
            request = urllib.request.Request(app.base + "/api" + endpoint(native["name"] + "/cancel.mp4"))
            response = app.opener.open(request, timeout=120)
            response.read(1024)
            pid = json.loads((app.root / "adapter-cat-pids.jsonl").read_text().splitlines()[-1])["pid"]
            os.kill(pid, 0)  # Prove the remote producer is still active.
            response.close()
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.05)
            else:
                raise AssertionError("Disconnected response left its rclone producer running")
        finally:
            fault.unlink(missing_ok=True)
        print("PASS mid-transfer disconnect kills real rclone producer", flush=True)

    # Restart clears the disposable cache, independently of TTL.
    app.upload(drive, "restart.bin", b"cached before restart")
    assert app.file(drive + "/restart.bin")[0] == b"cached before restart"
    assert cache_files(app)
    app.app.terminate()
    app.app.wait(timeout=10)
    app.app = app.start([str(app.executable)], "oasis-restart")
    wait_port(app.number, app.app)
    app.login()
    assert not cache_files(app)
    before = len(app.adapter_calls())
    assert app.file(drive + "/restart.bin")[0] == b"cached before restart"
    assert len(downloads(app, before)) == 1
    print("PASS corruption rejection and restart cache clearing", flush=True)

    if app.args.keep_alive:
        # A generated browser-compatible clip is optional and never committed.
        clip = app.root / "safari.mp4"
        subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=24",
                        "-t", "30", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-movflags", "+faststart", str(clip)], check=True)
        app.upload(drive, "Safari playback.mp4", clip.read_bytes())
        share = app.api("POST", "/file/share", {"path": urllib.parse.quote(drive + "/Safari playback.mp4", safe=""), "expire": int(time.time()) + 3600})[0].decode()
        print("Safari: %s (tester / storage123)" % app.base, flush=True)
        print("Public: %s/api/file/share?%s" % (app.base, share), flush=True)
        print("Fixture: %s" % app.root, flush=True)
        while True:
            time.sleep(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rclone", type=Path, required=True)
    parser.add_argument("--oasis", type=Path, default=REPO / "backend/target/debug/oasis")
    parser.add_argument("--protocols", default="webdav,b2")
    parser.add_argument("--test-python-path", type=Path)
    parser.add_argument("--passphrase", action="store_true")
    parser.add_argument("--keep-alive", action="store_true")
    args = parser.parse_args()
    args.rclone = args.rclone.resolve()
    with tempfile.TemporaryDirectory(prefix="oasis-video-tests-") as directory:
        app = Harness(Path(directory), args)
        try:
            run(app)
        except KeyboardInterrupt:
            pass
        except Exception:
            for log in Path(directory).glob("*.log"):
                print(log.name, log.read_text(errors="replace")[-3000:])
            raise
        finally:
            app.close()


if __name__ == "__main__":
    main()
