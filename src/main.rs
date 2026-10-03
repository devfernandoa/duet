use duet::*;

use account::AccountStore;
use adw::prelude::*;
use app::{App, WorkspaceRuntimeState};
use gtk4::glib;
use libadwaita as adw;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use uuid::Uuid;

const APP_ID: &str = "dev.fernandoa.duet";

fn main() -> glib::ExitCode {
    // `duet agent list`/`duet agents list`/`duet send ...`/etc: the same
    // control CLI `duetctl` exposes as its own binary (see
    // `src/bin/duetctl.rs`), also reachable through `duet` itself so a
    // script that only knows the GUI binary's name still works. Dispatched
    // before GTK/GApplication ever sees argv — it needs no display and must
    // work from inside a headless agent shell.
    let args: Vec<String> = std::env::args().collect();
    if args
        .get(1)
        .is_some_and(|arg| control::is_cli_verb(arg.as_str()))
    {
        return if control::run_cli(&args[1..]) {
            glib::ExitCode::SUCCESS
        } else {
            glib::ExitCode::FAILURE
        };
    }

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
            let selected = app_ref.selected_edge;
            app_ref
                .edge_lines()
                .into_iter()
                .map(|(edge, from, to)| canvas::LinkLine {
                    from,
                    to,
                    selected: selected == Some(edge.id),
                    overlap: app_ref.edge_overlap(edge.id),
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
            App::pump_inboxes(&app);
            // Dev-server URLs `pump_output` just spotted in terminal output
            // are offered (a toast with "Open in Portal"), never opened.
            App::offer_dev_server_urls(&app);
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

    // The header answers four questions and nothing else: where am I
    // (workspace, and the project folder under the title), what repository
    // state am I in (the Git control), how do I add something (+), and where
    // is everything else (the ⋯ menu). Every command still has its action
    // and shortcut; only the permanent buttons went away.
    let header = adw::HeaderBar::new();
    let workspace_label = gtk4::Label::new(None);
    workspace_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    workspace_label.set_max_width_chars(18);
    let workspace_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    workspace_box.append(&gtk4::Image::from_icon_name("view-paged-symbolic"));
    workspace_box.append(&workspace_label);
    workspace_box.append(&gtk4::Image::from_icon_name("pan-down-symbolic"));
    let workspace_button = gtk4::Button::new();
    workspace_button.set_child(Some(&workspace_box));
    workspace_button.add_css_class("flat");
    workspace_button.set_tooltip_text(Some(
        "Workspace — switch, create or rename (Ctrl+1…9 to jump)",
    ));
    header.pack_start(&workspace_button);
    let git_button = app::scm::build_git_button(&app, &window);
    header.pack_start(&git_button);

    let window_title = adw::WindowTitle::new("Duet", "");
    header.set_title_widget(Some(&window_title));

    let more_button = gtk4::MenuButton::new();
    more_button.set_icon_name("open-menu-symbolic");
    more_button.set_tooltip_text(Some("Main menu (F10)"));
    more_button.set_primary(true);
    header.pack_end(&more_button);
    let add_button = gtk4::MenuButton::new();
    add_button.set_icon_name("list-add-symbolic");
    add_button.set_tooltip_text(Some(
        "Add a card (or right-click the canvas to add it there)",
    ));
    header.pack_end(&add_button);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&app.borrow().canvas.overlay));

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&toolbar_view));
    window.set_content(Some(&toast_overlay));
    app.borrow_mut().set_toast_overlay(&toast_overlay);

    // Esc leaves "connect" mode. Capture phase on the window, so it works
    // even while a terminal has focus (and only consumes Esc in that mode).
    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        move |_controller, key, _code, _modifiers| {
            if key == gtk4::gdk::Key::Escape && App::cancel_link(&app) {
                toast_overlay.add_toast(adw::Toast::new("Connecting cancelled"));
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);

    // Link lines are drawn, not widgets, so deleting one needs a hit test
    // against the curve rather than a click on a control: one click selects
    // (the line thickens and turns orange), a second click on the same line
    // deletes it. `App::remove_link` existed from the start but had no UI
    // path to it at all until now.
    app.borrow().canvas.connect_background_click({
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        move |world| {
            // A click on empty canvas in "connect" mode cancels it.
            if !app.borrow().covers_point(world) && App::cancel_link(&app) {
                toast_overlay.add_toast(adw::Toast::new("Connecting cancelled"));
                return;
            }
            if let Some(message) = App::click_link_at(&app, world) {
                toast_overlay.add_toast(adw::Toast::new(&message));
            } else if !app.borrow().covers_point(world) {
                // A plain click that hit truly empty canvas (not a link, and
                // not a card either — `click_link_at` returns `None` for
                // both, since it skips link hit-testing entirely when a card
                // covers the click point): the usual "click empty space to
                // clear the selection" canvas convention. The `covers_point`
                // check is what keeps this from also firing — and wiping the
                // selection — on every click into a card itself: a title-bar
                // button, a drag-to-move press, or a click into a note's own
                // text view to start editing it.
                App::deselect_all(&app);
            }
        }
    });

    // Right-click on empty canvas: create a card right where you clicked.
    // (A right-click on a card is the card's business — its title bar has
    // its own menu, and a terminal or editor may use right-click itself.)
    let canvas_menu = gtk4::Popover::new();
    canvas_menu.set_has_arrow(false);
    canvas_menu.set_position(gtk4::PositionType::Bottom);
    canvas_menu.set_halign(gtk4::Align::Start);
    canvas_menu.set_parent(&app.borrow().canvas.fixed);
    app.borrow().canvas.connect_background_context_menu({
        let app = app.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        move |screen, world| {
            if app.borrow().covers_point(world) {
                return;
            }
            App::cancel_link(&app);
            canvas_menu.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(
                screen.0 as i32,
                screen.1 as i32,
                1,
                1,
            )));
            let mut items: Vec<app::MenuItem> = Vec::new();
            {
                let (app, window, toast_overlay) =
                    (app.clone(), window.clone(), toast_overlay.clone());
                items.push((
                    "New terminal…".to_string(),
                    Box::new(move || {
                        open_new_session_dialog(&app, &window, &toast_overlay, Some(world))
                    }),
                ));
            }
            let app_c = app.clone();
            items.push((
                "New note".to_string(),
                Box::new(move || App::create_note(&app_c, world)),
            ));
            let app_c = app.clone();
            items.push((
                "New text".to_string(),
                Box::new(move || App::create_text_node(&app_c, world)),
            ));
            let (app_c, toast_c) = (app.clone(), toast_overlay.clone());
            items.push((
                "New file tree".to_string(),
                Box::new(move || {
                    if let Err(error) = App::create_file_tree(&app_c, world) {
                        toast_c.add_toast(adw::Toast::new(&error));
                    }
                }),
            ));
            let (app_c, toast_c) = (app.clone(), toast_overlay.clone());
            items.push((
                "New browser portal".to_string(),
                Box::new(move || {
                    if let Err(error) = App::create_portal(&app_c, None, "", world) {
                        toast_c.add_toast(adw::Toast::new(&error));
                    }
                }),
            ));
            let app_c = app.clone();
            items.push((
                "New drawing".to_string(),
                Box::new(move || {
                    App::create_drawing(&app_c, world);
                }),
            ));
            items.push(app::separator());
            let app_c = app.clone();
            items.push((
                "New group section".to_string(),
                Box::new(move || {
                    App::create_group(&app_c, world);
                }),
            ));
            app::popup_menu(&canvas_menu, items);
        }
    });

    // Shift-drag on empty canvas (see `canvas.rs`'s pan gesture) marks out a
    // marquee; releasing it selects every node the rectangle touches.
    app.borrow().canvas.connect_marquee_end({
        let app = app.clone();
        move |start, current| App::apply_marquee_selection(&app, start, current)
    });

    // Restore after the toast overlay exists: `restore`'s own load/spawn
    // errors are toasted below, and card menus report through it too.
    let mut errors = App::restore(&app, &toast_overlay);
    app.borrow().refresh_empty_hint();
    sync_workspace_button(&workspace_label, &app);
    // Writes back immediately rather than waiting for the first edit, so a
    // migration from an older single-canvas store.json (see `store::Store`'s
    // pre-workspace migration) is durable on disk right away instead of only
    // in memory until something happens to trigger a save.
    let _ = app.borrow().persist();

    // The agent-messaging control socket (`duet agent list`/`duet agent
    // send`): accepted on a background thread, but answered here on the
    // main thread via the same timer-poll shape `pump_output` already uses
    // above — `App`'s session map and GTK widgets aren't `Send`, so a
    // request from another thread can only be handled by polling for it.
    let (control_tx, control_rx) = std::sync::mpsc::channel::<control::ControlEvent>();
    if let Err(error) = control::spawn_server(control_tx) {
        errors.push(format!("agent messaging is unavailable: {error}"));
    }
    // Milestone 6: file-backed notes and editors follow their project files
    // (and FileTrees their directories/Git status) by polling through
    // `ProjectFilesystem` — cheap metadata checks, hashing only on change —
    // rather than an inotify watcher, so the same loop works unchanged once a
    // project lives behind SSH/Docker.
    glib::timeout_add_local(Duration::from_millis(1000), {
        let app = app.clone();
        move || {
            App::sync_project_files(&app);
            glib::ControlFlow::Continue
        }
    });
    app::install_canvas_drop(&app, &toast_overlay);

    glib::timeout_add_local(Duration::from_millis(150), {
        let app = app.clone();
        move || {
            while let Ok(event) = control_rx.try_recv() {
                control::dispatch(&app, event);
            }
            glib::ControlFlow::Continue
        }
    });

    let workspaces_action = gtk4::gio::SimpleAction::new("manage-workspaces", None);
    workspaces_action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        let workspace_label = workspace_label.clone();
        move |_, _| open_workspace_switcher_dialog(&app, &window, &toast_overlay, &workspace_label)
    });
    application.add_action(&workspaces_action);
    workspace_button.set_action_name(Some("app.manage-workspaces"));

    let note_action = gtk4::gio::SimpleAction::new("new-note", None);
    note_action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        move |_, _| App::create_note(&app, centered_in_view(&app, &window, (220.0, 160.0)))
    });
    application.add_action(&note_action);

    let action = gtk4::gio::SimpleAction::new("new-session", None);
    action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| open_new_session_dialog(&app, &window, &toast_overlay, None)
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

    let menus = wire_canvas_edit_actions(application, &app, &window, &toast_overlay);
    add_button.set_menu_model(Some(&menus.add));
    more_button.set_menu_model(Some(&menus.more));
    wire_help_actions(application, &window);

    // The project folder under the title. Workspace activation notifies
    // the Git listeners (a new workspace may be a new repository), which is
    // exactly when this can change.
    let update_subtitle: Rc<dyn Fn()> = Rc::new({
        let app = app.clone();
        let window_title = window_title.clone();
        move || {
            let app_ref = app.borrow();
            let root = app_ref.workspace_root.display().to_string();
            let home = std::env::var("HOME").unwrap_or_default();
            let shown = match root.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => root,
            };
            if window_title.subtitle() != shown {
                window_title.set_subtitle(&shown);
            }
            window_title.set_tooltip_text(Some(&format!(
                "Workspace {} in {shown}",
                app_ref.workspace_name
            )));
        }
    });
    update_subtitle();
    app.borrow_mut().connect_git_state_changed({
        let update_subtitle = Rc::clone(&update_subtitle);
        move || update_subtitle()
    });

    window.present();

    for error in errors {
        toast_overlay.add_toast(adw::Toast::new(&error));
    }
}

