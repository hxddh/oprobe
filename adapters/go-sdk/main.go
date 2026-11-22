// Command oprobe-go-sdk is an oprobe client adapter for aws-sdk-go-v2 (protocol
// `oprobe.client/1`, see docs/clients.md).
//
//	cd adapters/go-sdk && go build -o oprobe-go-sdk .
//	oprobe run s3://bucket/prefix --client exec:adapters/go-sdk/oprobe-go-sdk
//
// Credentials and region come from the environment (oprobe sets AWS_ACCESS_KEY_ID,
// AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN and AWS_REGION from the target when it has them),
// else the SDK's default chain.
//
// Phases come from net/http/httptrace on every attempt: connection reuse or DNS, TCP and
// TLS on fresh connections, request write (send), time to the first response byte (ttfb),
// and the body read. Each attempt the SDK's retryer makes is reported in attempt_log.
package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptrace"
	"net/url"
	"os"
	"runtime/debug"
	"strings"
	"sync"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	awshttp "github.com/aws/aws-sdk-go-v2/aws/transport/http"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/s3/types"
	"github.com/aws/smithy-go"
)

const protocol = "oprobe.client/1"

var (
	outMu sync.Mutex
	out   = bufio.NewWriter(os.Stdout)
	t0    = time.Now()
)

func nowUS() uint64 { return uint64(time.Since(t0).Microseconds()) }

func emit(v any) {
	b, _ := json.Marshal(v)
	outMu.Lock()
	out.Write(b)
	out.WriteByte('\n')
	out.Flush()
	outMu.Unlock()
}

type target struct {
	Bucket     string `json:"bucket"`
	Endpoint   string `json:"endpoint"`
	Region     string `json:"region"`
	PathStyle  bool   `json:"path_style"`
	Insecure   bool   `json:"insecure"`
	CaCertFile string `json:"ca_cert_file"`
}

type hello struct {
	Protocol    string `json:"protocol"`
	Target      target `json:"target"`
	Concurrency int    `json:"concurrency_count"`
	Retries     int    `json:"retries_count"`
	TimeoutMS   int64  `json:"timeout_ms"`
}

type wireRange struct {
	Offset *uint64 `json:"offset_bytes"`
	Length *uint64 `json:"length_bytes"`
	Suffix *uint64 `json:"suffix_bytes"`
}

type op struct {
	ID          uint64            `json:"id"`
	Op          string            `json:"op"`
	Type        string            `json:"type"`
	Key         string            `json:"key"`
	Keys        []string          `json:"keys"`
	Source      string            `json:"source"`
	Range       *wireRange        `json:"range"`
	DataB64     string            `json:"data_b64"`
	IfMatch     string            `json:"if_match"`
	IfNoneMatch string            `json:"if_none_match"`
	ContentType string            `json:"content_type"`
	Metadata    map[string]string `json:"metadata"`
	Prefix      string            `json:"prefix"`
	Recursive   bool              `json:"recursive"`
	Limit       int32             `json:"limit"`
}

type phases struct {
	QueueUS uint64 `json:"queue_us,omitempty"`
	DNSUS   uint64 `json:"dns_us,omitempty"`
	TCPUS   uint64 `json:"tcp_us,omitempty"`
	TLSUS   uint64 `json:"tls_us,omitempty"`
	SendUS  uint64 `json:"send_us,omitempty"`
	TTFBUS  uint64 `json:"ttfb_us,omitempty"`
	BodyUS  uint64 `json:"body_us,omitempty"`
}

type attempt struct {
	Status   int     `json:"status"`
	TStartUS uint64  `json:"t_start_us"`
	TEndUS   uint64  `json:"t_end_us"`
	Phases   *phases `json:"phases,omitempty"`
	Error    string  `json:"error,omitempty"`
}

type entry struct {
	Key  string `json:"key"`
	Size int64  `json:"size_bytes"`
	Dir  bool   `json:"dir,omitempty"`
}

type wireErr struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

type result struct {
	ID          uint64    `json:"id"`
	OK          bool      `json:"ok"`
	Status      int       `json:"status"`
	Bytes       uint64    `json:"bytes"`
	Size        *uint64   `json:"size_bytes,omitempty"`
	ETag        string    `json:"etag,omitempty"`
	TStartUS    uint64    `json:"t_start_us"`
	TEndUS      uint64    `json:"t_end_us"`
	Phases      *phases   `json:"phases,omitempty"`
	Attempts    int       `json:"attempts_count"`
	Requests    int       `json:"requests_count"`
	AttemptLog  []attempt `json:"attempt_log,omitempty"`
	Entries     []entry   `json:"entries,omitempty"`
	FailedKeys  []string  `json:"failed_keys,omitempty"`
	HTTPVersion string    `json:"http_version,omitempty"`
	Error       *wireErr  `json:"error,omitempty"`
}

