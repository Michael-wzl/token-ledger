//! Offline exports only. Never resolves URLs or opens authentication stores.
//! Cursor columns/accounting follow Tokscale v4.21.0 sessions/cursor.rs.
use crate::model::{stable_id, Coverage, ScanResult, Tokens, UsageRecord};
use crate::parsers::{self, count, timestamp};
use anyhow::{bail, Result};
use serde_json::{Map, Value};
use std::io::Cursor;

pub fn parse(
    bytes: &[u8],
    format: &str,
    app: &str,
    account: &str,
    source_id: &str,
) -> Result<ScanResult> {
    if bytes.len() > 32 * 1024 * 1024 {
        bail!("Import exceeds the 32 MiB limit.");
    }
    if !matches!(app, "cursor" | "grok-bot" | "codex" | "claude" | "auto") {
        bail!("Unsupported import application.");
    }
    let format = if format == "auto" {
        if matches!(app, "codex" | "claude") {
            "jsonl"
        } else if bytes
            .iter()
            .find(|b| !b.is_ascii_whitespace())
            .is_some_and(|b| *b == b'{' || *b == b'[')
        {
            "json"
        } else {
            "csv"
        }
    } else {
        format
    };
    let mut result = match format {
        "jsonl" => {
            let actual = if app == "auto" {
                let head = std::str::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("The imported log must be UTF-8."))?;
                let v: Value = head
                    .lines()
                    .find_map(|s| serde_json::from_str(s).ok())
                    .ok_or_else(|| anyhow::anyhow!("No valid JSON log records found."))?;
                if v["type"].as_str() == Some("session_meta") {
                    "codex"
                } else {
                    "claude"
                }
            } else {
                app
            };
            match actual {
                "codex" => parsers::codex(Cursor::new(bytes), source_id, source_id),
                "claude" => {
                    parsers::claude(Cursor::new(bytes), source_id, source_id, "imported-remote")
                }
                _ => bail!("JSONL imports support Codex and Claude session logs."),
            }
        }
        "json" => json(bytes, app, account, source_id)?,
        "csv" => csv(bytes, app, account, source_id)?,
        _ => bail!("Supported import formats are csv, json, and jsonl."),
    };
    for r in &mut result.records {
        r.source_id = source_id.into();
        r.provenance = "import-recorded".into();
        if r.account == "unknown" && !account.is_empty() && account != "unknown" {
            r.account = account.into();
        }
    }
    if result.records.is_empty() {
        bail!("No supported numeric token records were found. Quota percentages and text estimates are not token usage.");
    }
    let conflicts = parsers::dedup(&mut result.records);
    for c in &mut result.coverage {
        c.errors += conflicts;
        if conflicts > 0 {
            c.message.push_str(&format!(
                " Held out {conflicts} conflicting logical request IDs."
            ));
        }
        c.source_id = source_id.into();
        c.records = result.records.len();
        c.undated = result
            .records
            .iter()
            .filter(|r| r.timestamp.is_none())
            .count();
    }
    Ok(result)
}

fn json(bytes: &[u8], app: &str, account: &str, source: &str) -> Result<ScanResult> {
    if !matches!(app, "cursor" | "grok-bot" | "auto") {
        bail!("JSON usage events support Cursor and Grok Bot. Use JSONL for Codex or Claude session logs.");
    }
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("The usage export is not valid JSON."))?;
    let rows = root
        .as_array()
        .or_else(|| root.get("usageEventsDisplay").and_then(Value::as_array))
        .or_else(|| root.get("usageEvents").and_then(Value::as_array))
        .or_else(|| root.get("events").and_then(Value::as_array))
        .or_else(|| root.get("data").and_then(Value::as_array))
        .or_else(|| root.pointer("/data/usageEvents").and_then(Value::as_array))
        .ok_or_else(|| anyhow::anyhow!("Expected an array of vendor usage events."))?;
    let mut result = ScanResult::default();
    let mut c = Coverage::new(
        source,
        app,
        "partial",
        "Imported recorded usage; coverage is limited to the supplied export and declared window.",
    );
    for (i, row) in rows.iter().enumerate() {
        let Some(object) = row.as_object() else {
            c.errors += 1;
            continue;
        };
        match event(object, app, account, source, i) {
            Ok(Some(r)) => result.records.push(r),
            Ok(None) => c.errors += 1,
            Err(_) => c.errors += 1,
        }
    }
    if c.errors > 0 {
        c.message.push_str(" Some rows lacked supported counters, timestamps, or reliable product attribution and were excluded.");
    }
    result.coverage.push(c);
    Ok(result)
}

