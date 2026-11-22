#!/usr/bin/env python3
"""oprobe client adapter for boto3 (protocol `oprobe.client/1`, see docs/clients.md).

    pip install boto3
    oprobe run s3://bucket/prefix --client "exec:python3 adapters/boto3/oprobe_boto3.py"

Credentials and region come from the environment (oprobe sets AWS_ACCESS_KEY_ID,
AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN and AWS_REGION from the target when it has them;
otherwise boto3's own chain applies: profiles, SSO, instance roles).

Phases come from botocore's event hooks: `before-send` marks the moment a request leaves
the client and `response-received` the response headers (ttfb); reading the streaming body
is `body`. botocore does not expose connection setup, so `connect` is not reported (oprobe
suppresses the findings that need it). Every attempt botocore makes is reported in
`attempt_log` with its status.
"""

import base64
import json
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor

import boto3
import botocore
from botocore.config import Config
from botocore.exceptions import BotoCoreError, ClientError

PROTOCOL = "oprobe.client/1"
OUT = threading.Lock()
TLS = threading.local()


def now_us():
    return time.monotonic_ns() // 1000


def emit(obj):
    line = json.dumps(obj, separators=(",", ":"))
    with OUT:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def attempts():
    log = getattr(TLS, "log", None)
    if log is None:
        log = TLS.log = []
    return log


def on_before_send(request, **_):
    attempts().append({"t_start_us": now_us(), "status": 0, "phases": {}})


def on_response_received(response_dict=None, exception=None, **_):
    log = attempts()
    if not log:
        return
    a = log[-1]
    t = now_us()
    a["t_end_us"] = t
    a["phases"]["ttfb_us"] = t - a["t_start_us"]
    if response_dict is not None:
        a["status"] = int(response_dict.get("status_code") or 0)
    if exception is not None:
        a["error"] = f"{type(exception).__name__}: {exception}"


def make_client(hello):
    t = hello["target"]
    opts = dict(
        retries={"max_attempts": int(hello.get("retries_count", 0)) + 1, "mode": "standard"},
        max_pool_connections=max(10, int(hello.get("concurrency_count", 10))),
        connect_timeout=10,
        read_timeout=max(1, hello.get("timeout_ms", 30000) / 1000),
        s3={"addressing_style": "path" if t.get("path_style") else "auto"},
    )
    try:
        # botocore >= 1.36 adds checksums to every upload unless told otherwise; the OpenDAL
        # client sends none, so neither does this one.
        cfg = Config(request_checksum_calculation="when_required",
                     response_checksum_validation="when_required", **opts)
    except TypeError:
        cfg = Config(**opts)
    verify = False if t.get("insecure") else (t.get("ca_cert_file") or None)
    s3 = boto3.session.Session().client(
        "s3",
        endpoint_url=t.get("endpoint"),
        region_name=t.get("region"),
        config=cfg,
        verify=verify,
    )
    s3.meta.events.register("before-send.s3.*", on_before_send)
    s3.meta.events.register("response-received.s3.*", on_response_received)
    return s3, t.get("bucket")


def finish(op, status, extra=None, error=None, body_us=0):
    log = attempts()
    TLS.log = []
    t = now_us()
    if not log:
        log = [{"t_start_us": t, "t_end_us": t, "status": status, "phases": {}}]
    for a in log:
        a.setdefault("t_end_us", t)
    log[-1]["t_end_us"] = log[-1]["t_end_us"] + body_us
    if body_us:
        log[-1]["phases"]["body_us"] = body_us
    if status:
        log[-1]["status"] = status
    r = {
        "id": op["id"],
        "ok": error is None,
        "status": log[-1]["status"],
        "t_start_us": log[0]["t_start_us"],
        "t_end_us": log[-1]["t_end_us"],
        "phases": log[-1]["phases"],
        "attempts": len([a for a in log if a.get("status", 0) or a.get("error")]) or 1,
        "http_version": "http1",
    }
    if len(log) > 1:
        r["attempt_log"] = log
    if error:
        r["error"] = error
    r.update(extra or {})
    return r


def client_error(e):
    if isinstance(e, ClientError):
        err = e.response.get("Error", {})
        status = int(e.response.get("ResponseMetadata", {}).get("HTTPStatusCode") or 0)
        code = err.get("Code") or (f"HTTP{status}" if status else "ClientError")
        if code == "404":
            code = "NoSuchKey"
        return status, {"code": code, "message": err.get("Message", "")}
    return 0, {"code": type(e).__name__, "message": str(e)}


