//! Restricted, offline accounting adapted from Tokscale v4.21.0
//! (b26c433d03f23a2255ad9f212512ce83ede83da3), sessions/codex.rs and
//! sessions/claudecode.rs. See vendor/tokscale and docs/UPSTREAM.md.
//! No authentication, pricing, recovery overlays, or source writes.

use crate::model::{stable_id, Coverage, ScanResult, Tokens, UsageRecord};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::{BufRead, Read};
use std::path::Path;

pub const MAX_LINE_BYTES: usize = 32 * 1024 * 1024;

pub fn timestamp(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64().or_else(|| v.as_str()?.parse().ok()) {
        return (n > 0).then_some(if n < 100_000_000_000 {
            n.saturating_mul(1000)
        } else {
            n
        });
    }
    let s = v.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|x| x.timestamp_millis())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S UTC")
                .ok()
                .map(|x| x.and_utc().timestamp_millis())
        })
}

pub fn count(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    let n = v
        .as_u64()
        .or_else(|| v.as_str()?.trim().parse().ok())
        .or_else(|| {
            v.as_f64()
                .filter(|x| {
                    x.is_finite() && *x >= 0.0 && x.fract() == 0.0 && *x <= 9_007_199_254_740_991.0
                })
                .map(|x| x as u64)
        })?;
    (n <= 9_007_199_254_740_991).then_some(n)
}

