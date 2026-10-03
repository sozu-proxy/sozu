#!/usr/bin/env python3
"""Hold one proxied TCP session open until the harness releases it."""

import json
import os
import socket
import sys
import time
from pathlib import Path


port = int(sys.argv[1])
runtime = Path(sys.argv[2])
opened = runtime / "client-session-open.json"
release = runtime / "release-client"
result = runtime / "client-result.json"


def exchange(stream: socket.SocketIO, message: bytes) -> bytes:
    stream.write(message + b"\n")
    answer = stream.readline()
    expected = b"ACK " + message + b"\n"
    if answer != expected:
        raise RuntimeError(f"expected {expected!r}, received {answer!r}")
    return answer


started = time.time()
record = {"pid": os.getpid()}
exit_code = 0
try:
    with socket.create_connection(("127.0.0.1", port), timeout=10) as connection:
        connection.settimeout(10)
        with connection.makefile("rwb", buffering=0) as stream:
            first = exchange(stream, b"OPEN actual-version-upgrade")
            record["first_ack"] = first.decode().rstrip()
            opened.write_text(
                json.dumps(
                    {
                        "pid": os.getpid(),
                        "opened_at": time.time(),
                        "first_ack": record["first_ack"],
                    },
                    sort_keys=True,
                )
                + "\n",
                encoding="utf-8",
            )
            deadline = time.monotonic() + 60
            heartbeat = 0
            while not release.exists():
                if time.monotonic() >= deadline:
                    raise TimeoutError("the harness did not release the long-lived client")
                heartbeat += 1
                exchange(stream, f"HEARTBEAT {heartbeat}".encode())
                time.sleep(0.05)
            record["heartbeats"] = heartbeat
            second = exchange(stream, b"AFTER main-and-worker-upgrade")
            third = exchange(stream, b"CLOSE")
            record["second_ack"] = second.decode().rstrip()
            record["close_ack"] = third.decode().rstrip()
except Exception as error:  # the result file records the observed lifecycle boundary
    record["error"] = f"{type(error).__name__}: {error}"
    exit_code = 1

record["elapsed_seconds"] = round(time.time() - started, 3)
record["exit_code"] = exit_code
result.write_text(json.dumps(record, sort_keys=True) + "\n", encoding="utf-8")
raise SystemExit(exit_code)
