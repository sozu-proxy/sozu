#!/usr/bin/env python3
"""Check request routing state on one H1 connection against a fresh connection."""
import argparse
import http.client
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument('--sozu', required=True, type=Path)
parser.add_argument(
    '--output',
    type=Path,
    default=Path(os.environ.get('TMPDIR', '/tmp')) / 'sozu-lifecycle',
    help='parent for disposable run directories (default: $TMPDIR/sozu-lifecycle)',
)
args = parser.parse_args()
repository = Path(__file__).resolve().parents[2]
output = args.output.resolve()
if output == repository or repository in output.parents:
    parser.error('--output must be outside the source checkout')
output.mkdir(parents=True, exist_ok=True)
run = Path(tempfile.mkdtemp(prefix='cookie-', dir=output))
servers = []
main = None
connections = []
log = None
try:
    for label in ['a', 'b']:
        class Backend(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'
            def do_GET(self):
                body = self.server.label.encode()
                self.send_response(200)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            def log_message(self, *unused):
                pass
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Backend)
        server.daemon_threads = True
        server.label = label
        threading.Thread(target=server.serve_forever, daemon=True).start()
        servers.append(server)
    with socket.socket() as reservation:
        reservation.bind(('127.0.0.1', 0))
        frontend_port = reservation.getsockname()[1]
    config = run / 'config.toml'
    config.write_text(f'''log_level = "info"
log_target = "stdout"
command_socket = "{run}/sozu.sock"
pid_file_path = "{run}/sozu.pid"
worker_count = 1
worker_automatic_restart = false
handle_process_affinity = false
max_connections = 100
max_buffers = 200
min_buffers = 1
buffer_size = 16393
activate_listeners = true
[[listeners]]
protocol = "http"
address = "127.0.0.1:{frontend_port}"
[clusters.a]
protocol = "http"
sticky_session = true
frontends = [{{address = "127.0.0.1:{frontend_port}", hostname = "a.test", path = "/"}}]
backends = [{{address = "127.0.0.1:{servers[0].server_port}", backend_id = "a", sticky_id = "sticky-a"}}]
[clusters.b]
protocol = "http"
sticky_session = false
frontends = [{{address = "127.0.0.1:{frontend_port}", hostname = "b.test", path = "/"}}]
backends = [{{address = "127.0.0.1:{servers[1].server_port}", backend_id = "b"}}]
''')
    log = (run / 'sozu.log').open('w')
    main = subprocess.Popen([str(args.sozu), '--config', str(config), 'start'], stdout=log, stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 10
    while True:
        if main.poll() is not None:
            raise RuntimeError(f'sozu exited before readiness: {main.returncode}')
        probe = http.client.HTTPConnection('127.0.0.1', frontend_port, timeout=1)
        try:
            probe.request('GET', '/', headers={'Host': 'b.test'})
            reply = probe.getresponse()
            if reply.status == 200 and reply.read() == b'b':
                break
        except (OSError, http.client.HTTPException):
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.02)
        finally:
            probe.close()
        if time.monotonic() >= deadline:
            raise RuntimeError('b.test route did not become ready')
    def request(conn, host):
        conn.request('GET', '/', headers={'Host': host})
        response = conn.getresponse()
        record = {'status': response.status, 'headers': response.getheaders(), 'body': response.read().decode()}
        record['local_address'] = conn.sock.getsockname() if conn.sock else None
        record['cookies'] = [v for k, v in record['headers'] if k.lower() == 'set-cookie']
        return record
    fresh = http.client.HTTPConnection('127.0.0.1', frontend_port, timeout=3)
    reused = http.client.HTTPConnection('127.0.0.1', frontend_port, timeout=3)
    connections += [fresh, reused]
    control = request(fresh, 'b.test')
    first = request(reused, 'a.test')
    second = request(reused, 'b.test')
    results = {'fresh_b': control, 'reused_a': first, 'reused_b': second}
    (run / 'result.json').write_text(json.dumps(results, indent=2) + '\n')
    print(json.dumps({'run': str(run), **results}, indent=2), flush=True)
    assert control['body'] == 'b' and control['cookies'] == [], 'fresh non-sticky baseline invalid'
    assert first['body'] == 'a' and first['cookies'], 'sticky baseline invalid'
    assert first['local_address'] == second['local_address'] and first['local_address'] is not None, 'frontend connection was not reused'
    assert second['status'] == 200 and second['body'] == 'b', 'second request failed to route to b'
    assert second['cookies'] == [], 'non-sticky request inherited Set-Cookie from previous request'
finally:
    for conn in connections:
        conn.close()
    if main is not None:
        if (run / 'sozu.sock').exists():
            try:
                subprocess.run([str(args.sozu), '--config', str(config), '--timeout', '5000', 'shutdown', '--hard'], stdout=log, stderr=subprocess.STDOUT, timeout=8)
            except subprocess.TimeoutExpired:
                pass
        if main.poll() is None:
            main.terminate()
            try:
                main.wait(timeout=5)
            except subprocess.TimeoutExpired:
                main.kill()
                main.wait()
    for server in servers:
        server.shutdown()
        server.server_close()
    if log:
        log.close()
