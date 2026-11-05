//! Read-only Claude Desktop usage collection.
//!
//! Adapted from Tokscale v4.21.0 (MIT; see vendor/tokscale/LICENSE).
//! The IndexedDB, MANIFEST and V8 readers are vendored upstream code. This
//! integration retains account/product provenance and exposes coverage instead
//! of inheriting Tokscale's networking, credentials, caches or configuration.
mod indexeddb;
mod manifest;
#[allow(dead_code)]
mod v8;

use crate::model::{stable_id, Coverage, ScanResult, Tokens, UsageRecord};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

#[derive(Default)]
struct Diagnostics {
    truncated: usize,
    missing_usage: usize,
    resets: usize,
    errors: usize,
}

#[derive(Default, Clone)]
struct Conversation {
    account: String,
    surface: String,
    session: String,
    session_ids: HashSet<String>,
    messages: Vec<UsageRecord>,
    totals: BTreeMap<String, Tokens>,
    last_snapshot: i64,
}

fn valid_count(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .filter(|n| *n <= 9_007_199_254_740_991)
        .or_else(|| {
            value
                .as_f64()
                .filter(|n| {
                    n.is_finite() && *n >= 0.0 && n.fract() == 0.0 && *n <= 9_007_199_254_740_991.0
                })
                .map(|n| n as u64)
        })
}

fn count(value: &Value) -> u64 {
    valid_count(value).unwrap_or(0)
}

fn has_usage(value: &Value, cumulative: bool) -> bool {
    let fields = if cumulative {
        [
            "inputTokens",
            "outputTokens",
            "cacheReadInputTokens",
            "cacheCreationInputTokens",
        ]
    } else {
        [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ]
    };
    fields
        .iter()
        .any(|field| valid_count(&value[*field]).is_some())
}

fn tokens(value: &Value, cumulative: bool) -> Tokens {
    let fields = if cumulative {
        [
            "inputTokens",
            "outputTokens",
            "cacheReadInputTokens",
            "cacheCreationInputTokens",
        ]
    } else {
        [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ]
    };
    Tokens {
        input: count(&value[fields[0]]),
        output: count(&value[fields[1]]),
        cache_read: count(&value[fields[2]]),
        cache_write: count(&value[fields[3]]),
        reasoning: 0,
    }
}

fn timestamp(value: &Value) -> i64 {
    if let Some(ts) = value
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    {
        return ts.timestamp_millis().max(0);
    }
    let n = count(value);
    let millis = if n > 0 && n < 100_000_000_000 {
        n.saturating_mul(1000)
    } else {
        n
    };
    i64::try_from(millis).unwrap_or(0)
}

fn model_id(model: &str) -> String {
    model
        .trim()
        .strip_suffix("[1m]")
        .unwrap_or(model.trim())
        .to_string()
}

fn real_model(model: &str) -> bool {
    !model.trim().is_empty() && !matches!(model, "<synthetic>" | "unknown")
}

fn max_tokens(dst: &mut Tokens, src: &Tokens) {
    dst.input = dst.input.max(src.input);
    dst.output = dst.output.max(src.output);
    dst.cache_read = dst.cache_read.max(src.cache_read);
    dst.cache_write = dst.cache_write.max(src.cache_write);
    dst.reasoning = dst.reasoning.max(src.reasoning).min(dst.output);
}

fn subtract(total: &Tokens, observed: &Tokens) -> Tokens {
    Tokens {
        input: total.input.saturating_sub(observed.input),
        output: total.output.saturating_sub(observed.output),
        cache_read: total.cache_read.saturating_sub(observed.cache_read),
        cache_write: total.cache_write.saturating_sub(observed.cache_write),
        reasoning: 0,
    }
}

