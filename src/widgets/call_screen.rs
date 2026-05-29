use gtk4::{glib, prelude::*, subclass::prelude::*, CompositeTemplate};
use libadwaita as adw;
use adw::subclass::prelude::*;
use std::cell::Cell;
use std::time::Instant;

mod imp {
    use super::*;
    use std::cell::RefCell;
    use std::sync::OnceLock;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(file = "../../data/ui/call_screen.ui")]
    pub struct CallScreen {
        #[template_child]
        pub caller_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub duration_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub held_label: TemplateChild<gtk4::Label>,
        #[template_child]
        pub mute_button: TemplateChild<gtk4::ToggleButton>,
        #[template_child]
        pub hold_button: TemplateChild<gtk4::ToggleButton>,
        #[template_child]
        pub dtmf_button: TemplateChild<gtk4::ToggleButton>,
        #[template_child]
        pub dtmf_revealer: TemplateChild<gtk4::Revealer>,
        #[template_child]
        pub transfer_button: TemplateChild<gtk4::ToggleButton>,
        #[template_child]
        pub transfer_entry: TemplateChild<gtk4::Entry>,
        #[template_child]
        pub transfer_revealer: TemplateChild<gtk4::Revealer>,
        #[template_child]
        pub consult_revealer: TemplateChild<gtk4::Revealer>,
        #[template_child]
        pub answer_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub hangup_button: TemplateChild<gtk4::Button>,

        pub call_start: Cell<Option<Instant>>,
        pub timer_id: RefCell<Option<glib::SourceId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CallScreen {
        const NAME: &'static str = "CallScreen";
        type Type = super::CallScreen;
        type ParentType = adw::Bin;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for CallScreen {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceLock<Vec<glib::subclass::Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    glib::subclass::Signal::builder("answer-clicked").build(),
                    glib::subclass::Signal::builder("hangup-clicked").build(),
                    glib::subclass::Signal::builder("mute-toggled")
                        .param_types([bool::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("hold-toggled")
                        .param_types([bool::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("dtmf-digit")
                        .param_types([String::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("transfer-blind-requested")
                        .param_types([String::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("consult-requested")
                        .param_types([String::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("transfer-complete-requested").build(),
                    glib::subclass::Signal::builder("consult-cancel-requested").build(),
                ]
            })
        }
    }

    impl WidgetImpl for CallScreen {}
    impl BinImpl for CallScreen {}

    #[gtk4::template_callbacks]
    impl CallScreen {
        #[template_callback]
        fn on_answer_clicked(&self, _button: &gtk4::Button) {
            self.obj().emit_by_name::<()>("answer-clicked", &[]);
        }

        #[template_callback]
        fn on_hangup_clicked(&self, _button: &gtk4::Button) {
            self.obj().emit_by_name::<()>("hangup-clicked", &[]);
        }

        #[template_callback]
        fn on_mute_toggled(&self, button: &gtk4::ToggleButton) {
            self.obj()
                .emit_by_name::<()>("mute-toggled", &[&button.is_active()]);
        }

        #[template_callback]
        fn on_hold_toggled(&self, button: &gtk4::ToggleButton) {
            self.obj()
                .emit_by_name::<()>("hold-toggled", &[&button.is_active()]);
        }

        #[template_callback]
        fn on_dtmf_toggled(&self, button: &gtk4::ToggleButton) {
            self.dtmf_revealer.set_reveal_child(button.is_active());
        }

        #[template_callback]
        fn on_dtmf_key_clicked(&self, button: &gtk4::Button) {
            if let Some(label) = button.label() {
                self.obj()
                    .emit_by_name::<()>("dtmf-digit", &[&label.as_str()]);
            }
        }

        #[template_callback]
        fn on_transfer_toggled(&self, button: &gtk4::ToggleButton) {
            self.transfer_revealer.set_reveal_child(button.is_active());
            if !button.is_active() {
                self.transfer_entry.set_text("");
            }
        }

        #[template_callback]
        fn on_blind_transfer_clicked(&self, _button: &gtk4::Button) {
            let number = self.transfer_entry.text().to_string();
            if !number.is_empty() {
                self.obj().emit_by_name::<()>("transfer-blind-requested", &[&number]);
                self.transfer_button.set_active(false);
            }
        }

        #[template_callback]
        fn on_consult_clicked(&self, _button: &gtk4::Button) {
            let number = self.transfer_entry.text().to_string();
            if !number.is_empty() {
                self.obj().emit_by_name::<()>("consult-requested", &[&number]);
                self.transfer_button.set_active(false);
            }
        }

        #[template_callback]
        fn on_complete_transfer_clicked(&self, _button: &gtk4::Button) {
            self.obj().emit_by_name::<()>("transfer-complete-requested", &[]);
        }

        #[template_callback]
        fn on_cancel_consult_clicked(&self, _button: &gtk4::Button) {
            self.obj().emit_by_name::<()>("consult-cancel-requested", &[]);
        }
    }
}

glib::wrapper! {
    pub struct CallScreen(ObjectSubclass<imp::CallScreen>)
        @extends adw::Bin, gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl CallScreen {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_caller(&self, name: &str) {
        self.imp().caller_label.set_label(name);
    }

    pub fn set_duration(&self, text: &str) {
        self.imp().duration_label.set_label(text);
    }

    pub fn show_answer_button(&self, visible: bool) {
        self.imp().answer_button.set_visible(visible);
    }

    pub fn start_timer(&self) {
        let imp = self.imp();
        imp.call_start.set(Some(Instant::now()));

        let weak = self.downgrade();
        let id = glib::timeout_add_seconds_local(1, move || {
            if let Some(cs) = weak.upgrade() {
                let elapsed = cs.imp().call_start.get()
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or(0);
                let h = elapsed / 3600;
                let m = (elapsed % 3600) / 60;
                let s = elapsed % 60;
                let label = if h > 0 {
                    format!("{h}:{m:02}:{s:02}")
                } else {
                    format!("{m:02}:{s:02}")
                };
                cs.imp().duration_label.set_label(&label);
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
        *imp.timer_id.borrow_mut() = Some(id);
    }

    pub fn stop_timer(&self) {
        let imp = self.imp();
        if let Some(id) = imp.timer_id.borrow_mut().take() {
            id.remove();
        }
        imp.call_start.set(None);
        // Reset all toggle buttons and their revealers
        imp.mute_button.set_active(false);
        imp.hold_button.set_active(false);
        imp.dtmf_button.set_active(false);
        imp.dtmf_revealer.set_reveal_child(false);
        // Reset transfer UI
        imp.transfer_button.set_active(false);
        imp.transfer_revealer.set_reveal_child(false);
        imp.transfer_entry.set_text("");
        // Exit consult mode
        self.exit_consult_mode();
    }

    pub fn enter_consult_mode(&self, held_name: &str) {
        let imp = self.imp();
        imp.held_label.set_label(&format!("Holding: {held_name}"));
        imp.held_label.set_visible(true);
        imp.transfer_revealer.set_reveal_child(false);
        imp.transfer_button.set_active(false);
        imp.consult_revealer.set_reveal_child(true);
    }

    pub fn exit_consult_mode(&self) {
        let imp = self.imp();
        imp.held_label.set_visible(false);
        imp.consult_revealer.set_reveal_child(false);
    }
}

impl Default for CallScreen {
    fn default() -> Self {
        Self::new()
    }
}
