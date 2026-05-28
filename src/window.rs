use std::cell::{OnceCell, RefCell};

use gtk4::{gio, glib, prelude::*, subclass::prelude::*, CompositeTemplate};
use libadwaita as adw;
use adw::prelude::*;
use adw::subclass::prelude::*;

use crate::audio::AudioSession;
use crate::sip::{SipEngine, SipEvent};
use crate::widgets::{CallScreen, Dialpad, SettingsDialog};

mod imp {
    use super::*;

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
        pub sip_engine: RefCell<Option<SipEngine>>,
        pub audio_session: RefCell<Option<AudioSession>>,
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
            self.call_screen.set(call_screen).unwrap();

            // Banner button: "Configure" opens settings; "Copy" copies the error text.
            self.status_banner.connect_button_clicked(glib::clone!(
                #[weak]
                obj,
                move |banner| {
                    if banner.button_label().as_deref() == Some("Copy") {
                        if let Some(display) = banner.display().downcast::<gtk4::gdk::Display>().ok() {
                            display.clipboard().set_text(banner.title().as_str());
                        }
                    } else {
                        obj.open_settings_dialog();
                    }
                }
            ));

            // Update banner based on current settings
            let settings = gio::Settings::new("net.loca.TMWPhone");
            if settings.string("sip-username").is_empty() {
                self.status_banner.set_title("Not registered — tap Configure");
            }
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
                    let settings = gio::Settings::new("net.loca.TMWPhone");
                    let user = settings.string("sip-username");
                    let server = settings.string("sip-server");
                    self.status_banner.set_revealed(false);
                    let toast = adw::Toast::new(&format!("Registered as {user}@{server}"));
                    toast.set_timeout(4);
                    self.toast_overlay.add_toast(toast);
                }
                SipEvent::RegistrationFailed(reason) => {
                    self.status_banner
                        .set_title(&format!("Registration failed: {reason}"));
                    self.status_banner.set_button_label(Some("Copy"));
                    self.status_banner.set_revealed(true);
                }
                SipEvent::IncomingCall { from } => {
                    if let Some(cs) = self.call_screen.get() {
                        cs.set_caller(&from);
                        cs.set_duration("Incoming call…");
                        cs.show_answer_button(true);
                    }
                    self.show_call_screen(true);
                }
                SipEvent::CallConnected => {
                    if let Some(cs) = self.call_screen.get() {
                        cs.show_answer_button(false);
                        cs.start_timer();
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
                    *self.audio_session.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() { cs.stop_timer(); }
                    self.show_call_screen(false);
                    if let Some(dialpad) = self.dialpad.get() {
                        dialpad.clear();
                    }
                }
                SipEvent::CallFailed(reason) => {
                    *self.audio_session.borrow_mut() = None;
                    if let Some(cs) = self.call_screen.get() { cs.stop_timer(); }
                    self.show_call_screen(false);
                    self.toast_overlay
                        .add_toast(error_toast(&format!("Call failed: {reason}")));
                }
            }
        }

        fn show_call_screen(&self, visible: bool) {
            self.call_revealer.set_reveal_child(visible);
            self.call_revealer.set_can_target(visible);
        }

        pub fn start_call(&self, number: &str) {
            if let Some(engine) = self.sip_engine.borrow().as_ref() {
                if let Some(cs) = self.call_screen.get() {
                    cs.set_caller(number);
                    cs.set_duration("Calling…");
                    cs.show_answer_button(false);
                }
                self.show_call_screen(true);
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
                password: settings.string("sip-password").into(),
                display_name: settings.string("sip-display-name").into(),
                port,
            });

            self.status_banner.set_title("Registering…");
            self.status_banner.set_button_label(None::<&str>);
            self.status_banner.set_revealed(true);

            *self.sip_engine.borrow_mut() = Some(engine);
        }
    }
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
