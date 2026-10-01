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
    let accounts_button = gtk4::Button::from_icon_name("system-users-symbolic");
    header.pack_start(&accounts_button);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&app.borrow().canvas.overlay));

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar_view));
    window.set_content(Some(&toast_overlay));

    new_session_button.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| open_new_session_dialog(&app, &window, &toast_overlay)
    });

    accounts_button.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| open_account_manager_dialog(&app, &window, &toast_overlay)
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
        let toast_overlay = toast_overlay.clone();
        move |_, _| open_new_session_dialog(&app, &window, &toast_overlay)
    });
    application.add_action(&action);
    application.set_accels_for_action("app.new-session", &["<Ctrl>T"]);

    let accounts_action = gtk4::gio::SimpleAction::new("manage-accounts", None);
    accounts_action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| open_account_manager_dialog(&app, &window, &toast_overlay)
    });
    application.add_action(&accounts_action);
    application.set_accels_for_action("app.manage-accounts", &["<Ctrl>period"]);

    window.present();

    for error in errors {
        toast_overlay.add_toast(adw::Toast::new(&error));
    }
}

fn open_new_session_dialog(
    app: &Rc<RefCell<App>>,
    parent: &adw::ApplicationWindow,
    toast_overlay: &adw::ToastOverlay,
) {
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
        let toast_overlay = toast_overlay.clone();
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
            match result {
                Ok(()) => dialog.close(),
                Err(error) => toast_overlay.add_toast(adw::Toast::new(&error.to_string())),
            }
        }
    });

    dialog.present();
}

fn open_account_manager_dialog(
    app: &Rc<RefCell<App>>,
    parent: &adw::ApplicationWindow,
    toast_overlay: &adw::ToastOverlay,
) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(360)
        .title("Accounts")
        .build();

    let list_box = gtk4::ListBox::new();
    let new_name_entry = gtk4::Entry::builder()
        .placeholder_text("New account name")
        .build();
    let add_button = gtk4::Button::with_label("Add");

    let add_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    add_row.append(&new_name_entry);
    add_row.append(&add_button);

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    body.append(&list_box);
    body.append(&add_row);
    dialog.set_content(Some(&body));

    populate_accounts(&list_box, app, toast_overlay);

    add_button.connect_clicked({
        let app = app.clone();
        let list_box = list_box.clone();
        let toast_overlay = toast_overlay.clone();
        let new_name_entry = new_name_entry.clone();
        move |_| {
            let name = new_name_entry.text().to_string();
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            match app.borrow().accounts.ensure(name) {
                Ok(_) => {
                    new_name_entry.set_text("");
                    populate_accounts(&list_box, &app, &toast_overlay);
                }
                Err(error) => {
                    toast_overlay.add_toast(adw::Toast::new(&format!("couldn't create account: {error}")));
                }
            }
        }
    });

    dialog.present();
}

/// Clears and repopulates `list_box` from `app.accounts.list()`, wiring a
/// delete button per row to `App::delete_account`. A plain function (not a
/// closure) so the per-row delete handler can call it again by name after a
/// deletion, without needing to capture itself.
fn populate_accounts(list_box: &gtk4::ListBox, app: &Rc<RefCell<App>>, toast_overlay: &adw::ToastOverlay) {
    while let Some(row) = list_box.row_at_index(0) {
        list_box.remove(&row);
    }
    let accounts = match app.borrow().accounts.list() {
        Ok(accounts) => accounts,
        Err(error) => {
            toast_overlay.add_toast(adw::Toast::new(&format!("couldn't list accounts: {error}")));
            Vec::new()
        }
    };
    for name in accounts {
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let label = gtk4::Label::new(Some(&name));
        label.set_hexpand(true);
        label.set_xalign(0.0);
        let delete_button = gtk4::Button::from_icon_name("user-trash-symbolic");
        row.append(&label);
        row.append(&delete_button);
        list_box.append(&row);

        delete_button.connect_clicked({
            let app = app.clone();
            let list_box = list_box.clone();
            let toast_overlay = toast_overlay.clone();
            let name = name.clone();
            move |_| {
                if let Err(error) = App::delete_account(&app, &name) {
                    toast_overlay.add_toast(adw::Toast::new(&format!("couldn't delete account: {error}")));
                }
                populate_accounts(&list_box, &app, &toast_overlay);
            }
        });
    }
}