pub fn project_label(cwd: Option<&str>) -> String {
    cwd.and_then(|x| Path::new(x).file_name())
        .and_then(|x| x.to_str())
        .filter(|x| !x.is_empty())
        .unwrap_or("Unknown project")
        .chars()
        .take(100)
        .collect()
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn account(v: &Value) -> String {
    let a = text(v, "accountUuid");
    let org = text(v, "orgUuid");
    if a.is_empty() && org.is_empty() {
        "unknown".into()
    } else {
        stable_id(&["claude-account", a, org])
    }
}

fn model(v: &Value) -> Option<String> {
    [
        v.get("model"),
        v.get("model_name"),
        v.pointer("/model_info/slug"),
        v.pointer("/info/model"),
        v.pointer("/state/model"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .filter(|s| !s.is_empty())
    .map(|s| s.chars().take(128).collect())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Total {
    input: u64,
    output: u64,
    cached: u64,
    write: u64,
    reasoning: u64,
}

impl Total {
    fn parse(v: &Value) -> Option<Self> {
        if [
            "input_tokens",
            "output_tokens",
            "cached_input_tokens",
            "cache_read_input_tokens",
            "cache_write_input_tokens",
            "reasoning_output_tokens",
        ]
        .iter()
        .any(|k| {
            v.get(*k)
                .is_some_and(|x| !x.is_null() && count(Some(x)).is_none())
        }) {
            return None;
        }
        let input = count(v.get("input_tokens"));
        let output = count(v.get("output_tokens"));
        if input.is_none() && output.is_none() {
            return None;
        }
        Some(Self {
            input: input.unwrap_or(0),
            output: output.unwrap_or(0),
            cached: count(v.get("cached_input_tokens"))
                .or_else(|| count(v.get("cache_read_input_tokens")))
                .unwrap_or(0),
            write: count(v.get("cache_write_input_tokens")).unwrap_or(0),
            reasoning: count(v.get("reasoning_output_tokens")).unwrap_or(0),
        })
    }
    fn tokens(self) -> Tokens {
        // TokenUsageInfo.last_usage_includes_reasoning describes retained
        // reasoning in the context count, not an output-bucket convention.
        // Both flag values still report reasoning as a subset of output.
        let cached = self.cached.min(self.input);
        let write = self.write.min(self.input - cached);
        Tokens {
            input: self.input - cached - write,
            output: self.output,
            cache_read: cached,
            cache_write: write,
            reasoning: self.reasoning.min(self.output),
        }
    }
    fn same_response(self, b: Self) -> bool {
        self.input == b.input && self.output == b.output
    }
    fn delta(self, b: Self) -> Option<Self> {
        Some(Self {
            input: self.input.checked_sub(b.input)?,
            output: self.output.checked_sub(b.output)?,
            // Detail refinements may regress without resetting cumulative
            // input/output. They cannot establish another model response.
            cached: self.cached.saturating_sub(b.cached),
            write: self.write.saturating_sub(b.write),
            reasoning: self.reasoning.saturating_sub(b.reasoning),
        })
    }
    fn refine(self, other: Self) -> Self {
        Self {
            cached: self.cached.max(other.cached),
            write: self.write.max(other.write),
            reasoning: self.reasoning.max(other.reasoning),
            ..self
        }
    }
    fn add(self, b: Self) -> Self {
        Self {
            input: self.input.saturating_add(b.input),
            output: self.output.saturating_add(b.output),
            cached: self.cached.saturating_add(b.cached),
            write: self.write.saturating_add(b.write),
            reasoning: self.reasoning.saturating_add(b.reasoning),
        }
    }
    fn size(self) -> u64 {
        self.input.saturating_add(self.output)
    }
    fn stale_regression(self, previous: Self, last: Self) -> bool {
        self.size() > 0
            && (self.size().saturating_mul(100) >= previous.size().saturating_mul(98)
                || self.size().saturating_add(last.size().saturating_mul(2)) >= previous.size())
    }
    fn key(self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.input, self.output, self.cached, self.write, self.reasoning
        )
    }
}

#[derive(Clone)]
struct Pending {
    usage: Total,
    time: Option<i64>,
    response: String,
    turn: String,
    model: String,
    compacted: bool,
}

pub fn codex<R: BufRead>(reader: R, source_id: &str, file_identity: &str) -> ScanResult {
    let mut result = ScanResult::default();
    let mut coverage = Coverage::new(
        source_id,
        "codex",
        "partial",
        "Local retained sessions only; remote/account-wide coverage is unknown.",
    );
    let mut session = String::new();
    let mut session_start = None;
    let mut parent = String::new();
    let mut waiting = false;
    let mut replay_meta = false;
    let mut user_fork = false;
    let mut own_tasks: Vec<String> = Vec::new();
    let mut previous: Option<Total> = None;
    let mut last_counted: Option<(usize, Total)> = None;
    let mut current_model = "unknown".to_string();
    let mut project = "Unknown project".to_string();
    let mut surface = "session".to_string();
    let mut turn = String::new();
    let mut pending: Option<Pending> = None;
    let mut settled: VecDeque<Total> = VecDeque::new();
    let mut ordinal = 0_u64;
    let mut epoch = 0_u64;
    let mut last_only_seen: Option<(Option<i64>, String, Total)> = None;
    let mut raw = Vec::new();
    let mut reader = reader;
    loop {
        raw.clear();
        match reader
            .by_ref()
            .take((MAX_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut raw)
        {
            Ok(0) => break,
            Ok(_) if raw.len() > MAX_LINE_BYTES => {
                coverage.errors += 1;
                coverage.status = "stale".into();
                break;
            }
            Err(_) => {
                coverage.errors += 1;
                coverage.status = "stale".into();
                break;
            }
            _ => {}
        }
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let v: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(_) => {
                coverage.errors += 1;
                continue;
            }
        };
        let typ = text(&v, "type");
        let p = v.get("payload").unwrap_or(&Value::Null);
        let ptyp = text(p, "type");
        let time = v.get("timestamp").and_then(timestamp);
        if typ == "session_meta" {
            let id = text(p, "id");
            if session.is_empty() {
                session = if id.is_empty() {
                    file_identity.into()
                } else {
                    id.into()
                };
                session_start = time;
                parent = text(p, "forked_from_id").to_string();
                if parent.is_empty() {
                    parent = p
                        .pointer("/source/subagent/thread_spawn/parent_thread_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into();
                }
                waiting = !parent.is_empty();
                user_fork = text(p, "thread_source") == "user";
                if p.pointer("/source/subagent").is_some() || text(p, "thread_source") == "subagent"
                {
                    surface = "subagent".into();
                }
                if text(p, "originator").contains("Desktop") && surface != "subagent" {
                    surface = "desktop".into();
                }
                project = project_label(p.get("cwd").and_then(Value::as_str));
            } else if waiting && !id.is_empty() && id != session {
                replay_meta = true;
            }
            continue;
        }
        if waiting {
            if typ == "event_msg" && ptyp == "task_started" {
                let tid = text(p, "turn_id");
                let starts_own = uuid_time(tid)
                    .zip(uuid_time(&session))
                    .map(|(a, b)| a >= b)
                    .unwrap_or_else(|| {
                        p.get("started_at")
                            .and_then(timestamp)
                            .or(time)
                            .zip(session_start)
                            .is_some_and(|(a, b)| a >= b)
                    });
                if !tid.is_empty() && starts_own {
                    own_tasks.push(tid.into());
                }
            }
            if typ == "turn_context" {
                let tid = text(p, "turn_id");
                let own = !replay_meta
                    || uuid_time(tid)
                        .zip(uuid_time(&session))
                        .map(|(a, b)| {
                            a > b || (a == b && (user_fork || own_tasks.iter().any(|x| x == tid)))
                        })
                        .unwrap_or(user_fork || own_tasks.iter().any(|x| x == tid));
                if own {
                    waiting = false;
                } else {
                    continue;
                }
            } else {
                if typ == "event_msg" && ptyp == "token_count" {
                    if let Some(t) = p.pointer("/info/total_token_usage").and_then(Total::parse) {
                        previous = Some(t);
                    }
                }
                continue;
            }
        }
        let changes_turn = (typ == "event_msg" && ptyp == "task_started")
            || (typ == "turn_context"
                && !text(p, "turn_id").is_empty()
                && text(p, "turn_id") != turn);
        let ends_turn = typ == "event_msg"
            && matches!(ptyp, "task_complete" | "turn_aborted")
            && pending
                .as_ref()
                .is_some_and(|x| text(p, "turn_id").is_empty() || x.turn == text(p, "turn_id"));
        if changes_turn || ends_turn {
            emit_pending(
                &mut pending,
                &mut settled,
                &mut result.records,
                source_id,
                &session,
                &parent,
                &surface,
                &project,
            );
        }
        if let Some(m) = model(p) {
            current_model = m;
        }
        if typ == "turn_context" || (typ == "event_msg" && ptyp == "task_started") {
            if !text(p, "turn_id").is_empty() {
                turn = text(p, "turn_id").into();
            }
        }
        if typ == "compacted" {
            if let Some(x) = pending.as_mut() {
                x.compacted = true;
            }
            continue;
        }
        if typ == "token_usage_record" {
            let thread = text(p, "thread_id");
            if !thread.is_empty() && !session.is_empty() && thread != session {
                continue;
            }
            emit_pending(
                &mut pending,
                &mut settled,
                &mut result.records,
                source_id,
                &session,
                &parent,
                &surface,
                &project,
            );
            if let Some(usage) = p.get("usage").and_then(Total::parse) {
                pending = Some(Pending {
                    usage,
                    time,
                    response: text(p, "response_id").into(),
                    turn: turn.clone(),
                    model: current_model.clone(),
                    compacted: false,
                });
            }
            continue;
        }
        if typ != "event_msg" || ptyp != "token_count" {
            continue;
        }
        ordinal += 1;
        let Some(info) = p.get("info") else {
            continue;
        };
        let total = info.get("total_token_usage").and_then(Total::parse);
        let last = info.get("last_token_usage").and_then(Total::parse);
        if let (Some(t), Some(prev)) = (total, previous) {
            if t.same_response(prev) {
                // Cache/reasoning fields can arrive after input/output. Refine
                // the existing response rather than appending its usage again.
                // A cumulative-only refinement is assignable only when that
                // response itself accounts for the cumulative scope.
                if let Some((index, prior_usage)) = last_counted {
                    let refinement = match (
                        last.filter(|l| prior_usage.same_response(*l)),
                        prior_usage.same_response(prev).then_some(t),
                    ) {
                        (Some(a), Some(b)) => Some(a.refine(b)),
                        (a, b) => a.or(b),
                    };
                    if let Some(refinement) = refinement {
                        let refined = prior_usage.refine(refinement);
                        result.records[index].tokens = refined.tokens();
                        last_counted = Some((index, refined));
                    } else if t != prev {
                        coverage.errors += 1;
                    }
                }
                previous = Some(prev.refine(t));
                // The post-compaction estimate may not advance totals; settle
                // its pending compaction-only response exactly as before.
                if pending.as_ref().is_some_and(|x| x.compacted) {
                    emit_pending(
                        &mut pending,
                        &mut settled,
                        &mut result.records,
                        source_id,
                        &session,
                        &parent,
                        &surface,
                        &project,
                    );
                }
                continue;
            }
        }
        let emitted = match (total, last, previous) {
            (Some(t), Some(l), Some(prev)) => {
                if t == prev || (t.delta(prev).is_none() && t.stale_regression(prev, l)) {
                    None
                } else {
                    if t.delta(prev).is_none() {
                        epoch += 1;
                    }
                    Some((l, Some(t)))
                }
            }
            (Some(t), Some(l), None) => Some((l, Some(t))),
            (Some(t), None, Some(prev)) => match t.delta(prev) {
                Some(d) => Some((d, Some(t))),
                None => {
                    previous = Some(t);
                    epoch += 1;
                    coverage.errors += 1;
                    None
                }
            },
            (Some(t), None, None) => Some((t, Some(t))),
            (None, Some(l), prev) => {
                if last_only_seen
                    .as_ref()
                    .is_some_and(|(ts, tr, u)| *ts == time && *tr == turn && *u == l)
                {
                    None
                } else {
                    last_only_seen = Some((time, turn.clone(), l));
                    Some((l, prev.map(|x| x.add(l))))
                }
            }
            _ => None,
        }
        .filter(|(u, _)| u.size() > 0);
        let Some((usage, next)) = emitted else {
            if pending.as_ref().is_some_and(|x| x.compacted) {
                emit_pending(
                    &mut pending,
                    &mut settled,
                    &mut result.records,
                    source_id,
                    &session,
                    &parent,
                    &surface,
                    &project,
                );
            }
            continue;
        };
        previous = next;
        let response = if pending
            .as_ref()
            .is_some_and(|x| x.usage.same_response(usage))
        {
            pending.take().map(|x| x.response)
        } else if let Some(i) = settled.iter().position(|x| x.same_response(usage)) {
            settled.remove(i);
            continue;
        } else if pending.as_ref().is_some_and(|x| x.compacted) {
            emit_pending(
                &mut pending,
                &mut settled,
                &mut result.records,
                source_id,
                &session,
                &parent,
                &surface,
                &project,
            );
            None
        } else {
            pending.take().map(|x| x.response)
        };
        let request = if let Some(response) = response.filter(|s| !s.is_empty()) {
            stable_id(&["codex-response", &session, &response])
        } else if let Some(t) = total {
            // Detail refinements do not change response identity across copies.
            stable_id(&[
                "codex-total",
                &session,
                &epoch.to_string(),
                &format!("{}:{}", t.input, t.output),
            ])
        } else {
            stable_id(&[
                "codex-event",
                &session,
                &turn,
                &time.unwrap_or(0).to_string(),
                &ordinal.to_string(),
                &usage.key(),
            ])
        };
        last_counted = Some((result.records.len(), usage));
        result.records.push(UsageRecord {
            id: request.clone(),
            source_id: source_id.into(),
            app: "codex".into(),
            surface: surface.clone(),
            account: "unknown".into(),
            session: stable_id(&["codex-session", &session]),
            request,
            timestamp: time,
            model: current_model.clone(),
            project: project.clone(),
            tokens: usage.tokens(),
            provenance: "local-recorded".into(),
        });
    }
    // A final pending record may still receive a token_count. Do not treat it
    // as final; preserve a coverage diagnostic until the next scan resolves it.
    if pending.is_some() {
        coverage
            .message
            .push_str(" Latest response usage is pending confirmation.");
    }
    let identity_conflicts = dedup(&mut result.records);
    coverage.errors += identity_conflicts;
    if identity_conflicts > 0 {
        coverage.message.push_str(&format!(
            " Held out {identity_conflicts} conflicting logical request IDs."
        ));
    }
    coverage.records = result.records.len();
    coverage.undated = result
        .records
        .iter()
        .filter(|x| x.timestamp.is_none())
        .count();
    if coverage.errors > 0 {
        coverage
            .message
            .push_str(" Some rows were unreadable or counters reset; totals may be incomplete.");
    }
    result.coverage.push(coverage);
    result
}

#[cfg(test)]
mod codex_refinement_tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    fn scan(rows: Vec<Value>) -> ScanResult {
        let meta = json!({"type":"session_meta","payload":{"id":"refinement-session"}});
        let bytes = std::iter::once(meta)
            .chain(rows)
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        codex(Cursor::new(bytes), "fixture", "fixture")
    }
    fn snapshot(input: u64, output: u64, cache: u64, reasoning: u64, with_last: bool) -> Value {
        let usage = json!({"input_tokens":input,"output_tokens":output,"cached_input_tokens":cache,"reasoning_output_tokens":reasoning});
        let mut info = json!({"total_token_usage":usage});
        if with_last {
            info["last_token_usage"] = usage;
        }
        json!({"type":"event_msg","timestamp":"2026-11-05T10:00:00Z","payload":{"type":"token_count","info":info}})
    }
    #[test]
    fn unchanged_cumulative_totals_refine_details_instead_of_counting_again() {
        let r = scan(vec![
            snapshot(100, 10, 0, 0, true),
            snapshot(100, 10, 50, 5, true),
        ]);
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].tokens.total(), 110);
        assert_eq!(r.records[0].tokens.input, 50);
        assert_eq!(r.records[0].tokens.cache_read, 50);
        assert_eq!(r.records[0].tokens.reasoning, 5);
    }
    #[test]
    fn total_only_cache_refinement_has_stable_identity() {
        let a = scan(vec![
            snapshot(100, 10, 0, 0, false),
            snapshot(100, 10, 50, 5, false),
        ]);
        let b = scan(vec![snapshot(100, 10, 50, 5, false)]);
        assert_eq!(a.records.len(), 1);
        assert_eq!(a.records[0].tokens.total(), 110);
        assert_eq!(a.records[0].tokens.cache_read, 50);
        assert_eq!(a.records[0].id, b.records[0].id);
    }
    #[test]
    fn cumulative_details_can_refine_a_bare_last_usage() {
        let mut refined = snapshot(100, 10, 50, 5, true);
        refined["payload"]["info"]["last_token_usage"] =
            json!({"input_tokens":100,"output_tokens":10});
        let r = scan(vec![snapshot(100, 10, 0, 0, true), refined]);
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].tokens.cache_read, 50);
        assert_eq!(r.records[0].tokens.total(), 110);
    }
    #[test]
    fn retained_reasoning_context_flag_does_not_change_output_units() {
        for flag in [true, false] {
            let mut row = snapshot(100, 10, 50, 3, true);
            row["payload"]["info"]["last_usage_includes_reasoning"] = json!(flag);
            let r = scan(vec![row]);
            assert_eq!(r.records[0].tokens.total(), 110);
            assert_eq!(r.records[0].tokens.output, 10);
            assert_eq!(r.records[0].tokens.reasoning, 3);
        }
    }
    #[test]
    fn detail_regression_does_not_erase_new_response() {
        let mut second = snapshot(150, 15, 40, 2, true);
        second["payload"]["info"]["last_token_usage"] = json!({"input_tokens":50,"output_tokens":5,"cached_input_tokens":10,"reasoning_output_tokens":1});
        let r = scan(vec![snapshot(100, 10, 50, 3, true), second]);
        assert_eq!(r.records.len(), 2);
        assert_eq!(r.records.iter().map(|r| r.tokens.total()).sum::<u64>(), 165);
    }
}