def do(s3, bucket, op):
    kind = op["op"]
    TLS.log = []
    try:
        if kind == "get":
            args = {"Bucket": bucket, "Key": op["key"]}
            rg = op.get("range")
            if rg:
                if rg.get("suffix") is not None:
                    args["Range"] = f"bytes=-{rg['suffix']}"
                else:
                    off, ln = rg.get("offset", 0), rg.get("length")
                    args["Range"] = f"bytes={off}-" + (str(off + ln - 1) if ln else "")
            resp = s3.get_object(**args)
            t0 = now_us()
            data = resp["Body"].read()
            body_us = now_us() - t0
            size = len(data)
            cr = resp.get("ContentRange")
            if cr and "/" in cr:
                size = int(cr.rsplit("/", 1)[1])
            return finish(op, resp["ResponseMetadata"]["HTTPStatusCode"],
                          {"bytes": len(data), "size": size, "etag": resp.get("ETag")}, body_us=body_us)
        if kind == "put":
            body = base64.b64decode(op.get("data_b64") or "")
            args = {"Bucket": bucket, "Key": op["key"], "Body": body}
            if op.get("if_match"):
                args["IfMatch"] = op["if_match"]
            if op.get("if_none_match"):
                args["IfNoneMatch"] = op["if_none_match"]
            if op.get("content_type"):
                args["ContentType"] = op["content_type"]
            if op.get("metadata"):
                args["Metadata"] = op["metadata"]
            resp = s3.put_object(**args)
            return finish(op, resp["ResponseMetadata"]["HTTPStatusCode"],
                          {"bytes": len(body), "etag": resp.get("ETag")})
        if kind == "head":
            resp = s3.head_object(Bucket=bucket, Key=op["key"])
            return finish(op, resp["ResponseMetadata"]["HTTPStatusCode"],
                          {"size": resp.get("ContentLength", 0), "etag": resp.get("ETag")})
        if kind == "delete":
            keys = op.get("keys") or []
            if len(keys) == 1:
                resp = s3.delete_object(Bucket=bucket, Key=keys[0])
                return finish(op, resp["ResponseMetadata"]["HTTPStatusCode"])
            resp = s3.delete_objects(Bucket=bucket, Delete={"Objects": [{"Key": k} for k in keys], "Quiet": True})
            failed = [e.get("Key") for e in resp.get("Errors", [])]
            return finish(op, resp["ResponseMetadata"]["HTTPStatusCode"], {"failed_keys": failed})
        if kind == "list":
            args = {"Bucket": bucket, "Prefix": op.get("prefix") or ""}
            if not op.get("recursive"):
                args["Delimiter"] = "/"
            if op.get("limit"):
                args["PaginationConfig"] = {"PageSize": int(op["limit"])}
            entries, pages, status = [], 0, 200
            for page in s3.get_paginator("list_objects_v2").paginate(**args):
                pages += 1
                status = page["ResponseMetadata"]["HTTPStatusCode"]
                entries += [{"key": o["Key"], "size": o.get("Size", 0)} for o in page.get("Contents", [])]
                entries += [{"key": p["Prefix"], "dir": True} for p in page.get("CommonPrefixes", [])]
            r = finish(op, status, {"entries": entries, "requests": max(1, pages)})
            r.pop("attempt_log", None)
            return r
        if kind == "copy":
            resp = s3.copy_object(Bucket=bucket, Key=op["key"], CopySource={"Bucket": bucket, "Key": op["source"]})
            etag = (resp.get("CopyObjectResult") or {}).get("ETag")
            return finish(op, resp["ResponseMetadata"]["HTTPStatusCode"], {"etag": etag})
        return finish(op, 0, error={"code": "Unsupported", "message": kind})
    except (ClientError, BotoCoreError) as e:
        status, err = client_error(e)
        return finish(op, status, error=err)


def main():
    first = sys.stdin.readline()
    if not first:
        return
    hello = json.loads(first)
    if not str(hello.get("protocol", "")).startswith("oprobe.client/1"):
        print(f"unsupported protocol {hello.get('protocol')}", file=sys.stderr)
        sys.exit(2)
    s3, bucket = make_client(hello)
    emit({
        "type": "hello",
        "protocol": PROTOCOL,
        "client": "boto3",
        "version": f"{boto3.__version__} (botocore {botocore.__version__})",
        "phases": ["ttfb", "body"],
        "ops": ["get", "put", "head", "delete", "list", "copy"],
        "features": ["if_match", "if_none_match", "metadata", "batch_delete"],
    })
    pool = ThreadPoolExecutor(max_workers=max(1, int(hello.get("concurrency_count", 1))))

    def run(op):
        try:
            emit(do(s3, bucket, op))
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
