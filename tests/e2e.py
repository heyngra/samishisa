#!/usr/bin/env python3
"""End-to-end tests: run the real samishisa binary against the hello and probe extensions.

    python3 tests/e2e.py [path/to/samishisa]    (default target/release/samishisa)

Needs target/wasm32-wasip2/release/{hello,probe}.wasm. Uses ports 3911-3913 and a local upstream
HTTP server; the one test that needs the internet (fetching https://example.com) is skipped when
offline. Set REDIS_URL to also run the shared-state tests against a real Redis.
"""

import gzip
import http.server
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target/release/samishisa"))
WASM = os.path.join(ROOT, "target/wasm32-wasip2/release")
HELLO, PROBE = os.path.join(WASM, "hello.wasm"), os.path.join(WASM, "probe.wasm")

passed, failed, skipped = 0, [], []


def check(name, cond, detail=""):
    global passed
    if cond:
        passed += 1
        print("  ok    " + name)
    else:
        failed.append(name)
        print("  FAIL  %s %s" % (name, detail))


def req(port, path, method="GET", body=None, headers=None, raw=False):
    """-> (status, headers dict lowercased, body bytes or parsed json)"""
    data = json.dumps(body).encode() if isinstance(body, (dict, list)) else body
    h = {"content-type": "application/json"} if data is not None else {}
    h.update(headers or {})
    r = urllib.request.Request("http://127.0.0.1:%d%s" % (port, path), data=data, method=method, headers=h)
    try:
        with urllib.request.urlopen(r, timeout=15) as res:
            status, hdrs, payload = res.status, {k.lower(): v for k, v in res.headers.items()}, res.read()
    except urllib.error.HTTPError as e:
        status, hdrs, payload = e.code, {k.lower(): v for k, v in e.headers.items()}, e.read()
    if raw:
        return status, hdrs, payload
    try:
        return status, hdrs, json.loads(payload) if payload else None
    except ValueError:
        return status, hdrs, payload.decode(errors="replace")


UPSTREAM_PORT = 3913
UPSTREAM_FILES = {}


class Upstream(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/json":
            body, ctype = b'{"from": "upstream"}', "application/json"
        elif self.path == "/redirect":
            self.send_response(302)
            self.send_header("location", "/json")
            self.end_headers()
            return
        elif self.path in UPSTREAM_FILES:
            body, ctype = UPSTREAM_FILES[self.path], "application/wasm"
        else:
            self.send_response(404)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


upstream = http.server.ThreadingHTTPServer(("127.0.0.1", UPSTREAM_PORT), Upstream)
threading.Thread(target=upstream.serve_forever, daemon=True).start()

TMP = tempfile.mkdtemp(prefix="samishisa-e2e-")


class Server:
    def __init__(self, name, port, cfg):
        self.port, self.dir = port, os.path.join(TMP, name)
        os.makedirs(self.dir, exist_ok=True)
        self.cfg_path = os.path.join(self.dir, "api.config.json")
        self.write(cfg)
        self.log_path = os.path.join(self.dir, "server.log")
        self.proc = None

    def write(self, cfg):
        cfg = dict(cfg, port=self.port)
        with open(self.cfg_path, "w") as f:
            json.dump(cfg, f, indent=2)

    def start(self):
        self.log = open(self.log_path, "a")
        env = dict(os.environ, LOG_LEVEL="info")
        env.pop("REDIS_URL", None)
        self.proc = subprocess.Popen([BIN, "serve", "--config", self.cfg_path, "--threads", "2"], stdout=self.log, stderr=self.log, env=env)
        for _ in range(300):
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=0.2):
                    return self
            except OSError:
                if self.proc.poll() is not None:
                    raise SystemExit("server exited early:\n" + open(self.log_path).read())
                time.sleep(0.05)
        raise SystemExit("server did not start:\n" + open(self.log_path).read())

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.log.close()

    def logtext(self):
        return open(self.log_path).read()


