#!/usr/bin/env python3
"""Probe shutdown with an intentionally idle TCP peer; not a CI reproduction."""
import argparse
import json
import os
import re
import signal
import socket
import subprocess
import threading
import time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("binary")
parser.add_argument("--log", required=True)
parser.add_argument("--mode", choices=["idle", "partial", "http2"], default="idle")
parser.add_argument("--observe-seconds", type=float, default=6)
parser.add_argument("--release-seconds", type=float, default=2)
args = parser.parse_args()
path = Path(args.log)
path.parent.mkdir(parents=True, exist_ok=True)
connections = []


def read_exact(peer, count):
    result = bytearray()
    while len(result) < count:
        try:
            chunk = peer.recv(count - len(result))
        except socket.timeout:
            continue
        except OSError:
            return None
        if not chunk:
            return None
        result.extend(chunk)
    return bytes(result)


def respond_to_http2_control_frames(peer):
    # Act as a responsive idle h2 client, not a second unresponsive-peer case.
    while True:
        header = read_exact(peer, 9)
        if header is None:
            return
        length = int.from_bytes(header[:3], "big")
        payload = read_exact(peer, length)
        if payload is None:
            return
        kind, flags = header[3], header[4]
        try:
            if kind == 4 and not flags & 1:  # SETTINGS -> SETTINGS ACK
                peer.sendall(bytes.fromhex("000000040100000000"))
            if kind == 6 and not flags & 1:  # PING -> PING ACK
                peer.sendall(bytes.fromhex("000008060100000000") + payload)
        except OSError:
            return


start = time.monotonic()
blocked = False
phase_at_release = None
code = None
with path.open("w", buffering=1) as log:
    p = subprocess.Popen([args.binary, "--exact",
        "meta_node::meta_node_lifecycle::test_meta_node_join_with_state", "--nocapture"],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        start_new_session=True)

    def read_output():
        injected = False
        for line in p.stdout:
            log.write(line)
            if not injected:
                match = re.search(r"about to serve raft grpc on: http://([^\s]+)", line)
                if match:
                    host, port = match.group(1).rsplit(":", 1)
                    try:
                        peer = socket.create_connection((host.strip("[]"), int(port)), timeout=1)
                        prefix = {
                            "idle": b"",
                            "partial": b"PRI * HTTP/2.0\r\n",
                            "http2": b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" + bytes.fromhex("000000040000000000"),
                        }[args.mode]
                        if prefix:
                            peer.sendall(prefix)
                        connections.append(peer)
                        if args.mode == "http2":
                            threading.Thread(target=respond_to_http2_control_frames,
                                             args=(peer,), daemon=True).start()
                        injected = True
                        log.write(f"INJECT peer at {match.group(1)} mode={args.mode!r}\n")
                    except OSError as error:
                        log.write(f"INJECT failed: {error}\n")

    reader = threading.Thread(target=read_output, daemon=True)
    reader.start()
    try:
        code = p.wait(timeout=args.observe_seconds)
        print(f"completed while idle peer held: code={code} elapsed={time.monotonic()-start:.3f}s")
    except subprocess.TimeoutExpired:
        blocked = True
        log.flush()
        phases = [line for line in path.read_text().splitlines()
                  if line.startswith(("LIFECYCLE", "STOP "))]
        phase_at_release = phases[-1] if phases else None
        log.write(f"INJECT RELEASE peer after {args.observe_seconds}-second observation window\n")
        for peer in connections:
            peer.close()
        try:
            code = p.wait(timeout=args.release_seconds)
            print(f"completed only after peer release: code={code} elapsed={time.monotonic()-start:.3f}s")
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()
            print("did not exit after peer release; killed at bounded deadline")
    finally:
        for peer in connections:
            peer.close()
        reader.join(timeout=.5)
        print(f"injected_connections={len(connections)} log={path}")
result = {"mode": args.mode, "blocked_until_release": blocked,
          "phase_at_release": phase_at_release, "exit_code": code,
          "injected_connections": len(connections),
          "elapsed": time.monotonic() - start}
path.with_suffix(".result.json").write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps(result), flush=True)
assert len(connections) == 1, result
assert code == 0, result
assert blocked == (args.mode != "http2"), result
if blocked:
    assert phase_at_release == "STOP 0 join 1 BEGIN", result
