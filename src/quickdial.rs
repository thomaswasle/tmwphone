use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A configurable speed-dial entry: a human label and the number it dials.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct QuickDial {
    pub id: String,
    pub label: String,
    pub number: String,
}

impl QuickDial {
    pub fn new() -> Self {
        Self {
            id: crate::accounts::new_id(),
            ..Default::default()
        }
    }

    /// Text shown on the dial button: the label if set, else the number.
    pub fn display_label(&self) -> String {
        if !self.label.is_empty() {
            self.label.clone()
        } else if !self.number.is_empty() {
            self.number.clone()
        } else {
            "Quickdial".to_string()
        }
    }
}

fn quickdials_path() -> PathBuf {
    let mut path = glib::user_data_dir();
    path.push("tmwphone");
    path.push("quickdials.json");
    path
}

pub fn load() -> Vec<QuickDial> {
    let path = quickdials_path();
    if path.exists() {
        let data = std::fs::read_to_string(&path).unwrap_or_default();
        if let Ok(entries) = serde_json::from_str::<Vec<QuickDial>>(&data) {
            return entries;
        }
    }
    Vec::new()
}

pub fn save(entries: &[QuickDial]) {
    let path = quickdials_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_string_pretty(entries) {
        let _ = std::fs::write(path, data);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_label_prefers_label_then_number() {
        let mut q = QuickDial::new();
        assert_eq!(q.display_label(), "Quickdial");
        q.number = "1001".to_string();
        assert_eq!(q.display_label(), "1001");
        q.label = "Reception".to_string();
        assert_eq!(q.display_label(), "Reception");
    }

    #[test]
    fn serde_round_trip() {
        let entries = vec![
            QuickDial { id: "a".into(), label: "Reception".into(), number: "1001".into() },
            QuickDial { id: "b".into(), label: String::new(), number: "1002".into() },
        ];
        let json = serde_json::to_string(&entries).unwrap();
        let back: Vec<QuickDial> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].label, "Reception");
        assert_eq!(back[1].number, "1002");
    }
}
