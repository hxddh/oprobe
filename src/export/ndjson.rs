//! NDJSON rows: the run file's lines, uncompressed — a `report` line, then `op` and `req`
//! lines (`{"kind":…, …}`), ready for `jq`, DuckDB or a log pipeline.

use std::io::Write;

use serde::Serialize;

use crate::model::Run;

fn line<T: Serialize>(out: &mut impl Write, kind: &str, v: &T) -> anyhow::Result<()> {
    let mut m = serde_json::to_value(v)?;
    if let Some(o) = m.as_object_mut() {
        o.insert("kind".into(), kind.into());
    }
    serde_json::to_writer(&mut *out, &m)?;
    out.write_all(b"\n")?;
    Ok(())
}

/// Write the run as NDJSON (`with_report`: start with the report line).
pub fn write(run: &Run, with_report: bool, out: &mut impl Write) -> anyhow::Result<()> {
    if with_report {
        line(out, "report", &run.report)?;
    }
    for o in &run.ops {
        line(out, "op", o)?;
    }
    for r in &run.reqs {
        line(out, "req", r)?;
    }
    Ok(())
}

pub fn to_string(run: &Run, with_report: bool) -> anyhow::Result<String> {
    let mut v = Vec::new();
    write(run, with_report, &mut v)?;
    Ok(String::from_utf8(v)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::simulate::tests::straggler_run;

    #[test]
    fn rows_round_trip() {
        let run = straggler_run();
        let s = to_string(&run, true).unwrap();
        assert_eq!(s.lines().count(), 1 + run.ops.len() + run.reqs.len());
        let req: serde_json::Value = serde_json::from_str(s.lines().last().unwrap()).unwrap();
        assert_eq!(req["kind"], "req");
        let back: crate::model::ReqRow = serde_json::from_value(req).unwrap();
        assert_eq!(&back, run.reqs.last().unwrap());
    }
}
