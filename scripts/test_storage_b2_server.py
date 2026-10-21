#!/usr/bin/env python3
"""Disposable native B2 API fixture for real-rclone integration tests, not production."""
import base64
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import sys
import threading
import time
import urllib.parse
import uuid


class B2Server(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, root, port):
        super().__init__(("127.0.0.1", port), Handler)
        self.root = Path(root)
        self.bucket = self.root / "test-bucket"
        self.bucket.mkdir(exist_ok=True)
        self.base = "http://127.0.0.1:%s" % self.server_port
        self.versions = {}
        self.lock = threading.RLock()

    def refresh(self):
        # Import objects written through the fixture's companion S3 interface.
        for path in self.bucket.rglob("*"):
            if path.is_file():
                name, data = path.relative_to(self.bucket).as_posix(), path.read_bytes()
                versions = self.versions.setdefault(name, [])
                if not versions or versions[-1]["data"] != data:
                    self.add(name, data, {})

    def add(self, name, data, info, content_type="application/octet-stream"):
        entry = dict(fileId=uuid.uuid4().hex, fileName=name, action="upload",
                     accountId="test-account", bucketId="test-bucket-id",
                     size=len(data), contentLength=len(data), contentSha1=hashlib.sha1(data).hexdigest(),
                     contentType=content_type, fileInfo=info, uploadTimestamp=int(time.time() * 1000), data=data)
        self.versions.setdefault(name, []).append(entry)
        path = self.bucket / name
        if not path.resolve().is_relative_to(self.bucket.resolve()):
            raise ValueError("Invalid fixture path")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return entry


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def reply(self, value, status=200):
        data = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    def error(self, status, code):
        self.reply(dict(status=status, code=code, message="Fixture rejected request"), status)

    def public(self, entry):
        return {k: v for k, v in entry.items() if k != "data"}

    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        self.handle_request()

    def do_POST(self):
        self.handle_request()

    def handle_request(self):
        with self.server.lock:
            try:
                self.dispatch()
            except (BrokenPipeError, ConnectionResetError):
                pass

    def dispatch(self):
        server = self.server
        parsed = urllib.parse.urlsplit(self.path)
        operation = parsed.path.rsplit("/", 1)[-1]
        with (server.root / "b2-api-calls.jsonl").open("a") as log:
            log.write(json.dumps({"method": self.command, "operation": operation}) + "\n")
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if operation == "b2_authorize_account":
            expected = "Basic " + base64.b64encode(b"test-access-key:test-secret-key").decode()
            if self.headers.get("Authorization") != expected:
                return self.error(401, "unauthorized")
            return self.reply(dict(accountId="test-account", apiUrl=server.base, downloadUrl=server.base,
                                   authorizationToken="test-token", absoluteMinimumPartSize=5000000,
                                   minimumPartSize=100000000, recommendedPartSize=100000000,
                                   allowed=dict(bucketId="test-bucket-id", bucketName="test-bucket",
                                                namePrefix=None, capabilities=["listBuckets", "listFiles", "readFiles", "writeFiles", "deleteFiles"])))
        if self.headers.get("Authorization") not in ("test-token", "upload-token"):
            return self.error(401, "bad_auth_token")
        server.refresh()
        if operation == "b2_upload_file":
            fault = server.root / "b2-fault.json"
            if fault.exists() and json.loads(fault.read_text()).get("deny_upload"):
                return self.error(403, "unauthorized")
            name = urllib.parse.unquote(self.headers["X-Bz-File-Name"])
            checksum = self.headers["X-Bz-Content-Sha1"]
            if checksum == "hex_digits_at_end":
                raw, checksum = raw[:-40], raw[-40:].decode()
            if hashlib.sha1(raw).hexdigest() != checksum:
                return self.error(400, "bad_digest")
            info = {k.lower()[10:]: urllib.parse.unquote(v) for k, v in self.headers.items() if k.lower().startswith("x-bz-info-")}
            entry = server.add(name, raw, info, self.headers.get("Content-Type"))
            return self.reply(self.public(entry))
        if operation == "b2_download_file_by_id" or parsed.path.startswith("/file/"):
            query = urllib.parse.parse_qs(parsed.query)
            if operation == "b2_download_file_by_id":
                entry = next((v for versions in server.versions.values() for v in versions if v["fileId"] == query.get("fileId", [""])[0]), None)
            else:
                name = urllib.parse.unquote(parsed.path.split("/", 3)[-1])
                versions = server.versions.get(name, [])
                entry = versions[-1] if versions and versions[-1]["action"] != "hide" else None
            if not entry:
                return self.error(404, "file_not_present")
            data, start, end = entry["data"], 0, entry["size"] - 1
            range_header = self.headers.get("Range")
            if range_header:
                left, right = range_header.removeprefix("bytes=").split("-")
                start = int(left) if left else max(0, len(data) - int(right))
                end = min(end, int(right)) if right and left else end
            body = data[start:end + 1]
            self.send_response(206 if range_header else 200)
            for key, value in {"Content-Length": len(body), "Content-Type": entry["contentType"],
                               "X-Bz-File-Id": entry["fileId"], "X-Bz-File-Name": urllib.parse.quote(entry["fileName"]),
                               "X-Bz-Content-Sha1": entry["contentSha1"], "X-Bz-Upload-Timestamp": entry["uploadTimestamp"],
                               "Accept-Ranges": "bytes"}.items():
                self.send_header(key, str(value))
            if range_header:
                self.send_header("Content-Range", "bytes %s-%s/%s" % (start, end, len(data)))
            for key, value in entry["fileInfo"].items():
                self.send_header("X-Bz-Info-" + key, urllib.parse.quote(value))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)
            return
        body = json.loads(raw or b"{}")
        if operation == "b2_list_buckets":
            return self.reply(dict(buckets=[dict(bucketId="test-bucket-id", bucketName="test-bucket", bucketType="allPrivate")]))
        if operation in ("b2_list_file_names", "b2_list_file_versions"):
            entries, folders = [], set()
            prefix, delimiter = body.get("prefix", ""), body.get("delimiter", "")
            for name, versions in sorted(server.versions.items()):
                if not versions or not name.startswith(prefix) or name < body.get("startFileName", "") or (operation.endswith("names") and versions[-1]["action"] == "hide"):
                    continue
                rest = name[len(prefix):]
                if delimiter and delimiter in rest:
                    folder = prefix + rest.split(delimiter)[0] + delimiter
                    if folder not in folders:
                        entries.append(dict(fileName=folder, action="folder", size=0, uploadTimestamp=0))
                        folders.add(folder)
                else:
                    entries.extend(self.public(v) for v in (reversed(versions) if operation.endswith("versions") else [versions[-1]]))
            return self.reply(dict(files=entries, nextFileName=None, nextFileId=None))
        if operation == "b2_get_upload_url":
            return self.reply(dict(bucketId="test-bucket-id", uploadUrl=server.base + "/b2api/v1/b2_upload_file", authorizationToken="upload-token"))
        if operation == "b2_get_file_info":
            entry = next((v for versions in server.versions.values() for v in versions if v["fileId"] == body["fileId"]), None)
            return self.reply(self.public(entry)) if entry else self.error(404, "file_not_present")
        if operation == "b2_delete_file_version":
            versions = server.versions.get(body["fileName"], [])
            versions[:] = [v for v in versions if v["fileId"] != body["fileId"]]
            path = server.bucket / body["fileName"]
            if versions and versions[-1]["action"] != "hide":
                path.write_bytes(versions[-1]["data"])
            else:
                path.unlink(missing_ok=True)
            return self.reply(dict(fileId=body["fileId"], fileName=body["fileName"]))
        if operation == "b2_hide_file":
            name = body["fileName"]
            entry = server.add(name, b"", {})
            entry["action"] = "hide"
            (server.bucket / name).unlink()
            return self.reply(self.public(entry))
        # Reject management calls; Oasis must not need bucket creation or edits.
        return self.error(403, "unauthorized")


if __name__ == "__main__":
    B2Server(sys.argv[1], int(sys.argv[2])).serve_forever()
