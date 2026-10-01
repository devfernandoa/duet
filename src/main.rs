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

    app.borrow().canvas.set_link_lines_source({
        let app = app.clone();
        move || {
            let app_ref = app.borrow();
            let selected = app_ref.selected_link;
            app_ref
                .link_lines()
                .into_iter()
                .map(|(link, from, to)| canvas::LinkLine {
                    from,
                    to,
                    selected: selected == Some(link),
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
            app.borrow().canvas.links_area.queue_draw();
            glib::ControlFlow::Continue
        }
    });

    let css = gtk4::CssProvider::new();
    css.load_from_data(include_str!("style.css"));
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

    // Link lines are drawn, not widgets, so deleting one needs a hit test
    // against the curve rather than a click on a control: one click selects
    // (the line thickens and turns orange), a second click on the same line
    // deletes it. `App::remove_link` existed from the start but had no UI
    // path to it at all until now.
    app.borrow().canvas.connect_background_click({
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        move |world| {
            if let Some(message) = App::click_link_at(&app, world) {
                toast_overlay.add_toast(adw::Toast::new(&message));
            }
        }
    });

    // Restore after the toast overlay exists: restored sessions' handoff
    // buttons are wired (via `wire_link_controls`) to show a toast on
    // failure, and `restore`'s own load/spawn errors are also toasted below.
    let errors = App::restore(&app, &toast_overlay);

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

    // Ctrl+scroll already zoomed, but a modifier+scroll gesture is invisible
    // in a UI with no menu: the user asked for zoom without ever finding it.
    // `equal` as well as `plus` so the unshifted "+" key works, and the
    // keypad names so a numpad does too.
    for (name, accels, steps) in [
        ("zoom-in", vec!["<Ctrl>plus", "<Ctrl>equal", "<Ctrl>KP_Add"], 1),
        (
            "zoom-out",
            vec!["<Ctrl>minus", "<Ctrl>KP_Subtract"],
            -1,
        ),
        ("zoom-reset", vec!["<Ctrl>0", "<Ctrl>KP_0"], 0),
    ] {
        let zoom_action = gtk4::gio::SimpleAction::new(name, None);
        zoom_action.connect_activate({
            let app = app.clone();
            move |_, _| {
                {
                    // Scoped: `schedule_persist` takes a mutable borrow of
                    // the same `RefCell`, which panics if this one is live.
                    let app_ref = app.borrow();
                    if steps == 0 {
                        app_ref.canvas.reset_view();
                    } else {
                        app_ref.canvas.zoom_by_steps(steps);
                    }
                }
                App::schedule_persist(&app);
            }
        });
        application.add_action(&zoom_action);
        application.set_accels_for_action(&format!("app.{name}"), &accels);
    }

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

    // Claude-account picker: previously this dialog always passed `None` to
    // `App::create_session`, silently ignoring every account but the default
    // one — there was no way to actually pick an account from the UI even
    // though the account manager let you create more than one. `accounts`
    // always lists at least `default` because `ensure` is idempotent and
    // `App::new`/restore never delete it on their own; it's listed here only
    // if it already has a directory, so fall back to showing just `default`
    // when none exist yet.
    let mut account_names = app.borrow().accounts.list().unwrap_or_default();
    if !account_names.iter().any(|a| a == account::DEFAULT_ACCOUNT) {
        account_names.insert(0, account::DEFAULT_ACCOUNT.to_string());
    }
    let account_dropdown =
        gtk4::DropDown::from_strings(&account_names.iter().map(String::as_str).collect::<Vec<_>>());
    let account_label = gtk4::Label::new(Some("Claude account"));
    account_label.set_xalign(0.0);

    let create_button = gtk4::Button::with_label("Create");
    create_button.add_css_class("suggested-action");
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    body.append(&name_entry);
    body.append(&cwd_entry);
    body.append(&agent_dropdown);
    body.append(&account_label);
    body.append(&account_dropdown);
    body.append(&create_button);
    dialog.set_content(Some(&body));

    // The account picker only matters for Claude; keep it visible but
    // dimmed/disabled when Codex is selected rather than hiding it, so the
    // dialog's layout doesn't jump around as the user switches agents.
    let sync_account_sensitivity = {
        let agent_dropdown = agent_dropdown.clone();
        let account_label = account_label.clone();
        let account_dropdown = account_dropdown.clone();
        move || {
            let is_claude = agent_dropdown.selected() == 0;
            account_label.set_sensitive(is_claude);
            account_dropdown.set_sensitive(is_claude);
        }
    };
    sync_account_sensitivity();
    agent_dropdown.connect_selected_notify(move |_| sync_account_sensitivity());

    create_button.connect_clicked({
        let app = app.clone();
        let dialog = dialog.clone();
        let parent = parent.clone();
        let name_entry = name_entry.clone();
        let cwd_entry = cwd_entry.clone();
        let agent_dropdown = agent_dropdown.clone();
        let account_dropdown = account_dropdown.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let agent = if agent_dropdown.selected() == 0 {
                agent::Agent::Claude
            } else {
                agent::Agent::Codex
            };
            // Ignored entirely by `create_session`'s Codex path; only read
            // when `agent == Claude`.
            let claude_account = account_names.get(account_dropdown.selected() as usize).cloned();
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
                claude_account,
                viewport_center,
                &toast_overlay,
            );
            match result {
                Ok(()) => dialog.close(),
                Err(error) => toast_overlay.add_toast(adw::Toast::new(&error.to_string())),
            }
        }
    });

    dialog.present();
}

