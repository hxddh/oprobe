# Privacy

**oprobe sends no telemetry.** No usage statistics, crash reports or analytics, and no setting to
turn any on. Neither does `oprobe-trace`.

## What oprobe connects to

| When | Where |
|---|---|
| `run`, `prove`, the app | the storage endpoint of the operator you give it (and your proxy, if `HTTPS_PROXY` or `http.proxy` is set); that cloud's credential providers (instance metadata, STS, SSO) when the credential chain needs them |
| `demo`, `lab`, `lab://…` operators | nothing: the lab runs inside oprobe on 127.0.0.1 |
| `show`, `show --list`, `show --rm` | nothing |

Nothing else. oprobe never checks for updates by itself; the install scripts talk to GitHub when
you run them.

## What oprobe stores

- **Runs** in `<data dir>/oprobe/runs/` (or `$OPROBE_DATA_DIR`): the report, the operator spec,
  the workload and one row per operation and request. Object keys, request IDs and remote IP
  addresses are part of a run. Secrets are not: credential options (`secret_access_key`,
  `account_key`, `sas_token`, `password`, `*_key`, …) are redacted before anything is printed or
  saved. Every text loses the credentials it carries, in an operator, an option, a proxy
  setting or the text of an error ([what is removed](#what-is-removed-from-a-text)). Errors,
  warnings and log events are printed the same way.
- **The host** is recorded as OS, architecture, CPU count and a hash of the host name, never the
  name itself.
- **Pending-cleanup markers** in `<data dir>/oprobe/pending/`: while a run that writes is going,
  one small file names its target and its `oprobe-run-*` prefix. It is removed once the test data
  is deleted; one left by a killed run tells the next run against that target what to delete.
- **The config file** holds what you put there.

Delete runs with `oprobe show --rm RUN…`, or remove the data directory.

## What is removed from a text

One function redacts every text before it is recorded, stored or printed
(`oprobe_trace::util::strip_url_credentials`). It removes:

- **`user:password@`** of every URL, also when the password holds a `/`, `?` or `#` (an AWS
  secret key typed into a URL unescaped).
- **Secret parameters**, by name and whatever their case: `*signature*` (`X-Amz-Signature`,
  `X-Goog-Signature`, `x-oss-signature`, `q-signature`), `sig` and `*_sig` (an Azure SAS, a
  Swift `temp_url_sig`), `*token*`, `*credential*`, access key ids (`AWSAccessKeyId`, …),
  `upload_id` (the session of a GCS resumable upload is a bearer credential), `*secret*`,
  `*password*`, `*session*`, `*authorization*`, `*jwt*`, `*hmac*`, `key`, `*_key`, `apikey`.
  They are removed from a URL's query, from its fragment (`#access_token=…`), from a query
  with no URL around it (`GET /b/k?X-Amz-Signature=…`), from a bare parameter list (an SAS
  token `sv=…&sig=…`) and from a percent-encoded URL passed as a value
  (`?next=https%3A%2F%2F…%3Fsig%3D…`). The other parameters stay, in order.
- **The value of a lone `name=value`** with such a name (the `Signature=…` and `Credential=…`
  of a quoted `Authorization` header, `password=…`): it reads `name=<redacted>`.
- **The value of an `Authorization` header** quoted in a text (`Authorization: Bearer …`).

What only describes a request stays readable: `X-Amz-Expires`, `X-Amz-Date`, an SAS's `se` and
`sp`; the cursors and key ranges of a listing (`continuation-token`, `pageToken`, `start_key`,
`end_key`, `partition-key`, `marker`), which name a position and grant nothing; S3's `uploadId`;
switches such as `skip_signature=true`; an environment reference (`${AWS_SECRET_ACCESS_KEY}`);
and a bare `key=…` outside a parameter list, which names an object. A name that only looks
like a secret's is redacted.

What it cannot know: a credential in none of these forms (a bare token in a path or a
sentence, a JSON field), and a password before whose first `/` stands only a port number
(`user:8080/…@host` reads as `host:port/path`). Percent-encode a password you put in a URL, or
better, give credentials as options or through the environment.

## `.oprobe` files from `oprobe-trace`

`Handle::save` writes what the layer saw in your process: the aggregates and a sample of
operations and their requests, with their API, method, status, timing, bytes, connection and
request ID. Object keys are never stored: an operation's key is its first path segment (at most
32 bytes) and a hash of the whole key (`logs/#3f2a…`), so a top-level directory name can be in
the file, but not a key. Request IDs and remote addresses are. Credentials never are: the
text of a failed request or operation loses the credentials it carries
([what is removed](#what-is-removed-from-a-text): the signature and token of a presigned or SAS
URL included, with or without a URL around them) when it is recorded, so neither an
`Exporter`, a `Snapshot` nor a file ever holds them. Review a file before you attach it to an
issue.

## Test data in your bucket

`oprobe run` writes under `<prefix>/oprobe-run-<id>/` and deletes it at the end, verifying the
prefix is empty. `--read-only` and `--existing` never write; `--dry-run` sends nothing. A run
killed hard (a second Ctrl-C, OOM, power loss) can leave its prefix behind; delete it with your
usual tools.
