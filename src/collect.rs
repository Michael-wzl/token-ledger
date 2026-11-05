//! Fixed-root, read-only discovery. No auth files, owning-app databases, or
//! directory locks are opened. Extra roots must be explicitly supplied.
use crate::model::{stable_id, Coverage, Quota, ScanResult};
use crate::parsers;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

const MAX_SOURCE_BYTES: u64 = 512 * 1024 * 1024;

pub fn read_only_file(path: &Path) -> std::io::Result<File> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "parent traversal is not an approved source",
        ));
    }
    #[cfg(unix)]
    let f = {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        let parts: Vec<_> = path
            .components()
            .filter_map(|c| {
                if let Component::Normal(p) = c {
                    Some(p)
                } else {
                    None
                }
            })
            .collect();
        if parts.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "source must be a regular file",
            ));
        }
        let start = if path.is_absolute() {
            b"/\0".as_ptr()
        } else {
            b".\0".as_ptr()
        };
        let fd = unsafe {
            libc::open(
                start.cast(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut dir = unsafe { OwnedFd::from_raw_fd(fd) };
        for part in &parts[..parts.len() - 1] {
            let name = CString::new(part.as_bytes()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid source name")
            })?;
            let fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            dir = unsafe { OwnedFd::from_raw_fd(fd) };
        }
        let name = CString::new(parts.last().unwrap().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid source name")
        })?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        unsafe { File::from_raw_fd(fd) }
    };
    #[cfg(not(unix))]
    let f = {
        let mut prefix = PathBuf::new();
        for c in path.components() {
            prefix.push(c.as_os_str());
            if std::fs::symlink_metadata(&prefix)?.file_type().is_symlink() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "source symlinks are not followed",
                ));
            }
        }
        File::open(path)?
    };
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source must be a regular file",
        ));
    }
    Ok(f)
}

pub fn scan(home: &Path, extra_roots: &[PathBuf]) -> ScanResult {
    let mut result = ScanResult::default();
    scan_group(
        &mut result,
        "local:codex",
        "codex",
        &[
            home.join(".codex/sessions"),
            home.join(".codex/archived_sessions"),
        ],
        "session",
    );
    scan_group(
        &mut result,
        "local:claude-code",
        "claude",
        &[home.join(".claude/projects")],
        "code",
    );
    scan_group(
        &mut result,
        "local:claude-transcripts",
        "claude",
        &[
            home.join("Library/Application Support/Claude/local-agent-mode-sessions"),
            home.join("Library/Application Support/Claude/claude-code-sessions"),
        ],
        "desktop-code",
    );
    for root in extra_roots {
        let source = format!("extra:{}", stable_id(&[&root.to_string_lossy()]));
        scan_group(
            &mut result,
            &source,
            "auto",
            std::slice::from_ref(root),
            "imported-remote",
        );
    }
    let conflicts = parsers::dedup(&mut result.records);
    for coverage in &mut result.coverage {
        coverage.records = result
            .records
            .iter()
            .filter(|r| r.source_id == coverage.source_id)
            .count();
        coverage.undated = result
            .records
            .iter()
            .filter(|r| r.source_id == coverage.source_id && r.timestamp.is_none())
            .count();
    }
    if conflicts > 0 {
        let mut coverage = Coverage::new("local:identity-conflicts", "all", "partial", &format!("Held out {conflicts} logical request IDs with conflicting known accounts or model labels. Their copies are excluded from this scan's totals."));
        coverage.errors = conflicts;
        result.coverage.push(coverage);
    }
    crate::claude_desktop::merge(home, &mut result);
    quota(home, &mut result);
    result.coverage.push(Coverage::new("cursor-imports","cursor","unavailable","Offline mode requires a Cursor usage CSV/JSON export; no account or credential is queried."));
    result
}

