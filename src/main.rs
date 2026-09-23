mod actions;
mod completion;
mod connection_form;
mod db;
mod explain;
mod explorer;
mod export;
mod filter;
mod palette;
mod result_grid;
mod session;
mod sql;
mod store;
mod views;
mod workspace;

mod icons;
mod keybindings;
mod theme;
mod tls;
mod ui;

use std::{borrow::Cow, path::PathBuf, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, App, AppContext, ClickEvent, ClipboardItem, Context, Entity, EntityInputHandler,
    FocusHandle, Focusable, FontWeight, InteractiveElement, IntoElement, Menu, MenuItem,
    ParentElement, Render, StatefulInteractiveElement, Styled, TitlebarOptions, Window,
    WindowDecorations, WindowOptions, deferred, div, point, prelude::FluentBuilder, px,
};
use gpui_component::{
    Disableable, IndexPath, Root,
    input::{
        CompletionProvider, EditorMode, EditorState, Enter, IndentInline, Input, InputEvent,
        InputModeKind, InputState, Position,
    },
    list::{List, ListEvent, ListItem, ListState},
    resizable::{ResizableState, h_resizable, resizable_panel},
    tree::tree as render_tree,
};

use actions::{
    AcceptCompletion, AddFilter, ApplyEdits, CancelQuery, ClearFilter, CloseTab, CommandPalette,
    CopyCell, CycleTheme, DeleteRow, DiscardEdits, EditCell, ExplainQuery, FollowForeignKey,
    FormatQuery, FuzzyOpen, NewConnection, NewQuery, NewRow, NextPage, NextProfile, NextTab,
    OpenSettings, PaletteNext, PalettePrevious, PreviousPage, PreviousProfile, PreviousTab, Quit,
    RefreshRelation, RemoveFilter, RequestWriteMode, ResetConfirmations, ResetEditorZoom, RunQuery,
    SaveQuery, SetDefault, SetEmpty, SetFilterColumn, SetFilterOperator, SetFilterRaw, SetMode,
    SetNull, SetRowLimit, ShowEditor, SortColumn, ToggleFilterJoin, ToggleNextJoin, ToggleRowPanel,
    ToggleSidebar, ZoomEditorIn, ZoomEditorOut,
};
use completion::SchemaCompletions;
use connection_form::{ConnectionForm, default_profile_name};
use db::{
    Catalog, Connection, ConnectionConfig, DbError, Engine, ExplainMode, Fields, RelationKind,
    ServerConfig, SnowflakeConfig, SslMode,
};
use explorer::{ExplorerTarget, ObjectKind, PREVIEW_ROW_LIMIT, tree as build_explorer_tree};
use export::Format;
use filter::{
    Conjunction, FilterBar, FilterRow, Operator, changed_filter, cycle, derived_filter,
    filter_bars, filter_row, foreign_key_filter, relation_sql, restored_filter, sort_columns,
    sort_expression, value_placeholder,
};
use icons::{Icons, icon};
use palette::{Command, Mode as PaletteMode, Palette};
use result_grid::{NewValue, ResultGrid};
use session::{
    ApplyReview, CatalogState, CloseTarget, Explained, Focus, InsertField, InsertForm, ObjectBody,
    ObjectTab, OpenedObject, Profile, ProfileState, QueryState, QueryTab, Refresh, Session,
    StructureState, Tab, close_target, insert_value, matching_tab, relation_kind, restored_state,
    show_snapshot,
};
use sql::{Buffer, SortKey};
use theme::{ConnectionColor, FontSlot, Fonts, Theme, fonts, layout, theme};
use ui::{
    Control, Tone, button, button_label, dialog, group_thousands, human_bytes, icon_button,
    object_icon, relative_age, row_icon, row_icon_tinted, row_readout, section_label, titlebar,
};
use workspace::Workspace;

/// The platform's window buttons, which dbdelve positions but does not draw.
const TRAFFIC_LIGHT_DIAMETER: f32 = 14.0;

