#!/usr/bin/env python3
"""Opt-in integration tests using disposable local providers; no cloud credentials.

Build backend and frontend first. Requires rclone 1.71+, openssl and ssh-keygen.
Pass --smb to also start a loopback-only Samba Docker container.
"""
import argparse
import copy
import hashlib
import http.cookiejar
import io
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile

REPO = Path(__file__).resolve().parents[1]
SLICE = 2 * 1024 * 1024


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def wait_port(number, process=None):
    for _ in range(100):
        if process and process.poll() is not None:
            raise RuntimeError("Test server exited before accepting connections")
        try:
            with socket.create_connection(("127.0.0.1", number), timeout=0.2):
                return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("Test server did not start")


class Harness:
    def __init__(self, root, args):
        self.root, self.args = root, args
        self.processes, self.containers, self.logs = [], [], []
        self.local = root / "local"
        self.local.mkdir()
        self.appdir = root / "app"
        self.appdir.mkdir()
        self.executable = self.appdir / ("oasis.exe" if os.name == "nt" else "oasis")
        shutil.copy2(args.oasis, self.executable)
        self.number = port()
        self.base = "http://127.0.0.1:%s" % self.number
        (self.appdir / "oasis.conf").write_text("ip=127.0.0.1\nport=%s\n" % self.number)
        self.env = dict(os.environ, OASIS_RCLONE=str(args.rclone))
        self.env.pop("OASIS_STORAGE_KEY", None)
        # Only disposable test connections use this wrapper. Production logs never
        # expose raw provider errors; capture them here to diagnose test failures.
        wrapper = root / "rclone-test-adapter"
        wrapper.write_text("#!%s\nimport runpy\nrunpy.run_path(%r, run_name='__main__')\n" % (sys.executable, str(REPO / "scripts/test_storage_adapter.py")))
        wrapper.chmod(0o700)
        self.env["OASIS_RCLONE"] = str(wrapper)
        self.env["OASIS_TEST_ADAPTER_ROOT"] = str(root)
        self.env["OASIS_TEST_RCLONE"] = str(args.rclone)
        if args.test_python_path:
            self.env["PYTHONPATH"] = str(args.test_python_path)
        self.cookies = http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.cookies))
        self.make_certificate()

    def make_certificate(self):
        self.cert, self.key = self.root / "tls.crt", self.root / "tls.key"
        conf = self.root / "tls.cnf"
        conf.write_text("[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN=localhost\n[ext]\nsubjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,digitalSignature,keyEncipherment,keyCertSign\nextendedKeyUsage=serverAuth\n")
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-keyout", str(self.key), "-out", str(self.cert), "-config", str(conf)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.env["OASIS_STORAGE_CA_CERT"] = str(self.cert)

    def start(self, command, name, env=None):
        logfile = self.root / (name + ".log")
        handle = logfile.open("wb")
        self.logs.append(handle)
        process = subprocess.Popen(command, cwd=REPO / "backend", env=env or self.env, stdout=handle, stderr=subprocess.STDOUT)
        self.processes.append(process)
        return process

    def start_oasis(self):
        self.app = self.start([str(self.executable)], "oasis")
        wait_port(self.number, self.app)
        self.api("POST", "/sys/setup", {"sitename": "Storage test", "username": "tester", "password": "storage123", "language": "en", "storage": urllib.parse.quote(str(self.local), safe="")})
        self.login()

    def login(self):
        self.api("POST", "/login", {"username": "tester", "password": "storage123"})

    def api(self, method, endpoint, body=None, headers=None, status=200, anonymous=False):
        data = json.dumps(body).encode() if body is not None else (b"" if method in ("POST", "PUT") else None)
        actual_headers = {"Content-Type": "application/json"}
        actual_headers.update(headers or {})
        request = urllib.request.Request(self.base + "/api" + endpoint, data=data, headers=actual_headers, method=method)
        opener = urllib.request.build_opener() if anonymous else self.opener
        try:
            response = opener.open(request, timeout=120)
        except urllib.error.HTTPError as response:
            payload = response.read()
            if response.code != status:
                raise AssertionError("%s %s: expected %s, got %s: %s" % (method, endpoint, status, response.code, payload.decode(errors="replace")))
            return payload, response.headers
        with response:
            payload = response.read()
            assert response.status == status, (endpoint, response.status, status)
            return payload, response.headers

    def json(self, method, endpoint, body=None, **kwargs):
        data, _ = self.api(method, endpoint, body, **kwargs)
        return json.loads(data)

    def provider(self, protocol):
        remote = self.root / protocol
        remote.mkdir()
        (remote / "encrypted").mkdir()
        (remote / "plain").mkdir()
        number = port()
        provider = {"id": protocol, "name": protocol.upper(), "protocol": protocol, "root": "encrypted", "host": "127.0.0.1", "port": number, "username": "tester", "password": "remote-test-password", "endpoint": "", "encryption": True, "encryption_key": hashlib.sha256(protocol.encode()).hexdigest()}
        server_type = "ftp" if protocol == "ftps" else protocol
        flags = ["--addr", "127.0.0.1:%s" % number, "--dir-cache-time", "0s", "--user", "tester", "--pass", "remote-test-password", "--cache-dir", str(self.root / "rclone-cache")]
        if protocol == "webdav":
            provider["endpoint"] = "http://127.0.0.1:%s/" % number
        elif protocol in ("ftp", "ftps"):
            flags.extend(["--passive-port", "30000-32000", "--public-ip", "127.0.0.1"])
            if protocol == "ftps":
                process = self.start([sys.executable, str(REPO / "scripts/test_storage_ftps_server.py"), str(remote), str(number), str(self.cert), str(self.key)], protocol)
                wait_port(number, process)
                return provider, remote / "encrypted"
        elif protocol == "sftp":
            hostkey = self.root / "ssh_host_key"
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(hostkey)], check=True)
            pubkey = Path(str(hostkey) + ".pub").read_text().split()[:2]
            known_hosts = self.root / "known_hosts"
            known_hosts.write_text("[127.0.0.1]:%s %s\n" % (number, " ".join(pubkey)))
            flags.extend(["--key", str(hostkey)])
            provider["known_hosts_file"] = str(known_hosts)
            provider["root"] = "/encrypted"
        elif protocol == "s3":
            # S3 uses SigV4, not the HTTP server's separate Basic Auth layer.
            flags = ["--addr", "127.0.0.1:%s" % number, "--dir-cache-time", "0s", "--cache-dir", str(self.root / "rclone-cache")]
            flags.extend(["--auth-key", "test-access-key,test-secret-key"])
            provider.update(endpoint="http://127.0.0.1:%s" % number, root="test-bucket/encrypted", region="us-east-1", access_key_id="test-access-key", secret_access_key="test-secret-key", username="", password="")
            (remote / "test-bucket" / "encrypted").mkdir(parents=True)
            (remote / "test-bucket" / "plain").mkdir()
        elif protocol == "smb":
            name = "oasis-storage-test-%s" % os.getpid()
            command = ["docker", "run", "--rm", "--detach", "--name", name, "-p", "127.0.0.1:%s:445" % number, "-e", "NAME=Shared", "-e", "USER=tester", "-e", "PASS=remote-test-password", "-e", "UID=%s" % os.getuid(), "-e", "GID=%s" % os.getgid(), "--tmpfs", "/shared:rw,size=128m", "dockurr/samba:4"]
            subprocess.run(command, check=True, stdout=subprocess.DEVNULL)
            self.containers.append(name)
            wait_port(number)
            time.sleep(2)  # Docker's published port can open before smbd is ready.
            subprocess.run(["docker", "exec", "--user", "tester", name, "mkdir", "/shared/encrypted", "/shared/plain"], check=True)
            provider["root"] = "Shared/encrypted"
            return provider, remote / "encrypted"
        process = self.start([str(self.args.rclone), "serve", server_type, str(remote)] + flags, protocol)
        wait_port(number, process)
        return provider, remote / "test-bucket" / "encrypted" if protocol == "s3" else remote / "encrypted"

    def upload(self, directory, name, content, status=200):
        uuid = self.api("POST", "/pre-upload", {"filename": name, "target": urllib.parse.quote(directory, safe=""), "size": len(content), "hash": hashlib.md5(content).hexdigest()})[0].decode()
        for index, start in enumerate(range(0, len(content), SLICE), 1):
            request = urllib.request.Request(self.base + "/api/upload/%s/%s" % (uuid, index), data=content[start:start + SLICE], method="POST", headers={"Content-Type": "application/octet-stream"})
            with self.opener.open(request, timeout=120) as response:
                assert response.status == 200
        self.api("POST", "/finish-upload/" + uuid, status=status)
        return uuid

    def listing(self, path=""):
        return self.json("GET", "/dir?path=" + urllib.parse.quote(path, safe=""))

    def file(self, path, **kwargs):
        return self.api("GET", "/file/" + urllib.parse.quote(path, safe=""), **kwargs)

    def transfer(self, source, target, is_copy=True, overwrite=False, expected="Success"):
        uuid = self.api("POST", "/file/copy-move", {"source": urllib.parse.quote(source, safe=""), "target": urllib.parse.quote(target, safe=""), "is_copy": is_copy, "overwrite": overwrite})[0].decode()
        for _ in range(600):
            task = self.json("GET", "/file/copy-move-status/" + uuid)
            if task["status"] in ("Success", "Failed"):
                assert task["status"] == expected, task
                return
            time.sleep(0.05)
        raise AssertionError("Copy/move did not finish")

    def cleaned(self):
        for _ in range(50):
            if not list((self.appdir / "temp").iterdir()):
                return
            time.sleep(0.05)
        raise AssertionError("Temporary files remain: %s" % list((self.appdir / "temp").iterdir()))

    def adapter_calls(self):
        path = self.root / "adapter-calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def verification_failures(self, drive, complete_matrix=False):
        fault_file = self.root / "verification-fault.json"
        content = b"retain the source until the destination is verified"
        modes = ["size", "missing", "directory", "unknown-size", "malformed", "request-error"] if complete_matrix else ["size"]
        try:
            for mode in modes:
                fault_file.write_text(json.dumps({"mode": mode}))
                self.upload(drive, "failed-upload.bin", content, status=500)
                assert json.loads(fault_file.read_text())["count"] == 1
                self.cleaned()
                fault_file.unlink()
                # A verification failure must not automatically delete the destination.
                assert self.file(drive + "/failed-upload.bin")[0] == content
                self.api("DELETE", "/file/" + urllib.parse.quote(drive + "/failed-upload.bin", safe=""))
            source = self.local / "move-source.bin"
            source.write_bytes(content)
            fault_file.write_text(json.dumps({"mode": "size"}))
            self.transfer(source.name, drive, is_copy=False, expected="Failed")
            assert source.read_bytes() == content
            self.cleaned()
            fault_file.unlink()
            self.api("DELETE", "/file/" + urllib.parse.quote(drive + "/" + source.name, safe=""))
            source.unlink()

            # Verify that a later failed file retains the entire source folder,
            # including files whose uploads have already passed verification.
            source = self.local / "move-folder"
            source.mkdir()
            (source / "one.bin").write_bytes(content)
            (source / "two.bin").write_bytes(content)
            fault_file.write_text(json.dumps({"mode": "size", "fail_on": 2}))
            self.transfer(source.name, drive, is_copy=False, expected="Failed")
            assert (source / "one.bin").read_bytes() == content
            assert (source / "two.bin").read_bytes() == content
            assert json.loads(fault_file.read_text())["count"] == 2
            self.cleaned()
            fault_file.unlink()
            self.api("DELETE", "/file/" + urllib.parse.quote(drive + "/" + source.name, safe=""))
            shutil.rmtree(source)

            if complete_matrix:
                fault_file.write_text(json.dumps({"mode": "size", "include_markers": True}))
                self.api("POST", "/dir", {"parent": urllib.parse.quote(drive, safe=""), "name": "marker-failure"}, status=500)
                assert json.loads(fault_file.read_text())["count"] == 1
                self.cleaned()
                fault_file.unlink()
                self.api("DELETE", "/file/" + urllib.parse.quote(drive + "/marker-failure", safe=""))
        finally:
            fault_file.unlink(missing_ok=True)
        print("PASS %s: verification failures reject uploads and retain file/folder move sources" % drive, flush=True)

    def verified_without_download(self, calls):
        uploads = [call for call in calls if call[0] == "copyto" and call[2].startswith("oasis:")]
        assert uploads, "No upload was recorded"
        for upload in uploads:
            index = calls.index(upload)
            following = calls[index + 1]
            assert following[:3] == ["lsjson", upload[2], "--stat"], following
        assert not any(call[0] == "copyto" and call[1].startswith("oasis:") for call in calls), "Verification downloaded file data"

    def run(self):
        self.start_oasis()
        self.api("GET", "/sys/storage", status=401, anonymous=True)
        providers, roots = [], {}
        protocols = self.args.protocols.split(",") + (["smb"] if self.args.smb else [])
        for protocol in protocols:
            provider, root = self.provider(protocol)
            print("Testing connection: %s" % protocol, flush=True)
            self.api("POST", "/sys/storage/test", {"enabled": True, "mode": "mount", "main_provider": provider["id"], "providers": [provider]})
            providers.append(provider)
            roots[protocol] = root
        config = {"enabled": True, "mode": "mount", "main_provider": providers[0]["id"], "providers": providers}
        self.api("POST", "/sys/storage/test", config)
        config = self.json("PUT", "/sys/storage", config)
        assert all(not p["encryption_key"] and not p["password"] and not p["secret_access_key"] for p in config["providers"])
        assert set(p["name"] for p in providers) <= set(e["filename"] for e in self.listing())
        contents = b"oasis-secret-content" * 180000  # multiple upload and AEAD chunks
        for provider in providers:
            drive = provider["name"]
            folder = drive + "/Folder"
            self.api("POST", "/dir", {"parent": urllib.parse.quote(drive), "name": "Folder"})
            filename = "report %20.bin"
            before = len(self.adapter_calls())
            self.upload(folder, filename, contents)
            self.verified_without_download(self.adapter_calls()[before:])
            path = folder + "/" + filename
            if provider["protocol"] == "smb":
                names = subprocess.check_output(["docker", "exec", self.containers[0], "find", "/shared/encrypted", "-type", "f"]).decode().splitlines()
                encrypted_files = [name for name in names if Path(name).name.startswith("o1-")]
                raw = subprocess.check_output(["docker", "exec", self.containers[0], "cat", encrypted_files[0]])
            else:
                encrypted_files = [p for p in roots[provider["protocol"]].rglob("*") if p.is_file() and p.name.startswith("o1-")]
                raw = encrypted_files[0].read_bytes()
            assert raw.startswith(b"OASISA01") and b"oasis-secret-content" not in raw
            assert all("report" not in str(p) for p in encrypted_files)
            assert self.file(path)[0] == contents
            assert self.file(path, headers={"Range": "bytes=0-0"}, status=206)[0] == contents[:1]
            assert self.file(path, headers={"Range": "bytes=-17"}, status=206)[0] == contents[-17:]
            assert self.file(path, headers={"Range": "bytes=10-"}, status=206)[0] == contents[10:]
            assert self.file(path, headers={"Range": "bytes=%s-" % len(contents)}, status=416)[0] == b""
            assert next(e for e in self.listing(folder) if e["filename"] == filename)["size"] == len(contents)
            link = self.api("POST", "/file/share", {"path": urllib.parse.quote(path, safe=""), "expire": int(time.time()) + 600})[0].decode()
            assert self.api("GET", "/file/share?" + link, anonymous=True)[0] == contents
            self.upload(folder, "empty.bin", b"")
            assert self.file(folder + "/empty.bin")[0] == b""
            self.upload(folder, "track.srt", b"1\n00:00:01,000 --> 00:00:02,000\nHello\n")
            assert b"WEBVTT" in self.api("GET", "/file/track/" + urllib.parse.quote(folder + "/track.vtt", safe=""))[0]
            archive = self.api("GET", "/download/dir?path=" + urllib.parse.quote(folder, safe=""))[0]
            with zipfile.ZipFile(io.BytesIO(archive)) as zipped:
                assert zipped.read("Folder/" + filename) == contents
            self.api("PUT", "/file/" + urllib.parse.quote(path, safe="") + "/name", {"new_name": "renamed.bin"})
            assert self.file(folder + "/renamed.bin")[0] == contents
            self.transfer(folder + "/renamed.bin", "")
            assert (self.local / "renamed.bin").read_bytes() == contents
            self.transfer("renamed.bin", folder, is_copy=False, overwrite=True)
            assert not (self.local / "renamed.bin").exists()
            assert self.file(folder + "/renamed.bin")[0] == contents
            self.api("DELETE", "/file/" + urllib.parse.quote(folder, safe=""))
            assert not self.listing(drive)
            assert not any("report" in p.name or "renamed" in p.name for p in roots[provider["protocol"]].rglob("*"))
            self.cleaned()
            print("PASS encrypted %s: upload/list/download/ranges/share/subtitles/ZIP/rename/cross-drive copy+move/delete/cleanup" % provider["protocol"], flush=True)
            self.verification_failures(drive, complete_matrix=provider is providers[0])
        # Wrong key and disabling encryption must not expose or overwrite existing storage.
        changed = copy.deepcopy(config)
        changed["providers"][0]["encryption_key"] = "18" * 32
        self.api("PUT", "/sys/storage", changed, status=400)
        changed = copy.deepcopy(config)
        changed["providers"][0]["encryption"] = False
        self.api("PUT", "/sys/storage", changed, status=400)
        # Unencrypted storage must also reflect edits made directly by another client.
        encrypted_config = copy.deepcopy(config)
        for original in providers:
            plain_config = copy.deepcopy(encrypted_config)
            plain_config["mode"] = "main"
            plain_config["main_provider"] = original["id"]
            plain_provider = next(p for p in plain_config["providers"] if p["id"] == original["id"])
            plain_provider["root"] = original["root"].replace("encrypted", "plain")
            plain_provider["encryption"] = False
            self.json("PUT", "/sys/storage", plain_config)
            before = len(self.adapter_calls())
            self.upload("", "direct.bin", b"ordinary provider data")
            self.verified_without_download(self.adapter_calls()[before:])
            self.upload("", "empty.bin", b"")
            assert self.file("empty.bin")[0] == b""
            self.api("DELETE", "/file/empty.bin")
            assert self.file("direct.bin")[0] == b"ordinary provider data"
            if original["protocol"] == "smb":
                subprocess.run(["docker", "exec", "-i", self.containers[0], "tee", "/shared/plain/direct.bin"], input=b"provider-side edit", check=True, stdout=subprocess.DEVNULL)
            else:
                (roots[original["protocol"]].parent / "plain" / "direct.bin").write_bytes(b"provider-side edit")
            assert self.file("direct.bin")[0] == b"provider-side edit"
            self.api("DELETE", "/file/direct.bin")
            self.cleaned()
            print("PASS unencrypted %s and provider-side edits" % original["protocol"], flush=True)
        config = encrypted_config
        # Main mode must work without touching an absent local storage directory.
        self.local.rmdir()
        config["mode"] = "main"
        config = self.json("PUT", "/sys/storage", config)
        self.upload("", "main.bin", b"main provider only")
        assert self.file("main.bin")[0] == b"main provider only"
        assert not self.local.exists()
        self.api("DELETE", "/file/main.bin")
        # Settings and credentials must survive a process restart without exposure.
        self.app.terminate()
        self.app.wait(timeout=10)
        self.app = self.start([str(self.executable)], "oasis-restarted")
        wait_port(self.number, self.app)
        self.login()
        assert self.json("GET", "/sys/storage")["enabled"]
        self.upload("", "restart.bin", b"survived restart")
        assert self.file("restart.bin")[0] == b"survived restart"
        self.api("DELETE", "/file/restart.bin")
        self.cleaned()
        with sqlite3.connect(self.appdir / "data" / "db" / "main.db") as db:
            stored = db.execute("SELECT sealed_config FROM storage_config").fetchone()[0]
            assert "remote-test-password" not in stored and "17171717" not in stored
        print("PASS main-drive mode, key-change guards, settings persistence and sealed credentials", flush=True)
        if self.args.keep_alive:
            print("UI test server: %s (tester / storage123)" % self.base, flush=True)
            while True:
                time.sleep(1)

    def close(self):
        for process in reversed(self.processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        for name in self.containers:
            subprocess.run(["docker", "stop", "-t", "1", name], stdout=subprocess.DEVNULL, check=False)
        for handle in self.logs:
            handle.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rclone", type=Path, default=Path(shutil.which("rclone") or "rclone"))
    parser.add_argument("--oasis", type=Path, default=REPO / "backend/target/debug/oasis")
    parser.add_argument("--protocols", default="webdav,ftp,ftps,sftp,s3")
    parser.add_argument("--test-python-path", type=Path, help="Optional directory containing pyftpdlib and pyOpenSSL")
    parser.add_argument("--smb", action="store_true")
    parser.add_argument("--keep-alive", action="store_true")
    args = parser.parse_args()
    args.rclone = args.rclone.resolve()
    with tempfile.TemporaryDirectory(prefix="oasis-storage-tests-") as directory:
        harness = Harness(Path(directory), args)
        try:
            harness.run()
        except KeyboardInterrupt:
            pass
        except Exception:
            print("\nProvider/server log tails:")
            for name in harness.containers:
                subprocess.run(["docker", "logs", "--tail", "30", name], check=False)
            for log in Path(directory).glob("*.log"):
                print(log.name, log.read_text(errors="replace")[-2500:])
            raise
        finally:
            harness.close()


if __name__ == "__main__":
    main()
