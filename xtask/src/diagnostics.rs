use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub fn run(path: &Path) -> Result<()> {
    const MAX_INPUT_BYTES: u64 = 4 * 1024 * 1024;
    let mut input = String::new();
    File::open(path)
        .context("open diagnostic file")?
        .take(MAX_INPUT_BYTES + 1)
        .read_to_string(&mut input)
        .context("read diagnostic file")?;
    ensure!(
        input.len() as u64 <= MAX_INPUT_BYTES,
        "diagnostic file is too large"
    );
    println!("{}", serde_json::to_string_pretty(&summarize(&input)?)?);
    Ok(())
}

fn summarize(input: &str) -> Result<Value> {
    let partial_line = !input.ends_with('\n') && !input.is_empty();
    let mut session = None;
    let mut finish = None;
    let mut first_error = None;
    let mut incidents = BTreeMap::new();
    let mut tests = [0_u64; 3];
    for (index, line) in input.split_inclusive('\n').enumerate() {
        if !line.ends_with('\n') {
            break;
        }
        let event: Value = serde_json::from_str(line)
            .with_context(|| format!("invalid diagnostic event on line {}", index + 1))?;
        ensure!(
            event["schema_version"] == "1.0",
            "unsupported diagnostic schema"
        );
        ensure!(
            event["sequence"].as_u64() == Some(index as u64 + 1),
            "invalid sequence"
        );
        if index == 0 {
            ensure!(event["event"] == "session.started", "missing session start");
            ensure!(event["session_id"].is_string(), "missing session ID");
            session = Some(event);
            continue;
        }
        ensure!(finish.is_none(), "event after session finish");
        ensure!(
            session
                .as_ref()
                .is_some_and(|start| start["session_id"] == event["session_id"]),
            "event belongs to a different session"
        );
        match event["event"].as_str() {
            Some("session.finished") => finish = Some(event),
            Some("test.finished") => match event["status"].as_str() {
                Some("PASS") => tests[0] += 1,
                Some("FAIL") => tests[1] += 1,
                Some("SKIP") => tests[2] += 1,
                _ => anyhow::bail!("invalid test status"),
            },
            Some("operation.failed") => {
                let key = (
                    event["component"].as_str().unwrap_or("unknown").to_owned(),
                    event["operation"].as_str().unwrap_or("unknown").to_owned(),
                    event["error"]["native_code"]["domain"]
                        .as_str()
                        .map(str::to_owned),
                    event["error"]["native_code"]["value"].as_i64(),
                );
                let incident = incidents
                    .entry(key)
                    .or_insert_with(|| (0_u64, event.clone()));
                incident.0 += 1;
                first_error.get_or_insert(event["event_id"].clone());
            }
            _ => {}
        }
    }
    let session = session.context("missing complete session start")?;
    let complete = finish.is_some() && !partial_line;
    let finish = finish.unwrap_or(Value::Null);
    let incidents: Vec<_> = incidents
        .into_values()
        .map(|(count, event)| {
            json!({
                "count": count,
                "first_event_id": event["event_id"],
                "category": event["category"],
                "component": event["component"],
                "operation": event["operation"],
                "dependency": event["dependency"],
                "native_code": event["error"]["native_code"],
                "diagnostic": event["diagnostic"],
            })
        })
        .collect();
    Ok(json!({
        "schema_version": "1.0",
        "session_id": session["session_id"],
        "victory_version": session["victory_version"],
        "git_commit": session["git_commit"],
        "platform": session["platform"],
        "architecture": session["architecture"],
        "diagnostics_feature": session["diagnostics_feature"],
        "complete": complete,
        "success": finish["success"],
        "dropped_events": finish["dropped_events"],
        "first_error_event_id": first_error,
        "tests": {"pass": tests[0], "fail": tests[1], "skip": tests[2]},
        "incidents": incidents,
    }))
}

#[cfg(test)]
mod tests {
    use super::summarize;

    #[test]
    fn reports_repeated_errors_and_explicit_skips() {
        let input = concat!(
            r#"{"schema_version":"1.0","session_id":"test","sequence":1,"event":"session.started"}"#,
            "\n",
            r#"{"schema_version":"1.0","session_id":"test","sequence":2,"event":"test.finished","status":"SKIP"}"#,
            "\n",
            r#"{"schema_version":"1.0","session_id":"test","sequence":3,"event":"operation.failed","event_id":"test-3","component":"loader","operation":"LoadLibraryExW"}"#,
            "\n",
            r#"{"schema_version":"1.0","session_id":"test","sequence":4,"event":"operation.failed","event_id":"test-4","component":"loader","operation":"LoadLibraryExW"}"#,
            "\n",
        );
        let report = summarize(input).unwrap();
        assert_eq!(report["complete"], false);
        assert_eq!(report["first_error_event_id"], "test-3");
        assert_eq!(report["incidents"][0]["count"], 2);
        assert_eq!(report["tests"]["skip"], 1);
        assert_eq!(report["tests"]["pass"], 0);
    }
}
