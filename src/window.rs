use std::cell::{OnceCell, RefCell};

use gtk4::{gio, glib, prelude::*, subclass::prelude::*, CompositeTemplate};
use libadwaita as adw;
use adw::prelude::*;
use adw::subclass::prelude::*;

use crate::audio::AudioSession;
use crate::call_log;
use crate::ringer::Ringer;
use crate::sip::{SipEngine, SipEvent};
use crate::widgets::{CallScreen, Dialpad, SettingsDialog};

mod imp {
    use super::*;

    // ── Per-call tracking ─────────────────────────────────────────────────────

    pub struct PendingCall {
        pub direction: call_log::Direction,
        pub number: String,
        pub started_at: i64,
        pub connected_at: Option<i64>,
    }

    // ── Window struct ─────────────────────────────────────────────────────────

    #[derive(CompositeTemplate, Default)]
    #[template(file = "../data/ui/window.ui")]
    pub struct MainWindow {
        #[template_child]
        pub status_banner: TemplateChild<adw::Banner>,
        #[template_child]
        pub view_stack: TemplateChild<adw::ViewStack>,
        #[template_child]
        pub call_revealer: TemplateChild<gtk4::Revealer>,
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,

        pub dialpad: OnceCell<Dialpad>,
        pub call_screen: OnceCell<CallScreen>,
        pub call_list_box: OnceCell<gtk4::ListBox>,
        pub sip_engine: RefCell<Option<SipEngine>>,
        pub audio_session: RefCell<Option<AudioSession>>,
        pub consult_session: RefCell<Option<AudioSession>>,
        pub ringer: RefCell<Option<Ringer>>,
        /// Name/number of the primary caller, used for the "Holding: …" label.
        pub primary_caller: RefCell<String>,
        /// Watchdog timer — fires every 40 s and triggers a full reconnect if
        /// no REGISTER 200 OK has arrived in the last 90 s.
        pub keepalive_timer: RefCell<Option<glib::SourceId>>,
        /// Unix timestamp of the last successful REGISTER 200 OK.
        pub last_register_ok: RefCell<Option<i64>>,
        pub call_log: RefCell<call_log::CallLog>,
        pub pending_call: RefCell<Option<PendingCall>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MainWindow {
        const NAME: &'static str = "MainWindow";
        type Type = super::MainWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for MainWindow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();

            // Populate view stack with dialpad tab
            let dialpad = Dialpad::new();
            self.view_stack.add_titled_with_icon(
                &dialpad,
                Some("dialpad"),
                "Dial",
                "input-dialpad-symbolic",
            );
            dialpad.connect_local(
                "call-requested",
                false,
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    None,
                    move |args| {
                        let number = args[1].get::<String>().unwrap_or_default();
                        obj.imp().start_call(&number);
                        None
                    }
                ),
            );
            self.dialpad.set(dialpad).unwrap();

            // ── Recents tab ───────────────────────────────────────────────────

            let list_box = gtk4::ListBox::new();
            list_box.set_selection_mode(gtk4::SelectionMode::None);
            list_box.add_css_class("boxed-list");

            let placeholder = gtk4::Label::builder()
                .label("No recent calls")
                .margin_top(48)
                .margin_bottom(48)
                .build();
            placeholder.add_css_class("dim-label");
            list_box.set_placeholder(Some(&placeholder));

            let recents_inner = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            recents_inner.set_margin_top(12);
            recents_inner.set_margin_bottom(12);
            recents_inner.set_margin_start(12);
            recents_inner.set_margin_end(12);
            recents_inner.append(&list_box);

            let recents_scroll = gtk4::ScrolledWindow::new();
            recents_scroll.set_vexpand(true);
            recents_scroll.set_child(Some(&recents_inner));

            self.view_stack.add_titled_with_icon(
                &recents_scroll,
                Some("recents"),
                "Recents",
                "recent-activity-symbolic",
            );
            self.call_list_box.set(list_box.clone()).unwrap();

            // Populate the list from persisted log (newest first = index 0 appended first)
            let log = call_log::CallLog::load();
            for record in &log.records {
                list_box.append(&self.make_call_row(record));
            }
            *self.call_log.borrow_mut() = log;