// recorder collects the attempts of one operation (one per HTTP request).
type recorder struct {
	mu       sync.Mutex
	attempts []attempt
	proto    string
}

type recKey struct{}

// tracedTransport times every request with httptrace and records it in the operation's
// recorder (found in the request context).
type tracedTransport struct{ inner http.RoundTripper }

func (t tracedTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	rec, _ := req.Context().Value(recKey{}).(*recorder)
	if rec == nil {
		return t.inner.RoundTrip(req)
	}
	var (
		start                = nowUS()
		dnsStart, connStart  uint64
		tlsStart, wroteStart uint64
		p                    phases
	)
	tr := &httptrace.ClientTrace{
		DNSStart:          func(httptrace.DNSStartInfo) { dnsStart = nowUS() },
		DNSDone:           func(httptrace.DNSDoneInfo) { p.DNSUS = nowUS() - dnsStart },
		ConnectStart:      func(string, string) { connStart = nowUS() },
		ConnectDone:       func(string, string, error) { p.TCPUS = nowUS() - connStart },
		TLSHandshakeStart: func() { tlsStart = nowUS() },
		TLSHandshakeDone:  func(tls.ConnectionState, error) { p.TLSUS = nowUS() - tlsStart },
		GotConn: func(httptrace.GotConnInfo) {
			wroteStart = nowUS()
			p.QueueUS = wroteStart - start - p.DNSUS - p.TCPUS - p.TLSUS
		},
		WroteRequest: func(httptrace.WroteRequestInfo) {
			now := nowUS()
			p.SendUS = now - wroteStart
			wroteStart = now
		},
		GotFirstResponseByte: func() { p.TTFBUS = nowUS() - wroteStart },
	}
	req = req.WithContext(httptrace.WithClientTrace(req.Context(), tr))
	resp, err := t.inner.RoundTrip(req)
	a := attempt{TStartUS: start}
	if err != nil {
		a.TEndUS, a.Error = nowUS(), err.Error()
		a.Phases = &p
		rec.add(a, "")
		return nil, err
	}
	// Read the body now so its transfer is this attempt's `body` stage.
	t1 := nowUS()
	body, rerr := io.ReadAll(resp.Body)
	resp.Body.Close()
	p.BodyUS = nowUS() - t1
	resp.Body = io.NopCloser(bytes.NewReader(body))
	a.Status, a.TEndUS, a.Phases = resp.StatusCode, nowUS(), &p
	if rerr != nil {
		a.Error = rerr.Error()
	}
	proto := "http1"
	if resp.ProtoMajor == 2 {
		proto = "http2"
	}
	rec.add(a, proto)
	return resp, rerr
}

func (r *recorder) add(a attempt, proto string) {
	r.mu.Lock()
	r.attempts = append(r.attempts, a)
	if proto != "" {
		r.proto = proto
	}
	r.mu.Unlock()
}

// sdkVersion is the linked aws-sdk-go-v2 S3 module version.
func sdkVersion() string {
	if bi, ok := debug.ReadBuildInfo(); ok {
		for _, d := range bi.Deps {
			if d.Path == "github.com/aws/aws-sdk-go-v2/service/s3" {
				return "service/s3 " + d.Version
			}
		}
	}
	return "service/s3 (unknown version)"
}

func lastStatus(r *recorder) int {
	r.mu.Lock()
	defer r.mu.Unlock()
	if len(r.attempts) == 0 {
		return 0
	}
	return r.attempts[len(r.attempts)-1].Status
}

func errorOf(err error) (int, *wireErr) {
	status := 0
	var re *awshttp.ResponseError
	if errors.As(err, &re) {
		status = re.HTTPStatusCode()
	}
	var ae smithy.APIError
	if errors.As(err, &ae) {
		return status, &wireErr{Code: ae.ErrorCode(), Message: ae.ErrorMessage()}
	}
	return status, &wireErr{Code: fmt.Sprintf("%T", errors.Unwrap(err)), Message: err.Error()}
}

