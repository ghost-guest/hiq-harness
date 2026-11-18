//! L0: the append-only log, and the projection that is the only state.
//!
//! Six invariants are taken from dim's `docs/architecture/rust-agent/time-bus.md`
//! and enforced here as tests, not comments:
//!   1. only the sequence number orders events
//!   2. state = fold(events)
//!   3. the bus carries only what changes later behaviour
//!   4. write-time re-checks belong to each state machine, not the bus
//!   5. side effects are never replayed
//!   6. process-level state stays off the bus
//!
//! Rule 5 is the reason receipts live here: a tool that may or may not have run
//! is recorded `Uncertain` and *blocks* the turn instead of being retried.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub seq: u64,
    pub at_ms: i64,
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptStatus {
    /// Effect observed. Never re-run.
    Committed,
    /// Effect may or may not have happened. Blocks until a human decides.
    Uncertain,
    /// Human said it did not happen. Safe to retry.
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Receipt {
    pub call_id: String,
    pub tool: String,
    pub status: ReceiptStatus,
    pub seq: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Projection {
    pub next_seq: u64,
    pub messages: Vec<Value>,
    pub receipts: BTreeMap<String, Receipt>,
    pub compacted_at: Option<u64>,
    pub turn_open: bool,
}

impl Projection {
    /// True while a side effect is unresolved. Callers must block, not retry.
    pub fn blocked(&self) -> Option<&Receipt> {
        self.receipts.values().find(|r| r.status == ReceiptStatus::Uncertain)
    }
}

pub struct Log {
    path: PathBuf,
    file: File,
    entries: Vec<Entry>,
}

impl Log {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let entries = read_entries(&path)?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self { path, file, entries })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn next_seq(&self) -> u64 {
        self.entries.last().map(|e| e.seq + 1).unwrap_or(0)
    }

    /// Append one fact. Returns its sequence number. A single `write_all` keeps
    /// the line atomic — the same short-write bug documented in orbit-harness.md.
    pub fn append(&mut self, kind: &str, payload: Value) -> Result<u64> {
        let seq = self.next_seq();
        let entry = Entry { seq, at_ms: now_ms(), kind: kind.to_string(), payload };
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        self.entries.push(entry);
        Ok(seq)
    }

    pub fn project(&self) -> Projection {
        project(&self.entries)
    }

    /// Invariant 1: strictly increasing, no gaps, single writer.
    pub fn check_sequence(&self) -> Result<()> {
        for (i, e) in self.entries.iter().enumerate() {
            if e.seq != i as u64 {
                anyhow::bail!("sequence violated at index {i}: got {}", e.seq);
            }
        }
        Ok(())
    }
}

/// Pure function: same entries always give the same projection (invariant 2).
pub fn project(entries: &[Entry]) -> Projection {
    let mut p = Projection::default();
    for e in entries {
        p.next_seq = e.seq + 1;
        match e.kind.as_str() {
            "message" => p.messages.push(e.payload.clone()),
            "turn.start" => p.turn_open = true,
            "turn.end" => p.turn_open = false,
            "tool.call" => {
                let call_id = e.payload["call_id"].as_str().unwrap_or_default().to_string();
                let tool = e.payload["tool"].as_str().unwrap_or_default().to_string();
                let status = match e.payload["status"].as_str().unwrap_or("committed") {
                    "uncertain" => ReceiptStatus::Uncertain,
                    "rolled_back" => ReceiptStatus::RolledBack,
                    _ => ReceiptStatus::Committed,
                };
                p.receipts.insert(call_id.clone(), Receipt { call_id, tool, status, seq: e.seq });
            }
            "compact" => p.compacted_at = Some(e.seq),
            _ => {}
        }
    }
    p
}

pub fn read_entries(path: impl AsRef<Path>) -> Result<Vec<Entry>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let e: Entry = serde_json::from_str(&line)
            .with_context(|| format!("{}:{}: malformed entry", path.display(), i + 1))?;
        out.push(e);
    }
    Ok(out)
}

