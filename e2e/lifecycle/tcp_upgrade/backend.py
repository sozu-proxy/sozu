#!/usr/bin/env python3
"""Line-oriented TCP backend with explicit accept/session markers."""

import json
import os
import socket
import sys
import threading
import time
from pathlib import Path


runtime = Path(sys.argv[1])
runtime.mkdir(parents=True, exist_ok=True)
accepted = runtime / "backend-accepted.log"
ready = runtime / "backend-ready.json"


def record(message: str) -> None:
    with accepted.open("a", encoding="utf-8") as output:
        output.write(f"{time.time():.6f} {message}\n")
        output.flush()


def serve(connection: socket.socket, peer: tuple[str, int]) -> None:
    record(f"accepted peer={peer[0]}:{peer[1]}")
    with connection, connection.makefile("rwb", buffering=0) as stream:
        for raw_line in stream:
            line = raw_line.rstrip(b"\r\n")
            record(f"line={line.decode('utf-8', errors='replace')}")
            stream.write(b"ACK " + line + b"\n")
            if line == b"CLOSE":
                return


with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 0))
    listener.listen()
    port = listener.getsockname()[1]
    ready.write_text(
        json.dumps({"pid": os.getpid(), "address": f"127.0.0.1:{port}", "port": port})
        + "\n",
        encoding="utf-8",
    )
    record(f"ready port={port}")
    while True:
        connection, peer = listener.accept()
        threading.Thread(target=serve, args=(connection, peer), daemon=True).start()