fn uuid_time(s: &str) -> Option<u64> {
    let s = s.replace('-', "");
    if s.len() != 32 || s.as_bytes().get(12) != Some(&b'7') {
        return None;
    }
    u64::from_str_radix(&s[..12], 16).ok()
}

fn emit_pending(
    pending: &mut Option<Pending>,
    settled: &mut VecDeque<Total>,
    out: &mut Vec<UsageRecord>,
    source: &str,
    session: &str,
    _parent: &str,
    surface: &str,
    project: &str,
) {
    let Some(p) = pending.take() else {
        return;
    };
    if p.usage.size() == 0 {
        return;
    }
    let key = if p.response.is_empty() {
        stable_id(&[
            "codex-unconfirmed",
            session,
            &p.time.unwrap_or(0).to_string(),
            &p.usage.key(),
        ])
    } else {
        stable_id(&["codex-response", session, &p.response])
    };
    out.push(UsageRecord {
        id: key.clone(),
        source_id: source.into(),
        app: "codex".into(),
        surface: surface.into(),
        account: "unknown".into(),
        session: stable_id(&["codex-session", session]),
        request: key,
        timestamp: p.time,
        model: p.model,
        project: project.into(),
        tokens: p.usage.tokens(),
        provenance: "local-recorded".into(),
    });
    settled.push_back(p.usage);
    if settled.len() > 16 {
        settled.pop_front();
    }
}

