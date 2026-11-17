"""Isolated catalog capture mock (protocol note 76).

Wildcard DNS answering 127.0.0.1, a logging HTTP server on 80 and 8080 that answers catalog
calls with responses signed like the real catalog (note 62), and a TLS ClientHello logger on
443. Every request is appended to OUT/requests.jsonl.

Run it only inside a private network namespace (see run.sh). Replies use the synthetic
placeholder content id. CHECK_MODE=empty|zero|blank selects the checktorrent "not
registered" reply shape.

usage: mock.py OUT [TRANSPORT_FILE]
"""
import base64, json, os, socket, socketserver, struct, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

OUT = sys.argv[1]
TRANSPORT = sys.argv[2] if len(sys.argv) > 2 else None
PLACEHOLDER = "0123456789abcdef0123456789abcdef01234567"
import hashlib, re
from urllib.parse import urlparse, parse_qs
_src = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../../crates/ace-swarm/src/resolve.rs")).read()
_lit = re.search(r'const CATALOG_RESPONSE_KEY: &\[u8\] = b"((?:[^"\\]|\\.)*)";', _src).group(1)
RESPONSE_KEY = _lit.replace("\\\\", "\x00").replace('\\"', '"').replace("\x00", "\\").encode("latin1")
CHECK_MODE = os.environ.get("CHECK_MODE", "empty")


def signed(r, body):
    unsigned = f'<?xml version="1.0" encoding="utf-8"?><response r="{r}">{body}</response>'
    sig = hashlib.sha1(unsigned.encode() + RESPONSE_KEY).hexdigest()
    return unsigned.replace("<response", f'<response sig="{sig}"', 1).encode()
lock = threading.Lock()


def log(rec):
    rec["t"] = time.time()
    with lock, open(os.path.join(OUT, "requests.jsonl"), "a") as f:
        f.write(json.dumps(rec) + "\n")


class DNS(socketserver.BaseRequestHandler):
    def handle(self):
        data, sock = self.request
        tid, flags, qd = struct.unpack(">HHH", data[:6])
        i, labels = 12, []
        while data[i]:
            n = data[i]; labels.append(data[i + 1:i + 1 + n].decode(errors="replace")); i += n + 1
        qtype = struct.unpack(">H", data[i + 1:i + 3])[0]
        q = data[12:i + 5]
        name = ".".join(labels)
        log({"kind": "dns", "name": name, "qtype": qtype})
        if qtype == 1:
            ans = b"\xc0\x0c" + struct.pack(">HHIH", 1, 1, 5, 4) + socket.inet_aton("127.0.0.1")
            hdr = struct.pack(">HHHHHH", tid, 0x8180, 1, 1, 0, 0)
        else:
            ans = b""
            hdr = struct.pack(">HHHHHH", tid, 0x8180, 1, 0, 0, 0)
        sock.sendto(hdr + q + ans, self.client_address)


class HTTP(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _any(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        port = self.server.server_address[1]
        log({"kind": "http", "port": port, "method": self.command, "path": self.path,
             "headers": dict(self.headers.items()), "body_b64": base64.b64encode(body).decode()})
        if TRANSPORT and self.path.startswith("/capture.acelive"):
            data = open(TRANSPORT, "rb").read()
            return self._send(200, data, "application/octet-stream")
        if "upload/raw" in self.path:
            return self._send(200, json.dumps({"content_id": PLACEHOLDER}).encode(), "application/json")
        q = parse_qs(urlparse(self.path).query)
        if not q.get("_r") and body:
            q = parse_qs(body.decode("latin1"))
        r = (q.get("_r") or ["0"])[0]
        route = urlparse(self.path).path
        if route.startswith("/checktorrent"):
            inner = {"empty": "", "zero": "<id>0</id>", "blank": "<id></id>"}[CHECK_MODE]
            return self._send(200, signed(r, inner), "text/xml")
        if "puttorrent" in route or "puttorrent" in body.decode("latin1"):
            return self._send(200, signed(r, f"<id>{PLACEHOLDER}</id>"), "text/xml")
        return self._send(404, b"", "text/plain")

    def _send(self, code, data, ctype):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    do_GET = do_POST = do_PUT = do_HEAD = _any

    def log_message(self, *a):
        pass


def sni(port):
    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port)); s.listen(64)
    while True:
        c, _ = s.accept()
        try:
            c.settimeout(3)
            hello = c.recv(4096)
            log({"kind": "tls", "port": port, "hello_b64": base64.b64encode(hello).decode()})
        except Exception:
            pass
        c.close()


if __name__ == "__main__":
    socketserver.ThreadingUDPServer.allow_reuse_address = True
    udp = socketserver.ThreadingUDPServer(("127.0.0.1", 53), DNS)
    threading.Thread(target=udp.serve_forever, daemon=True).start()
    for port in (80, 8080):
        srv = ThreadingHTTPServer(("127.0.0.1", port), HTTP)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
    threading.Thread(target=sni, args=(443,), daemon=True).start()
    log({"kind": "ready"})
    while True:
        time.sleep(3600)
