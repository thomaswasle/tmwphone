use gtk4::{gio, prelude::SettingsExt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Account {
    pub id: String,
    pub display_name: String,
    pub username: String,
    pub server: String,
    pub register_on_startup: bool,
}

impl Account {
    pub fn new() -> Self {
        Self {
            id: new_id(),
            register_on_startup: true,
            ..Default::default()
        }
    }

    pub fn label(&self) -> String {
        if !self.display_name.is_empty() {
            self.display_name.clone()
        } else if !self.username.is_empty() {
            self.username.clone()
        } else {
            "New account".to_string()
        }
    }
}

fn accounts_path() -> PathBuf {
    let mut path = glib::user_data_dir();
    path.push("tmwphone");
    path.push("accounts.json");
    path
}

pub fn load() -> Vec<Account> {
    let path = accounts_path();
    if path.exists() {
        let data = std::fs::read_to_string(&path).unwrap_or_default();
        if let Ok(accounts) = serde_json::from_str::<Vec<Account>>(&data) {
            return accounts;
        }
    }
    migrate_from_gsettings()
}

pub fn save(accounts: &[Account]) {
    let path = accounts_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_string_pretty(accounts) {
        let _ = std::fs::write(path, data);
    }
}

fn migrate_from_gsettings() -> Vec<Account> {
    let settings = gio::Settings::new("net.loca.TMWPhone");
    let username = settings.string("sip-username").to_string();
    let server = settings.string("sip-server").to_string();
    if username.is_empty() || server.is_empty() {
        return vec![];
    }
    let account = Account {
        id: new_id(),
        display_name: settings.string("sip-display-name").to_string(),
        username,
        server,
        register_on_startup: true,
    };
    let accounts = vec![account];
    save(&accounts);
    accounts
}

pub fn new_id() -> String {
    use std::time::SystemTime;
    thread_local! {
        static CTR: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let ctr = CTR.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    });
    format!("{nanos:x}{ctr:04x}")
}
