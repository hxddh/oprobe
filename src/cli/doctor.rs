//! `oprobe doctor`: step-by-step connectivity, TLS, clock, credential and permission checks.

use std::fmt::Write as _;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::Result;

use super::args::{CostArgs, DoctorArgs, KnobArgs, WorkloadArgs};
use crate::analysis::hints::hint_for;
use crate::net::probe::{probe_endpoint, ProbeOptions, ProbeReport};
use crate::report::style::{bold, dim, green, red, yellow};
use crate::util::fmt_ms;

struct Checks {
    failed: usize,
    warned: usize,
    scheme: String,
}

impl Checks {
    fn ok(&self, name: &str, detail: &str) {
        println!("  {} {:<22} {}", green("✓"), name, dim(detail));
    }
    fn warn(&mut self, name: &str, detail: &str) {
        self.warned += 1;
        println!("  {} {:<22} {}", yellow("▲"), name, detail);
    }
    fn fail(&mut self, name: &str, err: &str) {
        self.failed += 1;
        println!("  {} {:<22} {}", red("✖"), name, red(err));
        if let Some(h) = hint_for(err, &self.scheme) {
            println!("    {} {}", yellow("hint:"), h.title);
            for s in h.steps {
                println!("      • {s}");
            }
        }
    }
}

pub async fn run(a: DoctorArgs) -> Result<ExitCode> {
    let profile = super::build_profile(
        &a.target,
        &WorkloadArgs::default(),
        &KnobArgs::default(),
        &CostArgs::default(),
    )?;
    let target = profile.target.normalized()?;
    println!("{} {}", bold("oprobe doctor"), target.display());
    let mut c = Checks {
        failed: 0,
        warned: 0,
        scheme: target.scheme.clone(),
    };

    // 1. Configuration
    match target.resolved_options() {
        Ok(opts) => {
            let shown: Vec<String> = crate::util::redact_options(&opts)
                .into_iter()
                .filter(|(k, _)| k != "ca_cert_pem")
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            c.ok(
                "configuration",
                &format!("{} · {}", target.scheme, shown.join(" ")),
            );
        }
        Err(e) => c.fail("configuration", &format!("{e:#}")),
    }
    if target.scheme == "s3" && !target.options.contains_key("allow_anonymous") {
        let has_keys = target.options.contains_key("access_key_id")
            || std::env::var("AWS_ACCESS_KEY_ID").is_ok()
            || std::env::var("AWS_PROFILE").is_ok()
            || std::env::var("AWS_WEB_IDENTITY_TOKEN_FILE").is_ok()
            || std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_ok();
        if has_keys {
            c.ok("credentials", "found (environment, profile or options)");
        } else {
            c.warn(
                "credentials",
                "none in env/options; relying on ~/.aws or instance metadata",
            );
        }
    }

    // 2. Network path
    if let Some(ep) = target.endpoint().filter(|_| target.is_network()) {
        let mut tls = crate::engine::operator::tls_options(&target)?;
        tls.alpn_protocols.clear();
        let opts = ProbeOptions {
            samples: 3,
            tls,
            proxy: crate::engine::operator::proxy_setting(&target),
            ..Default::default()
        };
        match probe_endpoint(&ep, &opts).await {
            Ok(r) => {
                match &r.proxy {
                    Some(p) => c.ok(
                        "proxy",
                        &format!(
                            "{p} → CONNECT {}:{} in {}",
                            r.host,
                            r.port,
                            fmt_ms(r.proxy_connect.p50)
                        ),
                    ),
                    None => c.ok(
                        "dns",
                        &format!(
                            "{} → {} ({})",
                            r.host,
                            r.resolved.join(", "),
                            fmt_ms(r.dns.p50)
                        ),
                    ),
                }
                c.ok("tcp", &format!("port {} in {}", r.port, fmt_ms(r.tcp.p50)));
                if r.tls {
                    c.ok(
                        "tls",
                        &format!(
                            "{} {} {} in {}",
                            r.tls_version,
                            r.alpn,
                            r.cipher,
                            fmt_ms(r.tls_handshake.p50)
                        ),
                    );
                }
                c.ok(
                    "http",
                    &format!(
                        "status {} · cold TTFB {} · warm {}",
                        r.samples.first().map(|s| s.status).unwrap_or(0),
                        fmt_ms(r.cold_ttfb.p50),
                        fmt_ms(r.warm_ttfb.p50)
                    ),
                );
                match r.clock_skew_s {
                    Some(s) if s.abs() > 900.0 => c.fail(
                        "clock",
                        &format!(
                            "RequestTimeTooSkewed risk: local clock differs from server by {s:.0}s"
                        ),
                    ),
                    Some(s) if s.abs() > 60.0 => c.warn(
                        "clock",
                        &format!("local clock differs from server by {s:.0}s"),
                    ),
                    Some(s) => c.ok("clock", &format!("skew {s:+.1}s")),
                    None => {}
                }
            }
            Err(e) => {
                c.fail("network", &format!("{e:#}"));
                return Ok(finish(c));
            }
        }
    }

    // 3. Storage permissions
    let base = match crate::engine::operator::base_operator(&target) {
        Ok(b) => b,
        Err(e) => {
            c.fail("backend", &format!("{e:#}"));
            return Ok(finish(c));
        }
    };
    let admin = crate::engine::operator::admin_operator(&base, &target)?;
    let t = Instant::now();
    match admin.list_with("").limit(5).await {
        Ok(entries) => c.ok(
            "list",
            &format!(
                "{} entries visible at root in {}",
                entries.len(),
                fmt_ms(t.elapsed().as_secs_f64() * 1000.0)
            ),
        ),
        Err(e) => c.fail("list", &e.to_string()),
    }
    if a.read_only {
        println!(
            "  {} {}",
            dim("·"),
            dim("skipping write/read/delete checks (--read-only)")
        );
        return Ok(finish(c));
    }
    let key = format!(
        "oprobe-run-{}-doctor/probe.bin",
        crate::util::compact_utc_now()
    );
    let (data, crc) = crate::engine::dataset::generate_payload(7, 256 * 1024);
    let t = Instant::now();
    match admin.write(&key, data).await {
        Ok(_) => {
            c.ok(
                "write",
                &format!("256 KiB in {}", fmt_ms(t.elapsed().as_secs_f64() * 1000.0)),
            );
            let t = Instant::now();
            match admin.read(&key).await {
                Ok(buf) => {
                    let got = buf
                        .clone()
                        .fold(0u32, |a, ch| crate::util::crc32c_append(a, &ch));
                    if got == crc {
                        c.ok(
                            "read + crc32c",
                            &format!("verified in {}", fmt_ms(t.elapsed().as_secs_f64() * 1000.0)),
                        );
                    } else {
                        c.fail("read + crc32c", "data read back differs from data written");
                    }
                }
                Err(e) => c.fail("read", &e.to_string()),
            }
            match admin.stat(&key).await {
                Ok(m) if m.content_length() == 256 * 1024 => c.ok("stat", "size matches"),
                Ok(m) => c.warn("stat", &format!("unexpected size {}", m.content_length())),
                Err(e) => c.fail("stat", &e.to_string()),
            }
            match admin.delete(&key).await {
                Ok(_) => c.ok("delete", "test object removed"),
                Err(e) => c.fail("delete", &e.to_string()),
            }
            let _ = admin
                .delete(key.rsplit_once('/').map(|(d, _)| d).unwrap_or(&key))
                .await;
        }
        Err(e) => c.fail("write", &e.to_string()),
    }
    Ok(finish(c))
}