/// Account manager: a titled window (matching `open_new_session_dialog`'s
/// shape) built from real Adwaita list widgets — an `adw::PreferencesGroup`
/// of `adw::ActionRow`s (one per account, each with a destructive-styled
/// trash button) plus an `adw::EntryRow` to create a new one — instead of
/// the original plain `ListBox` of hand-built `gtk4::Box` rows. Every
/// mutating action (create, delete) reports failures as a toast and the
/// account named `default` can't be deleted from here, since every Claude
/// session silently falls back to it and removing it out from under a
/// restored session would otherwise fail confusingly later.
fn open_account_manager_dialog(
    app: &Rc<RefCell<App>>,
    parent: &adw::ApplicationWindow,
    toast_overlay: &adw::ToastOverlay,
) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .title("Accounts")
        .build();

    let header = adw::HeaderBar::new();
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);

    let accounts_group = adw::PreferencesGroup::new();
    accounts_group.set_title("Claude accounts");
    accounts_group.set_description(Some(
        "Each account keeps its own isolated Claude login/config directory. \
         Pick one when creating a Claude session.",
    ));

    let new_name_row = adw::EntryRow::new();
    new_name_row.set_title("New account name");
    let add_button = gtk4::Button::from_icon_name("list-add-symbolic");
    add_button.add_css_class("flat");
    add_button.set_valign(gtk4::Align::Center);
    new_name_row.add_suffix(&add_button);
    accounts_group.add(&new_name_row);

    let page_box = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    page_box.set_margin_top(16);
    page_box.set_margin_bottom(16);
    page_box.set_margin_start(16);
    page_box.set_margin_end(16);
    page_box.append(&accounts_group);
    toolbar_view.set_content(Some(&page_box));
    dialog.set_content(Some(&toolbar_view));

    // `AdwPreferencesGroup` manages its rows in its own internal list box —
    // its *public* widget-tree children (`first_child`/`next_sibling`) don't
    // correspond 1:1 to the rows added via `add()`, so there's no reliable
    // way to enumerate "the rows I added" by walking the group's children
    // back. Tracking them explicitly here (separately from `new_name_row`,
    // which is permanent and never removed) is what lets `populate_accounts`
    // clear and rebuild just the account rows on every change.
    let account_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    populate_accounts(&accounts_group, &account_rows, app, toast_overlay, &dialog);

    let create_from_entry = {
        let app = app.clone();
        let accounts_group = accounts_group.clone();
        let account_rows = account_rows.clone();
        let new_name_row = new_name_row.clone();
        let toast_overlay = toast_overlay.clone();
        let dialog = dialog.clone();
        move || {
            let name = new_name_row.text().to_string();
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            match app.borrow().accounts.ensure(name) {
                Ok(_) => {
                    new_name_row.set_text("");
                    populate_accounts(&accounts_group, &account_rows, &app, &toast_overlay, &dialog);
                }
                Err(error) => {
                    toast_overlay.add_toast(adw::Toast::new(&format!("couldn't create account: {error}")));
                }
            }
        }
    };
    add_button.connect_clicked({
        let create_from_entry = create_from_entry.clone();
        move |_| create_from_entry()
    });
    // EntryRow's own activate signal (pressing Enter in the entry) — lets
    // the user create an account without reaching for the mouse.
    new_name_row.connect_entry_activated(move |_| create_from_entry());

    dialog.present();
}

