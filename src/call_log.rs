use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

// ── Data model ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Answered,
    Missed,   // incoming, not answered
    Failed,   // outgoing, rejected / no answer
}

#[derive(Debug, Clone)]
pub struct Record {
    pub direction: Direction,
    pub status: Status,
    pub number: String,      // raw from-URI or dialled number
    pub started_at: i64,     // Unix timestamp
    pub duration_secs: u32,  // 0 if unanswered
}

// ── Call log ─────────────────────────────────────────────────────────────────

pub struct CallLog {
    path: PathBuf,
    pub records: Vec<Record>, // index 0 = most recent
}

impl Default for CallLog {
    fn default() -> Self {
        CallLog { path: log_path(), records: Vec::new() }
    }
}

impl CallLog {
    pub fn load() -> Self {
        let path = log_path();
        let records = parse_file(&path);
        CallLog { path, records }
    }

    /// Add a new record (newest first) and persist immediately.
    pub fn push(&mut self, record: Record) {
        self.records.insert(0, record);
        self.records.truncate(500);
        self.save();
    }

    fn save(&self) {
        let Ok(mut f) = fs::File::create(&self.path) else { return };
        for r in &self.records {
            let d = match r.direction { Direction::Incoming => 'i', Direction::Outgoing => 'o' };
            let s = match r.status { Status::Answered => 'a', Status::Missed => 'm', Status::Failed => 'f' };
            let _ = writeln!(f, "{}|{}|{}|{}|{}", r.started_at, d, s, r.number, r.duration_secs);
        }
    }
}

fn log_path() -> PathBuf {
    let dir = glib::user_data_dir().join("tmwphone");
    let _ = fs::create_dir_all(&dir);
    dir.join("calls.log")
}

fn parse_file(path: &PathBuf) -> Vec<Record> {
    let Ok(f) = fs::File::open(path) else { return Vec::new() };
    BufReader::new(f)
        .lines()
        .flatten()
        .filter_map(|l| parse_line(&l))
        .collect()
}

fn parse_line(line: &str) -> Option<Record> {
    let mut p = line.splitn(5, '|');
    let started_at: i64 = p.next()?.parse().ok()?;
    let direction = match p.next()? { "i" => Direction::Incoming, "o" => Direction::Outgoing, _ => return None };
    let status    = match p.next()? { "a" => Status::Answered, "m" => Status::Missed, "f" => Status::Failed, _ => return None };
    let number    = p.next()?.to_string();
    let duration_secs: u32 = p.next()?.parse().ok()?;
    Some(Record { direction, status, number, started_at, duration_secs })
}

// ── Display helpers ───────────────────────────────────────────────────────────

/// Extract a human-readable label from a raw from-URI or dialled number.
/// "Alice <820@pbx>" → "Alice"
/// "820@pbx"         → "820"
/// "820"             → "820"
pub fn display_name(raw: &str) -> String {
    // Display-name before '<'
    if let Some(bracket) = raw.find('<') {
        let name = raw[..bracket].trim().trim_matches('"').trim();
        if !name.is_empty() {
            return name.to_string();
        }
        // No display name: extract user from <user@host>
        let inner = &raw[bracket + 1..raw.rfind('>').unwrap_or(raw.len())];
        // Strip leading "sip:" or "sips:"
        let inner = inner.trim_start_matches("sip:").trim_start_matches("sips:");
        return inner.split('@').next().unwrap_or(inner).to_string();
    }
    // "user@host"
    if let Some(at) = raw.find('@') {
        let prefix = &raw[..at];
        // Strip "sip:" prefix if present
        return prefix.trim_start_matches("sip:").trim_start_matches("sips:").to_string();
    }
    raw.to_string()
}

/// Return the number to use when calling back this record.
/// For incoming calls the raw string may be "Name <user@host>"; we want "user@host".
pub fn callable(raw: &str) -> String {
    if let Some(bracket) = raw.find('<') {
        if let Some(close) = raw.find('>') {
            return raw[bracket + 1..close].to_string();
        }
    }
    raw.to_string()
}

/// Format a Unix timestamp as a human-readable call time.
pub fn format_time(started_at: i64) -> String {
    let Ok(dt)  = glib::DateTime::from_unix_local(started_at) else { return String::new() };
    let Ok(now) = glib::DateTime::now_local()                  else { return dt.format("%H:%M").unwrap_or_default().to_string() };

    let age_days = (now.to_unix() - started_at) / 86_400;
    match age_days {
        0 => dt.format("%H:%M").unwrap_or_default().to_string(),
        1 => format!("Yesterday {}", dt.format("%H:%M").unwrap_or_default()),
        2..=6 => dt.format("%A %H:%M").unwrap_or_default().to_string(), // "Monday 15:30"
        _ => dt.format("%e %b, %H:%M").unwrap_or_default().to_string(),
    }
}

/// Format a duration in seconds as "mm:ss" or "h:mm:ss".
pub fn format_duration(secs: u32) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}
