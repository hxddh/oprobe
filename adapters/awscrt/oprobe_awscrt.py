#!/usr/bin/env python3
"""oprobe client adapter for the AWS Common Runtime S3 client (protocol `oprobe.client/1`,
see docs/clients.md).

    pip install awscrt
    oprobe run s3://bucket/prefix --client "exec:python3 adapters/awscrt/oprobe_awscrt.py"

awscrt's `S3Client` is the native, multi-connection transfer engine behind the AWS CLI's CRT
mode, boto3's CRT transfers and Mountpoint for S3. Credentials come from the environment
(oprobe sets AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN and AWS_REGION from
the target when it has them), else the CRT's default chain.

Phases come from the meta request's transfer callbacks: `ttfb` is the time to the response
headers, `body` the transfer after them. The CRT splits large GETs and PUTs into parallel
ranged requests and retries internally; it reports neither, so one result stands for the
whole meta request and `attempts` is 1.
"""

import base64
import io
import json
import os
import sys
import threading
import time
import urllib.parse
import xml.etree.ElementTree as ET

import awscrt
from awscrt.auth import AwsCredentialsProvider
from awscrt.http import HttpHeaders, HttpRequest
from awscrt.io import ClientBootstrap, ClientTlsContext, DefaultHostResolver, EventLoopGroup, TlsContextOptions
from awscrt.s3 import S3Client, S3RequestTlsMode, S3RequestType, create_default_s3_signing_config

PROTOCOL = "oprobe.client/1"
OUT = threading.Lock()


def now_us():
    return time.monotonic_ns() // 1000


def emit(obj):
    line = json.dumps(obj, separators=(",", ":"))
    with OUT:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


class Crt:
    def __init__(self, hello):
        t = hello["target"]
        self.bucket = t.get("bucket") or ""
        self.lock = threading.Lock()
        self.pending = 0
        region = t.get("region") or os.environ.get("AWS_REGION") or "us-east-1"
        ep = t.get("endpoint")
        if ep:
            u = urllib.parse.urlsplit(ep)
            self.https = u.scheme == "https"
            self.host = u.netloc
            self.path_style = True
        else:
            self.https = True
            self.path_style = bool(t.get("path_style"))
            self.host = (f"s3.{region}.amazonaws.com" if self.path_style
                         else f"{self.bucket}.s3.{region}.amazonaws.com")
        elg = EventLoopGroup()
        bootstrap = ClientBootstrap(elg, DefaultHostResolver(elg))
        if os.environ.get("AWS_ACCESS_KEY_ID") and os.environ.get("AWS_SECRET_ACCESS_KEY"):
            creds = AwsCredentialsProvider.new_static(
                os.environ["AWS_ACCESS_KEY_ID"],
                os.environ["AWS_SECRET_ACCESS_KEY"],
                os.environ.get("AWS_SESSION_TOKEN"),
            )
        else:
            creds = AwsCredentialsProvider.new_default_chain(bootstrap)
        kwargs = {}
        if self.https and (t.get("insecure") or t.get("ca_cert_file")):
            opts = TlsContextOptions()
            if t.get("insecure"):
                opts.verify_peer = False
            if t.get("ca_cert_file"):
                opts.override_default_trust_store_from_path(None, t["ca_cert_file"])
            kwargs["tls_connection_options"] = ClientTlsContext(opts).new_connection_options()
        self.client = S3Client(
            bootstrap=bootstrap,
            region=region,
            signing_config=create_default_s3_signing_config(region=region, credential_provider=creds),
            tls_mode=S3RequestTlsMode.ENABLED if self.https else S3RequestTlsMode.DISABLED,
            **kwargs,
        )

    def path(self, key="", query=None):
        p = ("/" + self.bucket) if self.path_style else ""
        if key:
            p += "/" + urllib.parse.quote(key, safe="/-_.~")
        if not p:
            p = "/"
        if query:
            p += "?" + urllib.parse.urlencode(query)
        return p

    def request(self, op, rtype, method, path, headers=(), body=b"", operation=None, done=None):
        """Start one meta request; `done(status, headers, body_bytes, timing, error)` runs when
        it finishes (on a CRT thread)."""
        hs = HttpHeaders([("Host", self.host), ("Content-Length", str(len(body)))] + list(headers))
        req = HttpRequest(method, path, hs, io.BytesIO(body) if body else None)
        st = {"t0": now_us(), "th": None, "headers": [], "chunks": []}

        def on_headers(status_code, headers, **_):
            st["th"] = now_us()
            st["headers"] = headers

        def on_body(chunk, **_):
            st["chunks"].append(bytes(chunk))

        def on_done(error=None, error_headers=None, error_body=None, status_code=None, **_):
            t1 = now_us()
            th = st["th"] or t1
            timing = {"t_start_us": st["t0"], "t_end_us": t1,
                      "phases": {"ttfb_us": th - st["t0"], "body_us": t1 - th}}
            hdrs = dict((k.lower(), v) for k, v in (error_headers or st["headers"] or []))
            data = error_body if error_body is not None else b"".join(st["chunks"])
            try:
                done(int(status_code or 0), hdrs, data, timing, error)
            except Exception as e:  # never leave oprobe waiting for an answer
                emit({"id": op["id"], "ok": False, "status": 0,
                      "error": {"code": type(e).__name__, "message": str(e)}})
            finally:
                with self.lock:
                    self.pending -= 1

        kw = {"operation_name": operation} if operation else {}
        with self.lock:
            self.pending += 1
        self.client.make_request(type=rtype, request=req, on_headers=on_headers,
                                 on_body=on_body, on_done=on_done, **kw)


