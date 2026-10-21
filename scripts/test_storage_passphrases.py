#!/usr/bin/env python3
"""Opt-in passphrase namespace recovery tests using disposable WebDAV storage."""
import argparse
import copy
import json
from pathlib import Path
import tempfile
import time

from test_external_storage import Harness, REPO, wait_port


def run(app):
    app.start_oasis()
    provider, root = app.provider("webdav")
    config = {"enabled": True, "mode": "mount", "main_provider": provider["id"], "providers": [provider]}
    secret = provider["encryption_key"]
    for invalid in ["short", " " * 12, "界" * 342, "x" * 1025]:
        bad = copy.deepcopy(config)
        bad["providers"][0]["encryption_key"] = invalid
        app.api("PUT", "/sys/storage", bad, status=400)
        assert not list(root.iterdir())
    app.json("PUT", "/sys/storage", config)
    marker_path = root / ".oasis-storage-v1"
    marker_bytes = marker_path.read_bytes()
    marker = json.loads(marker_bytes)
    assert marker["version"] == 2 and marker["derivation"]["version"] == 1
    assert secret not in marker_bytes.decode()
    app.upload("WEBDAV", "with spaces 日本語.txt", b"plaintext download from passphrase storage")
    assert app.file("WEBDAV/with spaces 日本語.txt")[0] == b"plaintext download from passphrase storage"
    bad = copy.deepcopy(config)
    bad["providers"][0]["encryption_key"] += " "
    app.api("PUT", "/sys/storage", bad, status=400)
    print("PASS passphrase limits, preserved spaces/Unicode, encryption and key-change protection", flush=True)

    # Remove configuration, retaining the namespace. Reconnecting must recover
    # its original salt from the marker instead of generating another one.
    app.json("PUT", "/sys/storage", {"enabled": False, "providers": []})
    for mutation in ["unsupported", "salt", "verification", "malformed", "oversized"]:
        modified = copy.deepcopy(marker)
        if mutation == "unsupported":
            modified["version"] = 3
        elif mutation == "salt":
            modified["derivation"]["salt"][0] ^= 1
        elif mutation == "verification":
            modified["verification"] = "invalid"
        content = json.dumps(modified).encode()
        if mutation == "malformed":
            content = b"{bad"
        if mutation == "oversized":
            content = b"x" * 16385
        marker_path.write_bytes(content)
        app.api("PUT", "/sys/storage", config, status=400)
        assert marker_path.read_bytes() == content
    marker_path.write_bytes(marker_bytes)
    app.api("PUT", "/sys/storage", bad, status=400)
    app.json("PUT", "/sys/storage", config)
    assert marker_path.read_bytes() == marker_bytes
    assert app.file("WEBDAV/with spaces 日本語.txt")[0] == b"plaintext download from passphrase storage"
    print("PASS reconnection with recovered salt; corrupt/unsupported markers remain untouched", flush=True)

    # Exercise environment precedence and loading from the sealed database.
    app.app.terminate(); app.app.wait(timeout=10)
    app.env["OASIS_STORAGE_KEY"] = secret
    app.app = app.start([str(app.executable)], "environment-restarted")
    wait_port(app.number, app.app, timeout=120); app.login()
    override = copy.deepcopy(config)
    override["providers"][0]["encryption_key"] = "different settings secret"
    app.json("PUT", "/sys/storage", override)
    assert app.file("WEBDAV/with spaces 日本語.txt")[0] == b"plaintext download from passphrase storage"
    app.app.terminate(); app.app.wait(timeout=10)
    app.app = app.start([str(app.executable)], "persisted-restarted")
    wait_port(app.number, app.app, timeout=120); app.login()
    assert app.file("WEBDAV/with spaces 日本語.txt")[0] == b"plaintext download from passphrase storage"
    app.cleaned()
    print("PASS environment precedence, sealed configuration and restart persistence", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rclone", type=Path, required=True)
    parser.add_argument("--oasis", type=Path, default=REPO / "backend/target/debug/oasis")
    parser.add_argument("--keep-alive", action="store_true")
    args = parser.parse_args()
    args.rclone = args.rclone.resolve()
    args.passphrase = True
    args.test_python_path = None
    with tempfile.TemporaryDirectory(prefix="oasis-passphrase-tests-") as directory:
        app = Harness(Path(directory), args)
        try:
            run(app)
            if args.keep_alive:
                print("Safari: %s/settings (tester / storage123)" % app.base, flush=True)
                while True:
                    time.sleep(1)
        except KeyboardInterrupt:
            pass
        finally:
            app.close()


if __name__ == "__main__":
    main()