/// The `(width, height)` of the window's current allocation, in screen
/// pixels — what `App::zoom_to_selection`/`zoom_to_fit` fit the canvas into.
fn viewport_size(window: &adw::ApplicationWindow) -> (f64, f64) {
    let (width, height) = (window.width(), window.height());
    if width > 0 && height > 0 {
        (width as f64, height as f64)
    } else {
        (1200.0, 800.0)
    }
}

/// Where a card of `size` created from a menu (rather than dropped at a
/// specific spot) goes: centered in the current view, title bar included.
fn centered_in_view(
    app: &Rc<RefCell<App>>,
    window: &adw::ApplicationWindow,
    size: (f64, f64),
) -> (f64, f64) {
    let (x, y) = viewport_center_world(app, window);
    (
        x - size.0 / 2.0,
        y - (size.1 + canvas::TITLE_BAR_HEIGHT) / 2.0,
    )
}

/// The world-space point under the center of the canvas (the window
/// minus its header, when the canvas is allocated).
fn viewport_center_world(app: &Rc<RefCell<App>>, window: &adw::ApplicationWindow) -> (f64, f64) {
    let app_ref = app.borrow();
    let fixed = &app_ref.canvas.fixed;
    let (width, height) = if fixed.width() > 0 && fixed.height() > 0 {
        (fixed.width() as f64, fixed.height() as f64)
    } else {
        viewport_size(window)
    };
    let state = app_ref.canvas.state.borrow();
    canvas::screen_to_world((width / 2.0, height / 2.0), state.pan, state.zoom)
}