/// Merge split and bare usage snapshots using Tokscale's completeness rule.
/// A bare prompt snapshot can include the whole prompt; a cache-bearing copy
/// describes the ordinary-input remainder and wins over that whole-prompt copy.
fn merge_message(old: &mut UsageRecord, new: &UsageRecord) {
    let old_cached = old.tokens.cache_read > 0 || old.tokens.cache_write > 0;
    let new_cached = new.tokens.cache_read > 0 || new.tokens.cache_write > 0;
    let input = match (old_cached, new_cached) {
        // Explicit zero is a valid ordinary-input remainder. If a partial copy
        // omitted it, retaining zero is conservative; retaining a bare prompt's
        // whole-input figure would inflate a single response by the cache size.
        (true, false) => old.tokens.input,
        (false, true) => new.tokens.input,
        _ => old.tokens.input.max(new.tokens.input),
    };
    max_tokens(&mut old.tokens, &new.tokens);
    old.tokens.input = input;
    if old.timestamp.is_none() {
        old.timestamp = new.timestamp;
    }
    if old.account == "unknown" {
        old.account.clone_from(&new.account);
    }
    if new.surface == "cowork" {
        old.surface.clone_from(&new.surface);
    }
}

fn identity(value: &Value) -> Option<String> {
    Some(stable_id(&[
        value.get("accountUuid")?.as_str()?,
        value.get("orgUuid")?.as_str()?,
        value.get("conversationUuid")?.as_str()?,
    ]))
}

fn live_trees(values: Vec<indexeddb::StoreValue>, diagnostics: &mut Diagnostics) -> Vec<Value> {
    let mut trees: BTreeMap<String, Value> = BTreeMap::new();
    let mut deleted = HashSet::new();
    for record in values {
        let Some(key) = identity(&record.value) else {
            diagnostics.errors += 1;
            continue;
        };
        if record.store == "meta" {
            if record.value["tombstone"].as_bool() == Some(true)
                || record.value.get("deletedAt").is_some_and(|v| !v.is_null())
            {
                deleted.insert(key);
            }
        } else if trees
            .get(&key)
            .is_none_or(|old| timestamp(&record.value["writtenAt"]) >= timestamp(&old["writtenAt"]))
        {
            trees.insert(key, record.value);
        }
    }
    trees
        .into_iter()
        .filter(|(key, _)| !deleted.contains(key))
        .map(|(_, value)| value)
        .collect()
}