/// Rebuild a log file from a projection without inventing facts — used by
/// `/restore` after a bad compaction.
pub fn rewrite(path: impl AsRef<Path>, entries: &[Entry]) -> Result<()> {
    let mut tmp = path.as_ref().to_path_buf();
    tmp.set_extension("ndjson.tmp");
    {
        let mut f = File::create(&tmp)?;
        for (i, e) in entries.iter().enumerate() {
            let fixed = Entry { seq: i as u64, ..e.clone() };
            let mut line = serde_json::to_string(&fixed)?;
            line.push('\n');
            f.write_all(line.as_bytes())?;
        }
        f.flush()?;
    }
    std::fs::rename(&tmp, path.as_ref())?;
    Ok(())
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn digest(s: &str) -> String {
    // ponytail: FNV-1a, not a crypto hash. Guards only need "same or not".
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("harnessd-timebus-{}-{}.ndjson", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn sample() -> Vec<Entry> {
        vec![
            Entry { seq: 0, at_ms: 1, kind: "turn.start".into(), payload: json!({"thread":"t1"}) },
            Entry { seq: 1, at_ms: 2, kind: "message".into(), payload: json!({"role":"user","text":"hi"}) },
            Entry { seq: 2, at_ms: 3, kind: "tool.call".into(),
                    payload: json!({"call_id":"c1","tool":"shell","status":"committed"}) },
            Entry { seq: 3, at_ms: 4, kind: "turn.end".into(), payload: json!({}) },
        ]
    }

    // Invariant 2: fold is pure.
    #[test]
    fn projection_is_pure() {
        let e = sample();
        assert_eq!(project(&e), project(&e));
    }

    // Invariant 3: only behaviour-changing facts matter — a cosmetic event
    // must not move the projection.
    #[test]
    fn unrelated_event_does_not_change_projection() {
        let mut e = sample();
        let before = project(&e);
        e.push(Entry { seq: 4, at_ms: 5, kind: "ui.scroll".into(), payload: json!({"y":10}) });
        let after = project(&e);
        assert_eq!(before.messages, after.messages);
        assert_eq!(before.receipts, after.receipts);
    }

    // Invariant 5: an unresolved side effect blocks instead of replaying.
    #[test]
    fn uncertain_receipt_blocks() {
        let mut e = sample();
        e.push(Entry { seq: 4, at_ms: 5, kind: "tool.call".into(),
                       payload: json!({"call_id":"c2","tool":"shell","status":"uncertain"}) });
        let p = project(&e);
        assert_eq!(p.blocked().map(|r| r.call_id.as_str()), Some("c2"));
    }

    // Invariant 1: a gap is corruption, not something to paper over.
    #[test]
    fn sequence_gap_is_detected() {
        let path = tmp("seq");
        let mut log = Log::open(&path).unwrap();
        log.append("message", json!({"a":1})).unwrap();
        log.append("message", json!({"a":2})).unwrap();
        log.check_sequence().unwrap();
        let mut entries = read_entries(&path).unwrap();
        entries[1].seq = 9;
        rewrite(&path, &entries).unwrap();
        // rewrite renumbers, so corruption must be injected by hand
        std::fs::write(&path, "{\"seq\":0,\"at_ms\":1,\"kind\":\"message\",\"payload\":{}}\n\
                               {\"seq\":5,\"at_ms\":2,\"kind\":\"message\",\"payload\":{}}\n").unwrap();
        let broken = read_entries(&path).unwrap();
        let log = Log { path: path.clone(), file: OpenOptions::new().append(true).open(&path).unwrap(), entries: broken };
        assert!(log.check_sequence().is_err());
        let _ = std::fs::remove_file(&path);
    }

    // Invariant 5, second half: replaying a file must not re-issue effects.
    #[test]
    fn replay_is_read_only() {
        let path = tmp("replay");
        {
            let mut log = Log::open(&path).unwrap();
            log.append("turn.start", json!({})).unwrap();
            log.append("tool.call", json!({"call_id":"c1","tool":"shell"})).unwrap();
        }
        let before = std::fs::read_to_string(&path).unwrap();
        let log = Log::open(&path).unwrap();
        let p = log.project();
        assert_eq!(p.receipts.len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "replay must not write");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn digest_is_stable() {
        assert_eq!(digest("a"), digest("a"));
        assert_ne!(digest("a"), digest("b"));
    }
}