def error_of(status, data, error):
    code, msg = "", str(error or "")
    try:
        root = ET.fromstring(data or b"")
        code = root.findtext("Code") or ""
        msg = root.findtext("Message") or msg
    except ET.ParseError:
        pass
    return {"code": code or (f"HTTP{status}" if status else type(error).__name__), "message": msg}


def reply(op, status, timing, error=None, data=b"", **extra):
    ok = error is None and 200 <= status < 300
    r = {"id": op["id"], "ok": ok, "status": status, "attempts_count": 1, "http_version": "http1", **timing}
    if not ok:
        r["error"] = error_of(status, data, error)
    r.update(extra)
    emit(r)


def do(crt, op):
    kind = op["op"]
    if kind == "get":
        headers = []
        rg = op.get("range")
        if rg:
            if rg.get("suffix_bytes") is not None:
                headers.append(("Range", f"bytes=-{rg['suffix_bytes']}"))
            else:
                off, ln = rg.get("offset_bytes", 0), rg.get("length_bytes")
                headers.append(("Range", f"bytes={off}-" + (str(off + ln - 1) if ln else "")))

        def done(status, hdrs, data, timing, error):
            size = len(data)
            cr = hdrs.get("content-range")
            if cr and "/" in cr:
                size = int(cr.rsplit("/", 1)[1])
            reply(op, status, timing, error, data, bytes=len(data) if error is None else 0,
                  size_bytes=size, etag=hdrs.get("etag"))
        crt.request(op, S3RequestType.GET_OBJECT, "GET", crt.path(op["key"]), headers, done=done)
    elif kind == "put":
        body = base64.b64decode(op.get("data_b64") or "")
        headers = []
        for h, k in (("If-Match", "if_match"), ("If-None-Match", "if_none_match"), ("Content-Type", "content_type")):
            if op.get(k):
                headers.append((h, op[k]))
        for k, v in (op.get("metadata") or {}).items():
            headers.append(("x-amz-meta-" + k, v))
        crt.request(op, S3RequestType.PUT_OBJECT, "PUT", crt.path(op["key"]), headers, body,
                    done=lambda s, h, d, t, e: reply(op, s, t, e, d, bytes=len(body), etag=h.get("etag")))
    elif kind == "head":
        crt.request(op, S3RequestType.DEFAULT, "HEAD", crt.path(op["key"]), operation="HeadObject",
                    done=lambda s, h, d, t, e: reply(op, s, t, e, d, size_bytes=int(h.get("content-length") or 0),
                                                     etag=h.get("etag")))
    elif kind == "delete":
        keys = list(op.get("keys") or [])
        state = {"left": len(keys), "failed": [], "t0": now_us(), "status": 204, "lock": threading.Lock()}
        if not keys:
            return reply(op, 204, {"t_start_us": now_us(), "t_end_us": now_us()})

        def one(key):
            def done(status, hdrs, data, timing, error):
                with state["lock"]:
                    if error is not None and status != 404:
                        state["failed"].append(key)
                        state["status"] = status
                    state["left"] -= 1
                    last = state["left"] == 0
                if last:
                    reply(op, 204 if not state["failed"] else state["status"],
                          {"t_start_us": state["t0"], "t_end_us": now_us()},
                          requests_count=len(keys), failed_keys=state["failed"])
            crt.request(op, S3RequestType.DEFAULT, "DELETE", crt.path(key), operation="DeleteObject", done=done)
        for k in keys:
            one(k)
    elif kind == "list":
        acc = {"entries": [], "pages": 0, "t0": now_us()}

        def page(token):
            q = {"list-type": "2", "prefix": op.get("prefix") or ""}
            if not op.get("recursive"):
                q["delimiter"] = "/"
            if op.get("limit"):
                q["max-keys"] = str(op["limit"])
            if token:
                q["continuation-token"] = token

            def done(status, hdrs, data, timing, error):
                acc["pages"] += 1
                timing = dict(timing, t_start_us=acc["t0"])
                if error is not None or not 200 <= status < 300:
                    return reply(op, status, timing, error or "list failed", data, requests_count=acc["pages"])
                root = ET.fromstring(data)
                ns = root.tag.split("}")[0] + "}" if root.tag.startswith("{") else ""
                for c in root.findall(f"{ns}Contents"):
                    acc["entries"].append({"key": c.findtext(f"{ns}Key"), "size_bytes": int(c.findtext(f"{ns}Size") or 0)})
                for p in root.findall(f"{ns}CommonPrefixes"):
                    acc["entries"].append({"key": p.findtext(f"{ns}Prefix"), "dir": True})
                nxt = root.findtext(f"{ns}NextContinuationToken")
                if (root.findtext(f"{ns}IsTruncated") or "false") == "true" and nxt:
                    return page(nxt)
                reply(op, status, timing, requests_count=acc["pages"], entries=acc["entries"])
            crt.request(op, S3RequestType.DEFAULT, "GET", crt.path(query=q), operation="ListObjectsV2", done=done)
        page(None)
    elif kind == "copy":
        src = "/" + crt.bucket + "/" + urllib.parse.quote(op["source"], safe="/-_.~")
        crt.request(op, S3RequestType.DEFAULT, "PUT", crt.path(op["key"]), [("x-amz-copy-source", src)],
                    operation="CopyObject", done=lambda s, h, d, t, e: reply(op, s, t, e, d))
    else:
        emit({"id": op["id"], "ok": False, "status": 0, "error": {"code": "Unsupported", "message": kind}})


def main():
    first = sys.stdin.readline()
    if not first:
        return
    hello = json.loads(first)
    if not str(hello.get("protocol", "")).startswith("oprobe.client/1"):
        print(f"unsupported protocol {hello.get('protocol')}", file=sys.stderr)
        sys.exit(2)
    crt = Crt(hello)
    emit({
        "type": "hello",
        "protocol": PROTOCOL,
        "client": "awscrt",
        "version": getattr(awscrt, "__version__", None) or "unknown",
        "phases": ["ttfb", "body"],
        "ops": ["get", "put", "head", "delete", "list", "copy"],
        "features": ["if_match", "if_none_match", "metadata"],
    })
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        msg = json.loads(line)
        if msg.get("type") == "shutdown":
            break
        try:
            do(crt, msg)
        except Exception as e:  # never leave oprobe waiting for an answer
            emit({"id": msg.get("id", 0), "ok": False, "status": 0,
                  "error": {"code": type(e).__name__, "message": str(e)}})
    # Let in-flight requests finish before exiting.
    deadline = time.monotonic() + 30
    while crt.pending > 0 and time.monotonic() < deadline:
        time.sleep(0.01)


if __name__ == "__main__":
    main()