fn parse_tree(value: &Value, diagnostics: &mut Diagnostics) -> Conversation {
    let account = match (value["accountUuid"].as_str(), value["orgUuid"].as_str()) {
        (Some(account), Some(org)) => stable_id(&["claude-account", account, org]),
        _ => "unknown".into(),
    };
    let raw_conversation = value["conversationUuid"].as_str().unwrap_or_default();
    let mut conversation = Conversation {
        account: account.clone(),
        surface: value["product"].as_str().unwrap_or("unknown").to_string(),
        session: stable_id(&["claude-session", raw_conversation]),
        ..Default::default()
    };
    let tree = &value["tree"];
    if tree["hasOlder"].as_bool() == Some(true)
        || tree
            .get("earlierCursor")
            .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
    {
        diagnostics.truncated += 1;
    }
    let mut previous: BTreeMap<String, Tokens> = BTreeMap::new();
    let mut messages: HashMap<String, UsageRecord> = HashMap::new();
    for collection in ["events", "messages", "chat_messages"] {
        for event in tree[collection].as_array().into_iter().flatten() {
            let payload = event.get("payload").unwrap_or(event);
            let ts = [
                payload.get("timestamp"),
                event.get("created_at"),
                event.get("serverCreatedAt"),
                payload.get("created_at"),
            ]
            .into_iter()
            .flatten()
            .map(timestamp)
            .find(|ts| *ts > 0)
            .unwrap_or(0);
            let kind = payload["type"]
                .as_str()
                .or_else(|| payload["role"].as_str())
                .or_else(|| payload["sender"].as_str())
                .unwrap_or_default();
            if kind == "result" {
                if let Some(usage) = payload["modelUsage"].as_object().filter(|u| !u.is_empty()) {
                    conversation.last_snapshot = conversation.last_snapshot.max(ts);
                    if let Some(sid) = payload["session_id"].as_str() {
                        conversation.session = stable_id(&["claude-session", sid]);
                        conversation
                            .session_ids
                            .insert(conversation.session.clone());
                    }
                    let mut snapshot: BTreeMap<String, Tokens> = BTreeMap::new();
                    for (model, usage) in usage {
                        if real_model(model) && has_usage(usage, true) {
                            snapshot
                                .entry(model_id(model))
                                .or_default()
                                .add(&tokens(usage, true));
                        } else if real_model(model) {
                            diagnostics.missing_usage += 1;
                        }
                    }
                    for (model, current) in snapshot {
                        if previous.get(&model).is_some_and(|old| {
                            current.input < old.input
                                || current.output < old.output
                                || current.cache_read < old.cache_read
                                || current.cache_write < old.cache_write
                        }) {
                            diagnostics.resets += 1;
                        }
                        previous.insert(model.clone(), current.clone());
                        max_tokens(conversation.totals.entry(model).or_default(), &current);
                    }
                }
                continue;
            }
            if kind != "assistant" {
                continue;
            }
            let message = payload.get("message").unwrap_or(payload);
            let (Some(model), Some(usage), Some(mid)) = (
                message["model"].as_str().filter(|s| real_model(s)),
                message.get("usage").filter(|u| u.is_object()),
                message["id"]
                    .as_str()
                    .or_else(|| message["uuid"].as_str())
                    .filter(|id| !id.is_empty()),
            ) else {
                diagnostics.missing_usage += 1;
                continue;
            };
            let sid = payload["session_id"]
                .as_str()
                .or_else(|| payload["sessionId"].as_str())
                .unwrap_or(raw_conversation);
            let session = stable_id(&["claude-session", sid]);
            conversation.session_ids.insert(session.clone());
            let raw_request = payload["request_id"]
                .as_str()
                .or_else(|| payload["requestId"].as_str())
                .unwrap_or("");
            let request = stable_id(&["claude-request", raw_request, mid]);
            let parent = UsageRecord {
                id: request.clone(),
                source_id: "claude-desktop".into(),
                app: "claude".into(),
                surface: conversation.surface.clone(),
                account: account.clone(),
                session,
                request,
                timestamp: (ts > 0).then_some(ts),
                model: model_id(model),
                project: "unknown".into(),
                tokens: tokens(usage, false),
                provenance: "local-cache".into(),
            };
            let mut incoming = Vec::new();
            if has_usage(usage, false) {
                incoming.push(parent.clone());
            } else {
                diagnostics.missing_usage += 1;
            }
            for (index, iteration) in usage["iterations"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                if iteration["type"].as_str() != Some("advisor_message") {
                    continue;
                }
                let Some(model) = iteration["model"].as_str().filter(|m| real_model(m)) else {
                    continue;
                };
                if !has_usage(iteration, false) {
                    diagnostics.missing_usage += 1;
                    continue;
                }
                let mut advisor = parent.clone();
                let index = index.to_string();
                advisor.id = stable_id(&["claude-advisor", &parent.id, &index]);
                advisor.request.clone_from(&advisor.id);
                advisor.model = model_id(model);
                advisor.tokens = tokens(iteration, false);
                incoming.push(advisor);
            }
            for msg in incoming {
                if let Some(old) = messages.get_mut(&msg.id) {
                    if old.model != msg.model {
                        diagnostics.errors += 1;
                        continue;
                    }
                    merge_message(old, &msg);
                } else {
                    messages.insert(msg.id.clone(), msg);
                }
            }
        }
    }
    conversation.messages = messages.into_values().collect();
    conversation
}

