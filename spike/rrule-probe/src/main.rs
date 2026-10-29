use std::fs;

use anyhow::{Context, Result};
use chrono::SecondsFormat;
use rrule::RRuleSet;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct Case {
    id: String,
    ics: String,
    limit: u16,
}

#[derive(Debug, Serialize)]
struct CaseResult {
    id: String,
    ok: bool,
    occurrences: Vec<String>,
    error: Option<String>,
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "spike/data/rrule-cases.json".into());
    let raw = fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
    let cases: Vec<Case> = serde_json::from_str(&raw)?;

    let mut out = Vec::with_capacity(cases.len());
    for case in cases {
        let parsed: Result<RRuleSet, _> = case.ics.parse();
        match parsed {
            Ok(set) => {
                let occurrences = set
                    .all(case.limit)
                    .dates
                    .into_iter()
                    .map(|dt| dt.to_utc().to_rfc3339_opts(SecondsFormat::Secs, true))
                    .collect();
                out.push(CaseResult {
                    id: case.id,
                    ok: true,
                    occurrences,
                    error: None,
                });
            }
            Err(err) => out.push(CaseResult {
                id: case.id,
                ok: false,
                occurrences: Vec::new(),
                error: Some(format!("{err:?}")),
            }),
        }
    }

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
