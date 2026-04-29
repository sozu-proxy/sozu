import socket
import subprocess
import logging
import sys
import time
from urllib.parse import urlparse


def wait_for_port(host: str, port: int, timeout: float = 15.0) -> bool:
    """Wait until a TCP port accepts connections, or timeout."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=1):
                return True
        except OSError:
            time.sleep(0.5)
    return False


def run(url: str, bombardierduration: str):
    logging.info("🎯 Initalize environnment")
    try:
        lagging_server = subprocess.Popen(
            ["./lagging_server", "-w", "4", "-p", "4444"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        sozu = subprocess.Popen(
            ["./sozu", "start", "-c", "bench.toml"],
            stderr=subprocess.PIPE,
        )

        parsed = urlparse(url)
        port = parsed.port or (443 if parsed.scheme == "https" else 80)
        if not wait_for_port("127.0.0.1", port):
            logging.error(f"🚨 Sōzu did not start listening on port {port}")
            stderr = sozu.stderr.read().decode() if sozu.stderr else ""
            if stderr:
                logging.error(f"sozu stderr: {stderr[:2000]}")
            sys.exit(1)

    except subprocess.CalledProcessError as e:
        logging.error(f"🚨 Command failed with return code {e.returncode}")

    try:
        subprocess.run(["./bombardier", "-c", "400", "-p", "intro,result", "--fasthttp", "-l", "-t", "10s", "-d", bombardierduration, url])

    except subprocess.CalledProcessError as e:
        logging.error(f"🚨 Failed to run benchmark {e.returncode}")

    logging.info("🪓 Destroy environment")
    try:
        subprocess.run(["kill", str(lagging_server.pid)])
        subprocess.run(["kill", str(sozu.pid)])
        time.sleep(1)
    except subprocess.CalledProcessError as e:
        logging.error(f"🚨 Failed to destroy environnement {e.returncode}")

def run_handshake(url: str, bombardierduration: str):
    logging.info("🎯 Initalize environnment")
    try:
        lagging_server = subprocess.Popen(
            ["./lagging_server", "-w", "4", "-p", "4444"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        sozu = subprocess.Popen(
            ["./sozu", "start", "-c", "bench.toml"],
            stderr=subprocess.PIPE,
        )

        parsed = urlparse(url)
        port = parsed.port or (443 if parsed.scheme == "https" else 80)
        if not wait_for_port("127.0.0.1", port):
            logging.error(f"🚨 Sōzu did not start listening on port {port}")
            stderr = sozu.stderr.read().decode() if sozu.stderr else ""
            if stderr:
                logging.error(f"sozu stderr: {stderr[:2000]}")
            sys.exit(1)

    except subprocess.CalledProcessError as e:
        logging.error(f"🚨 Command failed with return code {e.returncode}")

    try:
        # Do not use --fasthttp: use Go's net/http to get proper Connection: close support.
        # -H "Connection: close" forces a new TLS handshake per request,
        # isolating key exchange overhead (classical vs post-quantum).
        subprocess.run(["./bombardier", "-c", "100", "-p", "intro,result", "-l", "-t", "10s", "-d", bombardierduration, "-H", "Connection: close", url])

    except subprocess.CalledProcessError as e:
        logging.error(f"🚨 Failed to run benchmark {e.returncode}")

    logging.info("🪓 Destroy environment")
    try:
        subprocess.run(["kill", str(lagging_server.pid)])
        subprocess.run(["kill", str(sozu.pid)])
        time.sleep(1)
    except subprocess.CalledProcessError as e:
        logging.error(f"🚨 Failed to destroy environnement {e.returncode}")

logging.basicConfig(encoding='utf-8', level=logging.INFO)
logging.info("💣 Launching benchmark")

run("http://sozu.io:8080/api", "1m")
run("https://rsa-2048.sozu.io:8443/api", "1m")
run("https://rsa-4096.sozu.io:8443/api", "1m")
run("https://ecdsa.sozu.io:8443/api", "1m")

logging.info("🔐 Launching TLS handshake benchmark (Connection: close, measures key exchange overhead)")
run_handshake("https://ecdsa.sozu.io:8443/api", "30s")