/// Append cache usage to timestamped local transcripts and reconcile cumulative
/// balances against both. Neither cache write time nor file mtime is usage time.
pub fn merge(home: &Path, result: &mut ScanResult) {
    let dir = home
        .join("Library/Application Support/Claude/IndexedDB/https_claude.ai_0.indexeddb.leveldb");
    let mut coverage = Coverage::new(
        "claude-desktop",
        "claude",
        "unavailable",
        "No retained Claude Desktop conversation cache found; cloud/account coverage is unknown.",
    );
    let mut checked = home.to_path_buf();
    for component in [
        "Library",
        "Application Support",
        "Claude",
        "IndexedDB",
        "https_claude.ai_0.indexeddb.leveldb",
    ] {
        checked.push(component);
        if checked
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            coverage.status = "stale".into();
            coverage.errors = 1;
            coverage.message =
                "Claude source path contains a symlink; the cache was not opened.".into();
            result.coverage.push(coverage);
            return;
        }
    }
    if !dir.is_dir() {
        result.coverage.push(coverage);
        return;
    }
    let (values, errors) = match indexeddb::read(&dir) {
        Ok(data) => data,
        Err(_) => {
            coverage.status = "stale".into();
            coverage.errors = 1;
            coverage.message = "Claude Desktop cache unavailable or changed during both read attempts; retain the previous valid snapshot.".into();
            result.coverage.push(coverage);
            return;
        }
    };
    let mut diagnostics = Diagnostics {
        errors: errors.len(),
        ..Default::default()
    };
    let mut conversations: Vec<_> = live_trees(values, &mut diagnostics)
        .into_iter()
        .map(|value| parse_tree(&value, &mut diagnostics))
        .collect();
    merge_conversations(&mut conversations, &mut result.records, &mut diagnostics);
    coverage.status = "partial".into();
    coverage.errors = diagnostics.errors;
    coverage.records = result
        .records
        .iter()
        .filter(|m| m.source_id == "claude-desktop")
        .count();
    coverage.undated = result
        .records
        .iter()
        .filter(|m| m.source_id == "claude-desktop" && m.timestamp.is_none())
        .count();
    let cowork = result
        .records
        .iter()
        .filter(|m| m.app == "claude" && m.surface == "cowork" && m.timestamp.is_some())
        .count();
    coverage.message = format!("Retained cache only; account-wide completeness is unknown. {cowork} timestamped Cowork records; {} truncated conversations; {} messages without usable counters; {} counter resets; {} decode/conflict errors. Undated balances are excluded from daily totals.", diagnostics.truncated, diagnostics.missing_usage, diagnostics.resets, diagnostics.errors);
    result.coverage.push(coverage);
}

