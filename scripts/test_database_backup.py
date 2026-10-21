#!/usr/bin/env python3
"""Opt-in database migration tests with disposable installations and WebDAV."""
import argparse
import copy
import http.cookiejar
import json
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

from test_external_storage import Harness, REPO, wait_port


def client(app):
    other = copy.copy(app)
    other.cookies = http.cookiejar.CookieJar()
    other.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(other.cookies))
    return other


def raw_upload(app, identifier, content, status=200):
    request = urllib.request.Request(app.base + "/api/sys/database/import/" + identifier,
                                     data=content, method="PUT", headers={"Content-Type": "application/octet-stream"})
    try:
        response = app.opener.open(request, timeout=120)
    except urllib.error.HTTPError as response:
        assert response.code == status, (response.code, response.read())
        response.close()
        return
    with response:
        assert response.status == status
        return json.load(response)


def prepare_import(app, archive, phrase, password, status=200):
    identifier = app.json("POST", "/sys/database/import/prepare", {"password": password, "phrase": phrase})["id"]
    return raw_upload(app, identifier, archive, status=status)


def export(app, password):
    prepared = app.json("POST", "/sys/database/export/prepare", {"password": password})
    words = prepared["phrase"].split()
    assert len(words) == 12 and len(set(prepared["positions"])) == 3
    answers = [words[i - 1] for i in prepared["positions"]]
    # Bad answers and passwords cannot release the backup, or consume a valid job.
    endpoint = "/sys/database/export/%s/download" % prepared["id"]
    app.api("POST", endpoint, {"password": "wrong-password", "answers": answers}, status=403)
    app.api("POST", endpoint, {"password": password, "answers": ["wrong"] * 3}, status=400)
    archive, headers = app.api("POST", endpoint, {"password": password, "answers": answers})
    assert headers["Cache-Control"] == "no-store" and "attachment" in headers["Content-Disposition"]
    assert archive.startswith(b"OASISDB1") and b"remote-test-password" not in archive
    app.api("POST", endpoint, {"password": password, "answers": answers}, status=400)
    app.cleaned()
    return archive, prepared["phrase"]


