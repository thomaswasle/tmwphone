use gtk4::{glib, prelude::*, subclass::prelude::*, CompositeTemplate};
use std::cell::RefCell;

mod imp {
    use super::*;
    use std::sync::OnceLock;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(file = "../../data/ui/dialpad.ui")]
    pub struct Dialpad {
        #[template_child]
        pub number_entry: TemplateChild<gtk4::Entry>,
        #[template_child]
        pub call_button: TemplateChild<gtk4::Button>,
        #[template_child]
        pub account_selector: TemplateChild<gtk4::DropDown>,

        pub account_ids: RefCell<Vec<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Dialpad {
        const NAME: &'static str = "Dialpad";
        type Type = super::Dialpad;
        type ParentType = gtk4::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for Dialpad {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceLock<Vec<glib::subclass::Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![glib::subclass::Signal::builder("call-requested")
                    .param_types([String::static_type(), String::static_type()])
                    .build()]
            })
        }
    }

    impl WidgetImpl for Dialpad {}
    impl BoxImpl for Dialpad {}

    #[gtk4::template_callbacks]
    impl Dialpad {
        #[template_callback]
        fn on_digit_clicked(&self, button: &gtk4::Button) {
            let digit = button.label().unwrap_or_default();
            let entry = self.number_entry.get();
            let mut pos = entry.text_length() as i32;
            entry.insert_text(&digit, &mut pos);
        }

        #[template_callback]
        fn on_delete_clicked(&self, _button: &gtk4::Button) {
            let entry = self.number_entry.get();
            let len = entry.text_length() as i32;
            if len > 0 {
                entry.delete_text(len - 1, len);
            }
        }

        #[template_callback]
        fn on_entry_activate(&self, _entry: &gtk4::Entry) {
            self.on_call_clicked_inner();
        }

        #[template_callback]
        fn on_call_clicked(&self, _button: &gtk4::Button) {
            self.on_call_clicked_inner();
        }

        fn on_call_clicked_inner(&self) {
            let number = self.number_entry.text().to_string();
            if number.is_empty() {
                return;
            }
            let account_id = {
                let ids = self.account_ids.borrow();
                if self.account_selector.is_visible() {
                    let idx = self.account_selector.selected() as usize;
                    ids.get(idx).cloned().unwrap_or_default()
                } else {
                    ids.first().cloned().unwrap_or_default()
                }
            };
            self.obj()
                .emit_by_name::<()>("call-requested", &[&number, &account_id]);
        }
    }
}

glib::wrapper! {
    pub struct Dialpad(ObjectSubclass<imp::Dialpad>)
        @extends gtk4::Box, gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget, gtk4::Orientable;
}

impl Dialpad {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn number(&self) -> String {
        self.imp().number_entry.text().to_string()
    }

    pub fn clear(&self) {
        self.imp().number_entry.set_text("");
    }

    /// Update the account selector. Pass all currently registered accounts as
    /// (account_id, display_label) pairs. The selector is hidden when ≤ 1.
    pub fn set_registered_accounts(&self, accounts: Vec<(String, String)>) {
        let imp = self.imp();
        let labels: Vec<&str> = accounts.iter().map(|(_, l)| l.as_str()).collect();
        let model = gtk4::StringList::new(&labels);
        imp.account_selector.set_model(Some(&model));
        *imp.account_ids.borrow_mut() = accounts.into_iter().map(|(id, _)| id).collect();
        imp.account_selector
            .set_visible(imp.account_ids.borrow().len() > 1);
    }
}

impl Default for Dialpad {
    fn default() -> Self {
        Self::new()
    }
}
