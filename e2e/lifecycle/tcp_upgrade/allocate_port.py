#!/usr/bin/env python3
"""Allocate one loopback TCP port for a short-lived integration test."""

import socket


with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
