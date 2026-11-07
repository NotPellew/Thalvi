#!/usr/bin/env python3
"""A local HTTP CONNECT proxy for download checks (prints "ready" once it listens).

Usage: throttle-proxy.py PORT BYTES_PER_SECOND
  A positive rate slows the real download so a short one stays on screen.
  A rate of 0 refuses every tunnel with a 502 after 2 s, so a failure arrives late enough to see
  the notice before it.
"""
import select
import socket
import sys
import threading
import time


def pump(client, upstream, rate):
    sockets = [client, upstream]
    sent = 0
    start = time.monotonic()
    while True:
        readable, _, _ = select.select(sockets, [], [], 30)
        if not readable:
            return
        for source in readable:
            data = source.recv(4096)
            if not data:
                return
            if source is upstream:
                sent += len(data)
                lag = sent / rate - (time.monotonic() - start)
                if lag > 0:
                    time.sleep(lag)
                client.sendall(data)
            else:
                upstream.sendall(data)


def handle(client, rate):
    with client:
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = client.recv(4096)
            if not chunk:
                return
            head += chunk
        if rate == 0:
            time.sleep(2)
            client.sendall(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            return
        host, _, port = head.split(b" ")[1].decode().partition(":")
        with socket.create_connection((host, int(port or 443))) as upstream:
            client.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
            pump(client, upstream, rate)


def main():
    port, rate = int(sys.argv[1]), float(sys.argv[2])
    server = socket.create_server(("127.0.0.1", port))
    print("ready", flush=True)
    while True:
        client, _ = server.accept()
        threading.Thread(target=handle, args=(client, rate), daemon=True).start()


main()
