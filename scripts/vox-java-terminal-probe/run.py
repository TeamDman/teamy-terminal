#!/usr/bin/env python3
"""Bounded generated-Java/real-Terminal TCP probe; no Minecraft or GPU work."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--java-home', type=Path, required=True)
    parser.add_argument('--runtime-jar', type=Path, required=True)
    parser.add_argument('--server', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--early-input', action='store_true', help='diagnose input before shell readiness; may fail')
    args = parser.parse_args()
    jar, server_path = args.runtime_jar.resolve(), args.server.resolve()
    expected = 'be441a57e11d4981d283306fe471874e625bf2f423c83a91ce8f44dde623d062'
    if digest(jar) != expected:
        raise RuntimeError('runtime JAR differs from the qualified Vox cutover artifact')
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    classes = out / 'classes'
    classes.mkdir()
    source = Path(__file__).with_name('TerminalProbe.java').resolve()
    subprocess.run([str(args.java_home / 'bin/javac'), '--release', '17', '-Xlint:all',
                    '-Werror', '-cp', str(jar), '-d', str(classes), str(source)],
                   check=True, timeout=60)
    env = os.environ.copy()
    for key, name in [('XDG_DATA_HOME', 'data'), ('XDG_CACHE_HOME', 'cache'),
                      ('XDG_CONFIG_HOME', 'config')]:
        env[key] = str(out / name)
    env.pop('NO_COLOR', None)
    server_log = out / 'server.log'
    with server_log.open('w') as log:
        server = subprocess.Popen([str(server_path), '--listen', '127.0.0.1:0'],
                                  stdout=log, stderr=subprocess.STDOUT, env=env)
        try:
            deadline = time.monotonic() + 15
            while True:
                match = re.search(r'VOX_TERMINAL_LISTEN_ADDR=127\.0\.0\.1:(\d+)',
                                  server_log.read_text())
                if match:
                    break
                if server.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError('Terminal server did not become ready; inspect server.log')
                time.sleep(0.05)
            result = subprocess.run([str(args.java_home / 'bin/java'), '-ea', '-cp',
                                     os.pathsep.join([str(classes), str(jar)]),
                                     'TerminalProbe', match.group(1), *(['early'] if args.early_input else [])],
                                    env=env, capture_output=True, text=True, timeout=90)
            (out / 'client.stdout').write_text(result.stdout)
            (out / 'client.stderr').write_text(result.stderr)
            if server.poll() is not None:
                raise RuntimeError('server exited during probe')
            receipt = {'status': 'passed' if result.returncode == 0 else 'failed', 'client_exit': result.returncode, 'scope': 'real TCP generated Java to Rust Terminal, CPU only; not Minecraft',
                       'java_runtime_source': '5628c00d9f3816199f324a9ccc081de8a3a1b1bc',
                       'runtime_sha256': digest(jar), 'server_sha256': digest(server_path),
                       'probe_sha256': digest(source), 'early_input': args.early_input, 'checks': result.stdout.splitlines()}
            (out / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
            result.check_returncode()
            print(result.stdout, end='')
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)


if __name__ == '__main__':
    main()