/// Registers every selection/multi-node/layout/undo-redo/canvas-navigation
/// and card-creation command as a `gio::SimpleAction` on `application`, and
/// returns the header's two menus listing them. Selection/edit commands
/// deliberately carry no keyboard accelerator of their own: a card's
/// terminal takes the keyboard, and the obvious shortcuts — Ctrl+Z
/// (suspend), Ctrl+C (interrupt), Ctrl+A (line start), Delete — are exactly
/// the keys a running shell or agent most needs to receive untouched.
/// (action name, menu label, handler) for a command that takes no extra UI
/// input beyond `app` itself.
type SimpleEditAction<'a> = (&'a str, &'a str, Box<dyn Fn(&Rc<RefCell<App>>)>);

struct HeaderMenus {
    /// "+": every kind of card.
    add: gtk4::gio::Menu,
    /// "⋯": edit, arrange, view, workspace/agents/accounts, help.
    more: gtk4::gio::Menu,
}

fn wire_canvas_edit_actions(
    application: &adw::Application,
    app: &Rc<RefCell<App>>,
    window: &adw::ApplicationWindow,
    toast_overlay: &adw::ToastOverlay,
) -> HeaderMenus {
    // `new-text-node`, `new-file-tree`, `new-portal` and the two placeholder-kind creators, which need a
    // spawn position, and `toggle-snap-to-grid`, which needs a toast
    // describing its new state, are registered separately below instead of
    // forced into this shape.
    let simple_actions: Vec<SimpleEditAction> = vec![
        ("select-all", "Select All", Box::new(App::select_all)),
        ("deselect-all", "Deselect All", Box::new(App::deselect_all)),
        (
            "delete-selected",
            "Delete Selected",
            Box::new(|app| {
                let ids: Vec<Uuid> = app.borrow().selected.iter().copied().collect();
                App::request_delete(app, ids);
            }),
        ),
        (
            "lock-selected",
            "Lock Selected",
            Box::new(|app| App::set_selected_locked(app, true)),
        ),
        (
            "unlock-selected",
            "Unlock Selected",
            Box::new(|app| App::set_selected_locked(app, false)),
        ),
        (
            "collapse-selected",
            "Collapse Selected",
            Box::new(|app| App::set_selected_collapsed(app, true)),
        ),
        (
            "expand-selected",
            "Expand Selected",
            Box::new(|app| App::set_selected_collapsed(app, false)),
        ),
        (
            "raise-selected",
            "Bring to Front",
            Box::new(App::raise_selected),
        ),
        (
            "lower-selected",
            "Send to Back",
            Box::new(App::lower_selected),
        ),
        (
            "terminate-selected-terminals",
            "Terminate Terminal",
            Box::new(App::terminate_selected_terminals),
        ),
        ("align-left", "Align Left", Box::new(App::align_left)),
        ("align-right", "Align Right", Box::new(App::align_right)),
        ("align-top", "Align Top", Box::new(App::align_top)),
        ("align-bottom", "Align Bottom", Box::new(App::align_bottom)),
        ("copy-selected", "Copy", Box::new(App::copy_selected)),
    ];

    let selection_section = gtk4::gio::Menu::new();
    let layout_section = gtk4::gio::Menu::new();
    let clipboard_section = gtk4::gio::Menu::new();
    let view_section = gtk4::gio::Menu::new();

    for (name, label, handler) in simple_actions {
        let action = gtk4::gio::SimpleAction::new(name, None);
        action.connect_activate({
            let app = app.clone();
            move |_, _| handler(&app)
        });
        application.add_action(&action);
        let item = gtk4::gio::MenuItem::new(Some(label), Some(&format!("app.{name}")));
        let section = match name {
            "select-all"
            | "deselect-all"
            | "delete-selected"
            | "lock-selected"
            | "unlock-selected"
            | "collapse-selected"
            | "expand-selected"
            | "raise-selected"
            | "lower-selected"
            | "terminate-selected-terminals" => &selection_section,
            "align-left" | "align-right" | "align-top" | "align-bottom" => &layout_section,
            "copy-selected" => &clipboard_section,
            _ => &selection_section,
        };
        section.append_item(&item);
    }

    // Restarting a terminal needs `toast_overlay` the same way Duplicate and
    // Paste do: a failed respawn surfaces a toast rather than failing
    // silently. Lives in the selection section despite being registered
    // here, since it (like terminate, above) operates on the selection.
    let restart_action = gtk4::gio::SimpleAction::new("restart-selected-terminals", None);
    restart_action.connect_activate({
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| App::restart_selected_terminals(&app, &toast_overlay)
    });
    application.add_action(&restart_action);
    selection_section.append_item(&gtk4::gio::MenuItem::new(
        Some("Restart Terminal"),
        Some("app.restart-selected-terminals"),
    ));

    // Distribute needs `toast_overlay` too: below 3 selected nodes it's a
    // documented no-op (nothing "in between" two fixed ends to
    // redistribute), and a silent no-op otherwise reads as a broken command.
    let distribute_h_action = gtk4::gio::SimpleAction::new("distribute-horizontal", None);
    distribute_h_action.connect_activate({
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| App::distribute_horizontal(&app, &toast_overlay)
    });
    application.add_action(&distribute_h_action);
    layout_section.append_item(&gtk4::gio::MenuItem::new(
        Some("Distribute Horizontally"),
        Some("app.distribute-horizontal"),
    ));

    let distribute_v_action = gtk4::gio::SimpleAction::new("distribute-vertical", None);
    distribute_v_action.connect_activate({
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| App::distribute_vertical(&app, &toast_overlay)
    });
    application.add_action(&distribute_v_action);
    layout_section.append_item(&gtk4::gio::MenuItem::new(
        Some("Distribute Vertically"),
        Some("app.distribute-vertical"),
    ));

    // Duplicate and Paste need `toast_overlay` (a failed terminal respawn
    // surfaces a toast the same way creating one fresh does).
    let duplicate_action = gtk4::gio::SimpleAction::new("duplicate-selected", None);
    duplicate_action.connect_activate({
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| App::duplicate_selected(&app, &toast_overlay)
    });
    application.add_action(&duplicate_action);
    clipboard_section.append_item(&gtk4::gio::MenuItem::new(
        Some("Duplicate"),
        Some("app.duplicate-selected"),
    ));

    let paste_action = gtk4::gio::SimpleAction::new("paste-clipboard", None);
    paste_action.connect_activate({
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| App::paste_clipboard(&app, &toast_overlay)
    });
    application.add_action(&paste_action);
    clipboard_section.append_item(&gtk4::gio::MenuItem::new(
        Some("Paste"),
        Some("app.paste-clipboard"),
    ));

    // Undo/redo need `toast_overlay` too (a future variant might report
    // "nothing to undo"; today they're silent no-ops on an empty stack).
    let undo_action = gtk4::gio::SimpleAction::new("undo", None);
    undo_action.connect_activate({
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| App::undo(&app, &toast_overlay)
    });
    application.add_action(&undo_action);

    let redo_action = gtk4::gio::SimpleAction::new("redo", None);
    redo_action.connect_activate({
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| App::redo(&app, &toast_overlay)
    });
    application.add_action(&redo_action);

    // Zoom-to-selection/fit need the window's current size.
    let zoom_selection_action = gtk4::gio::SimpleAction::new("zoom-to-selection", None);
    zoom_selection_action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        move |_, _| App::zoom_to_selection(&app, viewport_size(&window))
    });
    application.add_action(&zoom_selection_action);
    view_section.append_item(&gtk4::gio::MenuItem::new(
        Some("Zoom to Selection"),
        Some("app.zoom-to-selection"),
    ));

    let zoom_fit_action = gtk4::gio::SimpleAction::new("zoom-to-fit", None);
    zoom_fit_action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        move |_, _| App::zoom_to_fit(&app, viewport_size(&window))
    });
    application.add_action(&zoom_fit_action);
    view_section.append_item(&gtk4::gio::MenuItem::new(
        Some("Zoom to Fit"),
        Some("app.zoom-to-fit"),
    ));

    let snap_action = gtk4::gio::SimpleAction::new("toggle-snap-to-grid", None);
    snap_action.connect_activate({
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| {
            let enabled = {
                let mut app_mut = app.borrow_mut();
                app_mut.snap_to_grid = !app_mut.snap_to_grid;
                app_mut.snap_to_grid
            };
            toast_overlay.add_toast(adw::Toast::new(if enabled {
                "Snap to grid on"
            } else {
                "Snap to grid off"
            }));
        }
    });
    application.add_action(&snap_action);
    view_section.append_item(&gtk4::gio::MenuItem::new(
        Some("Toggle Snap to Grid"),
        Some("app.toggle-snap-to-grid"),
    ));

    // New cards appear in the middle of the view.
    let text_action = gtk4::gio::SimpleAction::new("new-text-node", None);
    text_action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        move |_, _| App::create_text_node(&app, centered_in_view(&app, &window, (220.0, 160.0)))
    });
    application.add_action(&text_action);

    let file_tree_action = gtk4::gio::SimpleAction::new("new-file-tree", None);
    file_tree_action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| {
            if let Err(error) =
                App::create_file_tree(&app, centered_in_view(&app, &window, (320.0, 460.0)))
            {
                toast_overlay.add_toast(adw::Toast::new(&error));
            }
        }
    });
    application.add_action(&file_tree_action);

    let portal_action = gtk4::gio::SimpleAction::new("new-portal", None);
    portal_action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, _| {
            if let Err(error) = App::create_portal(
                &app,
                None,
                "",
                centered_in_view(&app, &window, (640.0, 480.0)),
            ) {
                toast_overlay.add_toast(adw::Toast::new(&error));
            }
        }
    });
    application.add_action(&portal_action);

    let drawing_action = gtk4::gio::SimpleAction::new("new-drawing", None);
    drawing_action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        move |_, _| {
            App::create_drawing(&app, centered_in_view(&app, &window, (420.0, 300.0)));
        }
    });
    application.add_action(&drawing_action);

    let group_action = gtk4::gio::SimpleAction::new("new-group", None);
    group_action.connect_activate({
        let app = app.clone();
        let window = window.clone();
        move |_, _| {
            App::create_group(&app, centered_in_view(&app, &window, (720.0, 460.0)));
        }
    });
    application.add_action(&group_action);

    let add = gtk4::gio::Menu::new();
    let agents_section = gtk4::gio::Menu::new();
    agents_section.append(Some("Agent or Terminal…"), Some("app.new-session"));
    add.append_section(None, &agents_section);
    let cards_section = gtk4::gio::Menu::new();
    cards_section.append(Some("Note"), Some("app.new-note"));
    cards_section.append(Some("Text"), Some("app.new-text-node"));
    cards_section.append(Some("File Tree"), Some("app.new-file-tree"));
    cards_section.append(Some("Browser Portal"), Some("app.new-portal"));
    cards_section.append(Some("Drawing"), Some("app.new-drawing"));
    add.append_section(None, &cards_section);
    let canvas_section = gtk4::gio::Menu::new();
    canvas_section.append(Some("Group Section"), Some("app.new-group"));
    add.append_section(None, &canvas_section);

    // Zoom commands first in the view section; they have shortcuts, which
    // the menu shows.
    view_section.prepend(Some("Reset Zoom"), Some("app.zoom-reset"));
    view_section.prepend(Some("Zoom Out"), Some("app.zoom-out"));
    view_section.prepend(Some("Zoom In"), Some("app.zoom-in"));

    let more = gtk4::gio::Menu::new();
    let edit_section = gtk4::gio::Menu::new();
    edit_section.append_item(&gtk4::gio::MenuItem::new(Some("Undo"), Some("app.undo")));
    edit_section.append_item(&gtk4::gio::MenuItem::new(Some("Redo"), Some("app.redo")));
    let selection_menu = gtk4::gio::Menu::new();
    selection_menu.append_section(None, &selection_section);
    selection_menu.append_section(None, &clipboard_section);
    edit_section.append_submenu(Some("Selection"), &selection_menu);
    edit_section.append_submenu(Some("Arrange"), &layout_section);
    more.append_section(None, &edit_section);
    more.append_section(None, &view_section);
    let manage_section = gtk4::gio::Menu::new();
    manage_section.append(Some("Workspaces…"), Some("app.manage-workspaces"));
    manage_section.append(Some("Agent Roles…"), Some("app.manage-roles"));
    manage_section.append(Some("Claude Accounts…"), Some("app.manage-accounts"));
    more.append_section(None, &manage_section);
    let help_section = gtk4::gio::Menu::new();
    help_section.append(Some("Keyboard Shortcuts"), Some("app.shortcuts"));
    help_section.append(Some("About Duet"), Some("app.about"));
    more.append_section(None, &help_section);
    HeaderMenus { add, more }
}

