import subprocess
import logging
import time

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
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        time.sleep(3)
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
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        time.sleep(3)
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