func finish(o op, rec *recorder, err error, r result) result {
	r.ID = o.ID
	rec.mu.Lock()
	log := append([]attempt(nil), rec.attempts...)
	r.HTTPVersion = rec.proto
	rec.mu.Unlock()
	now := nowUS()
	if len(log) == 0 {
		log = []attempt{{TStartUS: now, TEndUS: now}}
	}
	r.TStartUS, r.TEndUS = log[0].TStartUS, log[len(log)-1].TEndUS
	r.Phases = log[len(log)-1].Phases
	r.Status = log[len(log)-1].Status
	if r.Requests == 0 {
		r.Requests = 1
		r.Attempts = len(log)
		if len(log) > 1 {
			r.AttemptLog = log
		}
	} else {
		r.Attempts = 1
	}
	if err != nil {
		st, we := errorOf(err)
		if st != 0 {
			r.Status = st
		}
		r.OK, r.Error = false, we
	} else {
		r.OK = true
	}
	return r
}

func rangeHeader(rg *wireRange) *string {
	if rg == nil {
		return nil
	}
	if rg.Suffix != nil {
		return aws.String(fmt.Sprintf("bytes=-%d", *rg.Suffix))
	}
	var off uint64
	if rg.Offset != nil {
		off = *rg.Offset
	}
	if rg.Length != nil {
		return aws.String(fmt.Sprintf("bytes=%d-%d", off, off+*rg.Length-1))
	}
	return aws.String(fmt.Sprintf("bytes=%d-", off))
}

func optional(s string) *string {
	if s == "" {
		return nil
	}
	return aws.String(s)
}

func do(ctx context.Context, c *s3.Client, bucket string, o op) result {
	rec := &recorder{}
	ctx = context.WithValue(ctx, recKey{}, rec)
	switch o.Op {
	case "get":
		resp, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: &bucket, Key: &o.Key, Range: rangeHeader(o.Range)})
		if err != nil {
			return finish(o, rec, err, result{})
		}
		data, err := io.ReadAll(resp.Body)
		resp.Body.Close()
		size := uint64(len(data))
		if cr := aws.ToString(resp.ContentRange); strings.Contains(cr, "/") {
			fmt.Sscanf(cr[strings.LastIndex(cr, "/")+1:], "%d", &size)
		}
		return finish(o, rec, err, result{Bytes: uint64(len(data)), Size: &size, ETag: aws.ToString(resp.ETag)})
	case "put":
		data, _ := base64.StdEncoding.DecodeString(o.DataB64)
		in := &s3.PutObjectInput{Bucket: &bucket, Key: &o.Key, Body: bytes.NewReader(data),
			IfMatch: optional(o.IfMatch), IfNoneMatch: optional(o.IfNoneMatch),
			ContentType: optional(o.ContentType), Metadata: o.Metadata}
		resp, err := c.PutObject(ctx, in)
		r := result{Bytes: uint64(len(data))}
		if err == nil {
			r.ETag = aws.ToString(resp.ETag)
		}
		return finish(o, rec, err, r)
	case "head":
		resp, err := c.HeadObject(ctx, &s3.HeadObjectInput{Bucket: &bucket, Key: &o.Key})
		r := result{}
		if err == nil {
			size := uint64(aws.ToInt64(resp.ContentLength))
			r.Size, r.ETag = &size, aws.ToString(resp.ETag)
		}
		return finish(o, rec, err, r)
	case "delete":
		if len(o.Keys) == 1 {
			_, err := c.DeleteObject(ctx, &s3.DeleteObjectInput{Bucket: &bucket, Key: &o.Keys[0]})
			return finish(o, rec, err, result{})
		}
		ids := make([]types.ObjectIdentifier, len(o.Keys))
		for i := range o.Keys {
			ids[i] = types.ObjectIdentifier{Key: &o.Keys[i]}
		}
		resp, err := c.DeleteObjects(ctx, &s3.DeleteObjectsInput{Bucket: &bucket,
			Delete: &types.Delete{Objects: ids, Quiet: aws.Bool(true)}})
		r := result{}
		if err == nil {
			for _, e := range resp.Errors {
				r.FailedKeys = append(r.FailedKeys, aws.ToString(e.Key))
			}
		}
		return finish(o, rec, err, r)
	case "list":
		in := &s3.ListObjectsV2Input{Bucket: &bucket, Prefix: &o.Prefix}
		if !o.Recursive {
			in.Delimiter = aws.String("/")
		}
		if o.Limit > 0 {
			in.MaxKeys = aws.Int32(o.Limit)
		}
		r := result{}
		pages := s3.NewListObjectsV2Paginator(c, in)
		for pages.HasMorePages() {
			page, err := pages.NextPage(ctx)
			r.Requests++
			if err != nil {
				return finish(o, rec, err, r)
			}
			for _, obj := range page.Contents {
				r.Entries = append(r.Entries, entry{Key: aws.ToString(obj.Key), Size: aws.ToInt64(obj.Size)})
			}
			for _, p := range page.CommonPrefixes {
				r.Entries = append(r.Entries, entry{Key: aws.ToString(p.Prefix), Dir: true})
			}
		}
		return finish(o, rec, nil, r)
	case "copy":
		src := bucket + "/" + (&url.URL{Path: o.Source}).EscapedPath()
		_, err := c.CopyObject(ctx, &s3.CopyObjectInput{Bucket: &bucket, Key: &o.Key, CopySource: &src})
		// A 200 whose result document does not parse (some S3-compatible servers write
		// LastModified in another format) is still a copy; S3's embedded errors carry a code.
		var de *smithy.DeserializationError
		if errors.As(err, &de) && lastStatus(rec) == 200 {
			err = nil
		}
		return finish(o, rec, err, result{})
	}
	return result{ID: o.ID, Error: &wireErr{Code: "Unsupported", Message: o.Op}}
}

