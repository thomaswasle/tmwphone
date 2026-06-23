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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── display_name ──────────────────────────────────────────────────────────

    #[test]
    fn display_name_uses_quoted_display_name() {
        assert_eq!(display_name("Alice <820@pbx>"), "Alice");
        assert_eq!(display_name("\"Bob\" <sip:830@pbx>"), "Bob");
    }

    #[test]
    fn display_name_falls_back_to_user_part() {
        assert_eq!(display_name("<sip:840@pbx>"), "840");
        assert_eq!(display_name("<850@pbx>"), "850");
    }

    #[test]
    fn display_name_handles_bare_uris() {
        assert_eq!(display_name("820@pbx"), "820");
        assert_eq!(display_name("sip:820@pbx"), "820");
        assert_eq!(display_name("sips:820@pbx"), "820");
        assert_eq!(display_name("820"), "820");
    }

    // ── callable ────────────────────────────────────────────────────────────────

    #[test]
    fn callable_extracts_uri_inside_brackets() {
        assert_eq!(callable("Alice <820@pbx>"), "820@pbx");
        assert_eq!(callable("Name <sip:830@pbx>"), "sip:830@pbx");
    }

    #[test]
    fn callable_passes_through_bare_numbers() {
        assert_eq!(callable("820@pbx"), "820@pbx");
        assert_eq!(callable("820"), "820");
    }

    // ── format_duration ──────────────────────────────────────────────────────────

    #[test]
    fn format_duration_minutes_and_seconds() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(5), "0:05");
        assert_eq!(format_duration(65), "1:05");
        assert_eq!(format_duration(599), "9:59");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(3600), "1:00:00");
        assert_eq!(format_duration(3661), "1:01:01");
        assert_eq!(format_duration(7325), "2:02:05");
    }

    // ── parse_line ────────────────────────────────────────────────────────────────

    #[test]
    fn parse_line_valid_record() {
        let r = parse_line("1700000000|i|a|820@pbx|42").unwrap();
        assert_eq!(r.direction, Direction::Incoming);
        assert_eq!(r.status, Status::Answered);
        assert_eq!(r.number, "820@pbx");
        assert_eq!(r.started_at, 1_700_000_000);
        assert_eq!(r.duration_secs, 42);
    }

    #[test]
    fn parse_line_all_enum_variants() {
        assert_eq!(parse_line("1|o|f|x|0").unwrap().direction, Direction::Outgoing);
        assert_eq!(parse_line("1|o|f|x|0").unwrap().status, Status::Failed);
        assert_eq!(parse_line("1|i|m|x|0").unwrap().status, Status::Missed);
    }

    #[test]
    fn parse_line_rejects_malformed_input() {
        assert!(parse_line("notanumber|i|a|x|0").is_none());
        assert!(parse_line("1|x|a|x|0").is_none());     // bad direction
        assert!(parse_line("1|i|x|x|0").is_none());     // bad status
        assert!(parse_line("1|i|a|x|notanum").is_none()); // bad duration
        assert!(parse_line("1|i|a").is_none());          // too few fields
        assert!(parse_line("").is_none());
    }

    // ── save / load round-trip ────────────────────────────────────────────────────

    fn temp_log_path() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("tmwphone_test_{}_{nanos}.log", std::process::id()))
    }

    fn rec(started_at: i64, number: &str) -> Record {
        Record {
            direction: Direction::Outgoing,
            status: Status::Answered,
            number: number.to_string(),
            started_at,
            duration_secs: 12,
        }
    }

    #[test]
    fn push_persists_and_reloads_newest_first() {
        let path = temp_log_path();
        let mut log = CallLog { path: path.clone(), records: Vec::new() };

        log.push(rec(100, "alice@pbx"));
        log.push(rec(200, "bob@pbx"));

        // Re-read from disk via the same parser load() uses.
        let reloaded = parse_file(&path);
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded[0].number, "bob@pbx"); // newest first
        assert_eq!(reloaded[1].number, "alice@pbx");
        assert_eq!(reloaded[0].started_at, 200);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn push_caps_at_500_records() {
        let path = temp_log_path();
        let mut log = CallLog { path: path.clone(), records: Vec::new() };

        for i in 0..510 {
            log.push(rec(i, "x@pbx"));
        }
        assert_eq!(log.records.len(), 500);
        // The most recent push sits at index 0.
        assert_eq!(log.records[0].started_at, 509);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parse_file_missing_path_is_empty() {
        let path = temp_log_path(); // never created
        assert!(parse_file(&path).is_empty());
    }

    // ── format_time (smoke) ────────────────────────────────────────────────────────

    #[test]
    fn format_time_today_is_hh_mm() {
        let now = glib::DateTime::now_local().unwrap().to_unix();
        let s = format_time(now);
        // Today's calls render as zero-padded "HH:MM".
        assert_eq!(s.len(), 5, "got {s:?}");
        assert_eq!(s.as_bytes()[2], b':');
    }
}
