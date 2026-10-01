mod account;
mod agent;
mod app;
mod canvas;
mod handoff;
mod link;
mod node;
mod role;
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
use uuid::Uuid;

const APP_ID: &str = "dev.fernandoa.duet";

fn main() -> glib::ExitCode {
    let application = adw::Application::builder().application_id(APP_ID).build();
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
    let roles_button = gtk4::Button::from_icon_name("preferences-system-symbolic");
    roles_button.set_tooltip_text(Some("Manage agent roles"));
    header.pack_start(&roles_button);
    let workspace_icon = gtk4::Image::from_icon_name("view-paged-symbolic");
    let workspace_label = gtk4::Label::new(None);
    let workspace_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    workspace_box.append(&workspace_icon);
    workspace_box.append(&workspace_label);
    let workspace_button = gtk4::Button::new();
    workspace_button.set_child(Some(&workspace_box));
    workspace_button.set_tooltip_text(Some("Switch workspace"));
    header.pack_end(&workspace_button);

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
    sync_workspace_button(&workspace_label, &app);
    // Writes back immediately rather than waiting for the first edit, so a
    // migration from an older single-canvas store.json (see `store::Store`'s
    // pre-workspace migration) is durable on disk right away instead of only
    // in memory until something happens to trigger a save.
    let _ = app.borrow().persist();

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

    roles_button.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| open_role_manager_dialog(&app, &window, &toast_overlay)
    });

    workspace_button.connect_clicked({
        let app = app.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        let workspace_label = workspace_label.clone();
        move |_| open_workspace_switcher_dialog(&app, &window, &toast_overlay, &workspace_label)
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

    let roles_action = gtk4::gio::SimpleAction::new("manage-roles", None);
    roles_action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| open_role_manager_dialog(&app, &window, &toast_overlay)
    });
    application.add_action(&roles_action);
    application.set_accels_for_action("app.manage-roles", &["<Ctrl><Shift>R"]);

    // Ctrl+scroll already zoomed, but a modifier+scroll gesture is invisible
    // in a UI with no menu: the user asked for zoom without ever finding it.
    // `equal` as well as `plus` so the unshifted "+" key works, and the
    // keypad names so a numpad does too.
    for (name, accels, steps) in [
        (
            "zoom-in",
            vec!["<Ctrl>plus", "<Ctrl>equal", "<Ctrl>KP_Add"],
            1,
        ),
        ("zoom-out", vec!["<Ctrl>minus", "<Ctrl>KP_Subtract"], -1),
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

    // Ctrl+1..9 jump straight to the Nth workspace in `workspace_list`'s
    // stable (name-sorted) order — "quickly switching among the first
    // several workspaces" without opening the switcher dialog. Silently does
    // nothing past however many workspaces actually exist.
    for index in 1..=9u32 {
        let action = gtk4::gio::SimpleAction::new(&format!("switch-workspace-{index}"), None);
        action.connect_activate({
            let app = app.clone();
            let toast_overlay = toast_overlay.clone();
            let workspace_label = workspace_label.clone();
            move |_, _| {
                let target = app
                    .borrow()
                    .workspace_list()
                    .get((index - 1) as usize)
                    .map(|(id, _)| *id);
                if let Some(id) = target {
                    for error in App::switch_workspace(&app, id, &toast_overlay) {
                        toast_overlay.add_toast(adw::Toast::new(&error));
                    }
                    sync_workspace_button(&workspace_label, &app);
                }
            }
        });
        application.add_action(&action);
        application.set_accels_for_action(
            &format!("app.switch-workspace-{index}"),
            &[&format!("<Ctrl>{index}")],
        );
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

    let name_entry = gtk4::Entry::builder()
        .placeholder_text("Session name")
        .build();
    // The active workspace's root directory, not just the process's cwd —
    // that's the whole point of a per-workspace default.
    let cwd_entry = gtk4::Entry::builder()
        .text(app.borrow().workspace_root.display().to_string())
        .build();
    // This order (index 0-4) is matched by index in two places below:
    // `sync_field_visibility`'s `selected() == 0`/`== 4` checks, and the
    // `create_button` click handler's `match agent_dropdown.selected()`.
    // Reordering these strings means updating both.
    let agent_dropdown =
        gtk4::DropDown::from_strings(&["Claude", "Codex", "OpenCode", "Shell", "Custom command"]);
    let custom_command_entry = gtk4::Entry::builder()
        .placeholder_text("Command to run (e.g. mytool --flag value)")
        .build();
    custom_command_entry.set_visible(false);

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

    // Role picker: index 0 is always "No role" (`role_ids[0] == None`), then
    // every built-in role followed by every custom one, in `App::roles`'
    // order.
    let roles = app.borrow().roles();
    let mut role_names: Vec<String> = vec!["No role".to_string()];
    role_names.extend(roles.iter().map(|role| role.name.clone()));
    let role_ids: Vec<Option<Uuid>> = std::iter::once(None)
        .chain(roles.iter().map(|role| Some(role.id)))
        .collect();
    let role_dropdown =
        gtk4::DropDown::from_strings(&role_names.iter().map(String::as_str).collect::<Vec<_>>());
    let role_label = gtk4::Label::new(Some("Role"));
    role_label.set_xalign(0.0);

    let create_button = gtk4::Button::with_label("Create");
    create_button.add_css_class("suggested-action");
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    body.append(&name_entry);
    body.append(&cwd_entry);
    body.append(&agent_dropdown);
    body.append(&account_label);
    body.append(&account_dropdown);
    body.append(&custom_command_entry);
    body.append(&role_label);
    body.append(&role_dropdown);
    body.append(&create_button);
    dialog.set_content(Some(&body));

    // The account picker only matters for Claude; the command field only for
    // a custom provider. Both stay in the layout (dimmed/hidden, not
    // removed) rather than being added/removed, so the dialog doesn't jump
    // around as the user changes the agent dropdown.
    let sync_field_visibility = {
        let agent_dropdown = agent_dropdown.clone();
        let account_label = account_label.clone();
        let account_dropdown = account_dropdown.clone();
        let custom_command_entry = custom_command_entry.clone();
        move || {
            let is_claude = agent_dropdown.selected() == 0;
            account_label.set_sensitive(is_claude);
            account_dropdown.set_sensitive(is_claude);
            custom_command_entry.set_visible(agent_dropdown.selected() == 4);
        }
    };
    sync_field_visibility();
    agent_dropdown.connect_selected_notify(move |_| sync_field_visibility());

    create_button.connect_clicked({
        let app = app.clone();
        let dialog = dialog.clone();
        let parent = parent.clone();
        let name_entry = name_entry.clone();
        let cwd_entry = cwd_entry.clone();
        let agent_dropdown = agent_dropdown.clone();
        let account_dropdown = account_dropdown.clone();
        let custom_command_entry = custom_command_entry.clone();
        let role_dropdown = role_dropdown.clone();
        let role_ids = role_ids.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let agent = match agent_dropdown.selected() {
                0 => agent::Agent::Claude,
                1 => agent::Agent::Codex,
                2 => agent::Agent::OpenCode,
                3 => agent::Agent::Shell,
                _ => {
                    // Naive whitespace splitting, not a shell-quoting parser —
                    // "simple command configurations," per this milestone's
                    // own scope, not a full command-line grammar.
                    let command_text = custom_command_entry.text();
                    let mut parts = command_text.split_whitespace().map(str::to_string);
                    let program = parts.next().unwrap_or_default();
                    let args = parts.collect();
                    agent::Agent::Custom { program, args }
                }
            };
            // Ignored entirely by every non-Claude path; only read when
            // `agent` is `Agent::Claude`.
            let claude_account = account_names
                .get(account_dropdown.selected() as usize)
                .cloned();
            let role_id = role_ids
                .get(role_dropdown.selected() as usize)
                .copied()
                .flatten();
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
                role_id,
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
                    populate_accounts(
                        &accounts_group,
                        &account_rows,
                        &app,
                        &toast_overlay,
                        &dialog,
                    );
                }
                Err(error) => {
                    toast_overlay.add_toast(adw::Toast::new(&format!(
                        "couldn't create account: {error}"
                    )));
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
                move |_| {
                    confirm_delete_account(
                        &app,
                        &name,
                        &group,
                        &account_rows,
                        &toast_overlay,
                        &dialog_parent,
                    )
                }
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

    let confirm = adw::MessageDialog::new(
        Some(dialog_parent),
        Some(&format!("Delete account \"{name}\"?")),
        Some(&body),
    );
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
                    toast_overlay.add_toast(adw::Toast::new(&format!(
                        "couldn't delete account: {error}"
                    )));
                }
                populate_accounts(&group, &account_rows, &app, &toast_overlay, &dialog_parent);
            }
        }
    });
    confirm.present();
}

