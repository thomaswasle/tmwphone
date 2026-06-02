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

    // ── Per-account engine state ──────────────────────────────────────────────

    pub struct ActiveEngine {
        pub account_id: String,
        pub engine: SipEngine,
        pub registered: bool,
        pub last_register_ok: Option<i64>,
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

        /// All accounts that have a running SIP engine (registered or registering).
        pub active_engines: RefCell<Vec<ActiveEngine>>,
        /// Which account is handling the current call.
        pub active_account_id: RefCell<Option<String>>,

        pub audio_session: RefCell<Option<AudioSession>>,
        pub consult_session: RefCell<Option<AudioSession>>,
        pub ringer: RefCell<Option<Ringer>>,
        pub primary_caller: RefCell<String>,
        pub keepalive_timer: RefCell<Option<glib::SourceId>>,
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

            // ── Dialpad tab ───────────────────────────────────────────────────

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
                        let account_id = args[2].get::<String>().unwrap_or_default();
                        obj.imp().start_call(&number, &account_id);
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

            let log = call_log::CallLog::load();
            for record in &log.records {
                list_box.append(&self.make_call_row(record));
            }
            *self.call_log.borrow_mut() = log;

            // ── Call screen ───────────────────────────────────────────────────

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
                        obj.imp().with_active_engine(|e| e.set_muted(muted));
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
                        obj.imp().with_active_engine(|e| e.set_hold(hold));
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
                            obj.imp().with_active_engine(|e| e.send_dtmf(c));
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
                        obj.imp().with_active_engine(|e| e.blind_transfer(&number));
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
                        obj.imp().with_active_engine(|e| e.start_consultation(&number));
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
                        obj.imp().with_active_engine(|e| e.complete_transfer());
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
                        obj.imp().with_active_engine(|e| e.cancel_consultation());
                        None
                    }
                ),
            );
            self.call_screen.set(call_screen).unwrap();

            // ── Status banner button ──────────────────────────────────────────

            self.status_banner.connect_button_clicked(glib::clone!(
                #[weak]
                obj,
                move |banner| {
                    match banner.button_label().as_deref() {
                        Some("Reconnect") => obj.imp().reconnect_all(),
                        _ => obj.open_settings_dialog(),
                    }
                }
            ));

            // ── Auto-connect on startup ───────────────────────────────────────

            let accounts = crate::accounts::load();

            // Migrate password from old single-account keyring slot (one-time).
            for acc in &accounts {
                if crate::keyring::load_for(&acc.id).is_none() {
                    if let Some(old_pw) = crate::keyring::load() {
                        let _ = crate::keyring::save_for(&acc.id, &old_pw);
                    }
                }
            }

            let startup_accounts: Vec<_> = accounts
                .iter()
                .filter(|a| a.register_on_startup)
                .collect();

            if startup_accounts.is_empty() && accounts.is_empty() {
                self.status_banner
                    .set_title("No accounts configured — tap Configure");
                self.status_banner.set_button_label(Some("Configure"));
                self.status_banner.set_revealed(true);
            } else if !startup_accounts.is_empty() {
                self.status_banner.set_title("Registering…");
                self.status_banner.set_button_label(None::<&str>);
                self.status_banner.set_revealed(true);
                for acc in &startup_accounts {
                    self.connect_account(acc);
                }
            }

            // ── Network reconnect ─────────────────────────────────────────────

            let monitor = gio::NetworkMonitor::default();
            monitor.connect_network_changed(glib::clone!(
                #[weak]
                obj,
                move |_monitor, available| {
                    if !available {
                        return;
                    }
                    let imp = obj.imp();
                    // Never tear down engines while a call is in progress — this
                    // covers the auth-retry window (INVITE sent → 401 → retry
                    // INVITE) where audio_session is still None even though
                    // active_account_id is already set.
                    if imp.audio_session.borrow().is_some() {
                        return;
                    }
                    if imp.active_account_id.borrow().is_some() {
                        return;
                    }
                    if imp.active_engines.borrow().is_empty() {
                        return;
                    }
                    // Debounce: skip if any engine registered successfully in last 30 s.
                    let recently_ok = imp
                        .active_engines
                        .borrow()
                        .iter()
                        .any(|e| e.last_register_ok.map(|t| now_unix() - t < 30).unwrap_or(false));
                    if recently_ok {
                        return;
                    }
                    imp.reconnect_all();
                }
            ));
        }
    }

    impl WidgetImpl for MainWindow {}
    impl WindowImpl for MainWindow {}
    impl ApplicationWindowImpl for MainWindow {}
    impl AdwApplicationWindowImpl for MainWindow {}

    impl MainWindow {
        // ── Engine helpers ────────────────────────────────────────────────────

        /// Call `f` with the SipEngine that owns the current call, if any.
        fn with_active_engine<F: FnOnce(&SipEngine)>(&self, f: F) {
            let id = match self.active_account_id.borrow().clone() {
                Some(id) => id,
                None => return,
            };
            let engines = self.active_engines.borrow();
            if let Some(entry) = engines.iter().find(|e| e.account_id == id) {
                f(&entry.engine);
            }
        }

        pub fn connect_account(&self, account: &crate::accounts::Account) {
            // Don't double-create.
            if self
                .active_engines
                .borrow()
                .iter()
                .any(|e| e.account_id == account.id)
            {
                return;
            }

            if account.server.is_empty() {
                return;
            }

            let account_id = account.id.clone();
            let obj_weak = self.obj().downgrade();
            let engine = SipEngine::new(
                &account.server,
                account.port,
                &account.proxy,
                account.transport.as_c_int(),
                account.tls_verify,
                &account.tls_ca_file,
                move |event| {
                    if let Some(obj) = obj_weak.upgrade() {
                        obj.imp().handle_sip_event(account_id.clone(), event);
                    }
                },
            );

            engine.register(crate::sip::SipConfig {
                server: account.server.clone(),
                username: account.username.clone(),
                password: crate::keyring::load_for(&account.id).unwrap_or_default(),
                display_name: account.display_name.clone(),
                port: account.port,
            });

            self.active_engines.borrow_mut().push(ActiveEngine {
                account_id: account.id.clone(),
                engine,
                registered: false,
                last_register_ok: Some(now_unix()),
            });

            self.start_keepalive_timer();
        }

        pub fn disconnect_account(&self, account_id: &str) {
            self.active_engines
                .borrow_mut()
                .retain(|e| e.account_id != account_id);
            self.refresh_dialpad_accounts();
        }

        pub fn connect_account_by_id(&self, account_id: &str) {
            self.disconnect_account(account_id);
            let accounts = crate::accounts::load();
            if let Some(acc) = accounts.iter().find(|a| a.id == account_id) {
                self.connect_account(acc);
            }
        }

        pub fn reconnect_all(&self) {
            let ids: Vec<String> = self
                .active_engines
                .borrow()
                .iter()
                .map(|e| e.account_id.clone())
                .collect();
            self.active_engines.borrow_mut().clear();
            let accounts = crate::accounts::load();
            for id in ids {
                if let Some(acc) = accounts.iter().find(|a| a.id == id) {
                    self.connect_account(acc);
                }
            }
        }

        fn start_keepalive_timer(&self) {
            if self.keepalive_timer.borrow().is_some() {
                return;
            }
            let obj_weak = self.obj().downgrade();
            let id = glib::timeout_add_seconds_local(40, move || {
                let Some(obj) = obj_weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let imp = obj.imp();

                // Lightweight REGISTER refresh for all active engines.
                for entry in imp.active_engines.borrow().iter() {
                    entry.engine.reregister();
                }

                // Full reconnect for any engine that hasn't confirmed in 180 s.
                // Skip if a call is in progress (audio_session or active_account_id
                // set) to avoid destroying the engine mid-auth-retry.
                if imp.audio_session.borrow().is_none()
                    && imp.active_account_id.borrow().is_none()
                {
                    let stale: Vec<String> = imp
                        .active_engines
                        .borrow()
                        .iter()
                        .filter(|e| {
                            e.last_register_ok
                                .map(|t| now_unix() - t > 180)
                                .unwrap_or(false)
                        })
                        .map(|e| e.account_id.clone())
                        .collect();
                    for id in stale {
                        imp.connect_account_by_id(&id);
                    }
                }

                glib::ControlFlow::Continue
            });
            *self.keepalive_timer.borrow_mut() = Some(id);
        }

        fn refresh_dialpad_accounts(&self) {
            let Some(dialpad) = self.dialpad.get() else {
                return;
            };
            let accounts = crate::accounts::load();
            let registered: Vec<(String, String)> = self
                .active_engines
                .borrow()
                .iter()
                .filter(|e| e.registered)
                .filter_map(|e| {
                    accounts
                        .iter()
                        .find(|a| a.id == e.account_id)
                        .map(|a| (a.id.clone(), a.label()))
                })
                .collect();
            dialpad.set_registered_accounts(registered);
        }

        // ── SIP event handler ─────────────────────────────────────────────────

        pub fn handle_sip_event(&self, account_id: String, event: SipEvent) {
            match event {
                SipEvent::Registered => {
                    let is_first = {
                        let mut engines = self.active_engines.borrow_mut();
                        if let Some(entry) = engines.iter_mut().find(|e| e.account_id == account_id) {
                            let was = entry.registered;
                            entry.registered = true;
                            entry.last_register_ok = Some(now_unix());
                            !was
                        } else {
                            false
                        }
                    };

                    // Hide the banner if all engines are now happy.
                    let all_ok = self
                        .active_engines
                        .borrow()
                        .iter()
                        .all(|e| e.registered);
                    if all_ok {
                        self.status_banner.set_revealed(false);
                    }

                    if is_first {
                        let accounts = crate::accounts::load();
                        if let Some(acc) = accounts.iter().find(|a| a.id == account_id) {
                            let toast = adw::Toast::new(&format!(
                                "Registered as {}@{}",
                                acc.username, acc.server
                            ));
                            toast.set_timeout(4);
                            self.toast_overlay.add_toast(toast);
                        }
                        self.refresh_dialpad_accounts();
                    }
                }

                SipEvent::RegistrationFailed(reason) => {
                    {
                        let mut engines = self.active_engines.borrow_mut();
                        if let Some(entry) = engines.iter_mut().find(|e| e.account_id == account_id) {
                            entry.registered = false;
                        }
                    }
                    let accounts = crate::accounts::load();
                    let label = accounts
                        .iter()
                        .find(|a| a.id == account_id)
                        .map(|a| a.label())
                        .unwrap_or_else(|| account_id.clone());
                    self.status_banner
                        .set_title(&format!("{label}: Registration failed: {reason}"));
                    self.status_banner.set_button_label(Some("Reconnect"));
                    self.status_banner.set_revealed(true);
                    self.refresh_dialpad_accounts();
                }

                SipEvent::IncomingCall { from } => {
                    *self.active_account_id.borrow_mut() = Some(account_id);
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
                    *self.active_account_id.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() {
                        cs.stop_timer();
                    }
                    self.show_call_screen(false);
                    if let Some(dialpad) = self.dialpad.get() {
                        dialpad.clear();
                    }
                    self.finalize_pending_call(false);
                }

                SipEvent::CallFailed(reason) => {
                    *self.ringer.borrow_mut() = None;
                    *self.audio_session.borrow_mut() = None;
                    *self.consult_session.borrow_mut() = None;
                    *self.active_account_id.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() {
                        cs.stop_timer();
                    }
                    self.show_call_screen(false);
                    self.toast_overlay
                        .add_toast(error_toast(&format!("Call failed: {reason}")));
                    self.finalize_pending_call(true);
                }

                SipEvent::TransferOk => {
                    *self.ringer.borrow_mut() = None;
                    *self.audio_session.borrow_mut() = None;
                    *self.consult_session.borrow_mut() = None;
                    *self.active_account_id.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() {
                        cs.stop_timer();
                    }
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
                    if let Some(session) = self.audio_session.borrow().as_ref() {
                        session.set_hold(false);
                    }
                }
            }
        }

        // ── Call actions ──────────────────────────────────────────────────────

        fn show_call_screen(&self, visible: bool) {
            self.call_revealer.set_reveal_child(visible);
            self.call_revealer.set_can_target(visible);
        }

        pub fn start_call(&self, number: &str, account_id: &str) {
            // Find the right engine: explicit account first, else first registered.
            let chosen_id = {
                let engines = self.active_engines.borrow();
                if !account_id.is_empty() {
                    engines
                        .iter()
                        .find(|e| e.account_id == account_id && e.registered)
                        .map(|e| e.account_id.clone())
                } else {
                    engines
                        .iter()
                        .find(|e| e.registered)
                        .map(|e| e.account_id.clone())
                }
            };

            let Some(id) = chosen_id else {
                let toast =
                    adw::Toast::new("No registered account — configure SIP account first");
                self.toast_overlay.add_toast(toast);
                return;
            };

            *self.active_account_id.borrow_mut() = Some(id.clone());
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

            let engines = self.active_engines.borrow();
            if let Some(entry) = engines.iter().find(|e| e.account_id == id) {
                entry.engine.make_call(number);
            }
        }

        fn answer_call(&self) {
            self.with_active_engine(|e| e.answer_call());
        }

        fn hangup_call(&self) {
            self.with_active_engine(|e| e.hangup());
        }

        // ── Call log ─────────────────────────────────────────────────────────

        fn finalize_pending_call(&self, outgoing_failed: bool) {
            let Some(pending) = self.pending_call.borrow_mut().take() else {
                return;
            };
            let now = now_unix();
            let (status, duration) = match pending.connected_at {
                Some(t) => (call_log::Status::Answered, (now - t).max(0) as u32),
                None => {
                    let status = if pending.direction == call_log::Direction::Incoming {
                        call_log::Status::Missed
                    } else if outgoing_failed {
                        call_log::Status::Failed
                    } else {
                        call_log::Status::Failed
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
                (Direction::Incoming, Status::Answered) => ("call-incoming-symbolic", "success"),
                (Direction::Incoming, _) => ("call-missed-symbolic", "error"),
                (Direction::Outgoing, Status::Answered) => ("call-outgoing-symbolic", "accent"),
                (Direction::Outgoing, _) => ("call-outgoing-symbolic", "dim-label"),
            };

            let icon = gtk4::Image::from_icon_name(icon_name);
            icon.add_css_class(icon_css);
            icon.set_pixel_size(16);
            icon.set_margin_top(8);
            icon.set_margin_bottom(8);

            let title = call_log::display_name(&record.number);
            let time = call_log::format_time(record.started_at);
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

            let number = call_log::callable(&record.number);
            let weak = self.obj().downgrade();
            row.connect_activated(move |_| {
                if let Some(obj) = weak.upgrade() {
                    obj.imp().start_call(&number, "");
                    obj.imp().view_stack.set_visible_child_name("dialpad");
                }
            });

            row
        }
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}


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
        let registered_ids: Vec<String> = self
            .imp()
            .active_engines
            .borrow()
            .iter()
            .filter(|e| e.registered)
            .map(|e| e.account_id.clone())
            .collect();

        let dialog = SettingsDialog::new(&registered_ids);
        let win = self.clone();

        dialog.connect_local(
            "account-register-toggled",
            false,
            glib::clone!(
                #[weak]
                win,
                #[upgrade_or]
                None,
                move |args| {
                    let account_id = args[1].get::<String>().unwrap_or_default();
                    let should_register = args[2].get::<bool>().unwrap_or(false);
                    if should_register {
                        win.imp().connect_account_by_id(&account_id);
                    } else {
                        win.imp().disconnect_account(&account_id);
                    }
                    None
                }
            ),
        );

        dialog.connect_local(
            "account-reconnect",
            false,
            glib::clone!(
                #[weak]
                win,
                #[upgrade_or]
                None,
                move |args| {
                    let account_id = args[1].get::<String>().unwrap_or_default();
                    win.imp().connect_account_by_id(&account_id);
                    None
                }
            ),
        );

        dialog.connect_local(
            "account-removed",
            false,
            glib::clone!(
                #[weak]
                win,
                #[upgrade_or]
                None,
                move |args| {
                    let account_id = args[1].get::<String>().unwrap_or_default();
                    win.imp().disconnect_account(&account_id);
                    None
                }
            ),
        );

        dialog.present(Some(self));
    }
}