fn scan_group(result: &mut ScanResult, source: &str, app: &str, roots: &[PathBuf], surface: &str) {
    let mut cov = Coverage::new(
        source,
        if app == "auto" { "remote" } else { app },
        "unavailable",
        "No readable retained usage records; missing usage is unknown, not zero.",
    );
    let mut visited = 0usize;
    let mut existing = false;
    let mut records = Vec::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        existing = true;
        let mut paths = Vec::new();
        if root.is_file() {
            paths.push(root.clone());
        } else {
            for entry in WalkDir::new(root)
                .follow_links(false)
                .max_depth(24)
                .into_iter()
                .filter_entry(|e| {
                    let name = e.file_name().to_string_lossy();
                    !e.file_type().is_symlink()
                        && !matches!(
                            name.as_ref(),
                            "plugins"
                                | "node_modules"
                                | ".git"
                                | "auth"
                                | "Cookies"
                                | "Local Storage"
                        )
                })
            {
                match entry {
                    Ok(e)
                        if e.file_type().is_file()
                            && e.path().extension().is_some_and(|x| x == "jsonl") =>
                    {
                        paths.push(e.path().into())
                    }
                    Err(_) => cov.errors += 1,
                    _ => {}
                }
                if paths.len() > 100_000 {
                    cov.errors += 1;
                    cov.status = "stale".into();
                    break;
                }
            }
        }
        paths.sort();
        for path in paths {
            visited += 1;
            if path.extension().is_none_or(|x| x != "jsonl") {
                cov.errors += 1;
                continue;
            }
            let identity = stable_id(&[&path.to_string_lossy()]);
            let mut parsed = None;
            for _ in 0..2 {
                let Ok(f) = read_only_file(&path) else {
                    cov.errors += 1;
                    break;
                };
                let Ok(before) = f.metadata() else {
                    cov.errors += 1;
                    break;
                };
                if before.len() > MAX_SOURCE_BYTES {
                    cov.errors += 1;
                    break;
                }
                let actual_app = if app == "auto" {
                    detect_app(&f)
                } else {
                    Some(app.to_string())
                };
                let Some(actual_app) = actual_app else {
                    cov.errors += 1;
                    break;
                };
                let r = if actual_app == "codex" {
                    parsers::codex(BufReader::new(&f), source, &identity)
                } else {
                    parsers::claude(BufReader::new(&f), source, &identity, surface)
                };
                let unchanged = f.metadata().ok().zip(path.metadata().ok()).is_some_and(
                    |(after, path_meta)| {
                        before.len() == after.len()
                            && before.modified().ok() == after.modified().ok()
                            && after.len() == path_meta.len()
                            && after.modified().ok() == path_meta.modified().ok()
                    },
                );
                if unchanged {
                    parsed = Some(r);
                    break;
                }
            }
            if let Some(r) = parsed {
                for c in r.coverage {
                    cov.errors += c.errors;
                    if c.status == "stale" {
                        cov.status = "stale".into();
                    }
                }
                records.extend(r.records);
            } else {
                cov.errors += 1;
                cov.status = "stale".into();
            }
        }
    }
    // Deduplicate once after all source groups, so a later group cannot
    // resurrect a logical ID quarantined by conflicting copies in this group.
    cov.records = records.len();
    cov.undated = records.iter().filter(|r| r.timestamp.is_none()).count();
    if existing && cov.status != "stale" {
        cov.status = "partial".into();
    }
    cov.message = if cov.status == "stale" {
        "A source changed, was unreadable, or exceeded limits. Preserve the last successful snapshot; retry on the next refresh.".into()
    } else {
        format!("Read {visited} retained log files. Account-wide/cloud coverage is unknown; {} source diagnostics (missing counters or malformed rows). Missing usage is never counted as zero.",cov.errors)
    };
    result.records.extend(records);
    result.coverage.push(cov);
}

fn detect_app(f: &File) -> Option<String> {
    use std::io::{BufRead, Seek, SeekFrom};
    let mut f = f.try_clone().ok()?;
    f.seek(SeekFrom::Start(0)).ok()?;
    let mut line = Vec::new();
    let mut reader = BufReader::new(&mut f);
    let mut found = None;
    for _ in 0..32 {
        line.clear();
        if reader
            .by_ref()
            .take((parsers::MAX_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .ok()?
            == 0
        {
            break;
        }
        if line.len() > parsers::MAX_LINE_BYTES {
            break;
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) {
            match v["type"].as_str() {
                Some("session_meta" | "turn_context" | "token_usage_record") => {
                    found = Some("codex".into());
                    break;
                }
                Some("assistant" | "user") if v.get("message").is_some() => {
                    found = Some("claude".into());
                    break;
                }
                _ => {}
            }
        }
    }
    drop(reader);
    f.seek(SeekFrom::Start(0)).ok()?;
    found
}

fn quota(home: &Path, result: &mut ScanResult) {
    let source = "local:grok-quota";
    let mut cov=Coverage::new(source,"grok-bot","unavailable","Grok Bot tokens are unavailable locally. Cached account quota is separate from token usage.");
    let root = home.join("Library/Application Support/Grok Bot/sand-client-persistence");
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten().take(10000) {
            let p = entry.path();
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if p.extension().is_none_or(|s| s != "blob")
                || !base32(stem).is_some_and(|s| s.ends_with(".weekly-usage.cache"))
            {
                continue;
            }
            let Ok(mut f) = read_only_file(&p) else {
                cov.errors += 1;
                continue;
            };
            if f.metadata().is_err() || f.metadata().is_ok_and(|m| m.len() > 65536) {
                cov.errors += 1;
                continue;
            }
            let mut bytes = Vec::new();
            if f.by_ref().take(65537).read_to_end(&mut bytes).is_err() || bytes.len() > 65536 {
                cov.errors += 1;
                continue;
            }
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                cov.errors += 1;
                continue;
            };
            let usage = &v["value"]["reading"]["usage"];
            let Some(percent) = usage["percentUsed"]
                .as_f64()
                .filter(|x| x.is_finite() && (0.0..=100.0).contains(x))
            else {
                continue;
            };
            let read_at = v["value"]["reading"]["readAtMs"].as_i64();
            result.quotas.push(Quota{app:"grok-bot".into(),percent_used:percent,next_reset_ms:usage["nextResetMs"].as_i64(),note:format!("Cached vendor quota snapshot{}; may cover the shared Cursor account. Percentage is not a token total.",read_at.and_then(|t|chrono::DateTime::from_timestamp_millis(t)).map(|t|format!(" from {}",t.to_rfc3339())).unwrap_or_default())});
            cov.status = "partial".into();
        }
    }
    result.coverage.push(cov);
}