/// Role manager: same shape as `open_account_manager_dialog`. Built-in
/// roles (`role::builtin_roles`) are listed read-only — no edit/delete
/// button, same reasoning as the `default` account row; custom roles get an
/// edit (pencil) and a destructive delete (trash) button. The header's
/// "add" button opens `open_role_editor_dialog` with no existing role to
/// fill in.
fn open_role_manager_dialog(
    app: &Rc<RefCell<App>>,
    parent: &adw::ApplicationWindow,
    toast_overlay: &adw::ToastOverlay,
) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .title("Agent roles")
        .build();

    let header = adw::HeaderBar::new();
    let new_role_button = gtk4::Button::from_icon_name("list-add-symbolic");
    header.pack_end(&new_role_button);
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);

    let roles_group = adw::PreferencesGroup::new();
    roles_group.set_title("Roles");
    roles_group.set_description(Some(
        "A role's instructions become the agent's first prompt when a \
         session using it launches fresh. Assign one from the new-session \
         dialog.",
    ));

    let page_box = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    page_box.set_margin_top(16);
    page_box.set_margin_bottom(16);
    page_box.set_margin_start(16);
    page_box.set_margin_end(16);
    page_box.append(&roles_group);
    toolbar_view.set_content(Some(&page_box));
    dialog.set_content(Some(&toolbar_view));

    let role_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    populate_roles(&roles_group, &role_rows, app, toast_overlay, &dialog);

    new_role_button.connect_clicked({
        let app = app.clone();
        let roles_group = roles_group.clone();
        let role_rows = role_rows.clone();
        let toast_overlay = toast_overlay.clone();
        let dialog = dialog.clone();
        move |_| {
            let app = app.clone();
            let roles_group = roles_group.clone();
            let role_rows = role_rows.clone();
            let toast_overlay = toast_overlay.clone();
            let dialog = dialog.clone();
            open_role_editor_dialog(
                &app.clone(),
                &dialog.clone(),
                &toast_overlay.clone(),
                None,
                move || populate_roles(&roles_group, &role_rows, &app, &toast_overlay, &dialog),
            );
        }
    });

    dialog.present();
}