/// Clears and repopulates the account rows tracked in `account_rows` (see
/// its doc comment in `open_account_manager_dialog` for why they must be
/// tracked explicitly rather than enumerated from `group`'s own widget
/// tree), from `app.accounts.list()`, wiring a destructive trash button per
/// row. The `default` account has no delete button — it's the implicit
/// fallback every Claude session uses when no other account is picked, so
/// removing it from here would just be confusing. `dialog_parent` is the
/// account-manager window itself, used as the `transient_for` anchor for the
/// delete-confirmation dialog each trash button opens (see
/// `confirm_delete_account` below) — `App::delete_account` is never called
/// directly from a click here, only after the user confirms. A plain
/// function (not a closure) so the per-row delete handler can call it again
/// by name after a deletion, without needing to capture itself.
fn populate_accounts(
    group: &adw::PreferencesGroup,
    account_rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    app: &Rc<RefCell<App>>,
    toast_overlay: &adw::ToastOverlay,
    dialog_parent: &adw::Window,
) {
    for row in account_rows.borrow_mut().drain(..) {
        group.remove(&row);
    }

    let accounts = match app.borrow().accounts.list() {
        Ok(accounts) => accounts,
        Err(error) => {
            toast_overlay.add_toast(adw::Toast::new(&format!("couldn't list accounts: {error}")));
            Vec::new()
        }
    };
    for name in accounts {
        let row = adw::ActionRow::new();
        row.set_title(&name);
        if name == crate::account::DEFAULT_ACCOUNT {
            row.set_subtitle("Used automatically when no other account is picked");
        } else {
            let delete_button = gtk4::Button::from_icon_name("user-trash-symbolic");
            delete_button.add_css_class("flat");
            delete_button.add_css_class("destructive-action");
            delete_button.set_valign(gtk4::Align::Center);
            delete_button.connect_clicked({
                let app = app.clone();
                let group = group.clone();
                let account_rows = account_rows.clone();
                let toast_overlay = toast_overlay.clone();
                let dialog_parent = dialog_parent.clone();
                let name = name.clone();
                move |_| confirm_delete_account(&app, &name, &group, &account_rows, &toast_overlay, &dialog_parent)
            });
            row.add_suffix(&delete_button);
        }
        group.add(&row);
        account_rows.borrow_mut().push(row);
    }
}

/// Asks "Delete account '{name}'? This will close N active session(s)." (via
/// `adw::MessageDialog` — this libadwaita version only has the pre-1.5
/// `MessageDialog`/`ResponseAppearance` API in scope, not the newer
/// `AlertDialog`, since `Cargo.toml` enables the `v1_4` feature and
/// `AlertDialog` needs `v1_5`) before actually calling `App::delete_account`.
/// Added because the trash button previously deleted — and killed every live
/// session using that account — on a single unconfirmed click, despite being
/// styled as a destructive action.
fn confirm_delete_account(
    app: &Rc<RefCell<App>>,
    name: &str,
    group: &adw::PreferencesGroup,
    account_rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    toast_overlay: &adw::ToastOverlay,
    dialog_parent: &adw::Window,
) {
    let session_count = app
        .borrow()
        .sessions
        .values()
        .filter(|entry| entry.record.claude_account.as_deref() == Some(name))
        .count();
    let body = if session_count == 0 {
        "No active sessions use this account.".to_string()
    } else if session_count == 1 {
        "This will close 1 active session.".to_string()
    } else {
        format!("This will close {session_count} active sessions.")
    };

    let confirm = adw::MessageDialog::new(Some(dialog_parent), Some(&format!("Delete account \"{name}\"?")), Some(&body));
    confirm.add_response("cancel", "Cancel");
    confirm.add_response("delete", "Delete");
    confirm.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    confirm.set_default_response(Some("cancel"));
    confirm.set_close_response("cancel");
    confirm.connect_response(None, {
        let app = app.clone();
        let group = group.clone();
        let account_rows = account_rows.clone();
        let toast_overlay = toast_overlay.clone();
        let dialog_parent = dialog_parent.clone();
        let name = name.to_string();
        move |_dialog, response| {
            if response == "delete" {
                if let Err(error) = App::delete_account(&app, &name) {
                    toast_overlay.add_toast(adw::Toast::new(&format!("couldn't delete account: {error}")));
                }
                populate_accounts(&group, &account_rows, &app, &toast_overlay, &dialog_parent);
            }
        }
    });
    confirm.present();
}