fn base32(s: &str) -> Option<String> {
    let mut bits = 0_u32;
    let mut len = 0;
    let mut bytes = Vec::new();
    for b in s.bytes() {
        let n = match b {
            b'a'..=b'z' => b - b'a',
            b'2'..=b'7' => b - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5) | u32::from(n);
        len += 5;
        if len >= 8 {
            len -= 8;
            bytes.push((bits >> len) as u8);
            bits &= (1 << len) - 1;
        }
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_sources_are_unknown() {
        let d = tempfile::tempdir().unwrap();
        let r = scan(d.path(), &[]);
        assert!(r.records.is_empty());
        assert!(r
            .coverage
            .iter()
            .any(|c| c.app == "codex" && c.status == "unavailable"));
    }
    #[cfg(unix)]
    #[test]
    fn symlink_source_rejected() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("real");
        std::fs::write(&p, "{}\n").unwrap();
        let link = d.path().join("link");
        symlink(&p, &link).unwrap();
        assert!(read_only_file(&link).is_err());
    }
    #[test]
    fn quota_name_decoder_only_accepts_cache_key() {
        assert_eq!(base32("mzxw6"), Some("foo".into()));
        assert_eq!(base32("AUTH!"), None);
    }

    #[test]
    fn cross_file_identity_conflicts_are_excluded_with_visible_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let root = home.join(".claude/projects");
        std::fs::create_dir_all(&root).unwrap();
        let a = serde_json::json!({"type":"assistant","requestId":"r","accountUuid":"a","orgUuid":"o","message":{"id":"m","model":"claude","usage":{"input_tokens":10,"output_tokens":2}}});
        let mut b = a.clone();
        b["accountUuid"] = serde_json::json!("b");
        std::fs::write(root.join("a.jsonl"), format!("{a}\n")).unwrap();
        std::fs::write(root.join("b.jsonl"), format!("{b}\n")).unwrap();
        let result = scan(&home, &[]);
        assert!(result.records.is_empty());
        let coverage = result
            .coverage
            .iter()
            .find(|c| c.source_id == "local:identity-conflicts")
            .unwrap();
        assert_eq!(coverage.errors, 1);
        assert!(coverage.message.contains("Held out 1"));
    }
    #[test]
    fn active_archive_duplicates_count_once_without_source_writes() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().canonicalize().unwrap();
        let active = home.join(".codex/sessions");
        let archive = home.join(".codex/archived_sessions");
        std::fs::create_dir_all(&active).unwrap();
        std::fs::create_dir_all(&archive).unwrap();
        let log="{\"type\":\"session_meta\",\"payload\":{\"id\":\"same-session\"}}\n{\"type\":\"event_msg\",\"timestamp\":\"2026-11-05T10:00:00Z\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":10,\"output_tokens\":2},\"last_token_usage\":{\"input_tokens\":10,\"output_tokens\":2}}}}\n";
        let a = active.join("a.jsonl");
        let b = archive.join("a.jsonl");
        std::fs::write(&a, log).unwrap();
        std::fs::write(&b, log).unwrap();
        let before = a.metadata().unwrap().modified().unwrap();
        let mut perms = a.metadata().unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&a, perms).unwrap();
        let r = scan(&home, &[]);
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].tokens.total(), 12);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), log);
        assert_eq!(a.metadata().unwrap().modified().unwrap(), before);
        assert_eq!(std::fs::read_dir(&active).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(&archive).unwrap().count(), 1);
    }
}