/// Clears and repopulates the role rows tracked in `role_rows`, from
/// `App::roles()` (built-ins then customs). `dialog_parent` is the role
/// manager window itself, used as the `transient_for` anchor for the editor
/// and delete-confirmation dialogs opened from a row's buttons.
fn populate_roles(
    group: &adw::PreferencesGroup,
    role_rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    app: &Rc<RefCell<App>>,
    toast_overlay: &adw::ToastOverlay,
    dialog_parent: &adw::Window,
) {
    for row in role_rows.borrow_mut().drain(..) {
        group.remove(&row);
    }

    for role in app.borrow().roles() {
        let row = adw::ActionRow::new();
        row.set_title(&role.name);
        if role::is_builtin(role.id) {
            row.set_subtitle(&format!("Built-in — {}", role.instructions));
        } else {
            row.set_subtitle(&role.instructions);
            let edit_button = gtk4::Button::from_icon_name("document-edit-symbolic");
            edit_button.add_css_class("flat");
            edit_button.set_valign(gtk4::Align::Center);
            edit_button.connect_clicked({
                let app = app.clone();
                let dialog_parent = dialog_parent.clone();
                let toast_overlay = toast_overlay.clone();
                let group = group.clone();
                let role_rows = role_rows.clone();
                let role = role.clone();
                move |_| {
                    let app = app.clone();
                    let group = group.clone();
                    let role_rows = role_rows.clone();
                    let toast_overlay = toast_overlay.clone();
                    let dialog_parent = dialog_parent.clone();
                    open_role_editor_dialog(
                        &app.clone(),
                        &dialog_parent.clone(),
                        &toast_overlay.clone(),
                        Some(role.clone()),
                        move || {
                            populate_roles(&group, &role_rows, &app, &toast_overlay, &dialog_parent)
                        },
                    );
                }
            });
            let delete_button = gtk4::Button::from_icon_name("user-trash-symbolic");
            delete_button.add_css_class("flat");
            delete_button.add_css_class("destructive-action");
            delete_button.set_valign(gtk4::Align::Center);
            delete_button.connect_clicked({
                let app = app.clone();
                let group = group.clone();
                let role_rows = role_rows.clone();
                let toast_overlay = toast_overlay.clone();
                let dialog_parent = dialog_parent.clone();
                let id = role.id;
                let name = role.name.clone();
                move |_| {
                    confirm_delete_role(
                        &app,
                        id,
                        &name,
                        &group,
                        &role_rows,
                        &toast_overlay,
                        &dialog_parent,
                    )
                }
            });
            row.add_suffix(&edit_button);
            row.add_suffix(&delete_button);
        }
        group.add(&row);
        role_rows.borrow_mut().push(row);
    }
}

