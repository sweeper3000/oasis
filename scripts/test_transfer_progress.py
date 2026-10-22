#!/usr/bin/env python3
"""Live copy/move progress using disposable providers and real throttled rclone."""
import argparse
import json
import os
from pathlib import Path
import tempfile
import time
import urllib.parse

from test_external_storage import Harness, REPO


def job(app, source, target, is_copy=True, overwrite=False, expected="Success", phase=None):
    uuid = app.api("POST", "/file/copy-move", {
        "source": urllib.parse.quote(source, safe=""), "target": urllib.parse.quote(target, safe=""),
        "is_copy": is_copy, "overwrite": overwrite,
    })[0].decode()
    samples, deadline = [], time.monotonic() + 180
    while time.monotonic() < deadline:
        task = app.json("GET", "/file/copy-move-status/" + uuid)
        assert task["user_id"] == 0
        assert task["uuid"] == uuid and 0 <= task["progress"] <= 1
        assert not samples or task["progress"] >= samples[-1]["progress"], samples + [task]
        if task["status"] != "Success":
            assert task["progress"] < 1
        samples.append(task)
        if task["status"] in ("Success", "Failed"):
            assert task["status"] == expected, task
            if expected == "Success":
                assert task["progress"] == 1 and task["phase"] == "completed"
            break
        time.sleep(0.05)
    else:
        raise AssertionError("Progress job did not finish")
    if phase:
        intermediate = [s for s in samples if s["phase"] == phase and 0 < s["progress"] < 0.99]
        assert len({s["progress"] for s in intermediate}) >= 2, (phase, samples)
        assert all(s["current_file"] == source for s in intermediate), intermediate
    return samples


def run(app):
    app.start_oasis()
    providers = [app.provider(protocol)[0] for protocol in app.args.protocols.split(",")]
    first = providers[0]
    providers.append(dict(first, id=first["id"] + "-plain", name=first["name"] + " plain", encryption=False,
                          root=first["root"].replace("encrypted", "plain")))
    config = {"enabled": True, "mode": "mount", "main_provider": providers[0]["id"], "providers": providers}
    app.json("PUT", "/sys/storage", config)
    payload = bytes(range(251)) * 17000  # ~4 MiB, several upload and AEAD chunks.
    source = app.local / "progress 日本語.bin"
    source.write_bytes(payload)
    (app.local / "local-target").mkdir()
    samples = job(app, source.name, "local-target")
    assert (app.local / "local-target" / source.name).read_bytes() == payload
    print("PASS local copy: correct bytes, progress lifecycle and completion", flush=True)

    throttle = app.root / "progress-throttle.json"
    throttle.write_text(json.dumps({"rate": "1M", "verify_delay": 0.4}))
    for provider in providers:
        drive = provider["name"]
        for folder in ["incoming", "remote copy", "failed"]:
            app.api("POST", "/dir", {"parent": urllib.parse.quote(drive, safe=""), "name": folder})
        incoming = drive + "/incoming"
        samples = job(app, source.name, incoming, phase="uploading")
        if not provider["encryption"]:
            assert not any(s["phase"] == "encrypting" for s in samples)
        assert any(s["phase"] == "verifying" and s["progress"] <= 0.99 for s in samples)
        remote_source = incoming + "/" + source.name
        assert app.file(remote_source)[0] == payload
        samples = job(app, remote_source, drive + "/remote copy", phase="downloading")
        assert any(s["phase"] == "uploading" and s["progress"] > 0 for s in samples)
        remote_copy = drive + "/remote copy/" + source.name
        assert app.file(remote_copy)[0] == payload
        samples = job(app, remote_copy, "", is_copy=False, overwrite=True, phase="downloading")
        assert source.read_bytes() == payload
        assert not any(e["filename"] == source.name for e in app.listing(drive + "/remote copy"))

        fault = app.root / "verification-fault.json"
        try:
            fault.write_text(json.dumps({"mode": "size"}))
            samples = job(app, source.name, drive + "/failed", is_copy=False, expected="Failed", phase="uploading")
            failed = samples[-1]
            assert failed["phase"] == "verifying" and failed["progress"] > 0
            assert failed["current_file"] == source.name and source.read_bytes() == payload
        finally:
            fault.unlink(missing_ok=True)
        print("PASS %s: local/remote and remote/remote live byte progress, verification failure retains source and progress" % drive, flush=True)

    if len(providers) > 2:
        remote_source = providers[0]["name"] + "/incoming/" + source.name
        destination = providers[1]["name"] + "/remote copy"
        job(app, remote_source, destination, phase="downloading")
        assert app.file(destination + "/" + source.name)[0] == payload
        print("PASS cross-provider copy: separate download/decrypt/encrypt/upload work", flush=True)

    throttle.unlink()
    # Folder totals include mixed sizes and empty files/directories.
    folder = app.local / "mixed folder"
    (folder / "nested" / "empty directory").mkdir(parents=True)
    (folder / "nested" / "zero.bin").write_bytes(b"")
    (folder / "small.txt").write_bytes(b"small")
    (folder / "large.bin").write_bytes(payload)
    job(app, folder.name, "local-target", is_copy=False)
    moved = app.local / "local-target" / folder.name
    assert not folder.exists() and (moved / "nested/zero.bin").read_bytes() == b""
    assert (moved / "nested/empty directory").is_dir() and (moved / "large.bin").read_bytes() == payload

    if os.geteuid() != 0:
        # Destination commit succeeds, but source cleanup lacks directory access.
        parent = app.local / "read-only-parent"
        parent.mkdir()
        (parent / "retain.txt").write_bytes(b"retain on cleanup failure")
        parent.chmod(0o555)
        try:
            samples = job(app, "read-only-parent/retain.txt", "local-target", is_copy=False, expected="Failed")
            assert samples[-1]["phase"] == "removing_source" and samples[-1]["progress"] == 0.99
            assert (parent / "retain.txt").exists()
        finally:
            parent.chmod(0o755)
    app.cleaned()
    print("PASS mixed-size folder, empty entries, local move and source cleanup failure", flush=True)
    if app.args.keep_alive:
        (app.local / "Copy me.bin").write_bytes(payload * 2)
        (app.local / "Move me.bin").write_bytes(payload * 2)
        throttle.write_text(json.dumps({"rate": "256k", "verify_delay": 1}))
        print("Safari: %s (tester / storage123)" % app.base, flush=True)
        print("Fixture: %s" % app.root, flush=True)
        while True:
            time.sleep(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rclone", type=Path, required=True)
    parser.add_argument("--oasis", type=Path, default=REPO / "backend/target/debug/oasis")
    parser.add_argument("--protocols", default="webdav,b2")
    parser.add_argument("--passphrase", action="store_true")
    parser.add_argument("--test-python-path", type=Path)
    parser.add_argument("--keep-alive", action="store_true")
    args = parser.parse_args()
    args.rclone = args.rclone.resolve()
    with tempfile.TemporaryDirectory(prefix="oasis-progress-tests-") as directory:
        app = Harness(Path(directory), args)
        try:
            run(app)
        except KeyboardInterrupt:
            pass
        except Exception:
            for log in Path(directory).glob("*.log"):
                print(log.name, log.read_text(errors="replace")[-3500:])
            raise
        finally:
            app.close()


if __name__ == "__main__":
    main()
