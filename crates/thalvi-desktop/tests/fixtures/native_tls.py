"""Serve isolated HTTPS/WSS fixtures until stdin closes; never modify system trust."""

import base64
import hashlib
import http.server
import json
import pathlib
import ssl
import subprocess
import sys
import tempfile
import threading


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers["Content-Length"]))
        assert self.path == "/v1/audio/transcriptions"
        body = b'{"text":"Trusted transcription."}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        key = self.headers["Sec-WebSocket-Key"]
        digest = hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
        self.protocol_version = "HTTP/1.1"
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", base64.b64encode(digest).decode())
        self.end_headers()


def openssl(directory, *args):
    subprocess.run(["openssl", *args], cwd=directory, check=True, capture_output=True)


def serve(directory):
    directory.mkdir()
    openssl(directory, "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", "ca.key", "-out", "ca.pem", "-days", "1", "-subj", f"/CN=Thalvi {directory.name} test CA",
            "-addext", "basicConstraints=critical,CA:TRUE")
    openssl(directory, "req", "-newkey", "rsa:2048", "-nodes", "-keyout", "server.key",
            "-out", "server.csr", "-subj", "/CN=localhost")
    (directory / "extensions").write_text(
        "subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\n"
        "keyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n"
    )
    openssl(directory, "x509", "-req", "-in", "server.csr", "-CA", "ca.pem",
            "-CAkey", "ca.key", "-CAcreateserial", "-out", "server.pem", "-days", "1",
            "-extfile", "extensions")
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(directory / "server.pem", directory / "server.key")
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


with tempfile.TemporaryDirectory(prefix="thalvi-native-tls-") as temporary:
    root = pathlib.Path(temporary)
    trusted = serve(root / "trusted")
    untrusted = serve(root / "untrusted")
    (root / "empty").mkdir()
    try:
        print(json.dumps({"ca": str(root / "trusted/ca.pem"), "directory": str(root / "empty"),
                          "trusted": trusted.server_port, "untrusted": untrusted.server_port}), flush=True)
        sys.stdin.readline()
    finally:
        for server in (trusted, untrusted):
            server.shutdown()
            server.server_close()