/// Asks "Delete role '{name}'? N session(s) will be unassigned." before
/// actually calling `App::delete_role` — same confirm-before-destructive
/// pattern as `confirm_delete_account`, though deleting a role only
/// unassigns sessions rather than closing them.
fn confirm_delete_role(
    app: &Rc<RefCell<App>>,
    id: Uuid,
    name: &str,
    group: &adw::PreferencesGroup,
    role_rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    toast_overlay: &adw::ToastOverlay,
    dialog_parent: &adw::Window,
) {
    let session_count = app
        .borrow()
        .sessions
        .values()
        .filter(|entry| entry.record.role_id == Some(id))
        .count();
    let body = match session_count {
        0 => "No sessions use this role.".to_string(),
        1 => "1 session will be unassigned from this role.".to_string(),
        n => format!("{n} sessions will be unassigned from this role."),
    };

    let confirm = adw::MessageDialog::new(
        Some(dialog_parent),
        Some(&format!("Delete role \"{name}\"?")),
        Some(&body),
    );
    confirm.add_response("cancel", "Cancel");
    confirm.add_response("delete", "Delete");
    confirm.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    confirm.set_default_response(Some("cancel"));
    confirm.set_close_response("cancel");
    confirm.connect_response(None, {
        let app = app.clone();
        let group = group.clone();
        let role_rows = role_rows.clone();
        let toast_overlay = toast_overlay.clone();
        let dialog_parent = dialog_parent.clone();
        move |_dialog, response| {
            if response == "delete" {
                if let Err(error) = App::delete_role(&app, id) {
                    toast_overlay
                        .add_toast(adw::Toast::new(&format!("couldn't delete role: {error}")));
                }
                populate_roles(&group, &role_rows, &app, &toast_overlay, &dialog_parent);
            }
        }
    });
    confirm.present();
}