def main():
    for f in (BIN, HELLO, PROBE):
        if not os.path.exists(f):
            raise SystemExit("missing %s (build it first)" % f)

    print("== main server")
    main_cfg = {
        "rateLimit": {"limit": 1000, "windowSec": 60},
        "extensions": [
            {"source": HELLO, "config": {"greeting": "Hey"}},
            PROBE,
            {"source": HELLO, "name": "typo", "config": {"greting": "x"}},
            "./does-not-exist.wasm",
        ],
    }
    s = Server("main", 3911, main_cfg).start()
    p = s.port
    try:
        st, _, idx = req(p, "/")
        names = [e["name"] for e in idx["extensions"]]
        check("index lists the loaded extensions", st == 200 and names == ["hello", "probe"], idx)
        errs = " | ".join(f["error"] for f in idx["failed"])
        check("a config typo stops only that extension", "'greting' was unexpected" in errs, errs)
        check("a missing file is reported", "does-not-exist.wasm not found" in errs, errs)

        st, h, b = req(p, "/hello/Ada?shout=true")
        check("greet", st == 200 and b == {"message": "HEY, ADA!"}, b)
        check("x-request-id on every response", len(h.get("x-request-id", "")) == 16, h)
        check("server-wide rate limit headers", h.get("ratelimit-limit") == "1000", h)

        st, _, b = req(p, "/hello/Ada?shout=maybe")
        check("validation error", st == 400 and b["error"]["code"] == "validation_error" and "boolean" in b["error"]["details"]["problems"][0], b)
        st, _, b = req(p, "/hello/echo", "POST", {"repeat": 30})
        check("body validation lists every problem", st == 400 and len(b["error"]["details"]["problems"]) == 2, b)
        st, _, b = req(p, "/hello/echo", "POST", b"{nope")
        check("invalid JSON body", st == 400 and b["error"]["code"] == "invalid_json", b)
        st, h, b = req(p, "/hello/Ada", "DELETE")
        check("405 with Allow", st == 405 and h.get("allow") == "GET", (st, h))
        st, _, b = req(p, "/helo/x")
        check("404 suggests the closest extension", st == 404 and "Did you mean /hello?" in b["error"]["hint"], b)
        st, _, b = req(p, "/hello/teapot")
        check("ApiError keeps status, code and hint", st == 418 and b["error"]["code"] == "teapot" and b["error"]["hint"], b)
        st, _, b = req(p, "/hello/boom")
        check("internal errors don't leak", st == 500 and "secret" not in json.dumps(b), b)
        check("...but are logged with the request id", b["error"]["requestId"] in s.logtext() and "invalid digit" in s.logtext())
        st, h, raw = req(p, "/hello/Ada", "HEAD", raw=True)
        check("HEAD answers without a body", st == 200 and raw == b"", (st, raw))

        st, _, v1 = req(p, "/hello/visits")
        st, _, v2 = req(p, "/hello/visits")
        check("static route beats :param, store counts", v2["visits"] == v1["visits"] + 1, (v1, v2))

        st, h1, c1 = req(p, "/probe/cached")
        st, h2, c2 = req(p, "/probe/cached")
        check("cache: second call is a HIT with the same body", h1.get("x-cache") == "MISS" and h2.get("x-cache") == "HIT" and c1 == c2, (h1.get("x-cache"), h2.get("x-cache"), c1, c2))
        check("cache: Cache-Control and ETag", "max-age=" in h2.get("cache-control", "") and h2.get("etag"), h2)
        st, h3, _ = req(p, "/probe/cached", headers={"if-none-match": h2.get("etag", "")}, raw=True)
        check("cache: If-None-Match gives 304", st == 304, st)

        st, h, raw = req(p, "/probe/big", headers={"accept-encoding": "gzip"}, raw=True)
        ok = st == 200 and h.get("content-encoding") == "gzip" and len(json.loads(gzip.decompress(raw))) == 2000
        check("gzip for large responses", ok, (st, h.get("content-encoding"), len(raw)))
        st, h, raw = req(p, "/probe/big", raw=True)
        check("no gzip without Accept-Encoding", "content-encoding" not in h and len(json.loads(raw)) == 2000)

        codes = [req(p, "/probe/limited")[0] for _ in range(4)]
        st, h, b = req(p, "/probe/limited")
        check("route rate limit: 3 then 429", codes[:3] == [200] * 3 and codes[3] == 429 and st == 429, codes)
        check("429 carries Retry-After and a hint", h.get("retry-after") and "3 per 60s" in b["error"]["hint"], (h, b))
        st, _, _ = req(p, "/probe/limited", headers={"x-forwarded-for": "203.0.113.5"})
        check("forwarded client from a trusted (loopback) peer gets its own budget", st == 200, st)
        st, h, _ = req(p, "/probe/free")
        check("no_rate_limit routes are exempt", st == 200 and "ratelimit-limit" not in h, h)

        t = time.time()
        st, _, b = req(p, "/probe/spin")
        dt = time.time() - t
        check("infinite loop is cut at the route timeout (504)", st == 504 and dt < 3, (st, round(dt, 2)))
        st, _, b = req(p, "/probe/spin?ms=50")
        check("short busy work completes", st == 200 and b == "done", b)
        st, _, b = req(p, "/probe/oom")
        check("memory bomb hits the cap (500)", st == 500, st)
        check("...and the log says it ran out of memory", "ran out of memory (the cap is 64 MB" in s.logtext())
        st, _, b = req(p, "/probe/panic")
        check("panic is a clean 500", st == 500 and b["error"]["code"] == "internal_error", b)
        check("...and the panic message is in the log", "panicked on purpose" in s.logtext())
        st, _, b = req(p, "/hello/Ada")
        check("server still serves after all that", st == 200, st)

        st, _, b = req(p, "/probe/fetch?url=http://127.0.0.1:%d/json" % UPSTREAM_PORT)
        check("fetch to a private address is blocked", st == 403 and b["error"]["code"] == "network_private_blocked", b)
        st, _, b = req(p, "/probe/fetch?url=http://example.org/")
        check("fetch to a host outside the allowlist is blocked", st == 403 and b["error"]["code"] == "network_not_allowed", b)
        st, _, b = req(p, "/probe/fetch?url=http://localhost:%d/json" % UPSTREAM_PORT)
        check("a hostname resolving to loopback is blocked too", st == 403 and b["error"]["code"] == "network_private_blocked", b)
        try:
            socket.create_connection(("example.com", 443), timeout=3).close()
            online = True
        except OSError:
            online = False
        if online:
            st, _, b = req(p, "/probe/fetch?url=https://example.com/")
            check("fetch over TLS to an allowed public host", st == 200 and b["status"] == 200 and "Example Domain" in b["body"], b)
        else:
            skipped.append("fetch over TLS (offline)")

        st, _, _ = req(p, "/probe/kv/color", "PUT", {"value": "teal"})
        st2, _, b = req(p, "/probe/kv/color")
        check("store set/get", st == 204 and b == "teal", (st, b))
        st, _, b = req(p, "/probe/kv/nope")
        check("missing key is a 404", st == 404, st)
        time.sleep(2.2)
        st, _, b = req(p, "/probe/ticks")
        check("scheduled job runs every second", b["n"] >= 2, b)

        st, h, _ = req(p, "/hello/Ada", headers={"origin": "https://example.net"})
        check("CORS header for browsers", h.get("access-control-allow-origin") == "*", h)
        st, h, _ = req(p, "/hello/echo", "OPTIONS", headers={"origin": "https://example.net", "access-control-request-method": "POST"}, raw=True)
        check("CORS preflight", st == 204 and "POST" in h.get("access-control-allow-methods", ""), (st, h))
        st, _, spec = req(p, "/openapi.json")
        check("OpenAPI document", st == 200 and spec["openapi"] == "3.1.0" and "/hello/{name}" in spec["paths"], list(spec.get("paths", {}))[:5])
        st, _, html = req(p, "/docs")
        check("docs page", st == 200 and "api-reference" in html)
        st, _, d = req(p, "/probe")
        check("GET /<ext> describes routes", any(r["path"] == "/probe/limited" and r["rateLimit"]["limit"] == 3 for r in d["routes"]), d)

        cfg = dict(main_cfg)
        cfg["extensions"] = cfg["extensions"][:2] + [{"source": HELLO, "name": "hi2", "config": {"greeting": "Yo"}}]
        s.write(cfg)
        s.proc.send_signal(signal.SIGHUP)
        for _ in range(100):
            st, _, b = req(p, "/hi2/Ada")
            if st == 200:
                break
            time.sleep(0.1)
        check("SIGHUP reloads the config without a restart", st == 200 and b == {"message": "Yo, Ada!"}, (st, b))
        st, _, b = req(p, "/hello/visits")
        check("the store survives a reload", b["visits"] == v2["visits"] + 1, b)
    finally:
        s.stop()

    s.start()
    try:
        st, _, b = req(p, "/hello/visits")
        check("the store survives a restart (SQLite)", st == 200 and b["visits"] == v2["visits"] + 2, b)
    finally:
        s.stop()

    print("== remote sources, allowPrivate")
    UPSTREAM_FILES["/hello.wasm"] = open(HELLO, "rb").read()
    url = "http://127.0.0.1:%d/hello.wasm" % UPSTREAM_PORT
    r = Server("remote", 3912, {"network": {"allowPrivate": True}, "extensions": [{"source": url, "name": "remote"}, PROBE]}).start()
    try:
        st, _, b = req(r.port, "/remote/Ada")
        check("an https/http .wasm source is downloaded and served", st == 200 and b == {"message": "Hello, Ada!"}, (st, b))
        lock = json.load(open(os.path.join(r.dir, "api.lock.json")))
        check("the download is pinned in api.lock.json", lock["sources"][url]["sha256"] and lock["sources"][url]["url"] == url, lock)
        st, _, b = req(r.port, "/probe/fetch?url=http://127.0.0.1:%d/redirect" % UPSTREAM_PORT)
        check("allowPrivate: local upstream reachable, redirects followed", st == 200 and b["status"] == 200 and "upstream" in b["body"], b)
    finally:
        r.stop()
    del UPSTREAM_FILES["/hello.wasm"]
    r.start()
    try:
        st, _, b = req(r.port, "/remote/Ada")
        check("a pinned source loads offline from the cache", st == 200, (st, r.logtext()[-500:]))
    finally:
        r.stop()
    cfg = json.load(open(r.cfg_path))
    cfg["extensions"][0]["sha256"] = "00" * 32
    r.write(cfg)
    r.start()
    try:
        st, _, idx = req(r.port, "/")
        check("a sha256 mismatch refuses the extension", any("SHA-256" in f["error"] for f in idx["failed"]), idx["failed"])
    finally:
        r.stop()

    print("== builtin and CLI")
    out = subprocess.run([BIN, "init", os.path.join(TMP, "init")], capture_output=True, text=True)
    cfg = json.load(open(os.path.join(TMP, "init", "api.config.json")))
    check("init writes a starter config", out.returncode == 0 and cfg["extensions"][0]["source"] in ("builtin:hello", "github:heyngra/samishisa"), (out.stdout, cfg))
    if cfg["extensions"][0]["source"] == "builtin:hello":
        b = Server("builtin", 3912, cfg).start()
        try:
            st, _, body = req(b.port, "/hello/world")
            check("builtin:hello works out of the box", st == 200 and body == {"message": "Hello, world!"}, body)
            hc = subprocess.run([BIN, "healthcheck", "--port", str(b.port)])
            check("healthcheck succeeds against a running server", hc.returncode == 0)
        finally:
            b.stop()
    else:
        skipped.append("builtin:hello (not embedded in this build)")
    add = subprocess.run([BIN, "add", "github:someone/thing#v1", "--name", "thing", "--config", os.path.join(TMP, "init", "api.config.json")], capture_output=True, text=True)
    cfg = json.load(open(os.path.join(TMP, "init", "api.config.json")))
    check("add appends an entry", add.returncode == 0 and cfg["extensions"][-1] == {"source": "github:someone/thing#v1", "name": "thing"}, (add.stdout, add.stderr, cfg))
    ins = subprocess.run([BIN, "inspect", PROBE], capture_output=True, text=True)
    check("inspect lists routes", ins.returncode == 0 and "/probe/limited" in ins.stdout, ins.stdout + ins.stderr)
    hc = subprocess.run([BIN, "healthcheck", "--port", "3999"])
    check("healthcheck fails when nothing listens", hc.returncode == 1)

    if os.environ.get("REDIS_URL"):
        print("== redis")
        cfgs = {"redis": os.environ["REDIS_URL"], "extensions": [PROBE]}
        a, b = Server("redis-a", 3911, cfgs).start(), Server("redis-b", 3912, cfgs).start()
        try:
            ip = "198.18.%d.%d" % (os.getpid() % 250, int(time.time()) % 250)
            codes = [req(a.port, "/probe/limited", headers={"x-forwarded-for": ip})[0] for _ in range(2)]
            codes += [req(b.port, "/probe/limited", headers={"x-forwarded-for": ip})[0] for _ in range(2)]
            check("redis: the rate limit is shared by two servers", codes == [200, 200, 200, 429], codes)
            ha = req(a.port, "/probe/cached")[1]
            hb = req(b.port, "/probe/cached")[1]
            check("redis: one server's cache fill is a hit on the other", hb.get("x-cache") in ("HIT", "STALE"), (ha.get("x-cache"), hb.get("x-cache")))
        finally:
            a.stop()
            b.stop()
        bad = Server("redis-down", 3911, {"redis": "redis://127.0.0.1:1", "extensions": [PROBE]}).start()
        try:
            t = time.time()
            st = req(bad.port, "/probe/limited")[0]
            check("redis down: requests still work, quickly", st == 200 and time.time() - t < 3, (st, time.time() - t))
        finally:
            bad.stop()
    else:
        skipped.append("redis (set REDIS_URL)")

    upstream.shutdown()
    print("\n%d passed, %d failed%s" % (passed, len(failed), "".join("\nskipped: " + s for s in skipped)))
    if failed:
        print("failed:\n  " + "\n  ".join(failed))
        print("(logs kept in %s)" % TMP)
        sys.exit(1)
    shutil.rmtree(TMP, ignore_errors=True)


if __name__ == "__main__":
    main()