fn merge_conversations(
    conversations: &mut Vec<Conversation>,
    records: &mut Vec<UsageRecord>,
    diagnostics: &mut Diagnostics,
) {
    let mut ids: HashMap<_, _> = records
        .iter()
        .enumerate()
        .map(|(i, record)| (record.id.clone(), i))
        .collect();
    let mut summaries: Vec<Conversation> = Vec::new();
    for mut conversation in conversations.drain(..) {
        for msg in conversation.messages.drain(..) {
            if let Some(index) = ids.get(&msg.id).copied() {
                let old = &mut records[index];
                if crate::parsers::identity_conflict(old, &msg) {
                    diagnostics.errors += 1;
                    continue;
                }
                conversation.session_ids.insert(old.session.clone());
                merge_message(old, &msg);
            } else {
                ids.insert(msg.id.clone(), records.len());
                records.push(msg);
            }
        }
        loop {
            let overlap = summaries.iter().position(|old| {
                old.account == conversation.account
                    && (old.session == conversation.session
                        || !old.session_ids.is_disjoint(&conversation.session_ids))
            });
            let Some(index) = overlap else {
                break;
            };
            let old = summaries.swap_remove(index);
            conversation.session_ids.extend(old.session_ids);
            conversation.last_snapshot = conversation.last_snapshot.max(old.last_snapshot);
            for (model, tokens) in old.totals {
                max_tokens(conversation.totals.entry(model).or_default(), &tokens);
            }
        }
        summaries.push(conversation);
    }
    for conversation in summaries {
        let mut observed: BTreeMap<String, Tokens> = BTreeMap::new();
        for record in records.iter().filter(|m| {
            m.app == "claude"
                && m.timestamp.is_some()
                && conversation.session_ids.contains(&m.session)
                && (m.account == "unknown" || m.account == conversation.account)
                && (conversation.last_snapshot == 0
                    || m.timestamp.unwrap_or(0) <= conversation.last_snapshot)
        }) {
            observed
                .entry(model_id(&record.model))
                .or_default()
                .add(&record.tokens);
        }
        for (model, total) in conversation.totals {
            let remaining = subtract(&total, observed.get(&model).unwrap_or(&Tokens::default()));
            if remaining.total() == 0 {
                continue;
            }
            let id = stable_id(&[
                "claude-desktop-balance",
                &conversation.account,
                &conversation.session,
                &model,
            ]);
            records.push(UsageRecord {
                id: id.clone(),
                source_id: "claude-desktop".into(),
                app: "claude".into(),
                surface: conversation.surface.clone(),
                account: conversation.account.clone(),
                session: conversation.session.clone(),
                request: id,
                timestamp: None,
                model,
                project: "unknown".into(),
                tokens: remaining,
                provenance: "undated-lower-bound".into(),
            });
        }
    }
}