/// "New role" / "Edit role": a small titled window with a name entry, an
/// instructions text view, an optional icon-name entry, and an accent
/// dropdown. Saves via `App::create_role`/`App::update_role` depending on
/// whether `existing` is `Some`, then calls `on_saved` (which repopulates
/// the role manager's list) and closes.
fn open_role_editor_dialog(
    app: &Rc<RefCell<App>>,
    parent: &adw::Window,
    toast_overlay: &adw::ToastOverlay,
    existing: Option<role::Role>,
    on_saved: impl Fn() + 'static,
) {
    let editing_id = existing.as_ref().map(|role| role.id);
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(380)
        .title(if existing.is_some() {
            "Edit role"
        } else {
            "New role"
        })
        .build();

    let name_entry = gtk4::Entry::builder()
        .placeholder_text("Role name")
        .text(existing.as_ref().map(|r| r.name.as_str()).unwrap_or(""))
        .build();

    let instructions_view = gtk4::TextView::new();
    instructions_view.set_wrap_mode(gtk4::WrapMode::Word);
    instructions_view.buffer().set_text(
        existing
            .as_ref()
            .map(|r| r.instructions.as_str())
            .unwrap_or(""),
    );
    let instructions_scroller = gtk4::ScrolledWindow::new();
    instructions_scroller.set_child(Some(&instructions_view));
    instructions_scroller.set_size_request(-1, 100);

    let icon_entry = gtk4::Entry::builder()
        .placeholder_text("Icon name (optional, e.g. face-smile-symbolic)")
        .text(
            existing
                .as_ref()
                .and_then(|r| r.icon.clone())
                .unwrap_or_default(),
        )
        .build();

    let accent_names: Vec<String> = std::iter::once("No accent".to_string())
        .chain(role::ACCENTS.iter().map(|accent| accent.to_string()))
        .collect();
    let accent_dropdown =
        gtk4::DropDown::from_strings(&accent_names.iter().map(String::as_str).collect::<Vec<_>>());
    let existing_accent_index = existing
        .as_ref()
        .and_then(|role| role.accent.as_deref())
        .and_then(|accent| role::ACCENTS.iter().position(|a| *a == accent))
        .map(|index| index as u32 + 1)
        .unwrap_or(0);
    accent_dropdown.set_selected(existing_accent_index);

    let save_button = gtk4::Button::with_label(if editing_id.is_some() {
        "Save"
    } else {
        "Create"
    });
    save_button.add_css_class("suggested-action");

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.append(&name_entry);
    body.append(&gtk4::Label::new(Some("Instructions")));
    body.append(&instructions_scroller);
    body.append(&icon_entry);
    body.append(&accent_dropdown);
    body.append(&save_button);
    dialog.set_content(Some(&body));

    save_button.connect_clicked({
        let app = app.clone();
        let dialog = dialog.clone();
        let toast_overlay = toast_overlay.clone();
        let name_entry = name_entry.clone();
        let instructions_view = instructions_view.clone();
        let icon_entry = icon_entry.clone();
        let accent_dropdown = accent_dropdown.clone();
        move |_| {
            let name = name_entry.text().to_string();
            let buffer = instructions_view.buffer();
            let instructions = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .to_string();
            let icon = Some(icon_entry.text().to_string()).filter(|text| !text.trim().is_empty());
            let accent = accent_dropdown
                .selected()
                .checked_sub(1)
                .and_then(|index| role::ACCENTS.get(index as usize))
                .map(|accent| accent.to_string());
            let result = match editing_id {
                Some(id) => App::update_role(&app, id, name, instructions, icon, accent),
                None => App::create_role(&app, name, instructions, icon, accent),
            };
            match result {
                Ok(()) => {
                    on_saved();
                    dialog.close();
                }
                Err(error) => toast_overlay.add_toast(adw::Toast::new(&error.to_string())),
            }
        }
    });

    dialog.present();
}