def run(source, target, root):
    source.start_oasis()
    target.start_oasis()
    target_password = "destination-password"
    target.api("PUT", "/user/password", {"username": "tester", "old_password": "storage123", "new_password": target_password})
    target.api("POST", "/login", {"username": "tester", "password": target_password})
    source.api("PUT", "/sys/config", {"sitename": "Backup source", "storage": urllib.parse.quote(str(source.local), safe=""), "language": "en", "update_freq": "monthly", "allow_guest": True})
    for name in ["copied.txt", "private.txt"]:
        source.upload("", name, ("already on disk: " + name).encode())
        shutil.copy2(source.local / name, target.local / name)
    source.api("PUT", "/file/private.txt/visibility", {"visible": False})
    target.upload("", "destination.txt", b"destination file must remain untouched")
    source_link = source.api("POST", "/file/share", {"path": "copied.txt", "expire": int(time.time()) + 600})[0].decode()
    target_link = target.api("POST", "/file/share", {"path": "destination.txt", "expire": int(time.time()) + 600})[0].decode()
    old_target_cookie = "; ".join(c.name + "=" + c.value for c in target.cookies)
    source_cookie = "; ".join(c.name + "=" + c.value for c in source.cookies)
    provider, _ = source.provider(source.args.storage_protocol)
    drive = provider["name"]
    source.json("PUT", "/sys/storage", {"enabled": True, "mode": "mount", "main_provider": provider["id"], "providers": [provider]})
    source.upload(provider["name"], "external.txt", b"encrypted provider survives migration")
    source_db = source.appdir / "data/db/main.db"
    target_db = target.appdir / "data/db/main.db"
    with sqlite3.connect(source_db) as db:
        hashed = db.execute("SELECT password FROM user LIMIT 1").fetchone()[0]
        db.execute("INSERT INTO user(user_id,username,password,permission,created_at) VALUES(2,'reader',?,1,1)", (hashed,))
        db.execute("INSERT INTO user(user_id,username,password,permission,created_at) VALUES(3,'other-admin',?,9,1)", (hashed,))
        db.execute("INSERT INTO reset VALUES('old-reset','code','tester',?)", (int(time.time()) + 600,))
    other_admin = client(source)
    other_admin.api("POST", "/login", {"username": "other-admin", "password": "storage123"})
    unprivileged = client(source)
    unprivileged.api("POST", "/login", {"username": "reader", "password": "storage123"})
    unprivileged.api("POST", "/sys/database/export/prepare", {"password": "storage123"}, status=401)
    source.api("POST", "/sys/database/export/prepare", {"password": "storage123"}, anonymous=True, status=401)
    source.api("POST", "/sys/database/export/prepare", {"password": "wrong"}, status=403)
    prepared = source.json("POST", "/sys/database/export/prepare", {"password": "storage123"})
    answers = [prepared["phrase"].split()[i - 1] for i in prepared["positions"]]
    other_admin.api("POST", "/sys/database/export/%s/download" % prepared["id"], {"password": "storage123", "answers": answers}, status=400)
    for _ in range(5):
        source.api("POST", "/sys/database/export/%s/download" % prepared["id"], {"password": "storage123", "answers": ["wrong"] * 3}, status=400)
    source.api("POST", "/sys/database/export/%s/download" % prepared["id"], {"password": "storage123", "answers": answers}, status=400)
    prepared = source.json("POST", "/sys/database/export/prepare", {"password": "storage123"})
    source.api("DELETE", "/sys/database/jobs/" + prepared["id"])
    source.api("POST", "/sys/database/export/%s/download" % prepared["id"], {"password": "storage123", "answers": answers}, status=400)
    archive, phrase = export(source, "storage123")
    print("PASS export authentication, word confirmation, ownership, cancellation, and encrypted download", flush=True)

    target.api("POST", "/sys/database/import/prepare", {"password": "wrong", "phrase": phrase}, status=403)
    target.api("POST", "/sys/database/import/prepare", {"password": target_password, "phrase": "only three words"}, status=400)
    wrong_phrase = source.json("POST", "/sys/database/export/prepare", {"password": "storage123"})["phrase"]
    for damaged, key in [(archive, wrong_phrase), (archive[:-1], phrase), (archive[:-1] + bytes([archive[-1] ^ 1]), phrase), (b"OASISDB2" + archive[8:], phrase)]:
        prepare_import(target, damaged, key, target_password, status=400)
        target.cleaned()
    oversized = target.json("POST", "/sys/database/import/prepare", {"password": target_password, "phrase": phrase})["id"]
    raw_upload(target, oversized, b"x" * (64 * 1024 * 1024 + 1), status=413)
    target.cleaned()
    print("PASS invalid passwords/phrases, corrupted/truncated/unsupported archives, size limit, and staging cleanup", flush=True)

    preview = prepare_import(target, archive, phrase.upper().replace(" ", "  \n"), target_password)
    assert preview["accounts"] == 3 and preview["providers"] == 1 and preview["visibility_rules"] == 1
    assert preview["site_name"] == "Backup source" and preview["local_storage"] == str(target.local)
    endpoint = "/sys/database/import/%s/restore" % preview["id"]
    payload = {"password": target_password, "local_storage": str(target.local), "confirm_replace": True}
    target.api("POST", endpoint, dict(payload, confirm_replace=False), status=400)
    target.api("POST", endpoint, dict(payload, password="wrong"), status=403)
    target.api("POST", endpoint, dict(payload, local_storage=str(root / "missing")), status=400)
    pending = target.api("POST", "/pre-upload", {"filename": "pending.txt", "target": "", "size": 1, "hash": "test"})[0].decode()
    target.api("POST", endpoint, payload, status=400)
    target.api("POST", "/cancel-upload", {"uuids": [pending]})
    with sqlite3.connect(target_db) as db:
        original = db.execute("SELECT secret FROM site").fetchone()[0]
        db.execute("CREATE TRIGGER reject_restore BEFORE INSERT ON hidden BEGIN SELECT RAISE(ABORT,'test rollback'); END")
    target.api("POST", endpoint, payload, status=400)
    with sqlite3.connect(target_db) as db:
        assert db.execute("SELECT secret FROM site").fetchone()[0] == original
        assert db.execute("SELECT count(*) FROM user").fetchone()[0] == 1
        db.execute("DROP TRIGGER reject_restore")
    print("PASS preview, password re-verification, destination validation, active-transfer rejection, and atomic rollback", flush=True)

    target.api("POST", endpoint, payload)
    target.api("GET", "/sys/storage", status=401)
    target.api("GET", "/sys/storage", headers={"Cookie": old_target_cookie}, anonymous=True, status=401)
    target.api("GET", "/sys/storage", headers={"Cookie": source_cookie}, anonymous=True, status=401)
    target.api("GET", "/file/share?" + source_link, anonymous=True, status=403)
    target.api("GET", "/file/share?" + target_link, anonymous=True, status=403)
    target.login()
    config = target.json("GET", "/sys/config?mode=full")
    assert config["name"] == "Backup source" and config["storage"] == str(target.local)
    assert target.file("copied.txt")[0] == b"already on disk: copied.txt"
    assert target.file("destination.txt")[0] == b"destination file must remain untouched"
    assert target.file(drive + "/external.txt")[0] == b"encrypted provider survives migration"
    saved = target.json("GET", "/sys/storage")["providers"][0]
    credential = "secret_access_key" if source.args.storage_protocol == "b2" else "password"
    assert saved[credential + "_set"] and saved["encryption_key_set"] and not saved[credential] and not saved["encryption_key"]
    assert saved["protocol"] == source.args.storage_protocol
    guest = client(target)
    guest.api("GET", "/login/guest")
    assert "private.txt" not in {entry["filename"] for entry in guest.listing()}
    with sqlite3.connect(target_db) as db:
        assert db.execute("SELECT count(*) FROM reset").fetchone()[0] == 0
        assert db.execute("SELECT secret FROM site").fetchone()[0] != original
    target.cleaned()
    target.app.terminate()
    target.app.wait(timeout=10)
    target.app = target.start([str(target.executable)], "target-restarted")
    wait_port(target.number, target.app, timeout=120)
    target.login()
    assert target.file(drive + "/external.txt")[0] == b"encrypted provider survives migration"
    target.cleaned()
    (root / "export.oasis-backup").write_bytes(archive)
    (root / "recovery-phrase.txt").write_text(phrase + "\n")
    print("PASS immediate migration, copied-file discovery, saved encrypted storage, visibility, session/link invalidation, and restart", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rclone", type=Path, required=True)
    parser.add_argument("--oasis", type=Path, default=REPO / "backend/target/debug/oasis")
    parser.add_argument("--storage-protocol", choices=["webdav", "b2"], default="webdav")
    parser.add_argument("--passphrase", action="store_true", help="Migrate passphrase-encrypted storage")
    parser.add_argument("--keep-alive", action="store_true")
    parser.add_argument("--test-python-path", type=Path)
    args = parser.parse_args()
    args.rclone = args.rclone.resolve()
    with tempfile.TemporaryDirectory(prefix="oasis-backup-tests-") as directory:
        root = Path(directory)
        (root / "source").mkdir(); (root / "target").mkdir()
        source, target = Harness(root / "source", args), Harness(root / "target", args)
        try:
            run(source, target, root)
            if args.keep_alive:
                print("Safari source: %s/settings; target: %s/settings (tester / storage123)" % (source.base, target.base), flush=True)
                print("Disposable backup and recovery phrase: %s" % root, flush=True)
                while True:
                    time.sleep(1)
        except KeyboardInterrupt:
            pass
        finally:
            target.close(); source.close()


if __name__ == "__main__":
    main()
