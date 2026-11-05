use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tokens {
    /// Ordinary input only; cached reads/writes are disjoint components.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Already included in output; diagnostic subset, never added to total.
    pub reasoning: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
    pub fn add(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub id: String,
    pub source_id: String,
    pub app: String,
    pub surface: String,
    pub account: String,
    pub session: String,
    pub request: String,
    pub timestamp: Option<i64>,
    pub model: String,
    pub project: String,
    pub tokens: Tokens,
    pub provenance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coverage {
    pub source_id: String,
    pub app: String,
    pub status: String,
    pub message: String,
    pub records: usize,
    pub undated: usize,
    pub errors: usize,
    pub scanned_at: i64,
}

impl Coverage {
    pub fn new(source_id: &str, app: &str, status: &str, message: &str) -> Self {
        Self {
            source_id: source_id.into(),
            app: app.into(),
            status: status.into(),
            message: message.into(),
            records: 0,
            undated: 0,
            errors: 0,
            scanned_at: now_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quota {
    pub app: String,
    pub percent_used: f64,
    pub next_reset_ms: Option<i64>,
    pub note: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub records: Vec<UsageRecord>,
    pub coverage: Vec<Coverage>,
    pub quotas: Vec<Quota>,
}

pub fn stable_id(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    hex::encode(hash.finalize())
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