/// Reflects the active workspace's name on the header-bar switcher button.
/// Called after `App::restore` and after anything that can change which
/// workspace is active or its name (switch, create, rename, delete).
fn sync_workspace_button(label: &gtk4::Label, app: &Rc<RefCell<App>>) {
    label.set_text(&app.borrow().workspace_name);
}

/// Workspace switcher: same shape as `open_account_manager_dialog` (a titled
/// window, an `adw::PreferencesGroup` of rows, an `adw::EntryRow` to create a
/// new one) — each row is a workspace, click one to switch to it, with a
/// rename and (when more than one workspace exists) a delete button per row.
fn open_workspace_switcher_dialog(
    app: &Rc<RefCell<App>>,
    parent: &adw::ApplicationWindow,
    toast_overlay: &adw::ToastOverlay,
    workspace_label: &gtk4::Label,
) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .title("Workspaces")
        .build();

    let header = adw::HeaderBar::new();
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);

    let workspaces_group = adw::PreferencesGroup::new();
    workspaces_group.set_title("Workspaces");
    workspaces_group.set_description(Some(
        "Each workspace keeps its own canvas: sessions, notes, and links, \
         independently saved.",
    ));

    let new_name_row = adw::EntryRow::new();
    new_name_row.set_title("New workspace name");
    let add_button = gtk4::Button::from_icon_name("list-add-symbolic");
    add_button.add_css_class("flat");
    add_button.set_valign(gtk4::Align::Center);
    new_name_row.add_suffix(&add_button);
    workspaces_group.add(&new_name_row);

    let page_box = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    page_box.set_margin_top(16);
    page_box.set_margin_bottom(16);
    page_box.set_margin_start(16);
    page_box.set_margin_end(16);
    page_box.append(&workspaces_group);
    toolbar_view.set_content(Some(&page_box));
    dialog.set_content(Some(&toolbar_view));

    // Same reason as `account_rows` in `open_account_manager_dialog`:
    // `AdwPreferencesGroup`'s own widget-tree children don't correspond 1:1
    // to the rows added via `add()`, so the rows this function added are
    // tracked explicitly to clear and rebuild them on every change.
    let workspace_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    populate_workspaces(
        &workspaces_group,
        &workspace_rows,
        app,
        toast_overlay,
        &dialog,
        workspace_label,
    );

    let create_from_entry = {
        let app = app.clone();
        let workspaces_group = workspaces_group.clone();
        let workspace_rows = workspace_rows.clone();
        let new_name_row = new_name_row.clone();
        let toast_overlay = toast_overlay.clone();
        let dialog = dialog.clone();
        let workspace_label = workspace_label.clone();
        move || {
            let name = new_name_row.text().to_string();
            if name.trim().is_empty() {
                return;
            }
            // The new workspace's default root directory is simply wherever
            // duet was launched from — there's no directory-chooser UI here,
            // to keep this dialog to "name in, workspace out" (a session's
            // own working directory can always be set to anything anyway).
            let root_dir = std::env::current_dir().unwrap_or_default();
            match App::create_workspace(&app, name, root_dir) {
                Ok(_) => {
                    new_name_row.set_text("");
                    sync_workspace_button(&workspace_label, &app);
                    populate_workspaces(
                        &workspaces_group,
                        &workspace_rows,
                        &app,
                        &toast_overlay,
                        &dialog,
                        &workspace_label,
                    );
                }
                Err(error) => {
                    toast_overlay.add_toast(adw::Toast::new(&error.to_string()));
                }
            }
        }
    };
    add_button.connect_clicked({
        let create_from_entry = create_from_entry.clone();
        move |_| create_from_entry()
    });
    new_name_row.connect_entry_activated(move |_| create_from_entry());

    dialog.present();
}

