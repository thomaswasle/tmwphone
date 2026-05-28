use gtk4::{gio, glib, prelude::*, subclass::prelude::*};
use libadwaita as adw;
use adw::prelude::*;
use adw::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct App;

    #[glib::object_subclass]
    impl ObjectSubclass for App {
        const NAME: &'static str = "LocaClientApp";
        type Type = super::App;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for App {}

    impl ApplicationImpl for App {
        fn activate(&self) {
            self.parent_activate();
            let app = self.obj();
            let window = app
                .active_window()
                .unwrap_or_else(|| crate::window::MainWindow::new(&*app).upcast());
            window.present();
        }

        fn startup(&self) {
            self.parent_startup();

            let css = gtk4::CssProvider::new();
            css.load_from_string(
                ".call-screen {
                    background: @window_bg_color;
                }",
            );
            if let Some(display) = gtk4::gdk::Display::default() {
                gtk4::style_context_add_provider_for_display(
                    &display,
                    &css,
                    gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
            }

            let app = self.obj();

            let quit = gio::SimpleAction::new("quit", None);
            quit.connect_activate(glib::clone!(
                #[weak]
                app,
                move |_, _| app.quit()
            ));
            app.add_action(&quit);
            app.set_accels_for_action("app.quit", &["<Ctrl>Q"]);

            let prefs = gio::SimpleAction::new("preferences", None);
            prefs.connect_activate(glib::clone!(
                #[weak]
                app,
                move |_, _| {
                    if let Some(win) = app.active_window() {
                        if let Ok(main_win) = win.downcast::<crate::window::MainWindow>() {
                            main_win.open_settings_dialog();
                        }
                    }
                }
            ));
            app.add_action(&prefs);

            let about = gio::SimpleAction::new("about", None);
            about.connect_activate(glib::clone!(
                #[weak]
                app,
                move |_, _| {
                    let dialog = adw::AboutDialog::builder()
                        .application_name("Loca Client")
                        .application_icon("net.loca.Client")
                        .developer_name("Loca")
                        .version(env!("CARGO_PKG_VERSION"))
                        .build();
                    dialog.present(app.active_window().as_ref());
                }
            ));
            app.add_action(&about);
        }
    }

    impl GtkApplicationImpl for App {}
    impl AdwApplicationImpl for App {}
}

glib::wrapper! {
    pub struct App(ObjectSubclass<imp::App>)
        @extends adw::Application, gtk4::Application, gio::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl App {
    pub fn new() -> Self {
        glib::Object::builder()
            .property("application-id", "net.loca.Client")
            .property("flags", gio::ApplicationFlags::FLAGS_NONE)
            .build()
    }
}
