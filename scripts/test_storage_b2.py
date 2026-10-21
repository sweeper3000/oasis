#!/usr/bin/env python3
"""Native B2 conversion, permissions, version deletion and recovery tests."""
import argparse
import copy
import json
from pathlib import Path
import sys
import tempfile
import time
import urllib.request
import urllib.parse

from test_external_storage import Harness, REPO, port, wait_port


def run(app):
    app.start_oasis()
    legacy, encrypted_root = app.provider("s3")
    api_port = port()
    remote = encrypted_root.parent.parent
    api_process = app.start([sys.executable, str(REPO / "scripts/test_storage_b2_server.py"), str(remote), str(api_port)], "b2-native")
    wait_port(api_port, api_process)
    endpoint = "http://127.0.0.1:%s" % api_port
    native = dict(legacy, id="native", name="Backblaze B2", protocol="b2", root="test-bucket/plain", endpoint=endpoint, encryption=False)
    config = dict(enabled=True, mode="mount", main_provider="native", providers=[native])
    app.json("PUT", "/sys/storage", config)
    native["encryption"] = True
    fault = remote / "b2-fault.json"
    fault.write_text(json.dumps({"deny_upload": True}))
    app.api("POST", "/sys/storage/test", config)
    error = app.api("PUT", "/sys/storage", config, status=400)[0].decode()
    assert "unauthorized" in error and native["secret_access_key"] not in error
    assert not app.json("GET", "/sys/storage")["providers"][0]["encryption"]
    assert not list((remote / "test-bucket/plain").iterdir())
    fault.unlink()
    app.json("PUT", "/sys/storage", config)
    app.cleaned()
    print("PASS read-only connection test, rejected marker upload, sanitized errors and retry", flush=True)

    app.upload("Backblaze B2", "history.txt", b"old version")
    objects = [p for p in (remote / "test-bucket/plain").iterdir() if p.name != ".oasis-storage-v1"]
    assert len(objects) == 1
    filename = objects[0].relative_to(remote / "test-bucket").as_posix()

    def versions():
        req = urllib.request.Request(endpoint + "/b2api/v1/b2_list_file_versions", method="POST",
                                     data=json.dumps({"bucketId": "test-bucket-id", "prefix": filename}).encode(),
                                     headers={"Authorization": "test-token", "Content-Type": "application/json"})
        with urllib.request.urlopen(req) as response:
            return json.load(response)["files"]

    old_id = versions()[0]["fileId"]
    app.upload("Backblaze B2", "history.txt", b"new version")
    new_id = versions()[0]["fileId"]
    app.api("DELETE", "/file/" + urllib.parse.quote("Backblaze B2/history.txt", safe=""))
    entries = versions()
    assert new_id not in {e["fileId"] for e in entries}
    assert old_id in {e["fileId"] for e in entries} and entries[0]["action"] == "hide"
    assert not app.listing("Backblaze B2")
    bad = copy.deepcopy(config)
    bad["providers"][0]["root"] = "missing-bucket"
    app.api("PUT", "/sys/storage", bad, status=400)
    print("PASS permanent current-version deletion, hidden older history and missing-bucket rejection", flush=True)

    # A real S3 server and native B2 fixture share the same underlying objects.
    legacy["name"] = "Backblaze B2"
    legacy_config = dict(enabled=True, mode="mount", main_provider=legacy["id"], providers=[legacy])
    app.json("PUT", "/sys/storage", legacy_config)
    app.upload("Backblaze B2", "legacy 日本語.txt", b"encrypted through S3; readable through B2")
    marker = (encrypted_root / ".oasis-storage-v1").read_bytes()
    converted = app.json("GET", "/sys/storage")
    converted["providers"][0].update(protocol="b2", endpoint=endpoint)
    app.json("PUT", "/sys/storage", converted)
    assert (encrypted_root / ".oasis-storage-v1").read_bytes() == marker
    assert app.file("Backblaze B2/legacy 日本語.txt")[0] == b"encrypted through S3; readable through B2"
    app.upload("", "copy me.txt", b"file for Safari copy and move")
    app.upload("", "move me.txt", b"move for Safari")
    app.api("POST", "/dir", {"parent": "Backblaze%20B2", "name": "Nested 日本語"})
    calls = [json.loads(line)["operation"] for line in (remote / "b2-api-calls.jsonl").read_text().splitlines()]
    assert "b2_upload_file" in calls and "b2_delete_file_version" in calls
    assert not {"b2_create_bucket", "b2_delete_bucket", "b2_update_bucket"} & set(calls)
    app.cleaned()
    print("PASS S3-to-native conversion, unchanged encrypted marker and files, nested directories, no bucket management calls", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rclone", type=Path, required=True)
    parser.add_argument("--oasis", type=Path, default=REPO / "backend/target/debug/oasis")
    parser.add_argument("--keep-alive", action="store_true")
    args = parser.parse_args()
    args.rclone = args.rclone.resolve()
    args.passphrase, args.test_python_path = True, None
    with tempfile.TemporaryDirectory(prefix="oasis-native-b2-tests-") as directory:
        app = Harness(Path(directory), args)
        try:
            run(app)
            if args.keep_alive:
                print("Safari: %s/settings (tester / storage123)" % app.base, flush=True)
                print("Fixture: %s" % directory, flush=True)
                while True:
                    time.sleep(1)
        except KeyboardInterrupt:
            pass
        finally:
            app.close()


if __name__ == "__main__":
    main()
