use gtk4::{gio, glib, prelude::*, subclass::prelude::*, CompositeTemplate};
use libadwaita as adw;
use adw::prelude::*;
use adw::subclass::prelude::*;
use std::cell::RefCell;
use std::collections::HashSet;

mod imp {
    use super::*;
    use std::sync::OnceLock;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(file = "../../data/ui/settings_dialog.ui")]
    pub struct SettingsDialog {
        #[template_child]
        pub accounts_page: TemplateChild<adw::PreferencesPage>,
        #[template_child]
        pub ringer_device_row: TemplateChild<adw::ComboRow>,

        pub accounts_group: RefCell<Option<adw::PreferencesGroup>>,
        pub registered_ids: RefCell<HashSet<String>>,
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
                vec![
                    glib::subclass::Signal::builder("account-register-toggled")
                        .param_types([String::static_type(), bool::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("account-reconnect")
                        .param_types([String::static_type()])
                        .build(),
                    glib::subclass::Signal::builder("account-removed")
                        .param_types([String::static_type()])
                        .build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
        }
    }

    impl WidgetImpl for SettingsDialog {}
    impl AdwDialogImpl for SettingsDialog {}
    impl PreferencesDialogImpl for SettingsDialog {}

    impl SettingsDialog {
        pub fn build_accounts_ui(&self) {
            let group = adw::PreferencesGroup::new();
            group.set_title("SIP Accounts");

            let add_btn = gtk4::Button::from_icon_name("list-add-symbolic");
            add_btn.set_tooltip_text(Some("Add account"));
            add_btn.add_css_class("flat");
            group.set_header_suffix(Some(&add_btn));

            for account in &crate::accounts::load() {
                group.add(&self.make_account_row(account));
            }

            self.accounts_page.add(&group);
            *self.accounts_group.borrow_mut() = Some(group);

            let obj = self.obj();
            add_btn.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |_| obj.imp().add_new_account()
            ));
        }

        pub fn make_account_row(
            &self,
            account: &crate::accounts::Account,
        ) -> adw::ExpanderRow {
            let id = account.id.clone();

            let row = adw::ExpanderRow::new();
            row.set_title(&account.label());
            row.set_subtitle(&format!("{}:{}", account.server, account.port));

            // ── Suffix widgets ────────────────────────────────────────────────

            let reg_switch = gtk4::Switch::new();
            reg_switch.set_active(self.registered_ids.borrow().contains(&id));
            reg_switch.set_valign(gtk4::Align::Center);
            reg_switch.set_tooltip_text(Some("Register this account now"));
            row.add_suffix(&reg_switch);

            let del_btn = gtk4::Button::from_icon_name("user-trash-symbolic");
            del_btn.add_css_class("flat");
            del_btn.set_valign(gtk4::Align::Center);
            del_btn.set_tooltip_text(Some("Delete account"));
            row.add_suffix(&del_btn);

            // ── Inner rows ────────────────────────────────────────────────────

            let dn_row = adw::EntryRow::new();
            dn_row.set_title("Display name");
            dn_row.set_text(&account.display_name);
            row.add_row(&dn_row);

            let user_row = adw::EntryRow::new();
            user_row.set_title("Username");
            user_row.set_text(&account.username);
            row.add_row(&user_row);

            let pw_row = adw::PasswordEntryRow::new();
            pw_row.set_title("Password");
            if let Some(pw) = crate::keyring::load_for(&id) {
                pw_row.set_text(&pw);
            }
            row.add_row(&pw_row);

            let srv_row = adw::EntryRow::new();
            srv_row.set_title("SIP server");
            srv_row.set_text(&account.server);
            row.add_row(&srv_row);

            let port_row = adw::EntryRow::new();
            port_row.set_title("Port");
            port_row.set_text(&account.port.to_string());
            row.add_row(&port_row);

            let proxy_row = adw::EntryRow::new();
            proxy_row.set_title("Outbound proxy (optional)");
            proxy_row.set_text(&account.proxy);
            row.add_row(&proxy_row);

            let transport_row = adw::ComboRow::new();
            transport_row.set_title("Transport");
            transport_row.set_model(Some(&gtk4::StringList::new(&["UDP", "TCP", "TLS"])));
            let is_tls = account.transport == crate::accounts::Transport::Tls;
            transport_row.set_selected(match account.transport {
                crate::accounts::Transport::Udp => 0,
                crate::accounts::Transport::Tcp => 1,
                crate::accounts::Transport::Tls => 2,
            });
            row.add_row(&transport_row);

            let tls_verify_row = adw::SwitchRow::new();
            tls_verify_row.set_title("Verify TLS certificate");
            tls_verify_row.set_active(account.tls_verify);
            tls_verify_row.set_visible(is_tls);
            row.add_row(&tls_verify_row);

            let tls_ca_row = adw::EntryRow::new();
            tls_ca_row.set_title("CA certificate file");
            tls_ca_row.set_text(&account.tls_ca_file);
            tls_ca_row.set_visible(is_tls);
            let open_btn = gtk4::Button::from_icon_name("document-open-symbolic");
            open_btn.add_css_class("flat");
            open_btn.set_valign(gtk4::Align::Center);
            open_btn.set_tooltip_text(Some("Choose CA certificate"));
            tls_ca_row.add_suffix(&open_btn);
            row.add_row(&tls_ca_row);

            // Show/hide TLS rows when transport selection changes.
            transport_row.connect_notify_local(
                Some("selected"),
                glib::clone!(
                    #[weak] tls_verify_row,
                    #[weak] tls_ca_row,
                    move |combo, _| {
                        let visible = combo.selected() == 2;
                        tls_verify_row.set_visible(visible);
                        tls_ca_row.set_visible(visible);
                    }
                ),
            );

            // File chooser for the CA certificate path.
            open_btn.connect_clicked(glib::clone!(
                #[weak] tls_ca_row,
                move |btn| {
                    let dialog = gtk4::FileDialog::new();
                    dialog.set_title("Select CA Certificate");
                    let filter = gtk4::FileFilter::new();
                    filter.set_name(Some("PEM / CRT files"));
                    filter.add_pattern("*.pem");
                    filter.add_pattern("*.crt");
                    filter.add_pattern("*.cer");
                    let store = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
                    store.append(&filter);
                    dialog.set_filters(Some(&store));
                    let window = btn.root().and_downcast::<gtk4::Window>();
                    dialog.open(
                        window.as_ref(),
                        None::<&gtk4::gio::Cancellable>,
                        glib::clone!(
                            #[weak] tls_ca_row,
                            move |result| {
                                if let Ok(file) = result {
                                    if let Some(path) = file.path() {
                                        tls_ca_row.set_text(&path.to_string_lossy());
                                    }
                                }
                            }
                        ),
                    );
                }
            ));

            let startup_row = adw::SwitchRow::new();
            startup_row.set_title("Register on startup");
            startup_row.set_active(account.register_on_startup);
            row.add_row(&startup_row);

            let save_row = adw::ButtonRow::new();
            save_row.set_title("Save");
            save_row.add_css_class("suggested-action");
            row.add_row(&save_row);

            // ── Signal connections ────────────────────────────────────────────

            let obj = self.obj();

            // Register switch: toggle live registration
            let id2 = id.clone();
            reg_switch.connect_state_set(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |_sw, state| {
                    if state {
                        obj.imp().registered_ids.borrow_mut().insert(id2.clone());
                    } else {
                        obj.imp().registered_ids.borrow_mut().remove(&id2);
                    }
                    obj.emit_by_name::<()>("account-register-toggled", &[&id2, &state]);
                    glib::Propagation::Proceed
                }
            ));

            // Delete button
            let id3 = id.clone();
            del_btn.connect_clicked(glib::clone!(
                #[weak]
                obj,
                #[weak]
                row,
                move |_| {
                    let mut accounts = crate::accounts::load();
                    accounts.retain(|a| a.id != id3);
                    crate::accounts::save(&accounts);
                    crate::keyring::clear_for(&id3).ok();

                    if let Some(group) = obj.imp().accounts_group.borrow().as_ref() {
                        group.remove(&row);
                    }
                    obj.imp().registered_ids.borrow_mut().remove(&id3);
                    obj.emit_by_name::<()>("account-removed", &[&id3]);
                }
            ));

            // Save button
            let id4 = id.clone();
            save_row.connect_activated(glib::clone!(
                #[weak]
                obj,
                #[weak]
                dn_row,
                #[weak]
                user_row,
                #[weak]
                pw_row,
                #[weak]
                srv_row,
                #[weak]
                port_row,
                #[weak]
                proxy_row,
                #[weak]
                transport_row,
                #[weak]
                tls_verify_row,
                #[weak]
                tls_ca_row,
                #[weak]
                startup_row,
                #[weak]
                row,
                move |_| {
                    let mut accounts = crate::accounts::load();
                    if let Some(acc) = accounts.iter_mut().find(|a| a.id == id4) {
                        acc.display_name = dn_row.text().to_string();
                        acc.username = user_row.text().to_string();
                        acc.server = srv_row.text().to_string();
                        acc.port = port_row.text().to_string().parse::<u16>().unwrap_or(5060);
                        acc.proxy = proxy_row.text().to_string();
                        acc.transport = match transport_row.selected() {
                            1 => crate::accounts::Transport::Tcp,
                            2 => crate::accounts::Transport::Tls,
                            _ => crate::accounts::Transport::Udp,
                        };
                        acc.tls_verify = tls_verify_row.is_active();
                        acc.tls_ca_file = tls_ca_row.text().to_string();
                        acc.register_on_startup = startup_row.is_active();
                        row.set_title(&acc.label());
                        row.set_subtitle(&format!("{}:{}", acc.server, acc.port));
                        crate::accounts::save(&accounts);
                    }
                    if let Err(e) = crate::keyring::save_for(&id4, &pw_row.text()) {
                        log::warn!("keyring save failed: {e}");
                    }
                    if obj.imp().registered_ids.borrow().contains(&id4) {
                        obj.emit_by_name::<()>("account-reconnect", &[&id4]);
                    }
                }
            ));

            row
        }

        pub fn build_audio_ui(&self) {
            let devices = crate::ringer::enumerate_output_devices();
            // Use UFCS to avoid ambiguity with gtk4::prelude::AppInfoExt::display_name.
            let device_names: Vec<String> = devices
                .iter()
                .map(|d| gstreamer::prelude::DeviceExt::display_name(d).to_string())
                .collect();

            let model_items: Vec<&str> = std::iter::once("None (disabled)")
                .chain(device_names.iter().map(|s| s.as_str()))
                .collect();
            let model = gtk4::StringList::new(&model_items);
            self.ringer_device_row.set_model(Some(&model));

            let settings = gio::Settings::new("io.github.thomaswasle.TMWPhone");
            let current = settings.string("ringer-output-device");
            if !current.is_empty() {
                if let Some(idx) = device_names.iter().position(|n| n == current.as_str()) {
                    self.ringer_device_row.set_selected((idx + 1) as u32);
                }
            }

            self.ringer_device_row.connect_selected_notify(glib::clone!(
                #[strong]
                device_names,
                move |row| {
                    let settings = gio::Settings::new("io.github.thomaswasle.TMWPhone");
                    let idx = row.selected() as usize;
                    let value = if idx == 0 { "" } else { &device_names[idx - 1] };
                    let _ = settings.set_string("ringer-output-device", value);
                }
            ));
        }

        fn add_new_account(&self) {
            let account = crate::accounts::Account::new();
            let mut accounts = crate::accounts::load();
            accounts.push(account.clone());
            crate::accounts::save(&accounts);

            let row = self.make_account_row(&account);
            if let Some(group) = self.accounts_group.borrow().as_ref() {
                group.add(&row);
            }
            row.set_expanded(true);
        }
    }
}

glib::wrapper! {
    pub struct SettingsDialog(ObjectSubclass<imp::SettingsDialog>)
        @extends adw::PreferencesDialog, adw::Dialog, gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl SettingsDialog {
    pub fn new(registered_ids: &[String]) -> Self {
        let obj: Self = glib::Object::new();
        {
            let mut set = obj.imp().registered_ids.borrow_mut();
            for id in registered_ids {
                set.insert(id.clone());
            }
        }
        obj.imp().build_accounts_ui();
        obj.imp().build_audio_ui();
        obj
    }
}
