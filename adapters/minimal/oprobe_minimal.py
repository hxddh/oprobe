#!/usr/bin/env python3
"""Minimal oprobe client adapter (protocol `oprobe.client/1`, see docs/clients.md).

The smallest complete adapter: the Python standard library only, unsigned path-style S3
requests. It drives the oprobe lab (`oprobe lab`) and S3-compatible servers that allow
anonymous access, and it is the template for an adapter of your own client:

    oprobe run s3://bkt/x --endpoint http://127.0.0.1:9000 --client "exec:python3 adapters/minimal/oprobe_minimal.py"

Protocol summary: read one JSON object per line on stdin, write one per line on stdout.
The first line is `hello` (answer with your own `hello`), then operations with an `id`
(answer each with a result carrying that `id`, in any order), then `shutdown` or EOF.
Diagnostics go to stderr only.
"""

import base64
import http.client
import json
import sys
import threading
import time
import urllib.parse
import xml.etree.ElementTree as ET
from concurrent.futures import ThreadPoolExecutor

PROTOCOL = "oprobe.client/1"
OUT = threading.Lock()


def now_us():
    return time.monotonic_ns() // 1000


def emit(obj):
    line = json.dumps(obj, separators=(",", ":"))
    with OUT:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


class Client:
    def __init__(self, hello):
        t = hello["target"]
        ep = urllib.parse.urlsplit(t.get("endpoint") or "http://127.0.0.1:9000")
        self.https = ep.scheme == "https"
        self.host = ep.hostname
        self.port = ep.port or (443 if self.https else 80)
        self.bucket = t.get("bucket") or ""
        self.retries = int(hello.get("retries_count", 0))
        self.timeout = max(1.0, hello.get("timeout_ms", 30000) / 1000.0)
        self.local = threading.local()

    def conn(self):
        c = getattr(self.local, "conn", None)
        if c is None:
            cls = http.client.HTTPSConnection if self.https else http.client.HTTPConnection
            c = cls(self.host, self.port, timeout=self.timeout)
            self.local.conn = c
        return c

    def path(self, key="", query=None):
        p = "/" + self.bucket
        if key:
            p += "/" + urllib.parse.quote(key, safe="/-_.~")
        if query:
            p += "?" + urllib.parse.urlencode(query)
        return p

    def request(self, method, path, body=None, headers=None):
        """One HTTP exchange with its phases (connect, send, ttfb, body)."""
        c = self.conn()
        phases = {}
        t0 = now_us()
        try:
            if c.sock is None:
                c.connect()
                phases["connect_us"] = now_us() - t0
            t1 = now_us()
            c.putrequest(method, path, skip_accept_encoding=True)
            for k, v in (headers or {}).items():
                c.putheader(k, v)
            c.putheader("Content-Length", str(len(body or b"")))
            c.endheaders(body)
            t2 = now_us()
            phases["send_us"] = t2 - t1
            resp = c.getresponse()
            t3 = now_us()
            phases["ttfb_us"] = t3 - t2
            data = resp.read()
            phases["body_us"] = now_us() - t3
            if resp.getheader("connection", "").lower() == "close":
                c.close()
                self.local.conn = None
            return resp.status, dict(resp.getheaders()), data, phases, t0, now_us(), None
        except (OSError, http.client.HTTPException) as e:
            c.close()
            self.local.conn = None
            return 0, {}, b"", phases, t0, now_us(), f"{type(e).__name__}: {e}"

    def exchange(self, method, path, body=None, headers=None):
        """With retries of throttles, server errors and network errors."""
        log = []
        for attempt in range(self.retries + 1):
            status, hdrs, data, phases, t0, t1, err = self.request(method, path, body, headers)
            log.append({"status": status, "t_start_us": t0, "t_end_us": t1, "phases": phases})
            if err:
                log[-1]["error"] = err
            if status in (0, 429, 500, 502, 503, 504) and attempt < self.retries:
                time.sleep(min(0.05 * (2 ** attempt), 2.0))
                continue
            return status, hdrs, data, log
        return status, hdrs, data, log


def error_of(status, data, fallback=""):
    code, msg = "", fallback
    try:
        root = ET.fromstring(data)
        code = root.findtext("Code") or ""
        msg = root.findtext("Message") or msg
    except ET.ParseError:
        pass
    return {"code": code or (f"HTTP{status}" if status else "NetworkError"), "message": msg}


def result(op, status, log, **extra):
    ok = 200 <= status < 300
    r = {
        "id": op["id"],
        "ok": ok,
        "status": status,
        "t_start_us": log[0]["t_start_us"],
        "t_end_us": log[-1]["t_end_us"],
        "attempts": len(log),
        "phases": log[-1]["phases"],
        "http_version": "http1",
    }
    if len(log) > 1:
        r["attempt_log"] = log
    r.update(extra)
    return r