fn event(
    row: &Map<String, Value>,
    requested: &str,
    account: &str,
    source: &str,
    index: usize,
) -> Result<Option<UsageRecord>> {
    let get = |keys: &[&str]| keys.iter().find_map(|k| row.get(*k));
    let model = get(&["model", "Model"])
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let product = get(&[
        "application_type",
        "applicationType",
        "product",
        "Product",
        "tool",
        "Tool",
    ])
    .and_then(Value::as_str)
    .unwrap_or("");
    let grok = is_grok(product) || is_grok(model);
    if requested == "grok-bot" && !grok {
        return Ok(None);
    }
    let app = if grok { "grok-bot" } else { "cursor" };
    let time = get(&["timestamp", "Date", "date", "createdAt"]).and_then(timestamp);
    if time.is_none() {
        return Ok(None);
    }
    let u = get(&["tokenUsage", "usage"])
        .and_then(Value::as_object)
        .unwrap_or(row);
    let field = |keys: &[&str]| -> Result<Option<u64>> {
        let v = keys.iter().find_map(|k| u.get(*k));
        match v {
            None | Some(Value::Null) => Ok(None),
            Some(v) => count(Some(v))
                .map(Some)
                .ok_or_else(|| anyhow::anyhow!("Invalid numeric token counter.")),
        }
    };
    let input = field(&["inputTokens", "input_tokens", "Input (w/o Cache Write)"])?;
    let output = field(&["outputTokens", "output_tokens", "Output Tokens"])?;
    let read = field(&["cacheReadTokens", "cache_read_input_tokens", "Cache Read"])?;
    let write = field(&[
        "cacheWriteTokens",
        "cache_creation_input_tokens",
        "Input (w/ Cache Write)",
    ])?;
    if input.is_none() && output.is_none() && read.is_none() && write.is_none() {
        return Ok(None);
    }
    let tokens = Tokens {
        input: input.unwrap_or(0),
        output: output.unwrap_or(0),
        cache_read: read.unwrap_or(0),
        cache_write: write.unwrap_or(0),
        reasoning: 0,
    };
    // Cursor exports report disjoint buckets. Generic OpenAI API `usage`
    // objects are deliberately not inferred from model names.
    let provided_id = get(&["eventId", "requestId", "id", "Event ID", "Request ID"])
        .and_then(Value::as_str)
        .unwrap_or("");
    let session = get(&["conversationId", "Conversation ID", "Cloud Agent ID"])
        .and_then(Value::as_str)
        .unwrap_or("");
    let time_key = time.unwrap_or(0).to_string();
    let fallback = format!(
        "{}:{}:{}:{}:{}",
        tokens.input, tokens.output, tokens.cache_read, tokens.cache_write, index
    );
    let id = if !provided_id.is_empty() {
        stable_id(&["vendor-event", app, account, provided_id])
    } else {
        stable_id(&[
            "vendor-row",
            app,
            account,
            session,
            &time_key,
            model,
            &fallback,
        ])
    };
    Ok(Some(UsageRecord {
        id: id.clone(),
        source_id: source.into(),
        app: app.into(),
        surface: "vendor-export".into(),
        account: if account.is_empty() {
            "unknown".into()
        } else {
            account.into()
        },
        session: stable_id(&["vendor-session", account, session]),
        request: id,
        timestamp: time,
        model: model.chars().take(128).collect(),
        project: "Unknown project".into(),
        tokens,
        provenance: "import-recorded".into(),
    }))
}

fn is_grok(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "grok bot" | "grok-bot" | "grok_bot" | "grokbot"
    )
}

