mod account;
mod agent;
mod app;
mod canvas;
mod handoff;
mod link;
mod node;
mod session;
mod store;

use account::AccountStore;
use adw::prelude::*;
use app::App;
use gtk4::glib;
use libadwaita as adw;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
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

    app.borrow().canvas.set_link_lines_source({
        let app = app.clone();
        move || {
            let app_ref = app.borrow();
            app_ref
                .links
                .iter()
                .filter_map(|link| {
                    let source = app_ref.sessions.get(&link.source)?;
                    let target = app_ref.sessions.get(&link.target)?;
                    Some((source.record.position, target.record.position))
                })
                .collect()
        }
    });

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("duet")
        .default_width(1200)
        .default_height(800)
        .build();

    glib::timeout_add_local(Duration::from_millis(33), {
        let app = app.clone();
        move || {
            app.borrow_mut().pump_output();
            app.borrow().canvas.drawing_area.queue_draw();
            glib::ControlFlow::Continue
        }
    });

    let css = gtk4::CssProvider::new();
    css.load_from_data(
        ".note-yellow { background-color: #fff3a0; } \
         .note-blue { background-color: #cfe8ff; } \
         .note-green { background-color: #d7f5d0; }",
    );
    gtk4::style_context_add_provider_for_display(
        &gtk4::prelude::WidgetExt::display(&window),
        &css,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let header = adw::HeaderBar::new();
    let new_session_button = gtk4::Button::from_icon_name("tab-new-symbolic");
    header.pack_start(&new_session_button);
    let new_note_button = gtk4::Button::from_icon_name("text-editor-symbolic");
    header.pack_start(&new_note_button);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&app.borrow().canvas.overlay));
    window.set_content(Some(&toolbar_view));

    new_session_button.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        move |_| open_new_session_dialog(&app, &window)
    });

    new_note_button.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        move |_| {
            let position = {
                let (width, height) = (window.width(), window.height());
                let screen_center = if width > 0 && height > 0 {
                    (width as f64 / 2.0, height as f64 / 2.0)
                } else {
                    // Defensive fallback: only reachable if the window hasn't
                    // been allocated a size yet, which shouldn't happen since
                    // window.present() runs before this button can be clicked.
                    (600.0, 400.0)
                };
                let app_ref = app.borrow();
                let state = app_ref.canvas.state.borrow();
                canvas::screen_to_world(screen_center, state.pan, state.zoom)
            };
            App::create_note(&app, position);
        }
    });

    let action = gtk4::gio::SimpleAction::new("new-session", None);
    action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        move |_, _| open_new_session_dialog(&app, &window)
    });
    application.add_action(&action);
    application.set_accels_for_action("app.new-session", &["<Ctrl>T"]);

    window.present();
}

fn open_new_session_dialog(app: &Rc<RefCell<App>>, parent: &adw::ApplicationWindow) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(400)
        .title("New session")
        .build();

    let name_entry = gtk4::Entry::builder().placeholder_text("Session name").build();
    let cwd_entry = gtk4::Entry::builder()
        .text(std::env::current_dir().unwrap_or_default().display().to_string())
        .build();
    let agent_dropdown = gtk4::DropDown::from_strings(&["Claude", "Codex"]);

    let create_button = gtk4::Button::with_label("Create");
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    body.append(&name_entry);
    body.append(&cwd_entry);
    body.append(&agent_dropdown);
    body.append(&create_button);
    dialog.set_content(Some(&body));

    create_button.connect_clicked({
        let app = app.clone();
        let dialog = dialog.clone();
        let parent = parent.clone();
        let name_entry = name_entry.clone();
        let cwd_entry = cwd_entry.clone();
        let agent_dropdown = agent_dropdown.clone();
        move |_| {
            let agent = if agent_dropdown.selected() == 0 {
                agent::Agent::Claude
            } else {
                agent::Agent::Codex
            };
            let viewport_center = {
                let (width, height) = (parent.width(), parent.height());
                let screen_center = if width > 0 && height > 0 {
                    (width as f64 / 2.0, height as f64 / 2.0)
                } else {
                    // Defensive fallback: only reachable if the window hasn't
                    // been allocated a size yet, which shouldn't happen since
                    // window.present() runs before this dialog can be opened.
                    (600.0, 400.0)
                };
                let app_ref = app.borrow();
                let state = app_ref.canvas.state.borrow();
                canvas::screen_to_world(screen_center, state.pan, state.zoom)
            };
            let result = App::create_session(
                &app,
                name_entry.text().to_string(),
                PathBuf::from(cwd_entry.text().to_string()),
                agent,
                None,
                viewport_center,
            );
            if result.is_ok() {
                dialog.close();
            }
            // Error display (a toast) is added in Task 11 alongside the
            // other error-reporting work; for now a failed create just
            // leaves the dialog open so the user can fix the input.
        }
    });

    dialog.present();
}
