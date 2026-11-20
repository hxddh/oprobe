# Cost model

oprobe prices **what it measured**, scaled to the volume you declare. No traffic model is assumed.

1. **Per-operation usage** from the measurement window: backend requests per logical operation
   by billing class, bytes downloaded and uploaded per operation, failed/throttled requests,
   over-read bytes, and GETs/bytes served by the cache.
2. **Billing class** of each request comes from the real request line:

   | Class | APIs |
   |---|---|
   | write | PutObject, CreateMultipartUpload, UploadPart, CompleteMultipartUpload, CopyObject |
   | list | ListObjects (S3 ListObjectsV2, GCS objects.list, Azure List Blobs, B2 list_file_names) |
   | read | GetObject, HeadObject, other GETs |
   | delete | DeleteObject(s), AbortMultipartUpload |
   | free | control-plane calls (e.g. B2 authorize / get_upload_url) |

3. **Projection**: `monthly cost = per-op usage × [cost].monthly_ops × prices + stored GB × storage price`.
   Internet egress applies only to the `egress_fraction` of downloaded bytes (default 0: compute
   in the same region).

## When cost is projected

A price table is applied only when it is known to be the right one:

* the endpoint is a recognised cloud storage endpoint (`*.amazonaws.com`,
  `storage.googleapis.com`, `*.blob.core.windows.net`, `*.r2.cloudflarestorage.com`,
  `*.backblazeb2.com`, `*.aliyuncs.com`, or the scheme's implied endpoint for `gcs`, `azblob`,
  `oss`, `b2`), or
* the profile chose one explicitly (`[cost].provider` / `--provider`, or `[cost.pricing]`).

Everything else — `memory://`, `fs://`, localhost, private addresses, MinIO, Ceph or any unknown
endpoint — gets no projection: the report's `cost.applicable` is `false` with a
`not_applicable_reason`, the terminal report omits the COST section, `oprobe diff` skips the cost
check, and `oprobe tune --objective cost` refuses to run. The measured per-operation usage
(`cost.per_op`) is still reported.

## Price presets (list prices, as of 2025-06)

| Key | Storage $/GB-mo | Write $/M | List $/M | Read $/M | Egress $/GB |
|---|---:|---:|---:|---:|---:|
| `aws-s3` | 0.023 | 5.00 | 5.00 | 0.40 | 0.09 |
| `gcs` | 0.020 | 5.00 | 5.00 | 0.40 | 0.12 |
| `azure-blob` | 0.0184 | 6.50 | 6.50 | 0.50 | 0.087 |
| `cloudflare-r2` | 0.015 | 4.50 | 4.50 | 0.36 | 0 |
| `backblaze-b2` | 0.006 | 0 | 4.00 | 0.40 | 0.01 |
| `aliyun-oss` | 0.017 | 1.00 | 1.00 | 1.00 | 0.074 |

Prices change and differ by region, tier and contract: override them.

```toml
[cost]
monthly_ops = 250000000
stored_gib = 20480
egress_fraction = 0.1

[cost.pricing]
provider = "S3 (our EDP rate)"
storage_gb_month = 0.018
write_per_million = 4.0
list_per_million = 4.0
read_per_million = 0.32
egress_per_gb = 0.05
```

## Waste and savings

* **failed/throttled requests** — requests that returned 429/503/5xx/4xx (except 404) or failed on
  the network, priced at the average request price; most providers bill them and they add no value.
* **over-read egress** — bytes downloaded but not requested (range coalescing, chunked reads).
* **cache savings** — GETs and bytes the local cache answered instead of the backend.

The comparison table applies the same measured usage to every preset, answering "what would this
exact workload cost on R2 / GCS / B2?". Latency and throughput of other providers are not implied —
measure them with `oprobe run` against each provider.
