//! The CLI parses every printed `oprobe run …` command ([`RunSpec::command`]) back to the
//! profile it came from: the replay contract of findings, verify and the App.

use clap::Parser;

use crate::config::{Access, DatasetSpec, OpSpec, Profile, TargetSpec};
use crate::model::{Overrides, RunSpec};
use crate::util::{ByteSize, HumanDuration};

fn lab_profile() -> Profile {
    let mut t = TargetSpec::from_url("s3://bkt/x").unwrap();
    t.options
        .insert("endpoint".into(), "https://minio.local:9000".into());
    t.options.insert("region".into(), "eu-west-1".into());
    t.options
        .insert("secret_access_key".into(), "hunter2".into());
    t.options
        .insert("access_key_id".into(), "${MY_KEY_ID}".into());
    let mut p = Profile::from_target(t);
    p.workload.preset = crate::config::Preset::SmallFiles;
    p.workload.duration = HumanDuration::secs(12);
    p.workload.warmup = HumanDuration(std::time::Duration::from_millis(500));
    p.workload.seed = 7;
    p.workload.max_ops = Some(900);
    p.workload.rate = Some(250.0);
    p.workload.access = Access::Zipf(1.2);
    p.workload.dataset = Some(DatasetSpec {
        objects: 64,
        object_size: ByteSize::kib(64),
        partitions: 2,
        ..Default::default()
    });
    p.workload.mix = vec![OpSpec {
        weight: 3,
        op: crate::config::OpKind::Read { size: None },
    }];
    p.knobs.concurrency = 24;
    p.knobs.io_concurrency = 3;
    p.knobs.http_version = crate::net::HttpVersion::H1;
    p.knobs.tail_cut = true;
    p.knobs.timeout = HumanDuration::secs(9);
    p.sla.max_throttle_rate = Some(0.01);
    p.cost.provider = Some("aws-s3".into());
    p.name = Some("base".into());
    p
}

/// Minimal POSIX word splitting with single quotes (what `command_line` produces).
fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q = false;
    let mut any = false;
    for c in s.chars() {
        match (c, q) {
            ('\'', _) => {
                q = !q;
                any = true;
            }
            (' ', false) => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            (c, _) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Parse a printed command with the CLI and build its profile (as `oprobe run` does, without
/// a config file).
fn replay(cmd: &str) -> Profile {
    let words = shell_words(cmd);
    assert_eq!(words[0], "oprobe");
    let cli = super::args::Cli::try_parse_from(&words).unwrap_or_else(|e| panic!("{cmd}: {e}"));
    let Some(super::args::Command::Run(a)) = cli.command else {
        panic!("not a run: {cmd}");
    };
    let req = super::convert::profile_request(&a);
    let mut p =
        crate::app::profile::build_profile(&req.target, &req.workload, &req.knobs, &req.cost)
            .unwrap();
    crate::app::profile::apply_sla(&mut p, &req.sla).unwrap();
    let mut label = req.label.clone();
    Overrides::parse(&req.set)
        .unwrap()
        .apply(&mut p, &mut label)
        .unwrap();
    p.name = label;
    p
}

#[test]
fn command_replays_every_setting() {
    let p = lab_profile();
    let s = RunSpec::new(&p, "s3://bkt/x");
    let cmd = s.command();
    let back = replay(&cmd);
    // Same workload, knobs, SLA, cost and label; the secret is not in the command.
    assert!(!cmd.contains("hunter2"), "{cmd}");
    assert_eq!(back.workload, p.workload, "{cmd}");
    assert_eq!(back.knobs, p.knobs, "{cmd}");
    assert_eq!(back.sla, p.sla, "{cmd}");
    assert_eq!(back.cost, p.cost, "{cmd}");
    assert_eq!(back.name, p.name, "{cmd}");
    assert_eq!(back.target.options["endpoint"], "https://minio.local:9000");
    assert_eq!(back.target.options["region"], "eu-west-1");
    assert_eq!(RunSpec::new(&back, "s3://bkt/x").hash(), s.hash());
}

#[test]
fn the_command_of_a_changed_spec_replays_the_change() {
    let s = RunSpec::new(&lab_profile(), "s3://bkt/x");
    let a = s.resolved_profile();
    let n = s.with(
        &Overrides::new()
            .with_set("concurrency", "48")
            .with_set("tail_cut", "off")
            .with_set("warmup", "2s"),
    );
    let back = replay(&n.command());
    assert_eq!(back.knobs.concurrency, 48);
    assert!(!back.knobs.tail_cut);
    assert_eq!(back.workload.warmup, HumanDuration::secs(2));
    assert_eq!(back.workload.mix, a.workload.mix);
}

#[test]
fn verification_replays_parse_to_the_spec_with_only_the_change() {
    let mut t = TargetSpec::from_url("s3://bkt/x").unwrap();
    t.options
        .insert("endpoint".into(), "https://minio.local:9000".into());
    let mut p = Profile::from_target(t);
    p.workload.seed = 9;
    p.workload.max_ops = Some(5000);
    p.knobs.concurrency = 24;
    p.knobs.keep_alive = false;
    let spec = RunSpec::new(&p, "s3://bkt/x");
    let o = Overrides::new().with_set("concurrency", "8");
    let v = crate::analysis::verification::Verification::new(
        "20261121-101010-abcd",
        "pool_starvation",
        &spec,
        o.clone(),
        "fewer workers",
    );
    let got = replay(&v.replay);
    let want = spec.with(&o).resolved_profile();
    assert_eq!(got.workload, want.workload, "{}", v.replay);
    assert_eq!(got.knobs, want.knobs, "{}", v.replay);
    assert_eq!(got.target.options["endpoint"], "https://minio.local:9000");
}
