use gio::{prelude::*, Settings};

pub struct Config {
    settings: Settings,
}

impl Config {
    pub fn new() -> Self {
        Config {
            settings: Settings::new("net.loca.Client"),
        }
    }

    pub fn server(&self) -> String {
        self.settings.string("sip-server").into()
    }
    pub fn username(&self) -> String {
        self.settings.string("sip-username").into()
    }
    pub fn password(&self) -> String {
        self.settings.string("sip-password").into()
    }
    pub fn display_name(&self) -> String {
        self.settings.string("sip-display-name").into()
    }
    pub fn port(&self) -> u16 {
        self.settings.int("sip-port") as u16
    }

    pub fn is_configured(&self) -> bool {
        !self.username().is_empty() && !self.server().is_empty()
    }
}
