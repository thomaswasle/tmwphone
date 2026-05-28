use gtk4::{gio, glib, prelude::*, subclass::prelude::*, CompositeTemplate};
use libadwaita as adw;
use adw::prelude::*;
use adw::subclass::prelude::*;

mod imp {
    use super::*;
    use std::sync::OnceLock;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(file = "../../data/ui/settings_dialog.ui")]
    pub struct SettingsDialog {
        #[template_child]
        pub display_name_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub username_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub password_row: TemplateChild<adw::PasswordEntryRow>,
        #[template_child]
        pub server_row: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub connect_row: TemplateChild<adw::ButtonRow>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SettingsDialog {
        const NAME: &'static str = "SettingsDialog";
        type Type = super::SettingsDialog;
        type ParentType = adw::PreferencesDialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for SettingsDialog {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceLock<Vec<glib::subclass::Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![glib::subclass::Signal::builder("connect-requested").build()]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();

            let settings = gio::Settings::new("net.loca.TMWPhone");
            self.display_name_row
                .set_text(&settings.string("sip-display-name"));
            self.username_row
                .set_text(&settings.string("sip-username"));
            self.password_row
                .set_text(&settings.string("sip-password"));
            self.server_row.set_text(&settings.string("sip-server"));

            let obj = self.obj();
            self.connect_row.connect_activated(glib::clone!(
                #[weak]
                obj,
                move |_| obj.imp().on_save_and_connect()
            ));
        }
    }

    impl WidgetImpl for SettingsDialog {}
    impl AdwDialogImpl for SettingsDialog {}
    impl PreferencesDialogImpl for SettingsDialog {}

    impl SettingsDialog {
        fn on_save_and_connect(&self) {
            let settings = gio::Settings::new("net.loca.TMWPhone");
            settings
                .set_string("sip-display-name", &self.display_name_row.text())
                .unwrap();
            settings
                .set_string("sip-username", &self.username_row.text())
                .unwrap();
            settings
                .set_string("sip-password", &self.password_row.text())
                .unwrap();
            settings
                .set_string("sip-server", &self.server_row.text())
                .unwrap();

            self.obj().emit_by_name::<()>("connect-requested", &[]);
            self.obj().close();
        }
    }
}

glib::wrapper! {
    pub struct SettingsDialog(ObjectSubclass<imp::SettingsDialog>)
        @extends adw::PreferencesDialog, adw::Dialog, gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl SettingsDialog {
    pub fn new() -> Self {
        glib::Object::new()
    }
}

impl Default for SettingsDialog {
    fn default() -> Self {
        Self::new()
    }
}