/// Clears and repopulates the workspace rows tracked in `workspace_rows` (see
/// `open_workspace_switcher_dialog`'s doc comment for why they're tracked
/// explicitly), from `App::workspace_list`. The active workspace's row is
/// shown but not clickable-to-switch (switching to the workspace you're
/// already in is a no-op); every other row switches on click. The delete
/// button is omitted entirely when only one workspace exists, since
/// `App::delete_workspace` refuses that anyway and a button that always fails
/// would just be confusing.
fn populate_workspaces(
    group: &adw::PreferencesGroup,
    workspace_rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    app: &Rc<RefCell<App>>,
    toast_overlay: &adw::ToastOverlay,
    dialog_parent: &adw::Window,
    workspace_label: &gtk4::Label,
) {
    for row in workspace_rows.borrow_mut().drain(..) {
        group.remove(&row);
    }

    let active_id = app.borrow().workspace_id;
    let workspaces = app.borrow().workspace_list();
    let can_delete = workspaces.len() > 1;

    for (id, name) in workspaces {
        let row = adw::ActionRow::new();
        row.set_title(&name);
        if id == active_id {
            row.set_subtitle("Current workspace");
        } else {
            row.set_activatable(true);
            row.connect_activated({
                let app = app.clone();
                let toast_overlay = toast_overlay.clone();
                let group = group.clone();
                let workspace_rows = workspace_rows.clone();
                let dialog_parent = dialog_parent.clone();
                let workspace_label = workspace_label.clone();
                move |_| {
                    for error in App::switch_workspace(&app, id, &toast_overlay) {
                        toast_overlay.add_toast(adw::Toast::new(&error));
                    }
                    sync_workspace_button(&workspace_label, &app);
                    populate_workspaces(
                        &group,
                        &workspace_rows,
                        &app,
                        &toast_overlay,
                        &dialog_parent,
                        &workspace_label,
                    );
                }
            });
        }

        let rename_button = gtk4::Button::from_icon_name("document-edit-symbolic");
        rename_button.add_css_class("flat");
        rename_button.set_valign(gtk4::Align::Center);
        rename_button.set_tooltip_text(Some("Rename"));
        rename_button.connect_clicked({
            let app = app.clone();
            let toast_overlay = toast_overlay.clone();
            let group = group.clone();
            let workspace_rows = workspace_rows.clone();
            let dialog_parent = dialog_parent.clone();
            let workspace_label = workspace_label.clone();
            let name = name.clone();
            move |_| {
                prompt_rename_workspace(
                    &app,
                    id,
                    &name,
                    &dialog_parent,
                    &toast_overlay,
                    &group,
                    &workspace_rows,
                    &workspace_label,
                )
            }
        });
        row.add_suffix(&rename_button);

        if can_delete {
            let delete_button = gtk4::Button::from_icon_name("user-trash-symbolic");
            delete_button.add_css_class("flat");
            delete_button.add_css_class("destructive-action");
            delete_button.set_valign(gtk4::Align::Center);
            delete_button.set_tooltip_text(Some("Delete workspace"));
            delete_button.connect_clicked({
                let app = app.clone();
                let group = group.clone();
                let workspace_rows = workspace_rows.clone();
                let toast_overlay = toast_overlay.clone();
                let dialog_parent = dialog_parent.clone();
                let workspace_label = workspace_label.clone();
                let name = name.clone();
                move |_| {
                    confirm_delete_workspace(
                        &app,
                        id,
                        &name,
                        &group,
                        &workspace_rows,
                        &toast_overlay,
                        &dialog_parent,
                        &workspace_label,
                    )
                }
            });
            row.add_suffix(&delete_button);
        }

        group.add(&row);
        workspace_rows.borrow_mut().push(row);
    }
}