fn csv(bytes: &[u8], app: &str, account: &str, source: &str) -> Result<ScanResult> {
    if !matches!(app, "cursor" | "grok-bot" | "auto") {
        bail!("CSV imports support Cursor and explicitly identified Grok Bot usage.");
    }
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(bytes);
    let headers = reader
        .headers()
        .map_err(|_| anyhow::anyhow!("The CSV header is unreadable."))?
        .clone();
    let mut seen = std::collections::BTreeSet::new();
    if headers.iter().any(|h| !seen.insert(h)) {
        bail!("The CSV header contains duplicate columns.");
    }
    if !headers.iter().any(|h| h == "Date") || !headers.iter().any(|h| h == "Model") {
        bail!("Expected Cursor usage CSV columns Date and Model.");
    }
    if ![
        "Input (w/o Cache Write)",
        "Input (w/ Cache Write)",
        "Cache Read",
        "Output Tokens",
    ]
    .iter()
    .any(|k| headers.iter().any(|h| h == *k))
    {
        bail!("The CSV has no supported token breakdown columns.");
    }
    let mut result = ScanResult::default();
    let mut c=Coverage::new(source,app,"partial","Imported vendor usage CSV; rows without token counters or product attribution are excluded.");
    for (i, record) in reader.records().enumerate() {
        let record = match record {
            Ok(r) => r,
            Err(_) => {
                c.errors += 1;
                continue;
            }
        };
        let mut row = Map::new();
        for (h, v) in headers.iter().zip(record.iter()) {
            if v.trim().is_empty() {
                continue;
            }
            row.insert(h.into(), Value::String(v.into()));
        }
        match event(&row, app, account, source, i) {
            Ok(Some(r)) => result.records.push(r),
            _ => c.errors += 1,
        }
    }
    result.coverage.push(c);
    Ok(result)
}

/// Stable compact source-manifest metadata; never stores import bodies.
pub fn fingerprint(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn cursor_all_csv_generations_and_numeric_zero() {
        for data in ["Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost\n2026-11-05T10:00:00Z,claude,5,10,90,20,125,1", "Date,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost\n2026-11-05T10:00:00Z,Included,claude,false,5,10,90,20,125,1", "Date,Cloud Agent ID,Automation ID,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost\n2026-11-05T10:00:00Z,,,Included,claude,false,5,10,90,20,125,1"]{
            let r=parse(data.as_bytes(),"csv","cursor","personal","import:test").unwrap();assert_eq!(r.records[0].tokens.total(),125);
        }
    }
    #[test]
    fn mixed_export_classifies_grok_only_with_evidence() {
        let b=serde_json::to_vec(&json!({"usageEvents":[{"id":"1","timestamp":1793872800000_i64,"model":"grok-4","tokenUsage":{"inputTokens":100,"outputTokens":10}},{"id":"2","timestamp":1793872800000_i64,"model":"Grok Bot","tokenUsage":{"inputTokens":20,"outputTokens":2}}]})).unwrap();
        let r = parse(&b, "json", "grok-bot", "personal", "test").unwrap();
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].tokens.total(), 22);
        assert_eq!(r.coverage[0].errors, 1);
    }
    #[test]
    fn quota_and_text_are_never_token_estimates() {
        let b=br#"[{"timestamp":1793872800000,"model":"Grok Bot","usage":{"percentUsed":50},"text":"this is not counted"}]"#;
        assert!(parse(b, "json", "grok-bot", "personal", "test").is_err());
    }
    #[test]
    fn invalid_numbers_and_bodies_are_sanitized() {
        let b=br#"[{"timestamp":1793872800000,"model":"claude","tokenUsage":{"inputTokens":"private-secret","outputTokens":4}}]"#;
        let err = parse(b, "json", "cursor", "personal", "test")
            .unwrap_err()
            .to_string();
        assert!(!err.contains("private-secret"));
    }
    #[test]
    fn repeated_import_ids_and_accounts() {
        let b=br#"[{"id":"one","timestamp":1793872800000,"model":"claude","tokenUsage":{"inputTokens":10,"outputTokens":2,"cacheReadTokens":100}}]"#;
        let a = parse(b, "json", "cursor", "personal", "first").unwrap();
        let again = parse(b, "json", "cursor", "personal", "second").unwrap();
        let other = parse(b, "json", "cursor", "work", "third").unwrap();
        assert_eq!(a.records[0].id, again.records[0].id);
        assert_ne!(a.records[0].id, other.records[0].id);
        assert_eq!(a.records[0].tokens.total(), 112);
    }
    #[test]
    fn remote_log_import_discards_text() {
        let b=br#"{"type":"assistant","timestamp":"2026-11-05T10:00:00Z","requestId":"r","message":{"id":"m","content":"private-text","usage":{"input_tokens":2,"output_tokens":3}}}"#;
        let r = parse(b, "jsonl", "claude", "remote", "test").unwrap();
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("private-text"));
        assert_eq!(r.records[0].account, "remote");
    }
    #[test]
    fn provider_format_mismatch_is_rejected() {
        let b =
            br#"[{"timestamp":1793872800000,"model":"claude","tokenUsage":{"inputTokens":10}}]"#;
        assert!(parse(b, "json", "codex", "personal", "test").is_err());
        assert!(parse(b, "json", "claude", "personal", "test").is_err());
    }
}