fn connection_config_from_environment() -> Result<Option<ConnectionConfig>, String> {
    let host = std::env::var("PGHOST").ok();
    let port = std::env::var("PGPORT").ok();
    let database = std::env::var("PGDATABASE").ok();
    let user = std::env::var("PGUSER").ok();

    if [&host, &port, &database, &user]
        .iter()
        .all(|value| value.is_none())
    {
        return Ok(None);
    }

    let missing = [
        ("PGHOST", &host),
        ("PGDATABASE", &database),
        ("PGUSER", &user),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.is_none().then_some(name))
    .collect::<Vec<_>>();

    // Destructured rather than unwrapped, so the compiler -- not a list of
    // names twenty lines up -- is what guarantees these are present.
    let (Some(host), Some(database), Some(user)) = (host, database, user) else {
        return Err(format!(
            "Connection configuration is missing {}.",
            missing.join(", ")
        ));
    };

    let sslmode = match std::env::var("PGSSLMODE") {
        Ok(sslmode) => SslMode::parse(&sslmode)?,
        Err(_) => SslMode::default(),
    };
    let root_certificate = std::env::var("PGSSLROOTCERT")
        .ok()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty());

    let port = port
        .map(|port| {
            port.parse()
                .map_err(|_| "PGPORT is not a valid port.".to_string())
        })
        .transpose()?;

    // The `PG*` variables configure a Postgres profile and are not generalised.
    // dbdelve is a generic client, not a generic environment reader, and there is
    // no convention for the other engines to read.
    Ok(Some(ConnectionConfig::Postgres(ServerConfig {
        host,
        port,
        database,
        user,
        password: std::env::var("PGPASSWORD").unwrap_or_default(),
        sslmode,
        root_certificate,
        // No `PG*` variable means it, and dbdelve is not inventing one.
        statement_timeout: 0,
    })))
}

/// Where a panic goes when nobody is watching stderr. A `.app` is spawned by
/// launchd, so a panic message lands in the unified log and the abort that
/// follows writes an `.ips` trace that does not carry it -- every stranger's
/// bug report would read "it closed" with nothing to read after it. Installed
/// first thing in `main`, because the deaths hardest to guess at from outside
/// are the ones before a window exists.
///
/// ponytail: one appended file, never rotated. A few kilobytes per crash; if
/// that ever becomes a real number, truncate on open past some size.
fn install_panic_log() {
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return;
    };
    // A crash log is state, not cache: XDG puts it under the state directory,
    // and a cache cleaner is entitled to delete anything in the other one.
    #[cfg(target_os = "macos")]
    let directory = PathBuf::from(home).join("Library/Logs/dbdelve");
    #[cfg(not(target_os = "macos"))]
    let directory = match std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        Some(state_home) => PathBuf::from(state_home).join("dbdelve"),
        None => PathBuf::from(home).join(".local/state/dbdelve"),
    };
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Nothing in here may panic: a panic inside the hook aborts with less
        // to read than the one it was called for. Hence every result dropped
        // rather than unwrapped.
        use std::io::Write as _;
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();
        if std::fs::create_dir_all(&directory).is_ok()
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join("panic.log"))
        {
            let _ = writeln!(
                file,
                "--- {at} (unix seconds)\n{info}\n{}",
                std::backtrace::Backtrace::force_capture()
            );
        }
        previous(info);
    }));
}