def do(cli, op):
    kind = op["op"]
    if kind == "get":
        headers = {}
        rg = op.get("range")
        if rg:
            if rg.get("suffix") is not None:
                headers["Range"] = f"bytes=-{rg['suffix']}"
            else:
                off = rg.get("offset", 0)
                ln = rg.get("length")
                headers["Range"] = f"bytes={off}-" + (str(off + ln - 1) if ln else "")
        status, hdrs, data, log = cli.exchange("GET", cli.path(op["key"]), headers=headers)
        size = None
        cr = hdrs.get("Content-Range") or hdrs.get("content-range")
        if cr and "/" in cr:
            size = int(cr.rsplit("/", 1)[1])
        elif 200 <= status < 300:
            size = len(data)
        extra = {"bytes": len(data) if 200 <= status < 300 else 0, "etag": hdrs.get("ETag")}
        if size is not None:
            extra["size"] = size
        if not 200 <= status < 300:
            extra["error"] = error_of(status, data, log[-1].get("error", ""))
        return result(op, status, log, **extra)
    if kind == "put":
        body = base64.b64decode(op.get("data_b64") or "")
        headers = {}
        for h, k in (("If-Match", "if_match"), ("If-None-Match", "if_none_match"), ("Content-Type", "content_type")):
            if op.get(k):
                headers[h] = op[k]
        for k, v in (op.get("metadata") or {}).items():
            headers["x-amz-meta-" + k] = v
        status, hdrs, data, log = cli.exchange("PUT", cli.path(op["key"]), body, headers)
        extra = {"bytes": len(body), "etag": hdrs.get("ETag")}
        if not 200 <= status < 300:
            extra["error"] = error_of(status, data, log[-1].get("error", ""))
        return result(op, status, log, **extra)
    if kind == "head":
        status, hdrs, data, log = cli.exchange("HEAD", cli.path(op["key"]))
        extra = {"etag": hdrs.get("ETag")}
        if 200 <= status < 300:
            extra["size"] = int(hdrs.get("Content-Length") or 0)
        else:
            extra["error"] = error_of(status, b"", "NoSuchKey" if status == 404 else "")
        return result(op, status, log, **extra)
    if kind == "delete":
        logs, failed, last = [], [], 204
        for key in op.get("keys") or []:
            status, _, data, log = cli.exchange("DELETE", cli.path(key))
            logs.extend(log)
            last = status
            if not (200 <= status < 300 or status == 404):
                failed.append(key)
        status = 204 if not failed else last
        return result(op, status, logs or [{"t_start_us": now_us(), "t_end_us": now_us(), "phases": {}}],
                      requests=max(1, len(op.get("keys") or [])), failed_keys=failed)
    if kind == "list":
        entries, logs, token = [], [], None
        while True:
            q = {"list-type": "2", "prefix": op.get("prefix") or ""}
            if not op.get("recursive"):
                q["delimiter"] = "/"
            if op.get("limit"):
                q["max-keys"] = str(op["limit"])
            if token:
                q["continuation-token"] = token
            status, _, data, log = cli.exchange("GET", cli.path(query=q))
            logs.extend(log)
            if not 200 <= status < 300:
                return result(op, status, logs, requests=len(logs), error=error_of(status, data))
            root = ET.fromstring(data)
            ns = root.tag.split("}")[0] + "}" if root.tag.startswith("{") else ""
            for c in root.findall(f"{ns}Contents"):
                entries.append({"key": c.findtext(f"{ns}Key"), "size": int(c.findtext(f"{ns}Size") or 0)})
            for p in root.findall(f"{ns}CommonPrefixes"):
                entries.append({"key": p.findtext(f"{ns}Prefix"), "dir": True})
            token = root.findtext(f"{ns}NextContinuationToken")
            if (root.findtext(f"{ns}IsTruncated") or "false") != "true" or not token:
                break
        r = result(op, 200, logs, requests=len(logs), entries=entries)
        r.pop("attempt_log", None)
        return r
    if kind == "copy":
        src = "/" + cli.bucket + "/" + urllib.parse.quote(op["source"], safe="/-_.~")
        status, hdrs, data, log = cli.exchange("PUT", cli.path(op["key"]), headers={"x-amz-copy-source": src})
        extra = {}
        if not 200 <= status < 300:
            extra["error"] = error_of(status, data)
        return result(op, status, log, **extra)
    return {"id": op["id"], "ok": False, "status": 0, "error": {"code": "Unsupported", "message": kind}}


def main():
    first = sys.stdin.readline()
    if not first:
        return
    hello = json.loads(first)
    if not str(hello.get("protocol", "")).startswith("oprobe.client/1"):
        print(f"unsupported protocol {hello.get('protocol')}", file=sys.stderr)
        sys.exit(2)
    cli = Client(hello)
    emit({
        "type": "hello",
        "protocol": PROTOCOL,
        "client": "python-http.client",
        "version": sys.version.split()[0],
        "phases": ["connect", "send", "ttfb", "body"],
        "ops": ["get", "put", "head", "delete", "list", "copy"],
        "features": ["if_match", "if_none_match", "metadata"],
    })
    pool = ThreadPoolExecutor(max_workers=max(1, int(hello.get("concurrency_count", 1))))

    def run(op):
        try:
            emit(do(cli, op))
        except Exception as e:  # never leave oprobe waiting for an answer
            emit({"id": op.get("id", 0), "ok": False, "status": 0,
                  "error": {"code": type(e).__name__, "message": str(e)}})

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        msg = json.loads(line)
        if msg.get("type") == "shutdown":
            break
        pool.submit(run, msg)
    pool.shutdown(wait=True)


if __name__ == "__main__":
    main()
