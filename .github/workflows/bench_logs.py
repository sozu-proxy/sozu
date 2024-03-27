import subprocess
import logging
import time
import os

class Row(object):
    def __init__(self, color, target, iterations, level, time):
        self.color = color
        self.target = target
        self.iterations = iterations
        self.level = level
        self.time = time
        
    def __repr__(self):
        return f"| {self.color!s: <6} | {self.target: <8} | {self.iterations: <10} | {self.level: <8}| {self.time: <8.4f}"

def print_results(result_rows):
    print(f"| color  | target   | iterations | level   | time (s)")

    for row in result_rows:
        print(row)

def bench_logs(color: bool, target: str, iterations: int, level: str) -> Row:
    logging.info("🎯 Initalize environnment")

    start = time.time()

    env = {}
    env['BENCH_LOG_COLOR'] = str(color)
    env['BENCH_LOG_TARGET'] = target
    env['BENCH_LOG_ITERS'] = str(iterations)
    env['BENCH_LOG_FILTER'] = level

    try:
        bench_process = subprocess.Popen(
            ["./bench_logger"],
            stdout=subprocess.DEVNULL,
            env=env
        )

        stdout, stderr = bench_process.communicate()

        if stderr:
            logging.error(stderr.decode())

    except subprocess.CalledProcessError as e:
        logging.error(f"🚨 Command failed with return code {e.returncode}")

    elapsed_time = time.time() - start

    try:
        subprocess.run(["kill", str(bench_process.pid)])
    except subprocess.CalledProcessError as e:
        logging.error(f"🚨 Failed to destroy environnement {e.returncode}")

    return Row(color, target, iterations, level, elapsed_time)


logging.basicConfig(encoding='utf-8', level=logging.INFO)
logging.info("💣 Launching benchmark")

result_rows = [
    bench_logs(True, "stdout", 1000000, "debug"),
    bench_logs(True, "stdout", 1000000, "trace"),
    bench_logs(True, "udp", 1000000, "debug"),
    bench_logs(True, "udp", 1000000, "trace")
]

print_results(result_rows)