fn finish(c: Checks) -> ExitCode {
    println!();
    if c.failed == 0 {
        println!(
            "{} ready to benchmark{}",
            green("●"),
            if c.warned > 0 {
                format!(" ({} warnings)", c.warned)
            } else {
                String::new()
            }
        );
        ExitCode::SUCCESS
    } else {
        println!("{} {} check(s) failed", red("●"), c.failed);
        ExitCode::FAILURE
    }
}

pub fn render_probe(r: &ProbeReport) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{} {}", bold("oprobe probe"), r.endpoint);
    match &r.proxy {
        Some(p) => {
            let _ = writeln!(
                o,
                "  {} {}:{} via proxy {p} {}",
                dim("target"),
                r.host,
                r.port,
                dim("(the proxy resolves the name; dns/tcp below are to the proxy)")
            );
        }
        None => {
            let _ = writeln!(
                o,
                "  {} {}:{} → {}",
                dim("target"),
                r.host,
                r.port,
                r.resolved.join(", ")
            );
        }
    }
    if r.tls {
        let _ = writeln!(
            o,
            "  {} {} · ALPN {} · {}",
            dim("tls   "),
            r.tls_version,
            r.alpn,
            r.cipher
        );
    }
    let _ = writeln!(
        o,
        "\n  {:<18} {:>9} {:>9} {:>9}",
        dim("stage"),
        dim("p50"),
        dim("p99"),
        dim("max")
    );
    let row = |o: &mut String, name: &str, q: &crate::trace::Quantiles| {
        if q.count > 0 {
            let _ = writeln!(
                o,
                "  {:<18} {:>9} {:>9} {:>9}",
                name,
                fmt_ms(q.p50),
                fmt_ms(q.p99),
                fmt_ms(q.max)
            );
        }
    };
    row(&mut o, "dns", &r.dns);
    row(&mut o, "tcp connect", &r.tcp);
    row(&mut o, "proxy CONNECT", &r.proxy_connect);
    row(&mut o, "tls handshake", &r.tls_handshake);
    row(&mut o, "ttfb (cold conn)", &r.cold_ttfb);
    row(&mut o, "ttfb (warm conn)", &r.warm_ttfb);
    let _ = writeln!(
        o,
        "\n  a request on a {} connection costs {} vs {} on a {} one ({:.1}×)",
        bold("new"),
        bold(&fmt_ms(r.cold_request_ms)),
        bold(&fmt_ms(r.warm_request_ms)),
        bold("pooled"),
        if r.warm_request_ms > 0.0 {
            r.cold_request_ms / r.warm_request_ms
        } else {
            0.0
        }
    );
    if let Some(s) = r.clock_skew_s {
        let txt = format!("clock skew vs server: {s:+.1}s");
        let _ = writeln!(
            o,
            "  {}",
            if s.abs() > 900.0 {
                red(&txt)
            } else {
                dim(&txt)
            }
        );
    }
    o
}