fn main() {
    install_panic_log();
    gpui_platform::application()
        .with_assets(Icons)
        .run(|cx: &mut App| {
            cx.text_system()
                .add_fonts(
                    guic_gpui_assets::BUNDLED_FONTS
                        .iter()
                        .map(|font| Cow::Borrowed(*font))
                        .collect(),
                )
                .expect("bundled fonts must be loadable");
            gpui_component::init(cx);
            // The defaults stand in until `Workspace::new` has read the file; the
            // theme is applied through them, and `apply_to_components` reads the
            // global rather than naming a family itself.
            cx.set_global(Fonts::default());
            let theme = Theme::default();
            theme.apply_to_components(cx);
            cx.set_global(theme);
            // The keymap is built from `keybindings::REGISTRY` plus whatever
            // overrides are on disk, so a chord shown here is a default rather
            // than gospel -- see `src/keybindings.rs` for the full list and
            // `Settings > Keybindings` for where a user changes one. Read once,
            // here, rather than reused from `Workspace::new`'s own read of the
            // same file: nothing here can wait for a window and an entity to
            // exist first.
            let overrides = store::load_profiles()
                .ok()
                .and_then(|(_, _, _, settings)| settings)
                .and_then(|settings| settings.custom_keybindings)
                .unwrap_or_default();
            cx.bind_keys(keybindings::build_bindings(&overrides));

            // An application menu is what actually makes `cmd+q` quit: the menu bar
            // owns the keystroke at the AppKit level, so it fires whatever has
            // focus, including a native text field that swallows the rest. Set
            // after the bindings, because the shortcut the item displays is read
            // back out of the keymap.
            //
            // The rest of the bar is there for the same reason in reverse: every
            // item names an action dbdelve already dispatches, so the menu is a way
            // to discover the keystroke rather than a second path to the work. No
            // Edit menu -- dbdelve does not own cut, copy and paste, the focused
            // field does, and a menu claiming them would take them from it.
            cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
            cx.set_menus(vec![
                Menu {
                    name: "dbdelve".into(),
                    disabled: false,
                    items: vec![
                        MenuItem::action("Settings…", OpenSettings),
                        MenuItem::separator(),
                        MenuItem::action("Quit dbdelve", Quit),
                    ],
                },
                Menu {
                    name: "File".into(),
                    disabled: false,
                    items: vec![
                        MenuItem::action("New Query", NewQuery),
                        MenuItem::action("New Connection", NewConnection),
                        MenuItem::separator(),
                        MenuItem::action("Save Query", SaveQuery),
                        MenuItem::separator(),
                        MenuItem::action("Close Tab", CloseTab),
                    ],
                },
                Menu {
                    name: "Query".into(),
                    disabled: false,
                    items: vec![
                        MenuItem::action("Run", RunQuery),
                        MenuItem::action("Cancel", CancelQuery),
                    ],
                },
                Menu {
                    name: "View".into(),
                    disabled: false,
                    items: vec![
                        MenuItem::action("Toggle Sidebar", ToggleSidebar),
                        MenuItem::action("Toggle Row Panel", ToggleRowPanel),
                        MenuItem::separator(),
                        MenuItem::action("Zoom In", ZoomEditorIn),
                        MenuItem::action("Zoom Out", ZoomEditorOut),
                        MenuItem::action("Reset Zoom", ResetEditorZoom),
                        MenuItem::separator(),
                        MenuItem::action("Cycle Theme", CycleTheme),
                    ],
                },
            ]);

            // The platform titlebar is kept only for its window buttons: a system
            // bar in its own grey above dbdelve's chrome is the seam every native app
            // avoids. dbdelve paints that strip itself, and the buttons sit over it.
            //
            // ponytail: there is nothing to sit over on Linux -- no compositor
            // draws window buttons into a transparent titlebar -- so the window
            // asks for the real one and wears the seam. The upgrade is drawing
            // close, minimise and maximise into `ui::titlebar` and switching
            // back to `WindowDecorations::Client`.
            let options = WindowOptions {
                window_background: theme.window_background(),
                titlebar: Some(TitlebarOptions {
                    title: Some("dbdelve".into()),
                    appears_transparent: !cfg!(target_os = "linux"),
                    traffic_light_position: (!cfg!(target_os = "linux")).then(|| {
                        point(
                            px(layout::SPACE_MD),
                            px((layout::TITLEBAR_HEIGHT - TRAFFIC_LIGHT_DIAMETER) / 2.),
                        )
                    }),
                }),
                window_decorations: Some(WindowDecorations::Server),
                // Wayland matches a window to its desktop entry by app id and
                // by nothing else, so without this the app runs with a blank
                // icon however well the .desktop file is installed. It has to
                // equal the entry's basename.
                app_id: Some("dbdelve".into()),
                ..Default::default()
            };

            // Root must be the window's first layer or dialog and notification
            // layers panic when they look for it.
            cx.open_window(options, |window, cx| {
                let workspace = cx.new(|cx| Workspace::new(window, cx));
                cx.new(|cx| Root::new(workspace, window, cx))
            })
            .expect("failed to open window");

            // dbdelve has one window and no way to open a second: with it closed the
            // Dock icon is inert and the menu offers only Quit, which is an
            // application nobody can get back into. Quitting is the way back --
            // clicking the dead icon then launches dbdelve again, restoring the
            // profiles and the buffers `Workspace::on_release` has just written.
            // Reopening a window here would have to rebuild that same state anyway,
            // and would keep a process alive that is holding nothing.
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            cx.activate(true);
        });
}