pub fn claude<R: BufRead>(
    mut reader: R,
    source: &str,
    file_identity: &str,
    surface: &str,
) -> ScanResult {
    let mut result = ScanResult::default();
    let mut coverage = Coverage::new(
        source,
        "claude",
        "partial",
        "Retained Claude transcripts only; missing sessions or usage fields are unknown.",
    );
    let mut entries: BTreeMap<String, (UsageRecord, bool)> = BTreeMap::new();
    let mut raw = Vec::new();
    let mut n = 0;
    loop {
        raw.clear();
        match reader
            .by_ref()
            .take((MAX_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut raw)
        {
            Ok(0) => break,
            Ok(_) if raw.len() > MAX_LINE_BYTES => {
                coverage.errors += 1;
                coverage.status = "stale".into();
                break;
            }
            Err(_) => {
                coverage.errors += 1;
                coverage.status = "stale".into();
                break;
            }
            _ => {}
        }
        n += 1;
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let v: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(_) => {
                coverage.errors += 1;
                continue;
            }
        };
        let typ = text(&v, "type");
        let m = v.get("message").unwrap_or(&Value::Null);
        if typ != "assistant" && text(m, "role") != "assistant" {
            continue;
        }
        let Some(u) = m.get("usage").filter(|x| x.is_object()) else {
            coverage.errors += 1;
            continue;
        };
        if [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ]
        .iter()
        .any(|k| {
            u.get(*k)
                .is_some_and(|x| !x.is_null() && count(Some(x)).is_none())
        }) {
            coverage.errors += 1;
            continue;
        }
        if ![
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ]
        .iter()
        .any(|k| count(u.get(*k)).is_some())
        {
            coverage.errors += 1;
            continue;
        }
        let mid = text(m, "id");
        let rid = text(&v, "requestId");
        let key = if mid.is_empty() {
            stable_id(&["claude-fallback", file_identity, &n.to_string()])
        } else {
            stable_id(&["claude-request", rid, mid])
        };
        let split = u.get("cache_read_input_tokens").is_some()
            || u.get("cache_creation_input_tokens").is_some()
            || u.get("cache_creation").is_some();
        let write = count(u.get("cache_creation_input_tokens")).unwrap_or_else(|| {
            count(u.pointer("/cache_creation/ephemeral_5m_input_tokens"))
                .unwrap_or(0)
                .saturating_add(
                    count(u.pointer("/cache_creation/ephemeral_1h_input_tokens")).unwrap_or(0),
                )
        });
        let tokens = Tokens {
            input: count(u.get("input_tokens")).unwrap_or(0),
            output: count(u.get("output_tokens")).unwrap_or(0),
            cache_read: count(u.get("cache_read_input_tokens")).unwrap_or(0),
            cache_write: write,
            reasoning: 0,
        };
        let record = UsageRecord {
            id: key.clone(),
            source_id: source.into(),
            app: "claude".into(),
            surface: if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                "code-subagent".into()
            } else {
                surface.into()
            },
            account: account(&v),
            session: stable_id(&["claude-session", text(&v, "sessionId")]),
            request: key.clone(),
            timestamp: v.get("timestamp").and_then(timestamp),
            model: model(m).unwrap_or("unknown".into()),
            project: project_label(v.get("cwd").and_then(Value::as_str)),
            tokens,
            provenance: "local-recorded".into(),
        };
        // Preserve contradictory copies until the final identity quarantine;
        // normal streaming keys and request IDs remain unchanged.
        let assembly_key = if entries
            .get(&key)
            .is_some_and(|(old, _)| identity_conflict(old, &record))
        {
            stable_id(&[
                "claude-conflicting-copy",
                &key,
                &record.account,
                comparable_model(&record.model),
            ])
        } else {
            key.clone()
        };
        for (index, iteration) in u
            .get("iterations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if text(iteration, "type") != "advisor_message" || text(iteration, "model").is_empty() {
                continue;
            }
            if ![
                "input_tokens",
                "output_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ]
            .iter()
            .any(|k| count(iteration.get(*k)).is_some())
            {
                continue;
            }
            let mut advisor = record.clone();
            advisor.id = stable_id(&["claude-advisor", &record.id, &index.to_string()]);
            advisor.request = advisor.id.clone();
            advisor.model = text(iteration, "model").into();
            advisor.tokens = Tokens {
                input: count(iteration.get("input_tokens")).unwrap_or(0),
                output: count(iteration.get("output_tokens")).unwrap_or(0),
                cache_read: count(iteration.get("cache_read_input_tokens")).unwrap_or(0),
                cache_write: count(iteration.get("cache_creation_input_tokens")).unwrap_or(0),
                reasoning: 0,
            };
            let advisor_key = if entries
                .get(&advisor.id)
                .is_some_and(|(old, _)| identity_conflict(old, &advisor))
            {
                stable_id(&[
                    "claude-conflicting-copy",
                    &advisor.id,
                    &advisor.account,
                    comparable_model(&advisor.model),
                ])
            } else {
                advisor.id.clone()
            };
            if let Some((old, _)) = entries.get_mut(&advisor_key) {
                old.tokens.input = old.tokens.input.max(advisor.tokens.input);
                old.tokens.output = old.tokens.output.max(advisor.tokens.output);
                old.tokens.cache_read = old.tokens.cache_read.max(advisor.tokens.cache_read);
                old.tokens.cache_write = old.tokens.cache_write.max(advisor.tokens.cache_write);
            } else {
                entries.insert(advisor_key, (advisor, true));
            }
        }
        if let Some((old, old_split)) = entries.get_mut(&assembly_key) {
            if split && !*old_split {
                if u.get("input_tokens").is_some() {
                    old.tokens.input = record.tokens.input;
                }
            } else if !(!split && *old_split) {
                old.tokens.input = old.tokens.input.max(record.tokens.input);
            }
            old.tokens.output = old.tokens.output.max(record.tokens.output);
            old.tokens.cache_read = old.tokens.cache_read.max(record.tokens.cache_read);
            old.tokens.cache_write = old.tokens.cache_write.max(record.tokens.cache_write);
            if old.timestamp.is_none() {
                old.timestamp = record.timestamp;
            }
            if old.model == "unknown" {
                old.model = record.model;
            }
            if !known_identity(&old.account) && known_identity(&record.account) {
                old.account = record.account;
            }
            if old.project == "Unknown project" {
                old.project = record.project;
            }
            *old_split |= split;
        } else {
            entries.insert(assembly_key, (record, split));
        }
    }
    result.records = entries.into_values().map(|(r, _)| r).collect();
    let conflicts = dedup(&mut result.records);
    coverage.errors += conflicts;
    if conflicts > 0 {
        coverage.message.push_str(&format!(
            " Held out {conflicts} conflicting logical request IDs."
        ));
    }
    coverage.records = result.records.len();
    coverage.undated = result
        .records
        .iter()
        .filter(|r| r.timestamp.is_none())
        .count();
    result.coverage.push(coverage);
    result
}

