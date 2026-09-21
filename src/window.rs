use std::cell::{Cell, OnceCell, RefCell};

use gtk4::{gio, glib, prelude::*, subclass::prelude::*, CompositeTemplate};
use libadwaita as adw;
use adw::prelude::*;
use adw::subclass::prelude::*;

use crate::audio::AudioSession;
use crate::call_log;
use crate::ringer::Ringer;
use crate::sip::{SipEngine, SipEvent};
use crate::webrtc::WebrtcSession;
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
        /// Account negotiates WebRTC media, so calls go through
        /// [`WebrtcSession`] and the `*_sdp` engine entry points.
        pub webrtc: bool,
    }

    /// Which side of the SDP exchange this client is on for the current
    /// WebRTC call, which decides what an arriving remote SDP means.
    #[derive(Copy, Clone, PartialEq, Debug)]
    pub enum SdpRole {
        /// We offered; the remote SDP is the answer.
        Offerer,
        /// They offered; the remote SDP is an offer we must answer.
        Answerer,
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
        #[template_child]
        pub account_selector: TemplateChild<gtk4::DropDown>,
        #[template_child]
        pub quickdial_scroll: TemplateChild<gtk4::ScrolledWindow>,
        #[template_child]
        pub quickdial_bar: TemplateChild<gtk4::Box>,
        #[template_child]
        pub quickdial_separator: TemplateChild<gtk4::Separator>,

        /// account_id for each entry in `account_selector` (parallel to its model).
        pub selector_account_ids: RefCell<Vec<String>>,
        /// Guards `account_selector`'s notify handler while we repopulate it, so
        /// programmatic `set_selected` does not clobber the persisted default.
        pub suppress_account_save: Cell<bool>,

        pub dialpad: OnceCell<Dialpad>,
        pub call_screen: OnceCell<CallScreen>,
        pub call_list_box: OnceCell<gtk4::ListBox>,
        pub recents_entry: OnceCell<gtk4::Entry>,

        /// All accounts that have a running SIP engine (registered or registering).
        pub active_engines: RefCell<Vec<ActiveEngine>>,
        /// Which account is handling the current call.
        pub active_account_id: RefCell<Option<String>>,

        pub audio_session: RefCell<Option<AudioSession>>,
        /// Media session for WebRTC accounts, in place of `audio_session`.
        /// `Rc` because the async negotiation tasks outlive the call frame.
        pub webrtc_session: RefCell<Option<std::rc::Rc<WebrtcSession>>>,
        pub sdp_role: Cell<Option<SdpRole>>,
        /// Offer from an incoming WebRTC call, held until the user answers.
        pub pending_remote_offer: RefCell<Option<String>>,
        pub consult_session: RefCell<Option<AudioSession>>,
        pub ringer: RefCell<Option<Ringer>>,
        pub secondary_ringer: RefCell<Option<Ringer>>,
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

            // ── Recents tab (page 1): quick-dial entry + call log ─────────────

            let recents_entry = gtk4::Entry::builder()
                .hexpand(true)
                .placeholder_text("Enter number…")
                .input_purpose(gtk4::InputPurpose::Phone)
                .xalign(0.5)
                .build();
            recents_entry.add_css_class("title-2");

            let recents_call_btn = gtk4::Button::builder()
                .icon_name("call-start-symbolic")
                .tooltip_text("Call")
                .build();
            recents_call_btn.add_css_class("circular");
            recents_call_btn.add_css_class("suggested-action");
            recents_call_btn.add_css_class("dialpad-call");

            let entry_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            entry_row.set_margin_top(12);
            entry_row.set_margin_start(12);
            entry_row.set_margin_end(12);
            entry_row.append(&recents_entry);
            entry_row.append(&recents_call_btn);

            recents_entry.connect_activate(glib::clone!(
                #[weak]
                obj,
                move |entry| {
                    let number = entry.text().to_string();
                    if !number.is_empty() {
                        obj.imp().start_call(&number, "");
                        entry.set_text("");
                    }
                }
            ));
            recents_call_btn.connect_clicked(glib::clone!(
                #[weak]
                obj,
                #[weak]
                recents_entry,
                move |_| {
                    let number = recents_entry.text().to_string();
                    if !number.is_empty() {
                        obj.imp().start_call(&number, "");
                        recents_entry.set_text("");
                    }
                }
            ));

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

            let recents_page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            recents_page.append(&entry_row);
            recents_page.append(&recents_scroll);

            self.view_stack.add_titled_with_icon(
                &recents_page,
                Some("recents"),
                "Recents",
                "recent-activity-symbolic",
            );
            self.call_list_box.set(list_box.clone()).unwrap();
            self.recents_entry.set(recents_entry).unwrap();

            let log = call_log::CallLog::load();
            for record in &log.records {
                list_box.append(&self.make_call_row(record));
            }
            *self.call_log.borrow_mut() = log;

            // ── Dial tab (page 2): full dialpad ───────────────────────────────

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

            // ── Quickdial bar ─────────────────────────────────────────────────

            self.refresh_quickdials();

            // ── Header-bar account selector ───────────────────────────────────
            // Persist the chosen outgoing account whenever the user changes it.
            self.account_selector.connect_selected_notify(glib::clone!(
                #[weak]
                obj,
                move |_| {
                    let imp = obj.imp();
                    if imp.suppress_account_save.get() {
                        return;
                    }
                    if let Some(id) = imp.selected_outgoing_account_id() {
                        let settings = gio::Settings::new("io.github.thomaswasle.TMWPhone");
                        let _ = settings.set_string("default-account", &id);
                    }
                }
            ));

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
                        // Mute is local audio only (no SIP signaling).  Apply to
                        // whichever sessions exist so it also works while a
                        // consultation leg is active.
                        let imp = obj.imp();
                        if let Some(session) = imp.audio_session.borrow().as_ref() {
                            session.set_muted(muted);
                        }
                        if let Some(session) = imp.consult_session.borrow().as_ref() {
                            session.set_muted(muted);
                        }
                        if let Some(session) = imp.webrtc_session.borrow().as_ref() {
                            session.set_muted(muted);
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
                        let imp = obj.imp();
                        imp.with_active_engine(|e| e.set_hold(hold));
                        if let Some(session) = imp.audio_session.borrow().as_ref() {
                            session.set_hold(hold);
                        }
                        // WebRTC: silence the mic now, then re-INVITE with a
                        // renegotiated direction once webrtcbin has an offer.
                        // glue.c's sofia_set_hold only records the flag here.
                        if let Some(session) = imp.webrtc_session.borrow().clone() {
                            session.set_hold(hold);
                            let obj = obj.downgrade();
                            glib::MainContext::default().spawn_local(async move {
                                let result = session.renegotiate_hold(hold).await;
                                let Some(obj) = obj.upgrade() else { return };
                                let imp = obj.imp();
                                match result {
                                    Ok(sdp) => imp.with_active_engine(|e| e.reinvite_sdp(&sdp)),
                                    // Hold is not worth dropping a live call
                                    // over: the mic is already silenced, so the
                                    // user still gets privacy, just without the
                                    // peer's music-on-hold.
                                    Err(e) => {
                                        log::error!("webrtc: hold renegotiation: {e}");
                                        imp.toast_overlay.add_toast(adw::Toast::new(
                                            "Hold: the other side was not notified",
                                        ));
                                    }
                                }
                            });
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
                        let imp = obj.imp();
                        // A consultation leg is a second, independent media
                        // session; under WebRTC that means a second webrtcbin
                        // with its own DTLS/ICE negotiation, which is not
                        // built yet. Refuse plainly instead of dialling a leg
                        // that would carry plain-RTP SDP into a DTLS-SRTP
                        // account and connect with no audio. Blind transfer
                        // uses REFER and no SDP, so it still works.
                        if imp.active_is_webrtc() {
                            imp.toast_overlay.add_toast(adw::Toast::new(
                                "Attended transfer is not available on WebRTC                                  accounts — use blind transfer",
                            ));
                            return None;
                        }
                        imp.with_active_engine(|e| e.start_consultation(&number));
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
        ///
        /// INVARIANT: `f` runs while `active_engines` is borrowed. Some engine
        /// methods (e.g. `answer_call`, `cancel_consultation`) cause the C layer
        /// to fire a SIP event callback *synchronously*, which re-enters
        /// `handle_sip_event` on this same stack. Those handlers therefore must
        /// NOT borrow `active_engines` (mut or otherwise) — doing so would panic
        /// with a RefCell double-borrow. Handlers reached this way today only
        /// touch other fields (audio_session, ringer, call_screen), which is why
        /// it is safe; keep it that way.
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

        /// Run `f` against a specific account's engine. Used by the async
        /// WebRTC negotiation tasks, which resume after the original borrow of
        /// `active_engines` is long gone.
        fn with_engine<F: FnOnce(&SipEngine)>(&self, account_id: &str, f: F) {
            let engines = self.active_engines.borrow();
            if let Some(entry) = engines.iter().find(|e| e.account_id == account_id) {
                f(&entry.engine);
            }
        }

        fn active_is_webrtc(&self) -> bool {
            let Some(id) = self.active_account_id.borrow().clone() else { return false };
            self.active_engines
                .borrow()
                .iter()
                .any(|e| e.account_id == id && e.webrtc)
        }

        /// Drop the WebRTC media session and any half-finished negotiation.
        fn clear_webrtc(&self) {
            *self.webrtc_session.borrow_mut() = None;
            *self.pending_remote_offer.borrow_mut() = None;
            self.sdp_role.set(None);
        }

        /// Report a negotiation failure and tear the call down — a WebRTC call
        /// whose SDP exchange failed can never carry audio, so leaving it up
        /// would present a connected call that is silent.
        fn fail_webrtc(&self, what: &str, err: &str) {
            log::error!("webrtc: {what}: {err}");
            self.toast_overlay
                .add_toast(adw::Toast::new(&format!("Media setup failed — {err}")));
            self.with_active_engine(|e| e.hangup());
            self.clear_webrtc();
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
            // WebSocket accounts need bridge configuration; the native
            // transports pass None and sofia handles the socket itself.
            let ws = account.transport.is_websocket().then(|| crate::sip::WsConfig {
                host: account.server.clone(),
                port: account.port,
                path: account.ws_path_or_default().to_string(),
                secure: account.transport == crate::accounts::Transport::Wss,
                tls_verify: account.tls_verify,
                tls_ca_file: account.tls_ca_file.clone(),
            });
            let engine = SipEngine::new(
                &account.server,
                account.port,
                &account.proxy,
                account.transport.as_c_int(),
                account.tls_verify,
                &account.tls_ca_file,
                ws,
                move |event| {
                    if let Some(obj) = obj_weak.upgrade() {
                        obj.imp().handle_sip_event(account_id.clone(), event);
                    }
                },
            );

            // Must be set before the first call: it switches glue.c from
            // building SDP itself to carrying webrtcbin's verbatim.
            engine.set_webrtc(account.webrtc);

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
                webrtc: account.webrtc,
            });

            self.start_keepalive_timer();
        }

        pub fn disconnect_account(&self, account_id: &str) {
            self.active_engines
                .borrow_mut()
                .retain(|e| e.account_id != account_id);
            self.refresh_account_selector();
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

        /// Repopulate the header-bar account selector with the currently
        /// registered accounts and restore the persisted default selection.
        /// Hidden when ≤ 1 account is registered.
        fn refresh_account_selector(&self) {
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

            let labels: Vec<&str> = registered.iter().map(|(_, l)| l.as_str()).collect();
            let model = gtk4::StringList::new(&labels);
            let ids: Vec<String> = registered.into_iter().map(|(id, _)| id).collect();

            // Restore the persisted default if it is still registered, else index 0.
            let settings = gio::Settings::new("io.github.thomaswasle.TMWPhone");
            let saved = settings.string("default-account");
            let selected = ids
                .iter()
                .position(|id| id == saved.as_str())
                .unwrap_or(0) as u32;

            self.suppress_account_save.set(true);
            self.account_selector.set_model(Some(&model));
            self.account_selector.set_selected(selected);
            self.account_selector.set_visible(ids.len() > 1);
            *self.selector_account_ids.borrow_mut() = ids;
            self.suppress_account_save.set(false);
        }

        /// Rebuild the always-visible quickdial sidebar from the saved entries.
        /// Each button dials its number immediately. The sidebar (and its
        /// separator) is hidden when no quickdials are configured. Called on
        /// startup and after the settings dialog (where quickdials are edited)
        /// closes.
        pub fn refresh_quickdials(&self) {
            let bar = self.quickdial_bar.get();
            while let Some(child) = bar.first_child() {
                bar.remove(&child);
            }

            let entries = crate::quickdial::load();
            for entry in &entries {
                if entry.number.is_empty() {
                    continue;
                }
                let button = gtk4::Button::with_label(&entry.display_label());
                button.set_tooltip_text(Some(&entry.number));
                button.set_hexpand(true);
                // Ellipsize long labels rather than widening the sidebar.
                if let Some(label) = button.child().and_downcast::<gtk4::Label>() {
                    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                }
                let number = entry.number.clone();
                let weak = self.obj().downgrade();
                button.connect_clicked(move |_| {
                    if let Some(obj) = weak.upgrade() {
                        obj.imp().start_call(&number, "");
                    }
                });
                bar.append(&button);
            }

            let has_entries = bar.first_child().is_some();
            self.quickdial_scroll.set_visible(has_entries);
            self.quickdial_separator.set_visible(has_entries);
        }

        /// The account_id currently chosen in the header-bar selector, if any.
        fn selected_outgoing_account_id(&self) -> Option<String> {
            let ids = self.selector_account_ids.borrow();
            ids.get(self.account_selector.selected() as usize).cloned()
        }

        // ── SIP event handler ─────────────────────────────────────────────────

        pub fn handle_sip_event(&self, account_id: String, event: SipEvent) {
            match event {
                // ── WebRTC media negotiation ─────────────────────────────
                SipEvent::RemoteSdp(sdp) => {
                    let Some(session) = self.webrtc_session.borrow().clone() else {
                        log::warn!("remote SDP with no media session — ignoring");
                        return;
                    };
                    match self.sdp_role.get() {
                        // Our INVITE was answered: apply it and start media.
                        Some(SdpRole::Offerer) => {
                            if let Err(e) = session.apply_answer(&sdp) {
                                self.fail_webrtc("answer", &e);
                            }
                        }
                        // An incoming call's offer. Hold it until the user
                        // answers — answering is what commits us to a codec.
                        Some(SdpRole::Answerer) => {
                            *self.pending_remote_offer.borrow_mut() = Some(sdp);
                        }
                        None => log::warn!("remote SDP outside a WebRTC call — ignoring"),
                    }
                }
                SipEvent::ReinviteSdp(sdp) => {
                    // glue.c deferred the 200 OK: webrtcbin has to renegotiate
                    // before we can answer.
                    let Some(session) = self.webrtc_session.borrow().clone() else {
                        log::warn!("re-INVITE with no media session — ignoring");
                        return;
                    };
                    let obj = self.obj().downgrade();
                    glib::MainContext::default().spawn_local(async move {
                        let result = session.answer_reinvite(&sdp).await;
                        let Some(obj) = obj.upgrade() else { return };
                        let imp = obj.imp();
                        match result {
                            Ok(answer) => imp.with_active_engine(|e| e.respond_sdp(&answer)),
                            Err(e) => imp.fail_webrtc("re-INVITE", &e),
                        }
                    });
                }
                SipEvent::ConsultRemoteSdp(_) => {
                    // Attended transfer needs a second, independent webrtcbin
                    // session; it is refused up front for WebRTC accounts, so
                    // reaching here means a consult leg was started anyway.
                    log::warn!("consultation SDP on a WebRTC account — not supported");
                }
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
                        self.refresh_account_selector();
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
                    self.refresh_account_selector();
                }

                SipEvent::IncomingCall { from } => {
                    // Create the media session before the offer arrives — it
                    // follows immediately after this event.
                    let is_webrtc = self
                        .active_engines
                        .borrow()
                        .iter()
                        .any(|e| e.account_id == account_id && e.webrtc);
                    if is_webrtc {
                        self.clear_webrtc();
                        match WebrtcSession::new() {
                            Ok(s) => {
                                *self.webrtc_session.borrow_mut() = Some(std::rc::Rc::new(s));
                                self.sdp_role.set(Some(SdpRole::Answerer));
                            }
                            Err(e) => log::error!("webrtc: incoming call media: {e}"),
                        }
                    }
                    *self.active_account_id.borrow_mut() = Some(account_id);
                    *self.primary_caller.borrow_mut() = from.clone();
                    if let Some(cs) = self.call_screen.get() {
                        cs.set_caller(&call_log::display_name(&from));
                        cs.set_duration("Incoming call…");
                        cs.show_answer_button(true);
                    }
                    self.show_call_screen(true);
                    *self.ringer.borrow_mut() = Ringer::start_incoming(None);
                    {
                        let settings = gio::Settings::new("io.github.thomaswasle.TMWPhone");
                        let name = settings.string("ringer-output-device");
                        if !name.is_empty() {
                            let dev = crate::ringer::enumerate_output_devices()
                                .into_iter()
                                // UFCS: avoid ambiguity with gtk4::prelude::AppInfoExt::display_name.
                                .find(|d| gstreamer::prelude::DeviceExt::display_name(d).as_str() == name.as_str());
                            *self.secondary_ringer.borrow_mut() =
                                dev.as_ref().and_then(|d| Ringer::start_incoming(Some(d)));
                        }
                    }
                    *self.pending_call.borrow_mut() = Some(PendingCall {
                        direction: call_log::Direction::Incoming,
                        number: from,
                        started_at: now_unix(),
                        connected_at: None,
                    });
                }

                SipEvent::CallConnected => {
                    *self.ringer.borrow_mut() = None;
                    *self.secondary_ringer.borrow_mut() = None;
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
                    *self.secondary_ringer.borrow_mut() = None;
                    *self.audio_session.borrow_mut() = None;
                    *self.consult_session.borrow_mut() = None;
                    self.clear_webrtc();
                    *self.active_account_id.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() {
                        cs.stop_timer();
                    }
                    self.show_call_screen(false);
                    if let Some(dialpad) = self.dialpad.get() {
                        dialpad.clear();
                    }
                    if let Some(entry) = self.recents_entry.get() {
                        entry.set_text("");
                    }
                    self.finalize_pending_call();
                }

                SipEvent::CallFailed(reason) => {
                    *self.ringer.borrow_mut() = None;
                    *self.secondary_ringer.borrow_mut() = None;
                    *self.audio_session.borrow_mut() = None;
                    *self.consult_session.borrow_mut() = None;
                    self.clear_webrtc();
                    *self.active_account_id.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() {
                        cs.stop_timer();
                    }
                    self.show_call_screen(false);
                    self.toast_overlay
                        .add_toast(error_toast(&format!("Call failed: {reason}")));
                    self.finalize_pending_call();
                }

                SipEvent::TransferOk => {
                    *self.ringer.borrow_mut() = None;
                    *self.secondary_ringer.borrow_mut() = None;
                    *self.audio_session.borrow_mut() = None;
                    *self.consult_session.borrow_mut() = None;
                    self.clear_webrtc();
                    *self.active_account_id.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() {
                        cs.stop_timer();
                    }
                    self.show_call_screen(false);
                    self.finalize_pending_call();
                    let toast = adw::Toast::new("Call transferred successfully");
                    toast.set_timeout(4);
                    self.toast_overlay.add_toast(toast);
                }

                SipEvent::TransferFailed(reason) => {
                    // The transfer was rejected. For an attended transfer the
                    // consultation leg is still up and the primary call is still
                    // on hold (complete_transfer leaves both legs for the server
                    // to tear down on success). Cancel the consultation to return
                    // the user cleanly to the original party — otherwise the
                    // consult leg would orphan if they hang up next. This is a
                    // no-op for a blind transfer (no consult leg), leaving the
                    // call untouched. cancel_consultation fires CONSULT_ENDED
                    // synchronously, re-entering handle_sip_event to exit consult
                    // mode and unhold; that handler must not borrow
                    // active_engines (see with_active_engine).
                    self.with_active_engine(|e| e.cancel_consultation());
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
            // Resolve the outgoing engine: an explicit account_id wins; otherwise
            // use the header-bar selection; otherwise fall back to first registered.
            let is_registered = |id: &str| {
                self.active_engines
                    .borrow()
                    .iter()
                    .any(|e| e.account_id == id && e.registered)
            };
            let chosen_id = if !account_id.is_empty() && is_registered(account_id) {
                Some(account_id.to_string())
            } else {
                self.selected_outgoing_account_id()
                    .filter(|id| is_registered(id))
                    .or_else(|| {
                        self.active_engines
                            .borrow()
                            .iter()
                            .find(|e| e.registered)
                            .map(|e| e.account_id.clone())
                    })
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
            *self.ringer.borrow_mut() = Ringer::start_ringback(None);
            *self.pending_call.borrow_mut() = Some(PendingCall {
                direction: call_log::Direction::Outgoing,
                number: number.to_owned(),
                started_at: now_unix(),
                connected_at: None,
            });

            let uses_webrtc = self
                .active_engines
                .borrow()
                .iter()
                .any(|e| e.account_id == id && e.webrtc);

            if !uses_webrtc {
                self.with_engine(&id, |e| e.make_call(number));
                return;
            }

            // WebRTC: the INVITE cannot be sent until webrtcbin has produced an
            // offer and gathered ICE candidates, so dialling becomes async.
            let session = match WebrtcSession::new() {
                Ok(s) => std::rc::Rc::new(s),
                Err(e) => {
                    self.fail_webrtc("could not start media", &e);
                    return;
                }
            };
            *self.webrtc_session.borrow_mut() = Some(session.clone());
            self.sdp_role.set(Some(SdpRole::Offerer));

            let obj = self.obj().downgrade();
            let number = number.to_owned();
            glib::MainContext::default().spawn_local(async move {
                let result = session.create_offer().await;
                let Some(obj) = obj.upgrade() else { return };
                let imp = obj.imp();
                // The user may have hung up while ICE was gathering.
                if imp.webrtc_session.borrow().is_none() {
                    return;
                }
                match result {
                    Ok(sdp) => imp.with_engine(&id, |e| e.make_call_sdp(&number, &sdp)),
                    Err(e) => imp.fail_webrtc("offer", &e),
                }
            });
        }

        fn answer_call(&self) {
            if !self.active_is_webrtc() {
                self.with_active_engine(|e| e.answer_call());
                return;
            }

            let (Some(session), Some(offer)) = (
                self.webrtc_session.borrow().clone(),
                self.pending_remote_offer.borrow().clone(),
            ) else {
                self.fail_webrtc("answer", "no offer received from the caller");
                return;
            };

            let obj = self.obj().downgrade();
            glib::MainContext::default().spawn_local(async move {
                let result = session.create_answer(&offer).await;
                let Some(obj) = obj.upgrade() else { return };
                let imp = obj.imp();
                if imp.webrtc_session.borrow().is_none() {
                    return;
                }
                match result {
                    Ok(sdp) => imp.with_active_engine(|e| e.answer_call_sdp(&sdp)),
                    Err(e) => imp.fail_webrtc("answer", &e),
                }
            });
        }

        fn hangup_call(&self) {
            self.with_active_engine(|e| e.hangup());
        }

        // ── Call log ─────────────────────────────────────────────────────────

        fn finalize_pending_call(&self) {
            let Some(pending) = self.pending_call.borrow_mut().take() else {
                return;
            };
            let now = now_unix();
            let (status, duration) = match pending.connected_at {
                Some(t) => (call_log::Status::Answered, (now - t).max(0) as u32),
                // Never connected: incoming → missed, outgoing → failed
                // (covers reject, no-answer, and user cancel alike — the log
                // has no separate "cancelled" status).
                None => {
                    let status = if pending.direction == call_log::Direction::Incoming {
                        call_log::Status::Missed
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

        // Quickdials are edited in the dialog; rebuild the bar once it closes.
        dialog.connect_closed(glib::clone!(
            #[weak]
            win,
            move |_| win.imp().refresh_quickdials()
        ));

        dialog.present(Some(self));
    }
}