/// "Keyboard Shortcuts" and "About Duet".
fn wire_help_actions(application: &adw::Application, window: &adw::ApplicationWindow) {
    let shortcuts = gtk4::gio::SimpleAction::new("shortcuts", None);
    shortcuts.connect_activate({
        let window = window.clone();
        move |_, _| open_shortcuts_dialog(&window)
    });
    application.add_action(&shortcuts);
    application.set_accels_for_action("app.shortcuts", &["<Ctrl>question"]);

    let about = gtk4::gio::SimpleAction::new("about", None);
    about.connect_activate({
        let window = window.clone();
        move |_, _| {
            let about = adw::AboutWindow::builder()
                .transient_for(&window)
                .modal(true)
                .application_name("Duet")
                .application_icon("utilities-terminal-symbolic")
                .version(env!("CARGO_PKG_VERSION"))
                .comments(
                    "A spatial canvas for running and orchestrating coding agents side by side, \
                     with notes, files, Git and browser portals.",
                )
                .license_type(gtk4::License::MitX11)
                .website("https://github.com/devfernandoa/duet")
                .issue_url("https://github.com/devfernandoa/duet/issues")
                .build();
            about.present();
        }
    });
    application.add_action(&about);
}

/// The keyboard and pointer shortcuts, grouped, in one scrollable window.
fn open_shortcuts_dialog(parent: &adw::ApplicationWindow) {
    let groups: &[(&str, &[(&str, &str)])] = &[
        (
            "Canvas",
            &[
                ("Ctrl+T", "New agent or terminal"),
                ("Right-click empty canvas", "Add a card right there"),
                ("Drag empty canvas", "Pan"),
                ("Mouse wheel over empty canvas", "Zoom around the pointer"),
                ("Ctrl+wheel (anywhere) / pinch", "Zoom"),
                ("Two-finger touchpad scroll", "Pan"),
                ("Ctrl++ / Ctrl+- / Ctrl+0", "Zoom in / out / reset"),
                ("Shift+drag empty canvas", "Select with a rectangle"),
                (
                    "Ctrl/Shift+click a card",
                    "Add to or remove from the selection",
                ),
                ("Esc", "Cancel connecting two cards"),
            ],
        ),
        (
            "Cards",
            &[
                (
                    "Drag the title bar",
                    "Move (all selected cards move together)",
                ),
                ("Drag the corner grip", "Resize"),
                ("Right-click the title bar", "Card menu"),
                ("Double-click a title", "Rename"),
            ],
        ),
        (
            "Editor",
            &[
                ("Ctrl+S", "Save"),
                ("Ctrl+F / Ctrl+H", "Find / replace"),
                ("Ctrl+G", "Go to line"),
            ],
        ),
        (
            "Application",
            &[
                ("Ctrl+1 … Ctrl+9", "Switch to workspace 1–9"),
                ("Ctrl+Shift+R", "Agent roles"),
                ("Ctrl+.", "Claude accounts"),
                ("F10", "Main menu"),
                ("Ctrl+?", "This window"),
            ],
        ),
    ];
    let page = adw::PreferencesPage::new();
    for (title, rows) in groups {
        let group = adw::PreferencesGroup::new();
        group.set_title(title);
        for (keys, action) in *rows {
            let row = adw::ActionRow::new();
            row.set_title(action);
            let key_label = gtk4::Label::new(Some(keys));
            key_label.add_css_class("dim-label");
            key_label.add_css_class("monospace");
            row.add_suffix(&key_label);
            group.add(&row);
        }
        page.add(&group);
    }
    let header = adw::HeaderBar::new();
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&page));
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(520)
        .default_height(620)
        .title("Keyboard Shortcuts")
        .content(&toolbar_view)
        .build();
    let keys = gtk4::EventControllerKey::new();
    keys.connect_key_pressed({
        let dialog = dialog.clone();
        move |_controller, key, _code, _modifiers| {
            if key == gtk4::gdk::Key::Escape {
                dialog.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    dialog.add_controller(keys);
    dialog.present();
}

/// `position`: where on the canvas (world coordinates) the new terminal
/// goes; `None` for the middle of the current view.
fn open_new_session_dialog(
    app: &Rc<RefCell<App>>,
    parent: &adw::ApplicationWindow,
    toast_overlay: &adw::ToastOverlay,
    position: Option<(f64, f64)>,
) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(440)
        .title("New session")
        .build();

    let header = adw::HeaderBar::new();
    header.set_show_end_title_buttons(false);
    header.set_show_start_title_buttons(false);
    let cancel_button = gtk4::Button::with_label("Cancel");
    header.pack_start(&cancel_button);
    let create_button = gtk4::Button::with_label("Create");
    create_button.add_css_class("suggested-action");
    // Starts disabled: the name field starts empty, which `update_validity`
    // (below) already treats as invalid — there is no moment where the
    // button is clickable before the form has been looked at once.
    create_button.set_sensitive(false);
    header.pack_end(&create_button);
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);

    let name_row = adw::EntryRow::new();
    name_row.set_title("Session name");
    name_row.set_activates_default(true);
    let name_warning = gtk4::Image::from_icon_name("dialog-warning-symbolic");
    name_warning.add_css_class("warning");
    name_warning.set_visible(false);
    name_row.add_suffix(&name_warning);

    // The active workspace's root directory, not just the process's cwd —
    // that's the whole point of a per-workspace default.
    let cwd_row = adw::EntryRow::new();
    cwd_row.set_title("Working directory");
    cwd_row.set_text(app.borrow().workspace_root.display().to_string().as_str());
    let cwd_warning = gtk4::Image::from_icon_name("dialog-warning-symbolic");
    cwd_warning.add_css_class("warning");
    cwd_warning.set_visible(false);
    cwd_row.add_suffix(&cwd_warning);
    let browse_button = gtk4::Button::from_icon_name("folder-open-symbolic");
    browse_button.add_css_class("flat");
    browse_button.set_valign(gtk4::Align::Center);
    browse_button.set_tooltip_text(Some("Choose a working directory"));
    cwd_row.add_suffix(&browse_button);

    let session_group = adw::PreferencesGroup::new();
    session_group.set_title("Session");
    session_group.add(&name_row);
    session_group.add(&cwd_row);

    // This order (index 0-4) is matched by index in two places below:
    // `sync_field_visibility`'s `selected() == 0`/`== 4` checks, and the
    // `try_create` closure's `match agent_row.selected()`.
    // Reordering these strings means updating both.
    let agent_row = adw::ComboRow::new();
    agent_row.set_title("Agent");
    agent_row.set_model(Some(&gtk4::StringList::new(&[
        "Claude",
        "Codex",
        "OpenCode",
        "Shell",
        "Custom command",
    ])));

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
    let account_row = adw::ComboRow::new();
    account_row.set_title("Claude account");
    account_row.set_model(Some(&gtk4::StringList::new(
        &account_names.iter().map(String::as_str).collect::<Vec<_>>(),
    )));

    let custom_command_row = adw::EntryRow::new();
    custom_command_row.set_title("Command (e.g. mytool --flag value)");
    custom_command_row.set_visible(false);

    // Role picker: index 0 is always "No role" (`role_ids[0] == None`), then
    // every built-in role followed by every custom one, in `App::roles`'
    // order.
    let roles = app.borrow().roles();
    let mut role_names: Vec<String> = vec!["No role".to_string()];
    role_names.extend(roles.iter().map(|role| role.name.clone()));
    let role_ids: Vec<Option<Uuid>> = std::iter::once(None)
        .chain(roles.iter().map(|role| Some(role.id)))
        .collect();
    let role_row = adw::ComboRow::new();
    role_row.set_title("Role");
    role_row.set_model(Some(&gtk4::StringList::new(
        &role_names.iter().map(String::as_str).collect::<Vec<_>>(),
    )));

    let agent_group = adw::PreferencesGroup::new();
    agent_group.set_title("Agent");
    agent_group.add(&agent_row);
    agent_group.add(&account_row);
    agent_group.add(&custom_command_row);
    agent_group.add(&role_row);

    let page_box = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    page_box.set_margin_top(16);
    page_box.set_margin_bottom(16);
    page_box.set_margin_start(16);
    page_box.set_margin_end(16);
    page_box.append(&session_group);
    page_box.append(&agent_group);
    toolbar_view.set_content(Some(&page_box));
    dialog.set_content(Some(&toolbar_view));

    // The account picker only matters for Claude; the command field only for
    // a custom provider. Both stay in the layout (hidden/insensitive, not
    // removed) rather than being added/removed, so the dialog doesn't jump
    // around as the user changes the agent picker.
    let sync_field_visibility = {
        let agent_row = agent_row.clone();
        let account_row = account_row.clone();
        let custom_command_row = custom_command_row.clone();
        move || {
            let is_claude = agent_row.selected() == 0;
            account_row.set_sensitive(is_claude);
            custom_command_row.set_visible(agent_row.selected() == 4);
        }
    };
    sync_field_visibility();
    agent_row.connect_selected_notify(move |_| sync_field_visibility());

    // Live feedback instead of a failed click: a taken name or a directory
    // that doesn't exist is flagged on the field itself (red outline plus a
    // warning icon with the reason in its tooltip) and the Create button is
    // simply not clickable until both clear, rather than letting the user
    // submit and then telling them it didn't work.
    let update_validity = {
        let app = app.clone();
        let name_row = name_row.clone();
        let name_warning = name_warning.clone();
        let cwd_row = cwd_row.clone();
        let cwd_warning = cwd_warning.clone();
        let create_button = create_button.clone();
        move || {
            let name = name_row.text().trim().to_string();
            let name_taken = !name.is_empty()
                && app
                    .borrow()
                    .nodes
                    .values()
                    .any(|entry| entry.record.as_terminal().is_some_and(|t| t.name == name));
            name_row.set_css_classes(if name_taken { &["error"] } else { &[] });
            name_warning.set_visible(name_taken);
            name_warning
                .set_tooltip_text(name_taken.then_some("A session with this name already exists"));

            let cwd_text = cwd_row.text().to_string();
            let cwd_valid = PathBuf::from(&cwd_text).is_dir();
            cwd_row.set_css_classes(if cwd_valid { &[] } else { &["error"] });
            cwd_warning.set_visible(!cwd_valid);
            cwd_warning.set_tooltip_text((!cwd_valid).then_some("This directory does not exist"));

            create_button.set_sensitive(!name.is_empty() && !name_taken && cwd_valid);
        }
    };
    update_validity();
    name_row.connect_changed({
        let update_validity = update_validity.clone();
        move |_| update_validity()
    });
    cwd_row.connect_changed(move |_| update_validity());

    browse_button.connect_clicked({
        let dialog = dialog.clone();
        let cwd_row = cwd_row.clone();
        move |_| {
            let file_dialog = gtk4::FileDialog::builder()
                .title("Choose working directory")
                .build();
            let current = PathBuf::from(cwd_row.text().to_string());
            if current.is_dir() {
                file_dialog.set_initial_folder(Some(&gtk4::gio::File::for_path(&current)));
            }
            let cwd_row = cwd_row.clone();
            file_dialog.select_folder(Some(&dialog), gtk4::gio::Cancellable::NONE, move |result| {
                if let Ok(folder) = result
                    && let Some(path) = folder.path()
                {
                    cwd_row.set_text(&path.display().to_string());
                }
            });
        }
    });

    cancel_button.connect_clicked({
        let dialog = dialog.clone();
        move |_| dialog.close()
    });

    let try_create = {
        let app = app.clone();
        let dialog = dialog.clone();
        let parent = parent.clone();
        let name_row = name_row.clone();
        let cwd_row = cwd_row.clone();
        let agent_row = agent_row.clone();
        let account_row = account_row.clone();
        let custom_command_row = custom_command_row.clone();
        let role_row = role_row.clone();
        let toast_overlay = toast_overlay.clone();
        move || {
            let agent = match agent_row.selected() {
                0 => agent::Agent::Claude,
                1 => agent::Agent::Codex,
                2 => agent::Agent::OpenCode,
                3 => agent::Agent::Shell,
                _ => {
                    // Naive whitespace splitting, not a shell-quoting parser —
                    // "simple command configurations," per this milestone's
                    // own scope, not a full command-line grammar.
                    let command_text = custom_command_row.text();
                    let mut parts = command_text.split_whitespace().map(str::to_string);
                    let program = parts.next().unwrap_or_default();
                    let args = parts.collect();
                    agent::Agent::Custom { program, args }
                }
            };
            // Ignored entirely by every non-Claude path; only read when
            // `agent` is `Agent::Claude`.
            let claude_account = account_names.get(account_row.selected() as usize).cloned();
            let role_id = role_ids
                .get(role_row.selected() as usize)
                .copied()
                .flatten();
            let viewport_center = position.unwrap_or_else(|| {
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
                let center = canvas::screen_to_world(screen_center, state.pan, state.zoom);
                // Centered on the view (a new terminal is 720x504).
                (
                    center.0 - 360.0,
                    center.1 - (504.0 + canvas::TITLE_BAR_HEIGHT) / 2.0,
                )
            });
            let result = App::create_session(
                &app,
                name_row.text().to_string(),
                PathBuf::from(cwd_row.text().to_string()),
                agent,
                claude_account,
                role_id,
                viewport_center,
                &toast_overlay,
            );
            match result {
                Ok(_id) => dialog.close(),
                Err(error) => toast_overlay.add_toast(adw::Toast::new(&error.to_string())),
            }
        }
    };
    create_button.connect_clicked({
        let try_create = try_create.clone();
        move |_| try_create()
    });
    // The fields themselves still fall back to validation state rather than
    // actually submitting when invalid — `activates_default` fires this on
    // Enter, but the button's own sensitivity already guards `try_create`'s
    // precondition, so a disabled Create just does nothing here too.
    name_row.connect_entry_activated({
        let create_button = create_button.clone();
        let try_create = try_create.clone();
        move |_| {
            if create_button.is_sensitive() {
                try_create();
            }
        }
    });
    cwd_row.connect_entry_activated(move |_| {
        if create_button.is_sensitive() {
            try_create();
        }
    });

    dialog.present();
    name_row.grab_focus();
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
        .nodes
        .values()
        .filter(|entry| {
            entry
                .record
                .as_terminal()
                .is_some_and(|t| t.claude_account.as_deref() == Some(name))
        })
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
            open_role_editor_dialog(&app, &dialog, &toast_overlay, None, {
                let app = app.clone();
                let roles_group = roles_group.clone();
                let role_rows = role_rows.clone();
                let toast_overlay = toast_overlay.clone();
                let dialog = dialog.clone();
                move || populate_roles(&roles_group, &role_rows, &app, &toast_overlay, &dialog)
            });
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
                    open_role_editor_dialog(
                        &app,
                        &dialog_parent,
                        &toast_overlay,
                        Some(role.clone()),
                        {
                            let app = app.clone();
                            let group = group.clone();
                            let role_rows = role_rows.clone();
                            let toast_overlay = toast_overlay.clone();
                            let dialog_parent = dialog_parent.clone();
                            move || {
                                populate_roles(
                                    &group,
                                    &role_rows,
                                    &app,
                                    &toast_overlay,
                                    &dialog_parent,
                                )
                            }
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
        .nodes
        .values()
        .filter(|entry| {
            entry
                .record
                .as_terminal()
                .is_some_and(|t| t.role_id == Some(id))
        })
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
            let icon_text = icon_entry.text().trim().to_string();
            let icon = (!icon_text.is_empty()).then_some(icon_text);
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
        let runtime_state = app.borrow().workspace_runtime_state(id);
        let row = adw::ActionRow::new();
        row.set_title(&name);
        if id == active_id {
            row.set_subtitle("Current workspace");
        } else {
            if runtime_state == WorkspaceRuntimeState::Background {
                row.set_subtitle("Running in background");
            }
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

        if runtime_state == WorkspaceRuntimeState::Background {
            let unload_button = gtk4::Button::from_icon_name("media-playback-stop-symbolic");
            unload_button.add_css_class("flat");
            unload_button.set_valign(gtk4::Align::Center);
            unload_button.set_tooltip_text(Some("Unload (stop its background processes)"));
            unload_button.connect_clicked({
                let app = app.clone();
                let group = group.clone();
                let workspace_rows = workspace_rows.clone();
                let toast_overlay = toast_overlay.clone();
                let dialog_parent = dialog_parent.clone();
                let workspace_label = workspace_label.clone();
                move |_| {
                    if let Err(error) = App::unload_workspace(&app, id) {
                        toast_overlay.add_toast(adw::Toast::new(&error.to_string()));
                    }
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
            row.add_suffix(&unload_button);
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
// Each parameter is a distinct widget/state handle this one dialog-wiring
// function needs to re-sync on success; a params struct built once per call
// site wouldn't clarify anything here.
#[allow(clippy::too_many_arguments)]
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
// Each parameter is a distinct widget/state handle this one dialog-wiring
// function needs to re-sync on success; a params struct built once per call
// site wouldn't clarify anything here.
#[allow(clippy::too_many_arguments)]
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
    let count_terminals = |nodes: &[crate::model::NodeRecord]| {
        nodes.iter().filter(|n| n.as_terminal().is_some()).count()
    };
    let session_count = {
        let app_ref = app.borrow();
        if app_ref.workspace_id == id {
            app_ref
                .nodes
                .values()
                .filter(|entry| entry.record.as_terminal().is_some())
                .count()
        } else {
            app_ref
                .inactive_workspaces
                .iter()
                .find(|w| w.id == id)
                .map(|w| count_terminals(&w.nodes))
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