            // Attach call screen to the revealer overlay
            let call_screen = CallScreen::new();
            self.call_revealer.set_child(Some(&call_screen));
            call_screen.connect_local(
                "answer-clicked",
                false,
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    None,
                    move |_| {
                        obj.imp().answer_call();
                        None
                    }
                ),
            );
            call_screen.connect_local(
                "hangup-clicked",
                false,
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    None,
                    move |_| {
                        obj.imp().hangup_call();
                        None
                    }
                ),
            );
            call_screen.connect_local(
                "mute-toggled",
                false,
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    None,
                    move |args| {
                        let muted = args[1].get::<bool>().unwrap_or(false);
                        if let Some(engine) = obj.imp().sip_engine.borrow().as_ref() {
                            engine.set_muted(muted);
                        }
                        None
                    }
                ),
            );
            call_screen.connect_local(
                "hold-toggled",
                false,
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    None,
                    move |args| {
                        let hold = args[1].get::<bool>().unwrap_or(false);
                        if let Some(engine) = obj.imp().sip_engine.borrow().as_ref() {
                            engine.set_hold(hold);
                        }
                        if let Some(session) = obj.imp().audio_session.borrow().as_ref() {
                            session.set_hold(hold);
                        }
                        None
                    }
                ),
            );
            call_screen.connect_local(
                "dtmf-digit",
                false,
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    None,
                    move |args| {
                        let digit_str = args[1].get::<String>().unwrap_or_default();
                        if let Some(c) = digit_str.chars().next() {
                            if let Some(engine) = obj.imp().sip_engine.borrow().as_ref() {
                                engine.send_dtmf(c);
                            }
                        }
                        None
                    }
                ),
            );
            call_screen.connect_local(
                "transfer-blind-requested",
                false,
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    None,
                    move |args| {
                        let number = args[1].get::<String>().unwrap_or_default();
                        if let Some(engine) = obj.imp().sip_engine.borrow().as_ref() {
                            engine.blind_transfer(&number);
                        }
                        None
                    }
                ),
            );
            call_screen.connect_local(
                "consult-requested",
                false,
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    None,
                    move |args| {
                        let number = args[1].get::<String>().unwrap_or_default();
                        if let Some(engine) = obj.imp().sip_engine.borrow().as_ref() {
                            engine.start_consultation(&number);
                        }
                        None
                    }
                ),
            );
            call_screen.connect_local(
                "transfer-complete-requested",
                false,
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    None,
                    move |_| {
                        if let Some(engine) = obj.imp().sip_engine.borrow().as_ref() {
                            engine.complete_transfer();
                        }
                        None
                    }
                ),
            );
            call_screen.connect_local(
                "consult-cancel-requested",
                false,
                glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    None,
                    move |_| {
                        if let Some(engine) = obj.imp().sip_engine.borrow().as_ref() {
                            engine.cancel_consultation();
                        }
                        None
                    }
                ),
            );
            self.call_screen.set(call_screen).unwrap();

            // Banner button: "Connect" connects directly, "Configure" opens settings,
            // "Copy" copies the error text to clipboard.
            self.status_banner.connect_button_clicked(glib::clone!(
                #[weak]
                obj,
                move |banner| {
                    match banner.button_label().as_deref() {
                        Some("Connect") | Some("Reconnect") => {
                            obj.imp().on_connect_requested();
                        }
                        _ => {
                            obj.open_settings_dialog();
                        }
                    }
                }
            ));

            // Auto-connect if credentials are already saved; otherwise prompt to configure.
            let settings = gio::Settings::new("net.loca.TMWPhone");
            let has_credentials = !settings.string("sip-username").is_empty()
                && !settings.string("sip-server").is_empty();
            if has_credentials {
                self.on_connect_requested();
            } else {
                self.status_banner.set_title("Not registered — tap Configure");
            }

            // Re-register whenever the network comes back up (WiFi reconnect,
            // DHCP renewal, VPN change).  Creating a fresh SipEngine rebinds
            // the local socket and sends a new Contact header so the server
            // knows the current address.  We skip this during an active call
            // to avoid dropping it; the keepalive timer covers that window.
            let monitor = gio::NetworkMonitor::default();
            monitor.connect_network_changed(glib::clone!(
                #[weak]
                obj,
                move |_monitor, available| {
                    if !available { return; }
                    let imp = obj.imp();
                    if imp.audio_session.borrow().is_some() { return; }
                    if imp.sip_engine.borrow().is_none() { return; }
                    // Debounce: NetworkManager emits network-changed several times
                    // during connection establishment (link up, DHCP, connectivity
                    // checks).  Skip if we connected or re-registered less than 30 s
                    // ago — last_register_ok is stamped in on_connect_requested() so
                    // this is always non-None after the first connect attempt.
                    if imp.last_register_ok.borrow()
                        .map(|t| now_unix() - t < 30)
                        .unwrap_or(false)
                    {
                        return;
                    }
                    imp.on_connect_requested();
                }
            ));
        }
    }

    impl WidgetImpl for MainWindow {}
    impl WindowImpl for MainWindow {}
    impl ApplicationWindowImpl for MainWindow {}
    impl AdwApplicationWindowImpl for MainWindow {}

    impl MainWindow {
        pub fn handle_sip_event(&self, event: SipEvent) {
            match event {
                SipEvent::Registered => {
                    let first_registration = self.last_register_ok.borrow().is_none();
                    *self.last_register_ok.borrow_mut() = Some(now_unix());
                    self.status_banner.set_revealed(false);
                    if first_registration {
                        let settings = gio::Settings::new("net.loca.TMWPhone");
                        let user = settings.string("sip-username");
                        let server = settings.string("sip-server");
                        let toast = adw::Toast::new(&format!("Registered as {user}@{server}"));
                        toast.set_timeout(4);
                        self.toast_overlay.add_toast(toast);
                    }
                }
                SipEvent::RegistrationFailed(reason) => {
                    self.status_banner
                        .set_title(&format!("Registration failed: {reason}"));
                    self.status_banner.set_button_label(Some("Reconnect"));
                    self.status_banner.set_revealed(true);
                }
                SipEvent::IncomingCall { from } => {
                    *self.primary_caller.borrow_mut() = from.clone();
                    if let Some(cs) = self.call_screen.get() {
                        cs.set_caller(&call_log::display_name(&from));
                        cs.set_duration("Incoming call…");
                        cs.show_answer_button(true);
                    }
                    self.show_call_screen(true);
                    *self.ringer.borrow_mut() = Ringer::start_incoming();
                    *self.pending_call.borrow_mut() = Some(PendingCall {
                        direction: call_log::Direction::Incoming,
                        number: from,
                        started_at: now_unix(),
                        connected_at: None,
                    });
                }
                SipEvent::CallConnected => {
                    *self.ringer.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() {
                        cs.show_answer_button(false);
                        cs.start_timer();
                    }
                    if let Some(p) = self.pending_call.borrow_mut().as_mut() {
                        p.connected_at = Some(now_unix());
                    }
                }
                SipEvent::CallMedia { local_rtp_port, remote_ip, remote_rtp_port, codec } => {
                    match AudioSession::start(local_rtp_port, &remote_ip, remote_rtp_port, codec) {
                        Ok(session) => {
                            *self.audio_session.borrow_mut() = Some(session);
                        }
                        Err(e) => {
                            log::error!("audio start failed: {e}");
                            self.toast_overlay
                                .add_toast(error_toast(&format!("Audio failed: {e}")));
                        }
                    }
                }
                SipEvent::CallEnded => {
                    *self.ringer.borrow_mut() = None;
                    *self.audio_session.borrow_mut() = None;
                    *self.consult_session.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() { cs.stop_timer(); }
                    self.show_call_screen(false);
                    if let Some(dialpad) = self.dialpad.get() { dialpad.clear(); }
                    self.finalize_pending_call(false);
                }
                SipEvent::CallFailed(reason) => {
                    *self.ringer.borrow_mut() = None;
                    *self.audio_session.borrow_mut() = None;
                    *self.consult_session.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() { cs.stop_timer(); }
                    self.show_call_screen(false);
                    self.toast_overlay
                        .add_toast(error_toast(&format!("Call failed: {reason}")));
                    self.finalize_pending_call(true);
                }
                SipEvent::TransferOk => {
                    *self.ringer.borrow_mut() = None;
                    *self.audio_session.borrow_mut() = None;
                    *self.consult_session.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() { cs.stop_timer(); }
                    self.show_call_screen(false);
                    self.finalize_pending_call(false);
                    let toast = adw::Toast::new("Call transferred successfully");
                    toast.set_timeout(4);
                    self.toast_overlay.add_toast(toast);
                }
                SipEvent::TransferFailed(reason) => {
                    self.toast_overlay
                        .add_toast(error_toast(&format!("Transfer failed: {reason}")));
                }
                SipEvent::ConsultConnected => {
                    let held_name = self.primary_caller.borrow().clone();
                    if let Some(cs) = self.call_screen.get() {
                        cs.enter_consult_mode(&held_name);
                    }
                }
                SipEvent::ConsultMedia { local_rtp_port, remote_ip, remote_rtp_port, codec } => {
                    match AudioSession::start(local_rtp_port, &remote_ip, remote_rtp_port, codec) {
                        Ok(session) => {
                            *self.consult_session.borrow_mut() = Some(session);
                        }
                        Err(e) => {
                            log::error!("consult audio start failed: {e}");
                            self.toast_overlay
                                .add_toast(error_toast(&format!("Consult audio failed: {e}")));
                        }
                    }
                }
                SipEvent::ConsultEnded => {
                    *self.consult_session.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() {
                        cs.exit_consult_mode();
                    }
                    // Resume primary audio (C layer already sent re-INVITE with sendrecv).
                    if let Some(session) = self.audio_session.borrow().as_ref() {
                        session.set_hold(false);
                    }
                }
            }
        }

        fn show_call_screen(&self, visible: bool) {
            self.call_revealer.set_reveal_child(visible);
            self.call_revealer.set_can_target(visible);
        }

        pub fn start_call(&self, number: &str) {
            if let Some(engine) = self.sip_engine.borrow().as_ref() {
                *self.primary_caller.borrow_mut() = number.to_owned();
                if let Some(cs) = self.call_screen.get() {
                    cs.set_caller(number);
                    cs.set_duration("Calling…");
                    cs.show_answer_button(false);
                }
                self.show_call_screen(true);
                *self.ringer.borrow_mut() = Ringer::start_ringback();
                *self.pending_call.borrow_mut() = Some(PendingCall {
                    direction: call_log::Direction::Outgoing,
                    number: number.to_owned(),
                    started_at: now_unix(),
                    connected_at: None,
                });
                engine.make_call(number);
            } else {
                let toast = adw::Toast::new("Not registered — configure SIP account first");
                self.toast_overlay.add_toast(toast);
            }
        }

        fn answer_call(&self) {
            if let Some(engine) = self.sip_engine.borrow().as_ref() {
                engine.answer_call();
            }
        }

        fn hangup_call(&self) {
            if let Some(engine) = self.sip_engine.borrow().as_ref() {
                engine.hangup();
            }
        }

        // ── Call log helpers ─────────────────────────────────────────────────

        fn finalize_pending_call(&self, outgoing_failed: bool) {
            let Some(pending) = self.pending_call.borrow_mut().take() else { return };
            let now = now_unix();
            let (status, duration) = match pending.connected_at {
                Some(t) => (call_log::Status::Answered, (now - t).max(0) as u32),
                None => {
                    let status = if pending.direction == call_log::Direction::Incoming {
                        call_log::Status::Missed
                    } else if outgoing_failed {
                        call_log::Status::Failed
                    } else {
                        call_log::Status::Failed // outgoing ended before connect
                    };
                    (status, 0)
                }
            };
            let record = call_log::Record {
                direction: pending.direction,
                status,
                number: pending.number,
                started_at: pending.started_at,
                duration_secs: duration,
            };
            if let Some(lb) = self.call_list_box.get() {
                lb.prepend(&self.make_call_row(&record));
            }
            self.call_log.borrow_mut().push(record);
        }

        fn make_call_row(&self, record: &call_log::Record) -> adw::ActionRow {
            use call_log::{Direction, Status};

            let (icon_name, icon_css) = match (record.direction, record.status) {
                (Direction::Incoming, Status::Answered) => ("call-incoming-symbolic",  "success"),
                (Direction::Incoming, _)                => ("call-missed-symbolic",    "error"),
                (Direction::Outgoing, Status::Answered) => ("call-outgoing-symbolic",  "accent"),
                (Direction::Outgoing, _)                => ("call-outgoing-symbolic",  "dim-label"),
            };

            let icon = gtk4::Image::from_icon_name(icon_name);
            icon.add_css_class(icon_css);
            icon.set_pixel_size(16);
            icon.set_margin_top(8);
            icon.set_margin_bottom(8);

            let title = call_log::display_name(&record.number);
            let time  = call_log::format_time(record.started_at);
            let subtitle = if record.duration_secs > 0 {
                format!("{time} · {}", call_log::format_duration(record.duration_secs))
            } else {
                time
            };

            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .activatable(true)
                .build();
            row.add_prefix(&icon);

            // Tap to call back
            let number = call_log::callable(&record.number);
            let weak = self.obj().downgrade();
            row.connect_activated(move |_| {
                if let Some(obj) = weak.upgrade() {
                    obj.imp().start_call(&number);
                    // Switch to dialpad so the call screen is visible
                    obj.imp().view_stack.set_visible_child_name("dialpad");
                }
            });

            row
        }

        pub fn on_connect_requested(&self) {
            let settings = gio::Settings::new("net.loca.TMWPhone");

            // Accept "host" or "host:port" in the server field.
            let server_raw = settings.string("sip-server").to_string();
            let (host, port) = parse_server_field(&server_raw)
                .unwrap_or_else(|| (server_raw.clone(), settings.int("sip-port") as u16));

            if host.is_empty() {
                let toast = adw::Toast::new("SIP server must not be empty");
                self.toast_overlay.add_toast(toast);
                return;
            }

            let obj = self.obj();
            let obj_weak = obj.downgrade();
            let engine = SipEngine::new(&host, port, move |event| {
                if let Some(obj) = obj_weak.upgrade() {
                    obj.imp().handle_sip_event(event);
                }
            });

            engine.register(crate::sip::SipConfig {
                server: host,
                username: settings.string("sip-username").into(),
                password: crate::keyring::load().unwrap_or_default(),
                display_name: settings.string("sip-display-name").into(),
                port,
            });

            self.status_banner.set_title("Registering…");
            self.status_banner.set_button_label(None::<&str>);
            self.status_banner.set_revealed(true);

            *self.sip_engine.borrow_mut() = Some(engine);

            // Stamp the connect time so the watchdog doesn't fire before the
            // first REGISTER 200 OK has had a chance to arrive.
            *self.last_register_ok.borrow_mut() = Some(now_unix());

            // Cancel any previous watchdog and start a new one.
            // Every 40 s: if no REGISTER 200 OK has arrived in 90 s, do a
            // full reconnect (new socket + new Contact header).  This covers
            // silent registration expiry and broken sofia auto-refresh without
            // the churn of hammering nua_register every few seconds.
            if let Some(id) = self.keepalive_timer.borrow_mut().take() {
                id.remove();
            }
            let obj_weak = self.obj().downgrade();
            let id = glib::timeout_add_seconds_local(40, move || {
                let Some(obj) = obj_weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let imp = obj.imp();

                // Send a lightweight REGISTER refresh every 40 s.
                // This reuses the existing socket and port so no window of
                // unreachability is created — Asterisk keeps routing to the
                // same Contact address throughout.
                if let Some(engine) = imp.sip_engine.borrow().as_ref() {
                    engine.reregister();
                }

                // Only do a full reconnect (new socket, new port) when
                // sofia_reregister itself has been failing for a long time
                // (180 s > ~2 expected refresh cycles for a 120 s expiry).
                // This covers the case where the local IP actually changed
                // and the existing socket is no longer reachable.
                let very_stale = imp.last_register_ok.borrow()
                    .map(|t| now_unix() - t > 180)
                    .unwrap_or(false);
                if very_stale && imp.audio_session.borrow().is_none() {
                    imp.on_connect_requested();
                }
                glib::ControlFlow::Continue
            });
            *self.keepalive_timer.borrow_mut() = Some(id);
        }
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse "host" or "host:port" from the server settings field.
/// Returns None if the host part is empty.
fn parse_server_field(s: &str) -> Option<(String, u16)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(colon) = s.rfind(':') {
        if let Ok(port) = s[colon + 1..].parse::<u16>() {
            let host = s[..colon].trim().to_string();
            if !host.is_empty() {
                return Some((host, port));
            }
        }
    }
    Some((s.to_string(), 5060))
}

