#!/usr/bin/env python3
"""Open a new TCP session and verify one backend round trip."""

import socket
import sys


port = int(sys.argv[1])
message = sys.argv[2].encode()
with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
    connection.settimeout(5)
    with connection.makefile("rwb", buffering=0) as stream:
        stream.write(message + b"\n")
        answer = stream.readline()
expected = b"ACK " + message + b"\n"
if answer != expected:
    raise SystemExit(f"expected {expected!r}, received {answer!r}")
print(answer.decode().rstrip())
