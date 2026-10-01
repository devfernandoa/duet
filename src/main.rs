mod account;
mod agent;
mod app;
mod canvas;
mod handoff;
mod node;
mod session;
mod store;

use account::AccountStore;
use adw::prelude::*;
use app::App;
use gtk4::glib;
use libadwaita as adw;
use std::time::Duration;

const APP_ID: &str = "dev.fernandoa.duet";

fn main() -> glib::ExitCode {
    let application = adw::Application::builder()
        .application_id(APP_ID)
        .build();
    application.connect_activate(build_ui);
    application.run()
}

fn build_ui(application: &adw::Application) {
    let store_path = store::default_store_path().expect("data directory available");
    let accounts_dir = store::default_accounts_dir().expect("data directory available");
    let app = App::new(AccountStore::new(accounts_dir), store_path);
    let errors = App::restore(&app);
    for error in errors {
        eprintln!("duet: {error}"); // Task 11 replaces this with an adw::Toast
    }

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("duet")
        .default_width(1200)
        .default_height(800)
        .build();
    window.set_content(Some(&app.borrow().canvas.overlay));

    glib::timeout_add_local(Duration::from_millis(33), {
        let app = app.clone();
        move || {
            app.borrow_mut().pump_output();
            glib::ControlFlow::Continue
        }
    });

    window.present();
}