fn known_identity(value: &str) -> bool {
    !matches!(value.trim(), "" | "unknown" | "<synthetic>")
}

fn comparable_model(value: &str) -> &str {
    value.trim().strip_suffix("[1m]").unwrap_or(value.trim())
}

pub(crate) fn identity_conflict(a: &UsageRecord, b: &UsageRecord) -> bool {
    a.app != b.app
        || (known_identity(&a.account) && known_identity(&b.account) && a.account != b.account)
        || (known_identity(&a.model)
            && known_identity(&b.model)
            && comparable_model(&a.model) != comparable_model(&b.model))
}

/// Same response in active/archive logs or transcript/cache copies counts once.
/// Conflicting known identities quarantine the logical ID for this entire batch,
/// including later copies. Return the number of quarantined logical IDs.
pub fn dedup(records: &mut Vec<UsageRecord>) -> usize {
    let mut map: BTreeMap<String, UsageRecord> = BTreeMap::new();
    let mut quarantined = HashSet::new();
    for r in records.drain(..) {
        if quarantined.contains(&r.id) {
            continue;
        }
        if map.get(&r.id).is_some_and(|old| identity_conflict(old, &r)) {
            map.remove(&r.id);
            quarantined.insert(r.id);
            continue;
        }
        if let Some(old) = map.get_mut(&r.id) {
            if r.app == "claude" {
                let a = old.tokens.cache_read > 0 || old.tokens.cache_write > 0;
                let b = r.tokens.cache_read > 0 || r.tokens.cache_write > 0;
                old.tokens.input = match (a, b) {
                    // A zero ordinary-input remainder is valid. Prefer the
                    // cache-bearing snapshot even when its remainder is zero;
                    // max with a bare whole-prompt snapshot would count it twice.
                    (true, false) => old.tokens.input,
                    (false, true) => r.tokens.input,
                    _ => old.tokens.input.max(r.tokens.input),
                };
                old.tokens.output = old.tokens.output.max(r.tokens.output);
                old.tokens.cache_read = old.tokens.cache_read.max(r.tokens.cache_read);
                old.tokens.cache_write = old.tokens.cache_write.max(r.tokens.cache_write);
            } else if r.app == "codex"
                && old
                    .tokens
                    .input
                    .saturating_add(old.tokens.cache_read)
                    .saturating_add(old.tokens.cache_write)
                    == r.tokens
                        .input
                        .saturating_add(r.tokens.cache_read)
                        .saturating_add(r.tokens.cache_write)
                && old.tokens.output == r.tokens.output
            {
                // Cache/reasoning refinements do not add another response.
                let input_total = old
                    .tokens
                    .input
                    .saturating_add(old.tokens.cache_read)
                    .saturating_add(old.tokens.cache_write);
                old.tokens.cache_read = old
                    .tokens
                    .cache_read
                    .max(r.tokens.cache_read)
                    .min(input_total);
                old.tokens.cache_write = old
                    .tokens
                    .cache_write
                    .max(r.tokens.cache_write)
                    .min(input_total - old.tokens.cache_read);
                old.tokens.input = input_total - old.tokens.cache_read - old.tokens.cache_write;
                old.tokens.reasoning = old
                    .tokens
                    .reasoning
                    .max(r.tokens.reasoning)
                    .min(old.tokens.output);
            } else if r.tokens.total() > old.tokens.total() {
                old.tokens = r.tokens;
            }
            if old.timestamp.is_none() {
                old.timestamp = r.timestamp;
            }
            if !known_identity(&old.account) && known_identity(&r.account) {
                old.account = r.account;
            }
            if !known_identity(&old.model) && known_identity(&r.model) {
                old.model = r.model;
            }
            if old.project == "Unknown project" {
                old.project = r.project;
            }
        } else {
            map.insert(r.id.clone(), r);
        }
    }
    *records = map.into_values().collect();
    quarantined.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    fn parse(rows: Vec<Value>) -> ScanResult {
        let s = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        codex(Cursor::new(s), "local:codex", "fixture")
    }
    fn meta() -> Value {
        json!({"type":"session_meta","payload":{"id":"s","originator":"Codex Desktop","cwd":"/private/project"}})
    }
    fn tc(total: u64, last: u64) -> Value {
        json!({"type":"event_msg","timestamp":"2026-11-05T10:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":total,"output_tokens":total/10,"cached_input_tokens":total/2},"last_token_usage":{"input_tokens":last,"output_tokens":last/10,"cached_input_tokens":last/2,"reasoning_output_tokens":last/20}}}})
    }
    #[test]
    fn canonical_usage_and_record_count_once() {
        let r = parse(vec![
            meta(),
            json!({"type":"token_usage_record","payload":{"response_id":"r1","thread_id":"s","usage":{"input_tokens":100,"output_tokens":10}}}),
            tc(100, 100),
            tc(100, 100),
        ]);
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].tokens.total(), 110);
        assert_eq!(r.records[0].tokens.input, 50);
        assert_eq!(r.records[0].tokens.reasoning, 5);
    }
    #[test]
    fn late_confirmation_and_compaction() {
        let r = parse(vec![
            meta(),
            tc(100, 100),
            json!({"type":"token_usage_record","timestamp":"2026-11-05T10:01:00Z","payload":{"response_id":"compact","usage":{"input_tokens":20,"output_tokens":2}}}),
            json!({"type":"compacted","payload":{}}),
            tc(100, 100),
            tc(120, 20),
        ]);
        assert_eq!(r.records.iter().map(|r| r.tokens.total()).sum::<u64>(), 132);
    }
    #[test]
    fn hard_reset_uses_last_and_ignores_other_thread() {
        let r = parse(vec![
            meta(),
            tc(1000, 1000),
            tc(10, 10),
            json!({"type":"token_usage_record","payload":{"thread_id":"other","usage":{"input_tokens":300,"output_tokens":30}}}),
            tc(20, 10),
        ]);
        assert_eq!(
            r.records.iter().map(|r| r.tokens.total()).sum::<u64>(),
            1122
        );
    }
    #[test]
    fn final_pending_is_not_falsely_final() {
        let r = parse(vec![
            meta(),
            json!({"type":"token_usage_record","payload":{"usage":{"input_tokens":3,"output_tokens":2}}}),
        ]);
        assert!(r.records.is_empty());
        assert!(r.coverage[0].message.contains("pending"));
    }
    #[test]
    fn claude_streaming_split_and_archive_dedup() {
        let s=[json!({"type":"assistant","requestId":"req","timestamp":"2026-11-05T10:00:00Z","message":{"id":"msg","model":"claude","usage":{"input_tokens":100,"output_tokens":0}}}),json!({"type":"assistant","requestId":"req","timestamp":"2026-11-05T10:00:01Z","message":{"id":"msg","model":"claude","usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":90,"cache_creation_input_tokens":5}}})].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        let mut r = claude(Cursor::new(s), "local:claude", "fixture", "code");
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].tokens.total(), 125);
        r.records.push(r.records[0].clone());
        dedup(&mut r.records);
        assert_eq!(r.records.len(), 1);
    }
    #[test]
    fn child_replay_is_not_child_usage() {
        let r = parse(vec![
            json!({"type":"session_meta","payload":{"id":"child","forked_from_id":"parent","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}}}}),
            tc(100, 100),
            json!({"type":"turn_context","payload":{"turn_id":"own","model":"gpt"}}),
            tc(110, 10),
        ]);
        assert_eq!(r.records.iter().map(|r| r.tokens.total()).sum::<u64>(), 11);
    }
    #[test]
    fn invalid_and_undated_are_reported() {
        let r=claude(Cursor::new("not json\n{\"type\":\"assistant\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":2}}}"),"local:claude","fixture","code");
        assert_eq!(r.coverage[0].errors, 1);
        assert_eq!(r.coverage[0].undated, 1);
        assert_eq!(r.records[0].tokens.total(), 2);
    }
    #[test]
    fn malformed_numeric_usage_is_not_a_zero_input() {
        let r = claude(
            Cursor::new(
                r#"{"type":"assistant","message":{"id":"m","usage":{"input_tokens":-3,"output_tokens":4}}}"#,
            ),
            "local:claude",
            "fixture",
            "code",
        );
        assert!(r.records.is_empty());
        assert_eq!(r.coverage[0].errors, 1);
        assert_eq!(count(Some(&json!(2.5))), None);
        assert_eq!(count(Some(&json!(9007199254740993_u64))), None);
    }
    #[test]
    fn advisor_iterations_are_supplemental_and_deduped() {
        let row = json!({"type":"assistant","requestId":"r","message":{"id":"m","usage":{"input_tokens":10,"output_tokens":2,"iterations":[{"type":"advisor_message","model":"advisor","input_tokens":20,"output_tokens":3}]}}});
        let s = format!("{row}\n{row}\n");
        let r = claude(Cursor::new(s), "local:claude", "fixture", "code");
        assert_eq!(r.records.len(), 2);
        assert_eq!(r.records.iter().map(|r| r.tokens.total()).sum::<u64>(), 35);
    }

    #[test]
    fn cross_file_cache_split_keeps_explicit_zero_ordinary_input() {
        let bare = json!({"type":"assistant","requestId":"r","sessionId":"s","message":{"id":"m","model":"claude","usage":{"input_tokens":100,"output_tokens":5}}});
        let split = json!({"type":"assistant","requestId":"r","sessionId":"s","message":{"id":"m","model":"claude","usage":{"input_tokens":0,"output_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":0}}});
        let a = claude(Cursor::new(bare.to_string()), "a", "a-file", "code").records;
        let b = claude(Cursor::new(split.to_string()), "b", "b-file", "code").records;
        for mut records in [
            a.iter().chain(&b).cloned().collect(),
            b.iter().chain(&a).cloned().collect(),
        ] {
            dedup(&mut records);
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].tokens.input, 0);
            assert_eq!(records[0].tokens.cache_read, 100);
            assert_eq!(records[0].tokens.total(), 105);
        }
    }

    #[test]
    fn contradictory_accounts_quarantine_all_copies_including_later_ones() {
        let row = json!({"type":"assistant","requestId":"r","message":{"id":"m","model":"claude","usage":{"input_tokens":10,"output_tokens":2}}});
        let original = claude(Cursor::new(row.to_string()), "source", "fixture", "code")
            .records
            .remove(0);
        let mut a = original.clone();
        a.account = "account-a".into();
        let mut b = original.clone();
        b.account = "account-b".into();
        let mut safe = original;
        safe.id = "other-request".into();
        let mut records = vec![a.clone(), b, a, safe];
        assert_eq!(dedup(&mut records), 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "other-request");
    }

    #[test]
    fn model_conflicts_are_quarantined_but_unknown_upgrades_and_context_suffixes_match() {
        let row = json!({"type":"assistant","requestId":"r","message":{"id":"m","usage":{"input_tokens":10,"output_tokens":2}}});
        let original = claude(Cursor::new(row.to_string()), "source", "fixture", "code")
            .records
            .remove(0);
        let mut known = original.clone();
        known.model = "claude-future[1m]".into();
        known.account = "account".into();
        let mut same = known.clone();
        same.model = "claude-future".into();
        let mut records = vec![original, known, same.clone()];
        assert_eq!(dedup(&mut records), 0);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].account, "account");
        let mut different = same;
        different.model = "different-model".into();
        records.push(different);
        assert_eq!(dedup(&mut records), 1);
        assert!(records.is_empty());
    }

    #[test]
    fn codex_duplicate_cache_refinement_preserves_inclusive_consumption() {
        let original = parse(vec![meta(), tc(100, 100)]).records.remove(0);
        let mut bare = original.clone();
        bare.tokens.input = 100;
        bare.tokens.cache_read = 0;
        bare.tokens.reasoning = 0;
        let mut refined = original;
        refined.tokens.input = 50;
        refined.tokens.cache_read = 50;
        refined.tokens.reasoning = 5;
        for mut records in [vec![bare.clone(), refined.clone()], vec![refined, bare]] {
            assert_eq!(dedup(&mut records), 0);
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].tokens.total(), 110);
            assert_eq!(records[0].tokens.cache_read, 50);
            assert_eq!(records[0].tokens.reasoning, 5);
        }
    }

    #[test]
    fn same_file_streaming_conflict_is_not_hidden_by_assembly() {
        let a = json!({"type":"assistant","requestId":"r","accountUuid":"a","orgUuid":"o","message":{"id":"m","model":"claude-a","usage":{"input_tokens":10,"output_tokens":2}}});
        let mut b = a.clone();
        b["message"]["model"] = json!("claude-b");
        let result = claude(
            Cursor::new(format!("{a}\n{b}\n{a}\n")),
            "source",
            "fixture",
            "code",
        );
        assert!(result.records.is_empty());
        assert_eq!(result.coverage[0].errors, 1);
    }
}