/// A small titled window (matching `open_new_session_dialog`'s shape) with a
/// single pre-filled entry, for renaming one workspace. On success,
/// re-syncs the header button and repopulates the switcher dialog's rows.
fn prompt_rename_workspace(
    app: &Rc<RefCell<App>>,
    id: Uuid,
    current_name: &str,
    parent: &adw::Window,
    toast_overlay: &adw::ToastOverlay,
    group: &adw::PreferencesGroup,
    workspace_rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    workspace_label: &gtk4::Label,
) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(320)
        .title("Rename workspace")
        .build();

    let entry = gtk4::Entry::builder().text(current_name).build();
    let rename_button = gtk4::Button::with_label("Rename");
    rename_button.add_css_class("suggested-action");

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    body.set_margin_top(16);
    body.set_margin_bottom(16);
    body.set_margin_start(16);
    body.set_margin_end(16);
    body.append(&entry);
    body.append(&rename_button);
    dialog.set_content(Some(&body));

    let commit = {
        let app = app.clone();
        let dialog = dialog.clone();
        let entry = entry.clone();
        let toast_overlay = toast_overlay.clone();
        let group = group.clone();
        let workspace_rows = workspace_rows.clone();
        let parent = parent.clone();
        let workspace_label = workspace_label.clone();
        move || match App::rename_workspace(&app, id, &entry.text()) {
            Ok(()) => {
                dialog.close();
                sync_workspace_button(&workspace_label, &app);
                populate_workspaces(
                    &group,
                    &workspace_rows,
                    &app,
                    &toast_overlay,
                    &parent,
                    &workspace_label,
                );
            }
            Err(error) => toast_overlay.add_toast(adw::Toast::new(&error.to_string())),
        }
    };
    rename_button.connect_clicked({
        let commit = commit.clone();
        move |_| commit()
    });
    entry.connect_activate(move |_| commit());

    dialog.present();
}

/// Asks "Delete workspace '{name}'? This will close N session(s)." before
/// calling `App::delete_workspace` — the same confirm-before-destroy pattern
/// as `confirm_delete_account`. `session_count` is read from the live
/// sessions map when deleting the active workspace, or from the dormant
/// record's saved session list otherwise, since an inactive workspace's
/// sessions aren't running processes to begin with.
fn confirm_delete_workspace(
    app: &Rc<RefCell<App>>,
    id: Uuid,
    name: &str,
    group: &adw::PreferencesGroup,
    workspace_rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    toast_overlay: &adw::ToastOverlay,
    dialog_parent: &adw::Window,
    workspace_label: &gtk4::Label,
) {
    let session_count = {
        let app_ref = app.borrow();
        if app_ref.workspace_id == id {
            app_ref.sessions.len()
        } else {
            app_ref
                .inactive_workspaces
                .iter()
                .find(|w| w.id == id)
                .map(|w| w.sessions.len())
                .unwrap_or(0)
        }
    };
    let body = if session_count == 0 {
        "No sessions will be affected.".to_string()
    } else if session_count == 1 {
        "This will close 1 session.".to_string()
    } else {
        format!("This will close {session_count} sessions.")
    };

    let confirm = adw::MessageDialog::new(
        Some(dialog_parent),
        Some(&format!("Delete workspace \"{name}\"?")),
        Some(&body),
    );
    confirm.add_response("cancel", "Cancel");
    confirm.add_response("delete", "Delete");
    confirm.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    confirm.set_default_response(Some("cancel"));
    confirm.set_close_response("cancel");
    confirm.connect_response(None, {
        let app = app.clone();
        let group = group.clone();
        let workspace_rows = workspace_rows.clone();
        let toast_overlay = toast_overlay.clone();
        let dialog_parent = dialog_parent.clone();
        let workspace_label = workspace_label.clone();
        move |_dialog, response| {
            if response == "delete" {
                if let Err(error) = App::delete_workspace(&app, id, &toast_overlay) {
                    toast_overlay.add_toast(adw::Toast::new(&error.to_string()));
                }
                sync_workspace_button(&workspace_label, &app);
                populate_workspaces(
                    &group,
                    &workspace_rows,
                    &app,
                    &toast_overlay,
                    &dialog_parent,
                    &workspace_label,
                );
            }
        }
    });
    confirm.present();
}
