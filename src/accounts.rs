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
                migrate_embedded_port(account);
            }
            return accounts;
        }
    }
    migrate_from_gsettings()
}

/// Split a legacy `"host:port"` server field into separate `server` + `port`.
/// Only applies when `port` is still at the default (5060), the server contains
/// a `:`-suffixed parseable port, and the remaining host is non-empty.
fn migrate_embedded_port(account: &mut Account) {
    if account.port != 5060 {
        return;
    }
    let Some(colon) = account.server.rfind(':') else { return };
    let Ok(p) = account.server[colon + 1..].parse::<u16>() else { return };
    let host = account.server[..colon].trim().to_string();
    if host.is_empty() {
        return;
    }
    account.server = host;
    account.port = p;
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_default_ports() {
        assert_eq!(Transport::Udp.default_port(), 5060);
        assert_eq!(Transport::Tcp.default_port(), 5060);
        assert_eq!(Transport::Tls.default_port(), 5061);
    }

    #[test]
    fn transport_c_int_mapping() {
        assert_eq!(Transport::Udp.as_c_int(), 0);
        assert_eq!(Transport::Tcp.as_c_int(), 1);
        assert_eq!(Transport::Tls.as_c_int(), 2);
    }

    #[test]
    fn transport_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Transport::Tls).unwrap(), "\"tls\"");
        assert_eq!(serde_json::to_string(&Transport::Udp).unwrap(), "\"udp\"");
        assert_eq!(
            serde_json::from_str::<Transport>("\"tcp\"").unwrap(),
            Transport::Tcp
        );
    }

    #[test]
    fn transport_default_is_udp() {
        assert_eq!(Transport::default(), Transport::Udp);
    }

    #[test]
    fn label_prefers_display_name_then_username() {
        let mut a = Account::new();
        assert_eq!(a.label(), "New account");

        a.username = "820".to_string();
        assert_eq!(a.label(), "820");

        a.display_name = "Alice".to_string();
        assert_eq!(a.label(), "Alice");
    }

    #[test]
    fn new_account_has_sane_defaults() {
        let a = Account::new();
        assert!(a.register_on_startup);
        assert!(!a.id.is_empty());
        assert_eq!(a.transport, Transport::Udp);
        assert_eq!(a.port, 0); // Account::new() does not apply default_port
    }

    #[test]
    fn new_id_is_unique_and_monotonic_counter() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
    }

    #[test]
    fn deserialize_applies_field_defaults() {
        // A minimal record missing all the #[serde(default)] fields.
        let json = r#"{
            "id": "abc",
            "display_name": "",
            "username": "u",
            "server": "pbx",
            "register_on_startup": true
        }"#;
        let a: Account = serde_json::from_str(json).unwrap();
        assert_eq!(a.port, 5060);
        assert_eq!(a.proxy, "");
        assert_eq!(a.transport, Transport::Udp);
        assert!(!a.tls_verify);
        assert_eq!(a.tls_ca_file, "");
    }

    #[test]
    fn account_json_round_trip() {
        let mut a = Account::new();
        a.username = "830".to_string();
        a.server = "pbx.example.com".to_string();
        a.port = 5070;
        a.transport = Transport::Tls;
        a.tls_verify = true;
        a.tls_ca_file = "/etc/ca.pem".to_string();

        let json = serde_json::to_string(&a).unwrap();
        let b: Account = serde_json::from_str(&json).unwrap();

        assert_eq!(a.id, b.id);
        assert_eq!(a.username, b.username);
        assert_eq!(a.server, b.server);
        assert_eq!(a.port, b.port);
        assert_eq!(a.transport, b.transport);
        assert_eq!(a.tls_verify, b.tls_verify);
        assert_eq!(a.tls_ca_file, b.tls_ca_file);
    }

    fn acct(server: &str, port: u16) -> Account {
        Account { server: server.to_string(), port, ..Default::default() }
    }

    #[test]
    fn migrate_splits_embedded_port() {
        let mut a = acct("pbx.example.com:5070", 5060);
        migrate_embedded_port(&mut a);
        assert_eq!(a.server, "pbx.example.com");
        assert_eq!(a.port, 5070);
    }

    #[test]
    fn migrate_leaves_plain_host_untouched() {
        let mut a = acct("pbx.example.com", 5060);
        migrate_embedded_port(&mut a);
        assert_eq!(a.server, "pbx.example.com");
        assert_eq!(a.port, 5060);
    }

    #[test]
    fn migrate_skips_when_port_already_customized() {
        // port != 5060 means the user already set it explicitly; don't touch server.
        let mut a = acct("pbx.example.com:5070", 5061);
        migrate_embedded_port(&mut a);
        assert_eq!(a.server, "pbx.example.com:5070");
        assert_eq!(a.port, 5061);
    }

    #[test]
    fn migrate_ignores_unparseable_or_empty_host() {
        let mut a = acct("pbx.example.com:notaport", 5060);
        migrate_embedded_port(&mut a);
        assert_eq!(a.server, "pbx.example.com:notaport");
        assert_eq!(a.port, 5060);

        let mut b = acct(":5070", 5060);
        migrate_embedded_port(&mut b);
        assert_eq!(b.server, ":5070");
        assert_eq!(b.port, 5060);
    }
}