/// Build an error toast with a "Copy" button that puts the message on the clipboard.
/// The toast stays visible for 10 seconds so the user has time to click the button.
fn error_toast(msg: &str) -> adw::Toast {
    let toast = adw::Toast::new(msg);
    toast.set_timeout(10);
    toast.set_button_label(Some("Copy"));
    let text = msg.to_owned();
    toast.connect_button_clicked(move |_| {
        if let Some(display) = gtk4::gdk::Display::default() {
            display.clipboard().set_text(&text);
        }
    });
    toast
}

glib::wrapper! {
    pub struct MainWindow(ObjectSubclass<imp::MainWindow>)
        @extends adw::ApplicationWindow, gtk4::ApplicationWindow, gtk4::Window, gtk4::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk4::Accessible, gtk4::Buildable,
                    gtk4::ConstraintTarget, gtk4::Native, gtk4::Root, gtk4::ShortcutManager;
}

impl MainWindow {
    pub fn new(app: &impl IsA<adw::Application>) -> Self {
        glib::Object::builder()
            .property("application", app)
            .build()
    }

    pub fn open_settings_dialog(&self) {
        let dialog = SettingsDialog::new();
        let win = self.clone();
        dialog.connect_local("connect-requested", false, move |_| {
            win.imp().on_connect_requested();
            None
        });
        dialog.present(Some(self));
    }
}
