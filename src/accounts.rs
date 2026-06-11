use gtk4::{gio, prelude::SettingsExt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Udp,
    Tcp,
    Tls,
}

impl Transport {
    pub fn default_port(self) -> u16 {
        match self {
            Transport::Tls => 5061,
            _ => 5060,
        }
    }

    pub fn as_c_int(self) -> std::ffi::c_int {
        match self {
            Transport::Udp => 0,
            Transport::Tcp => 1,
            Transport::Tls => 2,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Account {
    pub id: String,
    pub display_name: String,
    pub username: String,
    pub server: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub proxy: String,
    #[serde(default)]
    pub transport: Transport,
    #[serde(default)]
    pub tls_verify: bool,
    #[serde(default)]
    pub tls_ca_file: String,
    pub register_on_startup: bool,
}

fn default_port() -> u16 {
    5060
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
        if let Ok(mut accounts) = serde_json::from_str::<Vec<Account>>(&data) {
            // Migrate old entries where port was embedded in the server field as "host:port".
            for account in &mut accounts {
                if account.port == 5060 {
                    if let Some(colon) = account.server.rfind(':') {
                        if let Ok(p) = account.server[colon + 1..].parse::<u16>() {
                            let host = account.server[..colon].trim().to_string();
                            if !host.is_empty() {
                                account.server = host;
                                account.port = p;
                            }
                        }
                    }
                }
            }
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
    let settings = gio::Settings::new("io.github.thomaswasle.TMWPhone");
    let username = settings.string("sip-username").to_string();
    let server = settings.string("sip-server").to_string();
    if username.is_empty() || server.is_empty() {
        return vec![];
    }
    let port = settings.int("sip-port").clamp(1, 65535) as u16;
    let account = Account {
        id: new_id(),
        display_name: settings.string("sip-display-name").to_string(),
        username,
        server,
        port,
        proxy: String::new(),
        transport: Transport::Udp,
        tls_verify: false,
        tls_ca_file: String::new(),
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
