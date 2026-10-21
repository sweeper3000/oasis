"""Disposable integration-test adapter: logs calls and injects metadata failures.

Executed by the harness wrapper, never used by the production storage adapter.
"""
import json
import os
from pathlib import Path
import subprocess
import sys

root = Path(os.environ["OASIS_TEST_ADAPTER_ROOT"])
args = sys.argv[1:]
with (root / "adapter-calls.jsonl").open("a") as log:
    log.write(json.dumps(args) + "\n")

if args and args[0] == "cat":
    with (root / "adapter-cat-pids.jsonl").open("a") as log:
        log.write(json.dumps({"pid": os.getpid(), "args": args}) + "\n")
    # Preserve streaming and cancellation instead of buffering video in Python.
    os.execvpe(os.environ["OASIS_TEST_RCLONE"], [os.environ["OASIS_TEST_RCLONE"]] + args, os.environ)

result = subprocess.run(
    [os.environ["OASIS_TEST_RCLONE"]] + args,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
)
output, code = result.stdout, result.returncode
fault_file = root / "verification-fault.json"
if code == 0 and args[0] == "lsjson" and "--stat" in args and fault_file.exists():
    fault = json.loads(fault_file.read_text())
    marker = args[1].rsplit("/", 1)[-1] in (".oasis-dir", ".oasis-storage-v1")
    if not marker or fault.get("include_markers"):
        fault["count"] = fault.get("count", 0) + 1
        fault_file.write_text(json.dumps(fault))
        if fault["count"] == fault.get("fail_on", 1):
            mode = fault["mode"]
            if mode in ("missing", "request-error"):
                code, output = (3 if mode == "missing" else 9), b""
            elif mode == "malformed":
                output = b"not JSON"
            else:
                entry = json.loads(output)
                if mode == "size":
                    entry["Size"] += 1
                elif mode == "unknown-size":
                    entry["Size"] = -1
                elif mode == "directory":
                    entry["IsDir"] = True
                else:
                    raise RuntimeError("Unknown metadata fault: " + mode)
                output = json.dumps(entry).encode()

if result.returncode:
    with (root / "adapter-errors.log").open("ab") as log:
        log.write((" ".join(args) + "\n").encode() + result.stderr)
sys.stdout.buffer.write(output)
sys.stderr.buffer.write(result.stderr)
sys.exit(code)