#[allow(dead_code)]
pub fn scan(home: &Path) -> ScanResult {
    let mut result = ScanResult::default();
    merge(home, &mut result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant(id: &str, output: u64) -> Value {
        json!({"serverCreatedAt":1700000000000_i64,"payload":{"type":"assistant","session_id":"s","request_id":"r","message":{"id":id,"model":"claude-future","usage":{"input_tokens":10,"output_tokens":output,"cache_read_input_tokens":100}}}})
    }
    fn cumulative(output: u64) -> Value {
        json!({"serverCreatedAt":1700000001000_i64,"payload":{"type":"result","session_id":"s","modelUsage":{"claude-future":{"inputTokens":30,"outputTokens":output,"cacheReadInputTokens":500,"thinkingTokens":100}}}})
    }
    fn tree(events: Value) -> Value {
        json!({"accountUuid":"a","orgUuid":"o","conversationUuid":"c","product":"cowork","tree":{"events":events}})
    }

    #[test]
    fn streamed_usage_and_cumulative_balances_are_disjoint() {
        let mut d = Diagnostics::default();
        let conversation = parse_tree(
            &tree(json!([
                assistant("m", 1),
                assistant("m", 20),
                cumulative(80),
                cumulative(100)
            ])),
            &mut d,
        );
        let mut records = vec![];
        merge_conversations(&mut vec![conversation], &mut records, &mut d);
        assert_eq!(records.len(), 2);
        assert_eq!(
            records
                .iter()
                .filter(|m| m.timestamp.is_some())
                .map(|m| m.tokens.total())
                .sum::<u64>(),
            130
        );
        assert_eq!(
            records
                .iter()
                .filter(|m| m.timestamp.is_none())
                .map(|m| m.tokens.total())
                .sum::<u64>(),
            500
        );
        assert_eq!(records.iter().map(|m| m.tokens.total()).sum::<u64>(), 630);
        assert!(records.iter().all(|m| m.tokens.reasoning == 0));
    }

    #[test]
    fn transcript_overlap_uses_complete_record_and_preserves_account() {
        let mut d = Diagnostics::default();
        let mut local = parse_tree(&tree(json!([assistant("m", 40)])), &mut d).messages;
        local[0].account = "unknown".into();
        local[0].source_id = "claude-jsonl".into();
        let source = tree(json!([assistant("m", 20), cumulative(100)]));
        let mut conversations = (0..2).map(|_| parse_tree(&source, &mut d)).collect();
        merge_conversations(&mut conversations, &mut local, &mut d);
        assert_eq!(local.len(), 2);
        assert_eq!(local[0].tokens.output, 40);
        assert_ne!(local[0].account, "unknown");
        assert_eq!(local.iter().map(|m| m.tokens.total()).sum::<u64>(), 630);
    }

    #[test]
    fn transcript_cache_overlap_keeps_zero_ordinary_input_in_both_orders() {
        let bare = json!({"type":"assistant","requestId":"r","sessionId":"s","message":{"id":"m","model":"claude-future","usage":{"input_tokens":100,"output_tokens":5}}});
        let mut cached = assistant("m", 5);
        cached["payload"]["message"]["usage"]["input_tokens"] = json!(0);
        let mut diagnostics = Diagnostics::default();
        let cached = parse_tree(&tree(json!([cached])), &mut diagnostics)
            .messages
            .remove(0);
        let local = crate::parsers::claude(
            std::io::Cursor::new(bare.to_string()),
            "local:claude-code",
            "fixture",
            "code",
        )
        .records
        .remove(0);
        for (mut existing, incoming) in [(local.clone(), cached.clone()), (cached, local)] {
            merge_message(&mut existing, &incoming);
            assert_eq!(existing.tokens.input, 0);
            assert_eq!(existing.tokens.cache_read, 100);
            assert_eq!(existing.tokens.total(), 105);
        }
    }

    #[test]
    fn resets_are_visible_and_late_requests_stay_additional() {
        let mut late = assistant("late", 7);
        late["serverCreatedAt"] = json!(1700000002000_i64);
        let mut d = Diagnostics::default();
        let conversation = parse_tree(
            &tree(json!([
                assistant("m", 20),
                cumulative(100),
                cumulative(2),
                late
            ])),
            &mut d,
        );
        let mut records = vec![];
        merge_conversations(&mut vec![conversation], &mut records, &mut d);
        assert_eq!(d.resets, 1);
        assert_eq!(records.iter().map(|m| m.tokens.total()).sum::<u64>(), 747);
    }

    #[test]
    fn truncation_and_absent_usage_are_not_zero_claims() {
        let mut value = tree(
            json!([{"payload":{"type":"assistant","message":{"model":"claude-future","id":"m"}}}]),
        );
        value["tree"]["earlierCursor"] = json!("older");
        let mut d = Diagnostics::default();
        let c = parse_tree(&value, &mut d);
        assert!(c.messages.is_empty());
        assert_eq!(d.truncated, 1);
        assert_eq!(d.missing_usage, 1);
        assert_eq!(
            tokens(&json!({"input_tokens":-1,"output_tokens":2.5}), false).total(),
            0
        );
    }

    #[test]
    fn meta_tombstones_and_accounts_are_applied_before_usage() {
        let mut other = tree(json!([assistant("m", 20)]));
        other["accountUuid"] = json!("other");
        let mut deleted = tree(json!([]));
        deleted["tombstone"] = json!(true);
        let values = vec![
            indexeddb::StoreValue {
                store: "trees".into(),
                value: tree(json!([assistant("m", 20)])),
            },
            indexeddb::StoreValue {
                store: "meta".into(),
                value: deleted,
            },
            indexeddb::StoreValue {
                store: "trees".into(),
                value: other,
            },
        ];
        let mut d = Diagnostics::default();
        let live = live_trees(values, &mut d);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0]["accountUuid"], "other");
        assert_eq!(d.errors, 0);
    }

    #[test]
    fn missing_cache_does_not_create_source_directories() {
        let temp = tempfile::tempdir().unwrap();
        let result = scan(temp.path());
        assert_eq!(result.coverage[0].status, "unavailable");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}
