use crate::model::{now_ms, stable_id, Coverage, Quota, ScanResult, Tokens, UsageRecord};
use anyhow::{bail, Context, Result};
use chrono::{Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

pub struct Store {
    db: Connection,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub tz: Tz,
    pub app: String,
    pub model: String,
    pub project: String,
    pub account: String,
    pub grain: String,
}

impl Query {
    pub fn parse(query: &str) -> Result<Self> {
        let p: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
        let tz: Tz = p
            .get("tz")
            .map(String::as_str)
            .unwrap_or("Asia/Hong_Kong")
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid timezone"))?;
        let today = Utc::now().with_timezone(&tz).date_naive();
        let from = match p.get("from") {
            Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d").context("Invalid start date")?,
            None => today,
        };
        let to = match p.get("to") {
            Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d").context("Invalid end date")?,
            None => today,
        };
        if to < from || to.signed_duration_since(from).num_days() > 3660 {
            bail!("Choose an ordered range of at most ten years");
        }
        let grain = p.get("grain").cloned().unwrap_or_else(|| {
            if from == to {
                "hour".into()
            } else {
                "day".into()
            }
        });
        if !["day", "hour"].contains(&grain.as_str()) {
            bail!("Grain must be day or hour");
        }
        Ok(Self {
            from,
            to,
            tz,
            app: p.get("app").cloned().unwrap_or_default(),
            model: p.get("model").cloned().unwrap_or_default(),
            project: p.get("project").cloned().unwrap_or_default(),
            account: p.get("account").cloned().unwrap_or_default(),
            grain,
        })
    }
    pub fn bounds(&self) -> Result<(i64, i64)> {
        let end = self
            .to
            .checked_add_signed(Duration::days(1))
            .context("End date overflow")?;
        let convert = |d: NaiveDate| {
            self.tz
                .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
                .earliest()
                .map(|x| x.timestamp_millis())
                .context("Date boundary unavailable in timezone")
        };
        Ok((convert(self.from)?, convert(end)?))
    }
    fn matches(&self, r: &UsageRecord) -> bool {
        (self.app.is_empty() || self.app == r.app)
            && (self.model.is_empty() || self.model == r.model)
            && (self.project.is_empty() || self.project == r.project)
            && (self.account.is_empty() || self.account == r.account)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportRequest {
    pub format: String,
    pub app: String,
    pub account: String,
    pub from: String,
    pub to: String,
    pub name: String,
    pub content: String,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if path
            .symlink_metadata()
            .map(|x| x.file_type().is_symlink())
            .unwrap_or(false)
        {
            bail!("Database cannot be a symlink");
        }
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(3))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY;
          CREATE TABLE IF NOT EXISTS records(origin TEXT NOT NULL,id TEXT NOT NULL,source_id TEXT NOT NULL,app TEXT NOT NULL,account TEXT NOT NULL,model TEXT NOT NULL,project TEXT NOT NULL,ts INTEGER,body TEXT NOT NULL,PRIMARY KEY(origin,id));
          CREATE INDEX IF NOT EXISTS record_time ON records(ts,app);
          CREATE TABLE IF NOT EXISTS coverage(source_id TEXT PRIMARY KEY,body TEXT NOT NULL,fingerprint TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS quotas(app TEXT PRIMARY KEY,body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS imports(id TEXT PRIMARY KEY,app TEXT NOT NULL,account TEXT NOT NULL,start_ms INTEGER NOT NULL,end_ms INTEGER NOT NULL,name TEXT NOT NULL,created_ms INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);")?;
        Ok(Self { db })
    }
    pub fn apply_scan(&mut self, scan: ScanResult) -> Result<()> {
        let tx = self.db.transaction()?;
        // Dated history survives source retention/deletion. Undated balances are
        // replaceable snapshots and must shrink as dated usage is recovered.
        for c in &scan.coverage {
            if !["stale", "error", "unavailable"].contains(&c.status.as_str()) {
                tx.execute(
                    "DELETE FROM records WHERE origin='local' AND source_id=?1 AND ts IS NULL",
                    [&c.source_id],
                )?;
            }
        }
        let stale_sources: std::collections::HashSet<&str> = scan
            .coverage
            .iter()
            .filter(|c| ["stale", "error"].contains(&c.status.as_str()))
            .map(|c| c.source_id.as_str())
            .collect();
        let sessions: std::collections::BTreeSet<(&str, &str)> = scan
            .records
            .iter()
            .filter(|r| !stale_sources.contains(r.source_id.as_str()))
            .map(|r| (r.source_id.as_str(), r.session.as_str()))
            .collect();
        for (source, session) in sessions {
            // Reconcile a present session as one snapshot: late confirmations
            // can change record identities. Preserve sessions that disappeared
            // entirely through source retention, but never append a revised
            // snapshot on top of its older identities.
            tx.execute("DELETE FROM records WHERE origin='local' AND source_id=?1 AND json_extract(body,'$.session')=?2",params![source,session])?;
        }
        for r in scan
            .records
            .iter()
            .filter(|r| !stale_sources.contains(r.source_id.as_str()))
        {
            tx.execute("INSERT INTO records(origin,id,source_id,app,account,model,project,ts,body) VALUES('local',?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(origin,id) DO UPDATE SET source_id=excluded.source_id,app=excluded.app,account=excluded.account,model=excluded.model,project=excluded.project,ts=excluded.ts,body=excluded.body", params![r.id,r.source_id,r.app,r.account,r.model,r.project,r.timestamp,serde_json::to_string(r)?])?;
        }
        for c in &scan.coverage {
            let mut ids: Vec<String> = scan
                .records
                .iter()
                .filter(|r| r.source_id == c.source_id)
                .map(|r| serde_json::to_string(r).unwrap_or_default())
                .collect();
            ids.sort();
            let fingerprint = stable_id(&ids.iter().map(String::as_str).collect::<Vec<_>>());
            tx.execute(
                "INSERT OR REPLACE INTO coverage VALUES(?1,?2,?3)",
                params![c.source_id, serde_json::to_string(c)?, fingerprint],
            )?;
        }
        for q in scan.quotas {
            tx.execute(
                "INSERT OR REPLACE INTO quotas VALUES(?1,?2)",
                params![q.app, serde_json::to_string(&q)?],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta VALUES('last_scan',?1)",
            [now_ms().to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn import(&mut self, req: &ImportRequest) -> Result<Value> {
        if req.content.len() > 32 * 1024 * 1024 {
            bail!("Import exceeds 32 MiB");
        }
        if !["csv", "json", "jsonl"].contains(&req.format.as_str())
            || !["cursor", "grok-bot", "codex", "claude"].contains(&req.app.as_str())
        {
            bail!("Unsupported format or source");
        }
        let account = req.account.trim();
        if account.is_empty() || account.len() > 120 || account.chars().any(char::is_control) {
            bail!("Use a short account label (or unknown)");
        }
        let from = NaiveDate::parse_from_str(&req.from, "%Y-%m-%d")
            .context("Invalid import start date")?;
        let to =
            NaiveDate::parse_from_str(&req.to, "%Y-%m-%d").context("Invalid import end date")?;
        if to < from || to.signed_duration_since(from).num_days() > 3660 {
            bail!("Invalid import date window");
        }
        let query = Query {
            from,
            to,
            tz: chrono_tz::Asia::Hong_Kong,
            app: String::new(),
            model: String::new(),
            project: String::new(),
            account: String::new(),
            grain: "day".into(),
        };
        let (start, end) = query.bounds()?;
        let source_id = stable_id(&["import", &req.app, account, &req.from, &req.to]);
        let result = crate::import::parse(
            req.content.as_bytes(),
            &req.format,
            &req.app,
            account,
            &source_id,
        )?;
        if result.records.is_empty() {
            bail!("No supported numeric usage records found; nothing imported");
        }
        let imported_total = result.records.iter().try_fold(0u64, |sum, r| {
            sum.checked_add(r.tokens.total())
                .filter(|n| *n <= 9_007_199_254_740_991)
        });
        if imported_total.is_none() {
            bail!("Import aggregate exceeds the exact supported numeric range");
        }
        if result
            .records
            .iter()
            .any(|r| r.timestamp.map(|t| t < start || t >= end).unwrap_or(false))
        {
            bail!("Some dated records fall outside the declared window; widen the import dates");
        }
        let apps: std::collections::BTreeSet<String> =
            result.records.iter().map(|r| r.app.clone()).collect();
        for app in &apps {
            let mut stmt = self.db.prepare("SELECT start_ms,end_ms FROM imports WHERE app=?1 AND account=?2 AND start_ms<?3 AND end_ms>?4")?;
            let overlaps: Vec<(i64, i64)> = stmt
                .query_map(params![app, account, end, start], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            if overlaps.iter().any(|(s, e)| *s != start || *e != end) {
                bail!("This overlaps a previous import. Replace its exact date window or import a disjoint window");
            }
        }
        let tx = self.db.transaction()?;
        let name = Path::new(&req.name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("import")
            .chars()
            .filter(|c| !c.is_control())
            .take(160)
            .collect::<String>();
        for app in apps {
            let actual_source = stable_id(&["import", &app, account, &req.from, &req.to]);
            tx.execute(
                "DELETE FROM records WHERE origin='import' AND source_id=?1",
                [&actual_source],
            )?;
            tx.execute("DELETE FROM imports WHERE id=?1", [&actual_source])?;
            tx.execute(
                "INSERT INTO imports VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![actual_source, app, account, start, end, name, now_ms()],
            )?;
            let mut coverage = Coverage::new(&actual_source,&app,"partial","User-supplied export window; account-wide completeness is not verified. Overlapping local records are held out of totals.");
            for mut r in result.records.iter().filter(|r| r.app == app).cloned() {
                r.source_id = actual_source.clone();
                r.provenance = "import".into();
                r.account = account.into();
                r.id = stable_id(&["import-record", account, &r.id]);
                coverage.records += 1;
                if r.timestamp.is_none() {
                    coverage.undated += 1;
                }
                tx.execute(
                    "INSERT OR REPLACE INTO records VALUES('import',?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        r.id,
                        r.source_id,
                        r.app,
                        r.account,
                        r.model,
                        r.project,
                        r.timestamp,
                        serde_json::to_string(&r)?
                    ],
                )?;
            }
            coverage.errors = result.coverage.iter().map(|c| c.errors).sum();
            tx.execute(
                "INSERT OR REPLACE INTO coverage VALUES(?1,?2,?3)",
                params![
                    actual_source,
                    serde_json::to_string(&coverage)?,
                    stable_id(&[&req.content])
                ],
            )?;
        }
        tx.commit()?;
        Ok(
            json!({"records":result.records.len(),"warnings":result.coverage.iter().filter(|c| c.errors>0 || !c.message.is_empty()).map(|c| &c.message).collect::<Vec<_>>() }),
        )
    }
    pub fn last_scan(&self) -> Option<i64> {
        self.db
            .query_row("SELECT value FROM meta WHERE key='last_scan'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()?
            .parse()
            .ok()
    }
    pub fn coverage(&self) -> Result<Value> {
        let mut s = self
            .db
            .prepare("SELECT body FROM coverage ORDER BY source_id")?;
        let sources: Vec<Coverage> = s
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|s| s.ok().and_then(|s| serde_json::from_str(&s).ok()))
            .collect();
        let mut s = self.db.prepare("SELECT body FROM quotas ORDER BY app")?;
        let quotas: Vec<Quota> = s
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|s| s.ok().and_then(|s| serde_json::from_str(&s).ok()))
            .collect();
        Ok(json!({"sources":sources,"quotas":quotas,"last_scan":self.last_scan()}))
    }
    fn selected(&self, q: &Query) -> Result<(Vec<UsageRecord>, usize)> {
        let (start, end) = q.bounds()?;
        let mut stmt = self
            .db
            .prepare("SELECT origin,body FROM records WHERE ts IS NULL OR (ts>=?1 AND ts<?2)")?;
        let rows: Vec<(String, String)> = stmt
            .query_map(params![start, end], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut stmt = self
            .db
            .prepare("SELECT app,start_ms,end_ms FROM imports WHERE start_ms<?1 AND end_ms>?2")?;
        let windows: Vec<(String, i64, i64)> = stmt
            .query_map(params![end, start], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut held = 0;
        let mut selected = Vec::new();
        for (origin, body) in rows {
            let r: UsageRecord = serde_json::from_str(&body)?;
            if !q.matches(&r) {
                continue;
            }
            // Without a proven account mapping, adding local and vendor records
            // would inflate totals. Keep both in the ledger, but hold local rows
            // out of combined totals throughout any imported app window.
            if origin == "local"
                && windows.iter().any(|(app, s, e)| {
                    *app == r.app && r.timestamp.map(|t| t >= *s && t < *e).unwrap_or(true)
                })
            {
                held += 1;
                continue;
            }
            selected.push(r);
        }
        Ok((selected, held))
    }
    pub fn summary(&self, q: &Query) -> Result<Value> {
        let (records, held) = self.selected(q)?;
        let mut total = Tokens::default();
        let mut undated = 0u64;
        let mut count = 0usize;
        let mut apps: BTreeMap<String, (u64, usize)> = BTreeMap::new();
        let mut models: BTreeMap<String, u64> = BTreeMap::new();
        let mut projects: BTreeMap<String, u64> = BTreeMap::new();
        for r in records {
            if r.timestamp.is_none() {
                undated = undated.saturating_add(r.tokens.total());
                continue;
            }
            count += 1;
            total.add(&r.tokens);
            let a = apps.entry(r.app).or_default();
            a.0 = a.0.saturating_add(r.tokens.total());
            a.1 += 1;
            let m = models.entry(r.model).or_default();
            *m = m.saturating_add(r.tokens.total());
            let p = projects.entry(r.project).or_default();
            *p = p.saturating_add(r.tokens.total());
        }
        let mut app_rows: Vec<Value> = apps
            .into_iter()
            .map(|(app, (n, c))| json!({"app":app,"total_tokens":n,"records":c}))
            .collect();
        sort_totals(&mut app_rows);
        let mut model_rows: Vec<Value> = models
            .into_iter()
            .map(|(model, n)| json!({"model":model,"total_tokens":n}))
            .collect();
        sort_totals(&mut model_rows);
        let mut project_rows: Vec<Value> = projects
            .into_iter()
            .map(|(project, n)| json!({"project":project,"total_tokens":n}))
            .collect();
        sort_totals(&mut project_rows);
        let values = |col: &str| -> Result<Vec<String>> {
            let mut stmt = self.db.prepare(&format!(
                "SELECT DISTINCT {col} FROM records ORDER BY {col}"
            ))?;
            let rows = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        };
        let mut warnings=vec!["Observed records are a partial account history. Missing records are unknown, not zero.".to_owned()];
        if held > 0 {
            warnings.push(format!("{held} overlapping local records are held out because an imported app window takes precedence; account attribution may be ambiguous."));
        }
        Ok(
            json!({"total_tokens":total.total(),"tokens":total,"records":count,"undated_tokens":undated,"held_records":held,"apps":app_rows,"models":model_rows,"projects":project_rows,"filters":{"apps":values("app")?,"models":values("model")?,"projects":values("project")?,"accounts":values("account")?},"coverage_status":"partial","timezone":q.tz.to_string(),"range":{"from":q.from.to_string(),"to":q.to.to_string()},"last_scan":self.last_scan(),"warnings":warnings}),
        )
    }
    pub fn timeseries(&self, q: &Query) -> Result<Value> {
        let (records, _) = self.selected(q)?;
        let mut buckets: BTreeMap<String, Tokens> = BTreeMap::new();
        if q.grain == "hour" {
            let (start, end) = q.bounds()?;
            if (end - start) / 3_600_000 > 9000 {
                bail!("Use daily granularity for ranges longer than one year");
            }
            let mut t = start;
            while t < end {
                let d = chrono::DateTime::from_timestamp_millis(t)
                    .context("Invalid chart date")?
                    .with_timezone(&q.tz);
                buckets.insert(d.format("%Y-%m-%dT%H:00%:z").to_string(), Tokens::default());
                t += 3_600_000;
            }
        } else {
            let mut day = q.from;
            while day <= q.to {
                buckets.insert(day.to_string(), Tokens::default());
                day = day
                    .checked_add_signed(Duration::days(1))
                    .context("Chart date overflow")?;
            }
        }
        for r in records {
            if let Some(t) = r
                .timestamp
                .and_then(chrono::DateTime::from_timestamp_millis)
            {
                let d = t.with_timezone(&q.tz);
                let bucket = if q.grain == "hour" {
                    d.format("%Y-%m-%dT%H:00%:z").to_string()
                } else {
                    d.format("%Y-%m-%d").to_string()
                };
                buckets.entry(bucket).or_default().add(&r.tokens);
            }
        }
        let points:Vec<Value>=buckets.into_iter().map(|(bucket,t)|json!({"bucket":bucket,"total_tokens":t.total(),"input":t.input,"output":t.output,"cache_read":t.cache_read,"cache_write":t.cache_write})).collect();
        Ok(json!({"points":points,"grain":q.grain}))
    }
    pub fn export(&self) -> Result<Value> {
        let mut s = self
            .db
            .prepare("SELECT origin,body FROM records ORDER BY ts,id")?;
        let rows: Vec<Value> = s
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .map(|r| {
                let (origin, body) = r?;
                let record: Value =
                    serde_json::from_str(&body).map_err(|_| rusqlite::Error::InvalidQuery)?;
                Ok(json!({"origin":origin,"record":record}))
            })
            .collect::<rusqlite::Result<_>>()?;
        Ok(json!({"schema_version":1,"records":rows,"coverage":self.coverage()?}))
    }
}

fn sort_totals(rows: &mut [Value]) {
    rows.sort_by_key(|v| std::cmp::Reverse(v["total_tokens"].as_u64().unwrap_or(0)));
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: &str, ts: Option<i64>) -> UsageRecord {
        UsageRecord {
            id: id.into(),
            source_id: "test".into(),
            app: "codex".into(),
            surface: "cli".into(),
            account: "unknown".into(),
            session: "session".into(),
            request: id.into(),
            timestamp: ts,
            model: "model".into(),
            project: "test".into(),
            tokens: Tokens {
                input: 100,
                output: 10,
                cache_read: 40,
                cache_write: 0,
                reasoning: 3,
            },
            provenance: "local".into(),
        }
    }
    fn db() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }
    #[test]
    fn scans_are_idempotent_and_undated_excluded() {
        let mut s = db();
        let t = 1735689600000;
        let scan = ScanResult {
            records: vec![record("a", Some(t)), record("b", None)],
            coverage: vec![Coverage::new("test", "codex", "partial", "test")],
            quotas: vec![],
        };
        s.apply_scan(scan.clone()).unwrap();
        s.apply_scan(scan).unwrap();
        let q = Query::parse("from=2025-01-01&to=2025-01-01").unwrap();
        let v = s.summary(&q).unwrap();
        assert_eq!(v["total_tokens"], 150);
        assert_eq!(v["undated_tokens"], 150);
        assert_eq!(v["records"], 1);
    }
    #[test]
    fn hong_kong_midnight_and_reasoning() {
        let mut s = db();
        let ts = chrono::DateTime::parse_from_rfc3339("2025-01-01T16:00:00Z")
            .unwrap()
            .timestamp_millis();
        s.apply_scan(ScanResult {
            records: vec![record("a", Some(ts))],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            s.summary(&Query::parse("from=2025-01-01&to=2025-01-01").unwrap())
                .unwrap()["total_tokens"],
            0
        );
        assert_eq!(
            s.summary(&Query::parse("from=2025-01-02&to=2025-01-02").unwrap())
                .unwrap()["total_tokens"],
            150
        );
    }
    #[test]
    fn invalid_dates_are_rejected() {
        assert!(Query::parse("from=2025-01-02&to=2025-01-01").is_err());
        assert!(Query::parse("tz=not-a-zone").is_err());
    }
    fn request(app: &str, account: &str, from: &str, to: &str, content: Value) -> ImportRequest {
        ImportRequest {
            format: "json".into(),
            app: app.into(),
            account: account.into(),
            from: from.into(),
            to: to.into(),
            name: "usage.json".into(),
            content: content.to_string(),
        }
    }
    fn events() -> Value {
        json!([{"id":"one","timestamp":"2025-01-01T10:00:00Z","model":"model","tokenUsage":{"inputTokens":10,"outputTokens":2,"cacheReadTokens":100}}])
    }
    #[test]
    fn identical_import_replaces_and_partial_window_is_rejected() {
        let mut s = db();
        let r = request("cursor", "personal", "2025-01-01", "2025-01-01", events());
        s.import(&r).unwrap();
        s.import(&r).unwrap();
        let q = Query::parse("from=2025-01-01&to=2025-01-01").unwrap();
        assert_eq!(s.summary(&q).unwrap()["total_tokens"], 112);
        let overlap = request("cursor", "personal", "2025-01-01", "2025-01-02", events());
        assert!(s.import(&overlap).is_err());
        assert_eq!(s.summary(&q).unwrap()["total_tokens"], 112);
    }
    #[test]
    fn imported_accounts_stay_separate() {
        let mut s = db();
        s.import(&request(
            "cursor",
            "personal",
            "2025-01-01",
            "2025-01-01",
            events(),
        ))
        .unwrap();
        s.import(&request(
            "cursor",
            "work",
            "2025-01-01",
            "2025-01-01",
            events(),
        ))
        .unwrap();
        assert_eq!(
            s.summary(&Query::parse("from=2025-01-01&to=2025-01-01").unwrap())
                .unwrap()["total_tokens"],
            224
        );
        assert_eq!(
            s.summary(&Query::parse("from=2025-01-01&to=2025-01-01&account=work").unwrap())
                .unwrap()["total_tokens"],
            112
        );
    }
    #[test]
    fn mixed_cursor_grok_exports_reconcile_by_actual_product() {
        let mut s = db();
        let mixed = json!([{"id":"one","timestamp":"2025-01-01T10:00:00Z","model":"model","tokenUsage":{"inputTokens":10,"outputTokens":2}},{"id":"two","timestamp":"2025-01-01T11:00:00Z","model":"Grok Bot","tokenUsage":{"inputTokens":20,"outputTokens":2}}]);
        s.import(&request(
            "cursor",
            "personal",
            "2025-01-01",
            "2025-01-01",
            mixed.clone(),
        ))
        .unwrap();
        s.import(&request(
            "grok-bot",
            "personal",
            "2025-01-01",
            "2025-01-01",
            mixed,
        ))
        .unwrap();
        assert_eq!(
            s.summary(&Query::parse("from=2025-01-01&to=2025-01-01").unwrap())
                .unwrap()["total_tokens"],
            34
        );
    }
    #[test]
    fn out_of_window_import_is_atomic() {
        let mut s = db();
        let r = request("cursor", "personal", "2025-01-02", "2025-01-02", events());
        assert!(s.import(&r).is_err());
        assert_eq!(s.export().unwrap()["records"].as_array().unwrap().len(), 0);
    }
    #[test]
    fn late_response_identity_replaces_same_session_snapshot() {
        let mut s = db();
        let t = 1735689600000;
        s.apply_scan(ScanResult {
            records: vec![record("old-cumulative-id", Some(t))],
            ..Default::default()
        })
        .unwrap();
        s.apply_scan(ScanResult {
            records: vec![record("confirmed-response-id", Some(t))],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            s.summary(&Query::parse("from=2025-01-01&to=2025-01-01").unwrap())
                .unwrap()["total_tokens"],
            150
        );
    }
    #[test]
    fn stale_source_keeps_previous_valid_snapshot() {
        let mut s = db();
        let t = 1735689600000;
        s.apply_scan(ScanResult {
            records: vec![record("old", Some(t))],
            ..Default::default()
        })
        .unwrap();
        s.apply_scan(ScanResult {
            records: vec![record("incomplete", Some(t))],
            coverage: vec![Coverage::new("test", "codex", "stale", "retry")],
            quotas: vec![],
        })
        .unwrap();
        let v = s.export().unwrap();
        assert_eq!(v["records"].as_array().unwrap().len(), 1);
        assert_eq!(v["records"][0]["record"]["id"], "old");
    }
    #[test]
    fn imported_log_window_holds_ambiguous_local_rows_out() {
        let mut s = db();
        let t = chrono::DateTime::parse_from_rfc3339("2025-01-01T10:00:00Z")
            .unwrap()
            .timestamp_millis();
        let mut local = record("local", Some(t));
        local.app = "cursor".into();
        s.apply_scan(ScanResult {
            records: vec![local],
            ..Default::default()
        })
        .unwrap();
        s.import(&request(
            "cursor",
            "personal",
            "2025-01-01",
            "2025-01-01",
            events(),
        ))
        .unwrap();
        let v = s
            .summary(&Query::parse("from=2025-01-01&to=2025-01-01").unwrap())
            .unwrap();
        assert_eq!(v["total_tokens"], 112);
        assert_eq!(v["held_records"], 1);
        assert_eq!(s.export().unwrap()["records"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn import_dates_cannot_inject_query_parameters() {
        let mut s = db();
        let mut r = request("cursor", "personal", "2025-01-01", "2025-01-01", events());
        r.to = "2025-01-01&tz=UTC".into();
        assert!(s.import(&r).is_err());
    }
    #[test]
    fn sparse_charts_preserve_empty_hours() {
        let s = db();
        let q = Query::parse("from=2025-01-01&to=2025-01-01&grain=hour").unwrap();
        let v = s.timeseries(&q).unwrap();
        assert_eq!(v["points"].as_array().unwrap().len(), 24);
        assert_eq!(v["points"][0]["bucket"], "2025-01-01T00:00+08:00");
        assert_eq!(v["points"][23]["total_tokens"], 0);
    }
}