func main() {
	in := bufio.NewReaderSize(os.Stdin, 1<<20)
	line, err := in.ReadBytes('\n')
	if err != nil && len(line) == 0 {
		return
	}
	var h hello
	if err := json.Unmarshal(line, &h); err != nil || !strings.HasPrefix(h.Protocol, "oprobe.client/1") {
		fmt.Fprintf(os.Stderr, "unsupported hello: %s\n", line)
		os.Exit(2)
	}
	ctx := context.Background()
	base := http.DefaultTransport.(*http.Transport).Clone()
	base.MaxIdleConnsPerHost = max(h.Concurrency, 2)
	base.MaxConnsPerHost = 0
	// Extra trust: the target's CA file and AWS_CA_BUNDLE (the SDK cannot add it to a
	// custom client itself).
	var caFiles []string
	for _, f := range []string{h.Target.CaCertFile, os.Getenv("AWS_CA_BUNDLE")} {
		if f != "" {
			caFiles = append(caFiles, f)
		}
	}
	if h.Target.Insecure || len(caFiles) > 0 {
		tc := &tls.Config{InsecureSkipVerify: h.Target.Insecure}
		if len(caFiles) > 0 {
			pool, _ := x509.SystemCertPool()
			if pool == nil {
				pool = x509.NewCertPool()
			}
			for _, f := range caFiles {
				if pem, err := os.ReadFile(f); err == nil {
					pool.AppendCertsFromPEM(pem)
				}
			}
			tc.RootCAs = pool
		}
		base.TLSClientConfig = tc
	}
	httpClient := &http.Client{Transport: tracedTransport{inner: base}}
	os.Unsetenv("AWS_CA_BUNDLE")
	opts := []func(*config.LoadOptions) error{
		config.WithRetryMaxAttempts(h.Retries + 1),
	}
	if h.Target.Region != "" {
		opts = append(opts, config.WithRegion(h.Target.Region))
	}
	cfg, err := config.LoadDefaultConfig(ctx, opts...)
	if err != nil {
		fmt.Fprintf(os.Stderr, "cannot configure the SDK: %v\n", err)
		os.Exit(2)
	}
	client := s3.NewFromConfig(cfg, func(o *s3.Options) {
		o.HTTPClient = httpClient
		o.UsePathStyle = h.Target.PathStyle
		if h.Target.Endpoint != "" {
			o.BaseEndpoint = aws.String(h.Target.Endpoint)
		}
		o.RequestChecksumCalculation = aws.RequestChecksumCalculationWhenRequired
		o.ResponseChecksumValidation = aws.ResponseChecksumValidationWhenRequired
	})
	emit(map[string]any{
		"type":     "hello",
		"protocol": protocol,
		"client":   "aws-sdk-go-v2",
		"version":  sdkVersion(),
		"phases":   []string{"queue", "connect", "send", "ttfb", "body"},
		"ops":      []string{"get", "put", "head", "delete", "list", "copy"},
		"features": []string{"if_match", "if_none_match", "metadata", "batch_delete"},
	})
	timeout := time.Duration(max(h.TimeoutMS, 1000)) * time.Millisecond
	sem := make(chan struct{}, max(h.Concurrency, 1))
	var wg sync.WaitGroup
	for {
		line, err := in.ReadBytes('\n')
		if len(bytes.TrimSpace(line)) > 0 {
			var o op
			if json.Unmarshal(line, &o) == nil {
				if o.Type == "shutdown" {
					break
				}
				sem <- struct{}{}
				wg.Add(1)
				go func(o op) {
					defer func() { <-sem; wg.Done() }()
					c, cancel := context.WithTimeout(ctx, timeout)
					defer cancel()
					emit(do(c, client, h.Target.Bucket, o))
				}(o)
			}
		}
		if err != nil {
			break
		}
	}
	wg.Wait()
}
