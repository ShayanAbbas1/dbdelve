//! The app driven the way a person drives it: launched from a saved
//! `profiles.toml`, typed into, run from the keyboard. GPUI's test platform
//! has no screen, so these run headless in CI like any other test.

use std::{borrow::Cow, cell::RefCell, rc::Rc};

use gpui::{
    App, AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, Window,
};
use gpui_component::{Root, input::InputState};

use super::Workspace;
use crate::{
    actions::{CancelQuery, Disconnect, ImportRowsFromClipboard},
    connection_form::ConnectionForm,
    db::{CancelToken, ConnectionConfig, Engine, ExternalConfig, Fetch, SshTunnel},
    explorer, keybindings,
    palette::{Mode as PaletteMode, Scope},
    session::{CatalogState, ObjectBody, ProfileState, QueryState, QueryTab, Tab},
    store,
    theme::{self, Fonts},
};

/// What `main` sets up before the window opens, minus the menu bar and the
/// panic log, which the test platform has no use for.
fn launch(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    launch_with(cx, Vec::new())
}

/// `launch` with these plugins discovered.
fn launch_with<'a>(
    cx: &'a mut TestAppContext,
    plugins: Vec<&'static crate::db::Plugin>,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    cx.update(|cx| {
        cx.text_system()
            .add_fonts(
                guic_gpui_assets::BUNDLED_FONTS
                    .iter()
                    .map(|font| Cow::Borrowed(*font))
                    .collect(),
            )
            .expect("bundled fonts must be loadable");
        gpui_component::init(cx);
        cx.set_global(Fonts::default());
        theme::user::load();
        let theme = theme::starting_theme(theme::Appearance::of_system(cx.window_appearance()));
        theme.apply_to_components(cx);
        cx.set_global(theme);
        let overrides = store::peek_profiles(&plugins)
            .and_then(|list| list.settings)
            .and_then(|settings| settings.custom_keybindings)
            .unwrap_or_default();
        cx.bind_keys(keybindings::build_bindings(&overrides));
    });
    let mut workspace = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let entity = cx.new(|cx| Workspace::new(plugins, window, cx));
        workspace = Some(entity.clone());
        Root::new(entity, window, cx)
    });
    (workspace.expect("the window builds the workspace"), cx)
}

/// Every write `Workspace` makes on the way out -- buffers on release,
/// profiles on quit -- has to land while `with_home` still points the store at
/// the temporary home, not after the test returns and `HOME` is the real one.
fn quit(workspace: Entity<Workspace>, cx: &mut VisualTestContext) {
    drop(workspace);
    // On the app rather than the window: shutting down removes the window.
    cx.cx.update(|cx| cx.shutdown());
    cx.cx.run_until_parked();
}

/// The app launched on one saved SQLite profile, an empty database, with its
/// query tab in front.
fn launch_on_sqlite(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    launch_on_sqlite_holding(cx, "")
}

/// `launch_on_sqlite` over a database `schema` has been run against first.
fn launch_on_sqlite_holding<'a>(
    cx: &'a mut TestAppContext,
    schema: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let directory = store::dbdelve_directory().expect("the test home has a data directory");
    std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
    // An empty file is an empty database. dbdelve opens a SQLite file
    // rather than creating one, so it has to be there first.
    let database = directory.join("e2e.db");
    std::fs::write(&database, b"").expect("the database file must be creatable");
    rusqlite::Connection::open(&database)
        .and_then(|connection| connection.execute_batch(schema))
        .expect("the schema must apply");
    std::fs::write(
        directory.join("profiles.toml"),
        format!(
            "active = \"e2e\"\n\
             [settings]\n\
             check_for_updates = false\n\
             [[profiles]]\n\
             id = \"e2e\"\n\
             name = \"E2E\"\n\
             host = \"\"\n\
             database = \"\"\n\
             user = \"\"\n\
             engine = \"sqlite\"\n\
             path = {:?}\n",
            database.display().to_string()
        ),
    )
    .expect("profiles.toml must be writable");

    let (workspace, cx) = launch(cx);
    // CI's live-test job sets `PG*`, which puts an environment profile in
    // front of the saved one.
    workspace.update(cx, |workspace, cx| {
        let index = workspace
            .profiles
            .iter()
            .position(|profile| profile.config.engine() == Engine::Sqlite)
            .expect("the saved SQLite profile is restored");
        workspace.activate(index, cx);
    });
    cx.run_until_parked();
    (workspace, cx)
}

/// Opens `main.<table>` of the profile in front as a relation tab, brings it
/// forward and settles; the tab's id.
fn open_own_table(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, table: &str) -> u64 {
    let id = workspace.update_in(cx, |workspace, window, cx| {
        let opened = crate::session::OpenedObject::Relation {
            schema: "main".into(),
            name: table.into(),
            kind: Default::default(),
            filter: String::new(),
            filters: Vec::new(),
        };
        let id = workspace
            .open_object(opened, window, cx)
            .expect("the relation opens");
        workspace.activate_tab(Tab::Object(id), window, cx);
        id
    });
    settle(cx);
    id
}

/// The grid of relation tab `id` of the profile in front.
fn relation_grid(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: u64,
) -> Entity<gpui_component::table::TableState<crate::result_grid::ResultGrid>> {
    workspace.read_with(cx, |workspace, _| {
        let tab = workspace
            .profile()
            .and_then(|profile| profile.session.objects.iter().find(|tab| tab.id == id))
            .expect("the relation tab is open");
        let ObjectBody::Relation { results, .. } = &tab.body else {
            panic!("the tab is a relation");
        };
        results.clone()
    })
}

/// The first cell `sql` reads from `e2e.db` on a connection of its own, so a
/// test sees what is committed and not what the window's session holds.
fn on_disk(sql: &str) -> Option<String> {
    use rusqlite::types::Value;
    let path = store::dbdelve_directory()
        .expect("the test home has a data directory")
        .join("e2e.db");
    let connection = rusqlite::Connection::open(path).expect("the database opens");
    match connection.query_row(sql, [], |row| row.get::<_, Value>(0)) {
        Ok(Value::Text(text)) => Some(text),
        Ok(Value::Integer(number)) => Some(number.to_string()),
        Ok(_) | Err(_) => None,
    }
}

/// The state of the tab in front, whichever kind it is.
fn front_state(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> QueryState {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .profile()
            .and_then(|profile| profile.session.active_query())
            .cloned()
            .expect("a tab is in front")
    })
}

/// The review in the session, as `(title, carries the draft, error)`.
fn front_review(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<(&'static str, bool, Option<String>)> {
    workspace.read_with(cx, |workspace, _| {
        let review = workspace.profile()?.session.apply_review.as_ref()?;
        Some((review.title, review.edits, review.error.clone()))
    })
}

const BIRDS: &str = "create table birds (id integer primary key, name text not null); \
                     insert into birds values (1, 'wren'), (2, 'tit');";

/// The test platform has no frame loop: what the app leaves for the next
/// frame, such as a dialog handing back the keyboard, waits for this.
fn settle(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.simulate_next_frame(cx));
    cx.run_until_parked();
}

fn query_tab<T>(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    read: impl FnOnce(&QueryTab, &Window, &App) -> T,
) -> T {
    cx.update(|window, cx| {
        let tab = workspace
            .read(cx)
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
            .expect("a fresh profile opens on a query tab");
        read(tab, window, cx)
    })
}

fn buffer(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    query_tab(workspace, cx, |tab, _, cx| {
        tab.editor.read(cx).value().to_string()
    })
}

fn editor_has_keyboard(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    query_tab(workspace, cx, |tab, window, cx| {
        tab.editor.focus_handle(cx).is_focused(window)
    })
}

fn grid_has_keyboard(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    query_tab(workspace, cx, |tab, window, cx| {
        tab.results.focus_handle(cx).is_focused(window)
    })
}

fn sidebar_has_keyboard(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| {
        workspace
            .read(cx)
            .explorer_focus
            .contains_focused(window, cx)
    })
}

fn dialog_has_keyboard(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| workspace.read(cx).dialog_focus.is_focused(window))
}

fn ringed_button(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    workspace.read_with(cx, |workspace, _| workspace.dialog_choice.get())
}

/// `selec` is no statement `sql::classify` can read, so running it puts up the
/// run-once prompt instead of reaching the database.
fn raise_run_once_prompt(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("secondary-enter");
    settle(cx);
    workspace.read_with(cx, |workspace, _| {
        assert_eq!(workspace.dialog_in_front.get(), Some("pending-run"));
    });
}

#[gpui::test]
fn a_query_typed_and_run_from_the_keyboard_lands_its_rows(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);

        cx.simulate_input("select 1");
        cx.simulate_keystrokes("secondary-enter");
        cx.run_until_parked();

        query_tab(&workspace, cx, |tab, _, _| match &tab.query {
            QueryState::Complete { rows, .. } => assert_eq!(*rows, 1),
            QueryState::Failed(error) => panic!("the query failed: {error}"),
            _ => panic!("the query never completed"),
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn hidden_results_come_back_from_the_keyboard_and_for_a_refused_run(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);

        cx.simulate_keystrokes("secondary-j");
        cx.run_until_parked();
        query_tab(&workspace, cx, |tab, _, _| assert!(tab.results_hidden));
        // Hiding hands the keyboard to the editor, so the chord still reaches
        // the workspace to bring the pane back.
        cx.simulate_keystrokes("secondary-j");
        cx.run_until_parked();
        query_tab(&workspace, cx, |tab, _, _| assert!(!tab.results_hidden));

        cx.simulate_keystrokes("secondary-j");
        cx.run_until_parked();
        // An empty buffer is refused straight into the slot, never landing as
        // a run does, and the refusal must still be seen.
        cx.simulate_keystrokes("secondary-enter");
        cx.run_until_parked();
        query_tab(&workspace, cx, |tab, _, _| {
            assert!(matches!(tab.query, QueryState::Failed(_)));
            assert!(!tab.results_hidden);
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn going_to_a_column_or_a_result_brings_hidden_results_back(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("select 1 as a, 2 as b");
        cx.simulate_keystrokes("secondary-enter");
        cx.run_until_parked();

        cx.simulate_keystrokes("secondary-j");
        cx.run_until_parked();
        // Go to column hands the grid the keyboard, which only a drawn grid
        // can keep.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.reveal_column(1, window, cx))
        });
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| assert!(!tab.results_hidden));
        assert!(grid_has_keyboard(&workspace, cx));

        // Hidden from the grid, the keyboard goes to the editor, and the chord
        // still has a path back to the pane.
        cx.simulate_keystrokes("secondary-j");
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| assert!(tab.results_hidden));
        assert!(editor_has_keyboard(&workspace, cx));
        cx.simulate_keystrokes("secondary-j");
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| assert!(!tab.results_hidden));

        cx.simulate_keystrokes("secondary-j");
        cx.run_until_parked();
        // Show result, the palette's way off the Messages chip.
        let id = query_tab(&workspace, cx, |tab, _, _| tab.id);
        workspace.update(cx, |workspace, cx| {
            workspace.show_queued_result(crate::session::Tab::Query(id), 0, cx)
        });
        query_tab(&workspace, cx, |tab, _, _| assert!(!tab.results_hidden));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_prompt_takes_the_keyboard_and_hands_it_back_once_answered(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("selec 1");
        raise_run_once_prompt(&workspace, cx);

        assert!(dialog_has_keyboard(&workspace, cx));
        assert_eq!(
            ringed_button(&workspace, cx),
            0,
            "the ring starts on Run once"
        );
        cx.simulate_input("x");
        settle(cx);
        assert_eq!(
            buffer(&workspace, cx),
            "selec 1",
            "the buffer behind took a key"
        );

        for (keystroke, ringed) in [("left", 1), ("right", 0), ("tab", 2), ("shift-tab", 0)] {
            cx.simulate_keystrokes(keystroke);
            settle(cx);
            assert_eq!(ringed_button(&workspace, cx), ringed, "after {keystroke}");
        }

        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(
            buffer(&workspace, cx),
            "selec 1",
            "enter reached the buffer"
        );
        query_tab(&workspace, cx, |tab, _, _| {
            assert!(
                matches!(tab.query, QueryState::Failed(_)),
                "Run once did not run the statement"
            );
        });
        assert!(editor_has_keyboard(&workspace, cx));
        cx.simulate_input("x");
        settle(cx);
        assert_eq!(buffer(&workspace, cx), "selec 1x");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_prompt_dismissed_from_the_keyboard_hands_it_back_to_the_editor(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("selec 1");

        for (dismiss, typed) in [("tab tab enter", "x"), ("escape", "y")] {
            raise_run_once_prompt(&workspace, cx);
            let before = buffer(&workspace, cx);
            cx.simulate_keystrokes(dismiss);
            settle(cx);
            workspace.read_with(cx, |workspace, _| {
                assert_eq!(
                    workspace.dialog_in_front.get(),
                    None,
                    "{dismiss} left it up"
                );
            });
            query_tab(&workspace, cx, |tab, _, _| {
                assert!(matches!(tab.query, QueryState::Idle), "{dismiss} ran it");
            });
            assert!(editor_has_keyboard(&workspace, cx), "after {dismiss}");
            cx.simulate_input(typed);
            settle(cx);
            assert_eq!(buffer(&workspace, cx), format!("{before}{typed}"));
        }

        quit(workspace, cx);
    });
}

/// A statement that is still running cannot be driven here: its clock ticks
/// on a timer, which the test scheduler fires at once, so nothing parks until
/// it ends. What the chord resolves to, and who handles it, is checked
/// instead, from the editor and from a grid on screen.
#[gpui::test]
fn the_cancel_chord_reaches_the_workspace_from_the_editor_and_the_grid(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("select 1");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| {
            assert!(matches!(tab.query, QueryState::Complete { .. }));
        });

        let resolved: Rc<RefCell<Vec<bool>>> = Rc::default();
        let _observer = cx.update({
            let resolved = resolved.clone();
            |_, cx| {
                cx.observe_keystrokes(move |event, _, _| {
                    resolved.borrow_mut().push(
                        event
                            .action
                            .as_ref()
                            .is_some_and(|action| action.as_any().is::<CancelQuery>()),
                    );
                })
            }
        });
        let (editor, results) = query_tab(&workspace, cx, |tab, _, _| {
            (tab.editor.clone(), tab.results.clone())
        });
        for in_grid in [false, true] {
            cx.update(|window, cx| match in_grid {
                true => results.focus_handle(cx).focus(window, cx),
                false => editor.focus_handle(cx).focus(window, cx),
            });
            settle(cx);
            cx.update(|window, cx| {
                let focused = match in_grid {
                    true => results.focus_handle(cx),
                    false => editor.focus_handle(cx),
                };
                assert!(focused.is_focused(window), "in grid: {in_grid}");
                assert!(
                    window.is_action_available(&CancelQuery, cx),
                    "in grid: {in_grid}"
                );
            });
            resolved.borrow_mut().clear();
            cx.simulate_keystrokes("secondary-.");
            assert_eq!(*resolved.borrow(), [true], "in grid: {in_grid}");
        }

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_note_about_one_tab_shows_only_while_that_tab_is_in_front(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let shown = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().expect("a profile is open").session;
                session.shown_notice().map(str::to_string)
            })
        };
        let first = workspace.update(cx, |workspace, _| {
            let session = &mut workspace.profile_mut().unwrap().session;
            session.tab_notice = Some((session.active, "cancelled here".into()));
            session.active
        });
        assert_eq!(shown(cx).as_deref(), Some("cancelled here"));

        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        assert_eq!(shown(cx), None);

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_tab(first, window, cx)
            })
        });
        settle(cx);
        assert_eq!(shown(cx).as_deref(), Some("cancelled here"));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_move_tab_chords_reorder_the_strip(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let strip = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().expect("a profile is open").session;
                (session.strip_tabs(), session.active)
            })
        };
        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        let (before, active) = strip(cx);
        assert_eq!(before.last(), Some(&active), "a new tab opens at the end");

        cx.simulate_keystrokes("ctrl-shift-pageup");
        settle(cx);
        let (after, _) = strip(cx);
        assert_eq!(after[before.len() - 2], active);

        cx.simulate_keystrokes("ctrl-shift-pagedown");
        settle(cx);
        assert_eq!(strip(cx).0, before);

        // The right end is a wall.
        cx.simulate_keystrokes("ctrl-shift-pagedown");
        settle(cx);
        assert_eq!(strip(cx).0, before);

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_command_palette_opens_on_the_welcome_surface(cx: &mut TestAppContext) {
    // A `PG*` connection from the environment opens in place of the welcome
    // surface, as CI's live-test job sets.
    if ["PGHOST", "PGPORT", "PGDATABASE", "PGUSER"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return;
    }
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        settle(cx);
        workspace.read_with(cx, |workspace, _| assert!(workspace.profile().is_none()));

        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        workspace.read_with(cx, |workspace, cx| {
            let palette = workspace.palette.as_ref().expect("the palette is up");
            assert!(palette.read(cx).mode() == PaletteMode::Search(Scope::Commands));
        });
        cx.simulate_keystrokes("escape");
        settle(cx);
        workspace.read_with(cx, |workspace, _| assert!(workspace.palette.is_none()));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn ctrl_space_asks_for_completions_in_the_query_buffer(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let menu_open = |cx: &mut VisualTestContext| {
            query_tab(&workspace, cx, |tab, _, cx| {
                tab.editor.read(cx).completion_menu_state().open
            })
        };
        cx.simulate_input("sel");
        settle(cx);
        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(!menu_open(cx), "escape left the menu up");

        cx.simulate_keystrokes("ctrl-space");
        settle(cx);
        assert!(menu_open(cx));
        assert_eq!(buffer(&workspace, cx), "sel");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_keyboard_moves_between_the_buffer_and_its_rows(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("select 1");
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        assert!(
            editor_has_keyboard(&workspace, cx),
            "with no rows yet, the buffer keeps it"
        );

        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        assert!(grid_has_keyboard(&workspace, cx));
        query_tab(&workspace, cx, |tab, _, cx| {
            assert_eq!(tab.results.read(cx).delegate().active(), Some((0, 0)));
        });

        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(editor_has_keyboard(&workspace, cx));
        cx.simulate_input("x");
        settle(cx);
        assert_eq!(buffer(&workspace, cx), "select 1x");

        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        assert!(grid_has_keyboard(&workspace, cx));
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        assert!(editor_has_keyboard(&workspace, cx));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_sidebar_takes_the_keyboard_and_escape_hands_it_back(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        workspace.update(cx, |workspace, _| workspace.sidebar_hidden = true);

        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        workspace.read_with(cx, |workspace, _| assert!(!workspace.sidebar_hidden));
        assert!(sidebar_has_keyboard(&workspace, cx));

        // On the schema's row, which folds away its Tables and the table.
        cx.simulate_keystrokes("enter");
        settle(cx);
        cx.update(|_, cx| {
            let tree = &workspace.read(cx).profile().unwrap().session.explorer_tree;
            assert!(
                tree.read(cx).entry(1).is_none(),
                "enter left the schema open"
            );
        });
        assert!(sidebar_has_keyboard(&workspace, cx));

        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(editor_has_keyboard(&workspace, cx));
        cx.simulate_input("x");
        settle(cx);
        assert_eq!(buffer(&workspace, cx), "x");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_schema_folded_without_a_filter_is_saved_and_collapse_all_closes_it(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let filter_to = |text: &'static str, cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let profile = workspace.profile().unwrap();
                    let (id, filter) =
                        (profile.id.clone(), profile.session.explorer_filter.clone());
                    filter.update(cx, |input, cx| input.set_value(text, window, cx));
                    workspace.refresh_explorer(&id, cx);
                });
            });
        };
        let fold_schema = |cx: &mut VisualTestContext| {
            cx.simulate_keystrokes("secondary-k e");
            settle(cx);
            cx.simulate_keystrokes("enter");
            settle(cx);
        };
        let schema_open = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let tree = &workspace.read(cx).profile().unwrap().session.explorer_tree;
                tree.read(cx).entry(0).unwrap().is_expanded()
            })
        };
        let saved = || {
            store::peek_profiles(&[]).unwrap().profiles[0]
                .folds
                .get("schema-main")
                .copied()
        };

        filter_to("birds", cx);
        fold_schema(cx);
        assert!(!schema_open(cx));
        assert_eq!(saved(), None);
        filter_to("", cx);
        assert!(schema_open(cx), "the only schema, opened by default");

        fold_schema(cx);
        assert!(!schema_open(cx));
        assert_eq!(saved(), Some(false));

        // Picked as the active schema while folded under a filter, it opens
        // there and then, not once the filter is cleared.
        filter_to("birds", cx);
        fold_schema(cx);
        assert!(!schema_open(cx));
        workspace.update(cx, |workspace, cx| workspace.set_schema("main".into(), cx));
        settle(cx);
        assert!(schema_open(cx));
        assert_eq!(saved(), Some(true));

        cx.dispatch_action(crate::actions::CollapseAllSchemas);
        settle(cx);
        let filter = cx.update(|_, cx| {
            let session = &workspace.read(cx).profile().unwrap().session;
            session.explorer_filter.read(cx).value().to_string()
        });
        assert_eq!(filter, "", "a filter would have held the schema open");
        assert!(!schema_open(cx));
        assert_eq!(saved(), Some(false));

        cx.dispatch_action(crate::actions::ExpandAllSchemas);
        settle(cx);
        assert!(schema_open(cx));
        assert_eq!(saved(), Some(true));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn enter_opens_a_sidebar_row_and_its_structure_takes_the_keyboard(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (name text); insert into birds values ('wren');",
        );

        // The schema, its Tables, then the table.
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("down down enter");
        settle(cx);
        // Whether the tab in front is the structure, and whether its grid has
        // the keyboard.
        let front_grid_has_keyboard = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let tab = workspace
                    .read(cx)
                    .profile()
                    .and_then(|profile| profile.session.active_object())
                    .expect("an object tab is in front");
                assert_eq!(tab.name, "birds");
                match &tab.body {
                    ObjectBody::Relation { results, .. } => {
                        (false, results.focus_handle(cx).is_focused(window))
                    }
                    ObjectBody::Structure { grid, .. } => {
                        (true, grid.focus_handle(cx).is_focused(window))
                    }
                    ObjectBody::Routine(_) | ObjectBody::Diagram { .. } => {
                        panic!("birds is a table")
                    }
                }
            })
        };
        assert_eq!(
            front_grid_has_keyboard(cx),
            (false, true),
            "enter opened the rows"
        );

        cx.simulate_keystrokes("secondary-shift-d");
        settle(cx);
        assert_eq!(front_grid_has_keyboard(cx), (true, true));
        cx.simulate_keystrokes("down");
        settle(cx);
        cx.update(|_, cx| {
            let workspace = workspace.read(cx);
            let Some(ObjectBody::Structure { grid, .. }) = workspace
                .profile()
                .and_then(|profile| profile.session.active_object())
                .map(|tab| &tab.body)
            else {
                panic!("the structure tab is in front");
            };
            assert!(
                grid.read(cx).delegate().selection_text().is_some(),
                "the arrow reached the structure grid"
            );
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_sidebar_selects_the_row_of_the_tab_in_front(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (name text); create table flocks (id integer);",
        );
        let selected = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let session = &workspace.read(cx).profile().unwrap().session;
                session
                    .explorer_tree
                    .read(cx)
                    .selected_item()
                    .map(|item| item.id.to_string())
            })
        };
        let active = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace.profile().unwrap().session.active
            })
        };
        let open = |name: &str, cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                let leaf = workspace.profile().unwrap().session.explorer_leaves
                    [&explorer::relation_row("main", name)];
                workspace.open_explorer_target(leaf.target, window, cx);
            });
            settle(cx);
        };
        let birds_row = explorer::relation_row("main", "birds");
        let flocks_row = explorer::relation_row("main", "flocks");
        assert_eq!(selected(cx), None, "a scratch query has no row");

        open("birds", cx);
        assert_eq!(selected(cx), Some(birds_row.clone()));
        let birds = active(cx);
        open("flocks", cx);
        assert_eq!(selected(cx), Some(flocks_row.clone()));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_tab(birds, window, cx);
        });
        settle(cx);
        assert_eq!(
            selected(cx),
            Some(birds_row.clone()),
            "a tab switch follows"
        );

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.new_query(&crate::actions::NewQuery, window, cx);
        });
        settle(cx);
        assert_eq!(selected(cx), None, "a new scratch query leaves no row lit");

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.ask_for_name(String::new(), window, cx);
            let name = workspace.profile().unwrap().session.save_name.clone();
            name.update(cx, |input, cx| input.set_value("daily", window, cx));
            workspace.confirm_save(window, cx);
        });
        settle(cx);
        assert_eq!(
            selected(cx),
            Some(explorer::query_row("daily")),
            "saved, the query has a row of its own"
        );

        let daily = active(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            let crate::session::Tab::Query(id) = daily else {
                panic!("the saved query is in front");
            };
            workspace.close_buffer(id, window, cx);
        });
        settle(cx);
        let fallback =
            workspace.read_with(cx, |workspace, _| workspace.profile().unwrap().front_row());
        assert!(fallback.is_some(), "a table tab came to the front");
        assert_eq!(selected(cx), fallback, "a close follows to the tab behind");

        // A pin repeats the table's row; selected, it keeps the selection.
        workspace.update(cx, |workspace, cx| {
            let profile = workspace.profile_mut().unwrap();
            profile
                .shortcuts
                .toggle_pin(explorer::Pin::Member(explorer::Member::Object(
                    explorer::ObjectRef {
                        database: None,
                        schema: "main".into(),
                        name: "birds".into(),
                        kind: explorer::ObjectKind::Relation(crate::db::RelationKind::Table),
                    },
                )));
            let id = profile.id.clone();
            workspace.refresh_explorer(&id, cx);
        });
        settle(cx);
        let pinned = workspace.update(cx, |workspace, cx| {
            let tree = workspace.profile().unwrap().session.explorer_tree.clone();
            tree.update(cx, |tree, cx| {
                let pinned = tree.entry(1).expect("the pin is listed").item().id.clone();
                assert_ne!(pinned.as_ref(), birds_row);
                tree.set_selected_index(Some(1), cx);
                pinned.to_string()
            })
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_tab(birds, window, cx);
        });
        settle(cx);
        assert_eq!(selected(cx), Some(pinned));

        // Arrowed off the tab's row, the selection outlives a rebuild that
        // lands in the background, as table sizes and folder changes do.
        workspace.update(cx, |workspace, cx| {
            let profile = workspace.profile().unwrap();
            let tree = profile.session.explorer_tree.clone();
            let id = profile.id.clone();
            tree.update(cx, |tree, cx| {
                let flocks = tree.index_of(&flocks_row.clone().into());
                assert!(flocks.is_some(), "flocks is listed");
                tree.set_selected_index(flocks, cx);
            });
            workspace.refresh_query_library(&id, cx);
        });
        settle(cx);
        assert_eq!(
            selected(cx),
            Some(flocks_row.clone()),
            "a background rebuild keeps the row the user moved to"
        );

        quit(workspace, cx);
    });
}

/// A key pressed and let go: a focused button clicks on the release, which
/// `simulate_keystrokes` never sends.
fn press(cx: &mut VisualTestContext, key: &str) {
    let keystroke = gpui::Keystroke::parse(key).expect("a key the test names");
    cx.simulate_event(gpui::KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    cx.simulate_event(gpui::KeyUpEvent { keystroke });
    settle(cx);
}

fn in_dialog(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| workspace.read(cx).dialog_focus.contains_focused(window, cx))
}

fn front_dialog(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<&'static str> {
    workspace.read_with(cx, |workspace, _| workspace.dialog_in_front.get())
}

fn palette_mode(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<PaletteMode> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .palette
            .as_ref()
            .map(|palette| palette.read(cx).mode())
    })
}

#[gpui::test]
fn tab_walks_the_import_dialog_and_its_buttons_answer_the_keyboard(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let file = store::dbdelve_directory().unwrap().join("birds.csv");
        std::fs::write(&file, "name\nwren\n").unwrap();
        workspace.update_in(cx, |workspace, window, cx| {
            let profile = workspace.profile().unwrap().id.clone();
            workspace.open_import_file(profile, "main".into(), "birds".into(), file, window, cx);
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));
        assert!(dialog_has_keyboard(&workspace, cx));
        let format = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
                dialog.map(|dialog| dialog.options.format)
            })
        };

        // Change file…, then the format's CSV and TSV.
        cx.simulate_keystrokes("tab tab tab");
        settle(cx);
        assert!(in_dialog(&workspace, cx));
        assert!(!dialog_has_keyboard(&workspace, cx));
        press(cx, "space");
        assert_eq!(format(cx), Some(crate::import_rows::Format::Tsv));

        // JSON, the header box, then the delimiter's list, which `escape` closes
        // without taking the dialog down with it.
        let in_menu = |cx: &mut VisualTestContext| {
            cx.update(|window, _| {
                window
                    .context_stack()
                    .iter()
                    .any(|context| context.contains("PopupMenu"))
            })
        };
        cx.simulate_keystrokes("tab tab tab");
        settle(cx);
        press(cx, "space");
        assert!(in_menu(cx), "space opened no list");
        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(!in_menu(cx));
        assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));

        // Round and round, never out of the dialog nor into its preview.
        let preview = workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
            dialog.unwrap().preview.clone()
        });
        for _ in 0..40 {
            cx.simulate_keystrokes("tab");
            settle(cx);
            assert!(in_dialog(&workspace, cx));
            cx.update(|window, cx| assert!(!preview.focus_handle(cx).is_focused(window)));
        }

        // The file is checked a moment after the last change to how it is read.
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        settle(cx);
        let skip_lines = workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
            assert_eq!(super::import_dialog::import_blocked(dialog.unwrap()), None);
            dialog.unwrap().inputs.skip_lines.clone()
        });
        cx.update(|window, cx| skip_lines.focus_handle(cx).focus(window, cx));
        press(cx, "enter");
        workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
            let editing = matches!(dialog.unwrap().phase, crate::session::ImportPhase::Editing);
            assert!(editing, "enter in a field started the import");
        });

        // From the dialog's own focus, back to Import and then Cancel.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.dialog_focus.focus(window, cx)
        });
        cx.simulate_keystrokes("shift-tab shift-tab");
        press(cx, "enter");
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(editor_has_keyboard(&workspace, cx));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn copy_and_paste_in_a_table_carry_rows_into_the_import_dialog(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text, note text);
             insert into birds values (1, 'wren', null), (2, 'robin', '');",
        );
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("down down enter");
        settle(cx);
        let clipboard = |cx: &mut VisualTestContext| {
            let item = cx.read_from_clipboard().expect("something was copied");
            let metadata = item.entries().iter().find_map(|entry| match entry {
                gpui::ClipboardEntry::String(string) => string.metadata.clone(),
                _ => None,
            });
            (item.text().unwrap_or_default(), metadata)
        };
        let gutter = |row: usize, cx: &mut VisualTestContext| {
            let selector: &'static str = format!("row-number-{row}").leak();
            cx.debug_bounds(selector).expect("the row is drawn")
        };

        let first = gutter(0, cx).center();
        cx.simulate_click(first, gpui::Modifiers::default());
        settle(cx);
        cx.simulate_keystrokes("secondary-c");
        let (text, metadata) = clipboard(cx);
        assert_eq!(text, "id\tname\tnote\n1\twren\t\n");
        assert!(metadata.is_some(), "a row copy carries its rows");

        let second = gutter(1, cx).center();
        cx.simulate_click(second, gpui::Modifiers::shift());
        settle(cx);
        cx.simulate_keystrokes("secondary-c");
        let (text, metadata) = clipboard(cx);
        assert_eq!(text, "id\tname\tnote\n1\twren\t\n2\trobin\t\"\"\n");
        let pasted = crate::import_rows::pasted(&text, metadata.as_deref(), false);
        assert_eq!(
            pasted.map(|(format, bytes, _)| (format, String::from_utf8(bytes).unwrap())),
            Some((
                crate::import_rows::Format::Json,
                "{\"id\":\"1\",\"name\":\"wren\",\"note\":null}\n\
                 {\"id\":\"2\",\"name\":\"robin\",\"note\":\"\"}"
                    .into()
            ))
        );

        // A cell of the first row, clicked: that cell alone, as Copy Cell has it.
        let cell = gutter(0, cx).right_center() + gpui::point(gpui::px(12.), gpui::px(0.));
        cx.simulate_click(cell, gpui::Modifiers::default());
        settle(cx);
        cx.simulate_keystrokes("right secondary-c");
        settle(cx);
        assert_eq!(clipboard(cx), ("wren".into(), None));

        // An open input keeps both keystrokes for its own text.
        let resolved: Rc<RefCell<Vec<Option<&'static str>>>> = Rc::default();
        let _observer = cx.update({
            let resolved = resolved.clone();
            |_, cx| {
                cx.observe_keystrokes(move |event, _, _| {
                    let action = event.action.as_ref().map(|action| action.name());
                    resolved.borrow_mut().push(action);
                })
            }
        });
        cx.simulate_keystrokes("enter");
        settle(cx);
        resolved.borrow_mut().clear();
        cx.simulate_keystrokes("secondary-c secondary-v");
        settle(cx);
        assert_eq!(
            *resolved.borrow(),
            [Some("input::Copy"), Some("input::Paste")]
        );
        assert_eq!(front_dialog(&workspace, cx), None);
        cx.simulate_keystrokes("escape");
        settle(cx);

        cx.write_to_clipboard(gpui::ClipboardItem::new_string("name\nfinch\n".into()));
        cx.simulate_keystrokes("secondary-v");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));
        workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
            let dialog = dialog.expect("the dialog is open");
            assert_eq!(dialog.source.name(), "Clipboard");
            assert_eq!(&*dialog.bytes, b"name\nfinch\n");
        });

        // A value a paste could not type holds the import up where it is.
        let profile =
            workspace.read_with(cx, |workspace, _| workspace.profile().unwrap().id.clone());
        workspace.update(cx, |workspace, cx| {
            let session = &mut workspace.profile_mut().unwrap().session;
            let dialog = session.import_rows.as_mut().unwrap();
            dialog.options.format = crate::import_rows::Format::Json;
            dialog.bytes = b"{\"name\":\"finch\"}".as_slice().into();
            dialog.source =
                crate::session::ImportSource::Clipboard(vec![crate::import_rows::Problem {
                    row: Some(0),
                    line: Some(1),
                    column: Some(0),
                    message: "unreadable".into(),
                }]);
            workspace.recheck_import(&profile, cx);
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
            let Some(Ok(prepared)) = &dialog.unwrap().prepared else {
                panic!("the paste was not checked");
            };
            let problems = &prepared.checked.problems;
            assert!(
                problems
                    .iter()
                    .any(|problem| problem.message == "unreadable"),
                "{problems:?}"
            );
        });

        // A paste read after another import was asked for, or after a dialog
        // opened, opens nothing.
        let take = |cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, _| {
                workspace.profile_mut().unwrap().session.import_rows.take()
            })
        };
        let shown = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
                dialog.map(|dialog| dialog.bytes.to_vec())
            })
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("late\n1\n".into()));
        let opened = take(cx);
        let opened_bytes = opened.as_ref().map(|dialog| dialog.bytes.to_vec());
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.import_rows_from_clipboard(&ImportRowsFromClipboard, window, cx);
            workspace.profile_mut().unwrap().session.import_rows = opened;
        });
        settle(cx);
        assert_eq!(shown(cx), opened_bytes);

        take(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.import_rows_from_clipboard(&ImportRowsFromClipboard, window, cx);
            workspace.import_rows_into("main".into(), "birds".into(), window, cx);
        });
        settle(cx);
        assert_eq!(shown(cx), None);

        // Nor one read while the profile moved to another database, whose
        // table of the same name is not the one asked for.
        let moved = |workspace: &mut Workspace| workspace.profile_mut().unwrap().generation += 1;
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.import_rows_from_clipboard(&ImportRowsFromClipboard, window, cx);
            moved(workspace);
        });
        settle(cx);
        assert_eq!(shown(cx), None);

        // A file read is held to both the same way.
        let file = store::dbdelve_directory().unwrap().join("late.csv");
        std::fs::write(&file, "id\n7\n").unwrap();
        let read_file =
            |workspace: &mut Workspace, window: &mut Window, cx: &mut gpui::Context<Workspace>| {
                let profile = workspace.profile().unwrap().id.clone();
                workspace.open_import_file(
                    profile,
                    "main".into(),
                    "birds".into(),
                    file.clone(),
                    window,
                    cx,
                );
            };
        workspace.update_in(cx, |workspace, window, cx| {
            read_file(workspace, window, cx);
            workspace.import_rows_from_clipboard(&ImportRowsFromClipboard, window, cx);
        });
        settle(cx);
        assert_eq!(shown(cx), Some(b"late\n1\n".to_vec()));
        take(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            read_file(workspace, window, cx);
            moved(workspace);
        });
        settle(cx);
        assert_eq!(shown(cx), None);

        quit(workspace, cx);
    });
}

#[gpui::test]
fn rows_copied_from_one_table_paste_into_another_exactly(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, note text, big integer, ratio real, body text, pic blob);
             insert into birds values
               (1, null, 9223372036854775807, 0.1, 'a	b \"q\" ''s''
ünï ✓', x'00FF10'),
               (2, '', -9007199254740993, 1.5e300, 'plain', null),
               (3, 'NULL', 0, -2.5, '', x'');
             create table nests (pic blob, body text, ratio real, big integer, note text, id integer primary key);",
        );
        cx.executor().allow_parking();
        let read = |table: &str| -> Vec<Vec<String>> {
            // Not `pic`: Import Rows writes the grid's `x'00FF10'` for a blob
            // as text, as it does reading the same value from a file.
            let columns = ["id", "note", "big", "ratio", "body"]
                .map(|c| format!("typeof({c}) || ' ' || quote({c})"))
                .join(", ");
            let db = rusqlite::Connection::open(store::dbdelve_directory().unwrap().join("e2e.db"))
                .unwrap();
            let mut statement = db
                .prepare(&format!("select {columns} from {table} order by id"))
                .unwrap();
            statement
                .query_map([], |row| (0..5).map(|i| row.get::<_, String>(i)).collect())
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let open = |name: &str, cx: &mut VisualTestContext| {
            cx.simulate_keystrokes("secondary-p");
            settle(cx);
            cx.simulate_input(name);
            cx.simulate_keystrokes("enter");
            settle(cx);
            let front = workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().unwrap().session;
                session.active_object().map(|tab| tab.name.clone())
            });
            assert_eq!(front.as_deref(), Some(name));
        };
        let paste_and_import = |cx: &mut VisualTestContext| {
            cx.simulate_keystrokes("secondary-v");
            settle(cx);
            assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));
            cx.executor()
                .advance_clock(std::time::Duration::from_secs(1));
            settle(cx);
            workspace.read_with(cx, |workspace, _| {
                let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
                assert_eq!(super::import_dialog::import_blocked(dialog.unwrap()), None);
            });
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.dialog_focus.focus(window, cx)
            });
            cx.simulate_keystrokes("shift-tab");
            press(cx, "enter");
            for _ in 0..100 {
                if front_dialog(&workspace, cx).is_none() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                settle(cx);
            }
            assert_eq!(
                front_dialog(&workspace, cx),
                None,
                "the import never finished"
            );
        };

        open("birds", cx);
        let gutter = |row: usize, cx: &mut VisualTestContext| {
            let selector: &'static str = format!("row-number-{row}").leak();
            cx.debug_bounds(selector).expect("the row is drawn")
        };
        let first = gutter(0, cx).center();
        cx.simulate_click(first, gpui::Modifiers::default());
        let last = gutter(2, cx).center();
        cx.simulate_click(last, gpui::Modifiers::shift());
        settle(cx);
        cx.simulate_keystrokes("secondary-c");

        // The way a mouse opens it: an empty table, its keyboard on nothing
        // the grid draws.
        let nests = explorer_row(&workspace, "relation-main\0nests", cx).center();
        cx.simulate_click(nests, gpui::Modifiers::default());
        settle(cx);
        paste_and_import(cx);
        assert_eq!(read("nests"), read("birds"));

        // A spreadsheet's cells, which carry no types and no NULL of their own.
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
            "id\tbody\tnote\tbig\n10\tfrom a sheet\t\t42\n".into(),
        ));
        paste_and_import(cx);
        assert_eq!(
            read("nests").last().unwrap(),
            &[
                "integer 10",
                "null NULL",
                "integer 42",
                "null NULL",
                "text 'from a sheet'",
            ]
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_import_question_picks_its_project_from_the_keyboard(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.pending_import = Some(super::PendingImport {
                source: crate::import::Source::TablePlus,
                report: crate::import::Report::default(),
                fresh: 1,
                project: Some("Imported from TablePlus".into()),
            });
            cx.notify();
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("import-choice"));
        assert_eq!(ringed_button(&workspace, cx), 0);

        cx.simulate_keystrokes("left left");
        settle(cx);
        assert_eq!(
            ringed_button(&workspace, cx),
            2,
            "the ring reaches the project"
        );
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert!(palette_mode(&workspace, cx) == Some(PaletteMode::ImportProject));

        cx.simulate_input("No project");
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert!(palette_mode(&workspace, cx).is_none());
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.pending_import.as_ref().unwrap().project, None);
        });
        assert_eq!(front_dialog(&workspace, cx), Some("import-choice"));
        assert!(dialog_has_keyboard(&workspace, cx));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_ring_reaches_dont_ask_again_and_enter_or_space_ticks_it(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::Full, cx)
        });
        cx.simulate_input("delete from birds");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("pending-run"));
        assert_eq!(ringed_button(&workspace, cx), 0, "the ring starts on Run");
        let dont_ask = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().unwrap().session;
                session.pending_run.as_ref().map(|pending| pending.dont_ask)
            })
        };

        for (keystroke, ringed, ticked) in [
            // A space typed on as the prompt goes up presses nothing.
            ("space", 0, false),
            ("left left", 2, false),
            ("enter", 2, true),
            ("space", 2, false),
            ("enter", 2, true),
            ("tab", 1, true),
            ("tab", 0, true),
            ("tab", 2, true),
        ] {
            cx.simulate_keystrokes(keystroke);
            settle(cx);
            assert_eq!(ringed_button(&workspace, cx), ringed, "after {keystroke}");
            assert_eq!(dont_ask(cx), Some(ticked), "after {keystroke}");
            assert_eq!(front_dialog(&workspace, cx), Some("pending-run"));
        }

        cx.simulate_keystrokes("right right enter");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().unwrap();
            assert!(
                profile
                    .confirmed
                    .contains(&crate::sql::Destructive::UnfilteredDelete),
                "Run ran with the box ticked"
            );
        });

        quit(workspace, cx);
    });
}

/// The opt-in coming into force under a prompt already up -- switched on, or
/// a Read-only hold landing -- must not take the prompt down with its
/// statement neither run nor cancelled.
#[gpui::test]
fn a_run_once_prompt_already_up_stays_up_when_the_opt_in_applies(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("selec 1");
        raise_run_once_prompt(&workspace, cx);

        workspace.update(cx, |workspace, cx| workspace.set_run_unreadable(true, cx));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("pending-run"));

        press(cx, "enter");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        query_tab(&workspace, cx, |tab, _, _| {
            assert!(matches!(tab.query, QueryState::Failed(_)), "Run once ran");
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn ticking_run_once_s_box_runs_the_next_unreadable_statement_unasked(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table t (a integer);");
        cx.executor().allow_parking();
        cx.simulate_input("selec 1");
        raise_run_once_prompt(&workspace, cx);

        cx.simulate_keystrokes("left left");
        settle(cx);
        assert_eq!(ringed_button(&workspace, cx), 2, "the ring reaches the box");
        cx.simulate_keystrokes("enter");
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert!(session.pending_run.as_ref().unwrap().dont_ask);
            assert!(!workspace.settings.run_unreadable, "ticking alone set it");
        });

        cx.simulate_keystrokes("right right enter");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        query_tab(&workspace, cx, |tab, _, _| {
            assert!(matches!(tab.query, QueryState::Failed(_)), "Run once ran");
        });
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.settings.run_unreadable)
        });
        let saved =
            std::fs::read_to_string(store::dbdelve_directory().unwrap().join("profiles.toml"))
                .unwrap();
        assert!(saved.contains("run_unreadable = true"), "{saved}");

        // Unreadable to sqlparser, and a read SQLite answers with rows.
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("PRAGMA table_info(t)");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None, "it asked again");
        query_tab(&workspace, cx, |tab, _, _| match &tab.query {
            QueryState::Complete { rows, .. } => assert_eq!(*rows, 1),
            QueryState::Failed(error) => panic!("the pragma failed: {error}"),
            _ => panic!("the pragma never ran"),
        });

        // What an edit's refresh reruns, unasked: that still asks, and offers
        // no box, because nobody pressed Run.
        workspace.update(cx, |workspace, cx| {
            let tab = workspace.profile().unwrap().session.active;
            workspace.execute_sql("PRAGMA table_info(t)".into(), tab, cx);
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("pending-run"));
        workspace.read_with(cx, |workspace, _| assert!(!workspace.dialog_checkbox()));

        quit(workspace, cx);
    });
}

/// Birds and the sightings that reference them, with the keyboard on the
/// first bird's key.
fn launch_on_a_referenced_key(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    let (workspace, cx) = launch_on_sqlite_holding(
        cx,
        "create table birds (id integer primary key, name text);
         create table sightings (id integer primary key, bird integer references birds(id));
         insert into birds values (1, 'wren');
         insert into sightings values (1, 1);",
    );
    cx.simulate_keystrokes("secondary-k e");
    settle(cx);
    cx.simulate_keystrokes("down down enter");
    settle(cx);
    assert_eq!(front_object(&workspace, cx).as_deref(), Some("birds"));
    cx.simulate_keystrokes("down");
    settle(cx);
    (workspace, cx)
}

fn front_object(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    workspace.read_with(cx, |workspace, _| {
        let session = &workspace.profile().unwrap().session;
        session.active_object().map(|tab| tab.name.clone())
    })
}

fn ask_for_references(cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("secondary-shift-p");
    settle(cx);
    cx.simulate_input("Show references");
    cx.simulate_keystrokes("enter");
    settle(cx);
}

#[gpui::test]
fn show_references_lists_the_referencing_tables_in_the_palette(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_a_referenced_key(cx);
        ask_for_references(cx);
        assert!(palette_mode(&workspace, cx) == Some(PaletteMode::References));

        cx.simulate_keystrokes("enter");
        settle(cx);
        assert!(palette_mode(&workspace, cx).is_none());
        assert_eq!(front_object(&workspace, cx).as_deref(), Some("sightings"));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn references_listed_for_one_key_never_open_from_another(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text);
             create table flocks (id integer primary key);
             create table members (id integer primary key, flock integer references flocks(id));
             create table sightings (id integer primary key, bird integer references birds(id));
             insert into birds values (1, 'wren');
             insert into flocks values (1);
             insert into members values (1, 1);
             insert into sightings values (1, 1);",
        );
        let active = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace.profile().unwrap().session.active
            })
        };
        // Flocks' key under the keyboard, then birds'.
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("down down down enter");
        settle(cx);
        assert_eq!(front_object(&workspace, cx).as_deref(), Some("flocks"));
        cx.simulate_keystrokes("down");
        settle(cx);
        let flocks = active(cx);
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("up enter");
        settle(cx);
        assert_eq!(front_object(&workspace, cx).as_deref(), Some("birds"));
        cx.simulate_keystrokes("down");
        settle(cx);
        let birds = active(cx);

        // Picked once flocks is in front again: its own reference sits at the
        // same index, and opening it would read members with flocks' key.
        ask_for_references(cx);
        assert!(palette_mode(&workspace, cx) == Some(PaletteMode::References));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_tab(flocks, window, cx);
        });
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(active(cx), flocks);
        assert_eq!(front_object(&workspace, cx).as_deref(), Some("flocks"));

        // Answered once flocks is in front again.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_tab(birds, window, cx);
        });
        settle(cx);
        let checks_before = workspace.read_with(cx, |workspace, _| workspace.reference_checks);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.show_references(&crate::actions::ShowReferences, window, cx);
            workspace.activate_tab(flocks, window, cx);
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.reference_checks,
                checks_before + 1,
                "no lookup started, so nothing stale was tested"
            );
        });
        assert!(
            palette_mode(&workspace, cx).is_none(),
            "a stale answer was listed"
        );
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.reference_popup.is_none())
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn escape_closes_the_reference_arrows_list(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.reference_popup = Some(super::ReferencePopup {
                at: Some(gpui::point(gpui::px(10.), gpui::px(10.))),
                source: super::ReferenceSource {
                    profile: String::new(),
                    tab: 0,
                    column: String::new(),
                    value: String::new(),
                    referenced_by: Vec::new(),
                },
                choices: Some(Vec::new()),
                check: 0,
            });
            cx.notify();
        });
        settle(cx);
        cx.simulate_keystrokes("escape");
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.reference_popup.is_none())
        });
        assert!(editor_has_keyboard(&workspace, cx));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn tab_reaches_the_default_sorting_choice_in_settings(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_keystrokes("secondary-,");
        settle(cx);
        cx.simulate_input("Default sorting");
        settle(cx);
        let client_sort =
            |cx: &mut VisualTestContext| workspace.read_with(cx, |w, _| w.settings.client_sort);
        assert!(!client_sort(cx));

        // Server, then Client.
        cx.simulate_keystrokes("tab tab");
        settle(cx);
        press(cx, "space");
        assert!(client_sort(cx));

        quit(workspace, cx);
    });
}

#[cfg(target_os = "macos")]
#[gpui::test]
fn an_existing_cmd_m_binding_can_be_recaptured_in_settings(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            "[settings]\ncheck_for_updates = false\n\
             [settings.custom_keybindings]\nshow_messages = \"secondary-m\"\n",
        )
        .unwrap();
        let (workspace, cx) = launch(cx);
        cx.simulate_keystrokes("secondary-,");
        settle(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.start_rebind("show_messages", window, cx);
            });
        });
        settle(cx);
        cx.simulate_keystrokes("cmd-m");
        settle(cx);
        let saved = store::peek_profiles(&[])
            .unwrap()
            .settings
            .unwrap()
            .custom_keybindings
            .unwrap();
        assert_eq!(
            saved.get("show_messages").map(String::as_str),
            Some("cmd-m")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_selection_failing_part_way_says_what_it_already_committed(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "CREATE TABLE t (a INTEGER);");
        cx.executor().allow_parking();
        cx.simulate_input(
            "insert into t values (1); insert into t values (2); \
             select * from nope; insert into t values (3)",
        );
        cx.simulate_keystrokes("secondary-a secondary-enter");
        settle(cx);
        let failed = workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("a profile is active");
            assert!(profile.session.queue_failure.is_some());
            let tab = profile.session.active_query_tab().expect("a query tab");
            let last = tab.queue.as_ref().and_then(|queue| queue.done.last());
            match &last.expect("a queue with results").state {
                QueryState::Failed(error) => error.message.clone(),
                _ => panic!("the third statement did not fail"),
            }
        });
        assert!(
            failed.ends_with("\n\n2 statements before it were already committed."),
            "{failed}"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn stepping_to_the_next_result_keeps_the_keyboard_in_the_grid(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.executor().allow_parking();
        cx.simulate_input("select 1; select 2");
        cx.simulate_keystrokes("secondary-a secondary-enter");
        settle(cx);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        let shown = |cx: &mut VisualTestContext| {
            query_tab(&workspace, cx, |tab, window, cx| {
                (
                    tab.switcher_selected(),
                    tab.shown().1.focus_handle(cx).is_focused(window),
                )
            })
        };
        assert_eq!(shown(cx), (Some(0), true));

        cx.dispatch_action(crate::actions::NextResult);
        settle(cx);
        assert_eq!(shown(cx), (Some(1), true));
        assert!(!editor_has_keyboard(&workspace, cx));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_palette_switches_who_sorts_the_view(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("select 1 as a");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        let client = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                crate::views::view_sorts_on_client(workspace.profile().unwrap(), cx)
            })
        };
        assert_eq!(client(cx), Some(false));

        for (row, after) in [("Sort on the client", true), ("Sort on the server", false)] {
            cx.simulate_keystrokes("secondary-shift-p");
            settle(cx);
            cx.simulate_input(row);
            cx.simulate_keystrokes("enter");
            settle(cx);
            assert_eq!(client(cx), Some(after), "after {row}");
        }

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_palette_copies_what_the_explorer_menu_copies(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("down down enter");
        settle(cx);

        for (row, copied) in [
            ("Copy SELECT (Top 100)", "SELECT"),
            ("Copy DROP", "DROP TABLE"),
        ] {
            cx.simulate_keystrokes("secondary-shift-p");
            settle(cx);
            cx.simulate_input(row);
            cx.simulate_keystrokes("enter");
            settle(cx);
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default();
            assert!(
                text.contains(copied) && text.contains("birds"),
                "{row}: {text}"
            );
        }
        // Copied, never run.
        let tables = rusqlite::Connection::open(store::dbdelve_directory().unwrap().join("e2e.db"))
            .and_then(|db| {
                db.query_row("select count(*) from sqlite_master", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .unwrap();
        assert_eq!(tables, 1);

        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_empty_table_opened_from_the_sidebar_keeps_the_keyboard(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("down down enter");
        settle(cx);
        let rows = workspace.read_with(cx, |workspace, cx| {
            let session = &workspace.profile().unwrap().session;
            session
                .active_results()
                .map(|grid| grid.read(cx).delegate().total_rows())
        });
        assert_eq!(rows, Some(0));
        cx.update(|window, _| assert!(!window.context_stack().is_empty(), "no dispatch path"));

        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        assert!(palette_mode(&workspace, cx).is_some());

        quit(workspace, cx);
    });
}

#[gpui::test]
fn tab_onto_a_welcome_project_shows_its_rename_and_delete(cx: &mut TestAppContext) {
    if ["PGHOST", "PGPORT", "PGDATABASE", "PGUSER"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return;
    }
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            "[settings]\ncheck_for_updates = false\n[[projects]]\nname = \"Birds\"\n",
        )
        .unwrap();
        let (workspace, cx) = launch(cx);
        settle(cx);
        let buttons_have_keyboard = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let buttons = workspace.read(cx).welcome_project_focus.borrow()[0].clone();
                buttons.contains_focused(window, cx)
            })
        };
        assert!(!buttons_have_keyboard(cx));

        let mut reached = false;
        for _ in 0..30 {
            cx.simulate_keystrokes("tab");
            settle(cx);
            if buttons_have_keyboard(cx) {
                reached = true;
                break;
            }
        }
        assert!(reached, "Tab never reached the project's buttons");
        cx.update(|window, cx| {
            let buttons = workspace.read(cx).welcome_project_focus.borrow()[0].clone();
            assert!(
                !buttons.is_focused(window),
                "Rename or Delete has it, not their holder"
            );
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_clicked_box_leaves_the_ring_in_charge_of_the_keyboard(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::Full, cx)
        });
        cx.simulate_input("delete from birds");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("pending-run"));

        let box_at = cx
            .debug_bounds("pending-run-2")
            .expect("the box is drawn in the ring")
            .center();
        cx.simulate_click(box_at, gpui::Modifiers::default());
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert!(session.pending_run.as_ref().unwrap().dont_ask);
        });

        // Round to Run, which `enter` presses rather than the box it would tick.
        cx.simulate_keystrokes("tab tab tab");
        settle(cx);
        assert_eq!(ringed_button(&workspace, cx), 0);
        assert!(dialog_has_keyboard(&workspace, cx));
        press(cx, "enter");
        assert_eq!(front_dialog(&workspace, cx), None, "enter pressed nothing");
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().unwrap();
            assert!(
                profile
                    .confirmed
                    .contains(&crate::sql::Destructive::UnfilteredDelete)
            );
        });

        quit(workspace, cx);
    });
}

/// A session moved on behind the modeless save panel still gets its rows, under
/// every column they hold, rather than what its connection now says of a table
/// of that name.
#[gpui::test]
fn a_session_changed_behind_the_save_panel_is_not_asked_about_the_table(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().expect("the test home has a data directory");
        std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
        let database = directory.join("e2e.duckdb");
        ::duckdb::Connection::open(&database)
            .and_then(|connection| {
                connection.execute_batch(
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, \
                     twice INTEGER GENERATED ALWAYS AS (id * 2) VIRTUAL); \
                     INSERT INTO t (id) VALUES (1);",
                )
            })
            .expect("the schema must apply");
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\n\
                 check_for_updates = false\n\
                 [[profiles]]\n\
                 id = \"e2e\"\n\
                 name = \"E2E\"\n\
                 host = \"\"\n\
                 database = \"\"\n\
                 user = \"\"\n\
                 engine = \"duckdb\"\n\
                 path = {:?}\n",
                database.display().to_string()
            ),
        )
        .expect("profiles.toml must be writable");
        let (workspace, cx) = launch(cx);
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| profile.config.engine() == Engine::DuckDb)
                .expect("the saved DuckDB profile is restored");
            workspace.activate(index, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("SELECT * FROM t");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);

        let mut export = |changed: bool, name: &str| {
            let path = directory.join(name);
            workspace.update(cx, |workspace, cx| {
                workspace.export_results(crate::export::FileAs::Inserts, cx);
            });
            settle(cx);
            if changed {
                workspace.update(cx, |workspace, _| {
                    workspace.profile_mut().unwrap().generation += 1;
                });
            }
            let chosen = path.clone();
            cx.simulate_new_path_selection(|_| Some(chosen));
            for _ in 0..500 {
                settle(cx);
                if path.exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            std::fs::read_to_string(&path).expect("the file is written")
        };

        assert!(!export(false, "same.sql").contains("twice"));
        assert!(export(true, "changed.sql").contains("twice"));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_export_question_takes_the_keyboard_and_hands_it_back_when_dismissed(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        // Past the default row limit, so the tab holds the first rows only.
        cx.simulate_input(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 5000) \
             SELECT x FROM n",
        );
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| {
            assert!(
                matches!(tab.query, QueryState::Complete { .. }),
                "the query never completed"
            );
        });

        for (dismiss, typed) in [("escape", " "), ("tab tab enter", "x")] {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .export_results(crate::export::FileAs::Rows(crate::export::Format::Csv), cx);
            });
            settle(cx);
            workspace.read_with(cx, |workspace, _| {
                assert_eq!(workspace.dialog_in_front.get(), Some("pending-export"));
            });
            assert!(dialog_has_keyboard(&workspace, cx));
            assert_eq!(
                ringed_button(&workspace, cx),
                0,
                "the ring starts on Export All Rows"
            );
            for (keystroke, ringed) in [
                ("left", 1),
                ("left", 2),
                ("left", 0),
                ("right", 2),
                ("right", 1),
                ("right", 0),
            ] {
                cx.simulate_keystrokes(keystroke);
                settle(cx);
                assert_eq!(ringed_button(&workspace, cx), ringed, "after {keystroke}");
            }

            let before = buffer(&workspace, cx);
            cx.simulate_keystrokes(dismiss);
            settle(cx);
            workspace.read_with(cx, |workspace, _| {
                assert_eq!(
                    workspace.dialog_in_front.get(),
                    None,
                    "{dismiss} left it up"
                );
                let session = &workspace.profile().unwrap().session;
                assert!(session.exporting.is_none(), "{dismiss} started an export");
            });
            assert!(editor_has_keyboard(&workspace, cx), "after {dismiss}");
            cx.simulate_input(typed);
            settle(cx);
            assert_eq!(buffer(&workspace, cx), format!("{before}{typed}"));
        }

        quit(workspace, cx);
    });
}

fn relation_names(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    workspace.read_with(cx, |workspace, _| {
        let Some(CatalogState::Loaded(catalog, _)) =
            workspace.profile().map(|profile| &profile.catalog)
        else {
            panic!("the catalog should have loaded");
        };
        catalog
            .schemas
            .iter()
            .flat_map(|schema| &schema.relations)
            .map(|relation| relation.name.clone())
            .collect()
    })
}

#[gpui::test]
fn the_refresh_shortcut_finds_a_table_made_elsewhere_over_the_same_session(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        settle(cx);
        assert!(relation_names(&workspace, cx).is_empty());
        let generation = workspace.read_with(cx, |workspace, _| {
            workspace.profile().map(|profile| profile.generation)
        });

        let database = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("e2e.db");
        rusqlite::Connection::open(database)
            .expect("the database should open")
            .execute_batch("CREATE TABLE made_elsewhere (id INTEGER)")
            .expect("the table should be created");

        cx.simulate_keystrokes("secondary-shift-r");
        settle(cx);
        assert_eq!(relation_names(&workspace, cx), ["made_elsewhere"]);

        // Sent straight to the session, because its table has to stay
        // uncommitted: only a read beside the session cannot see it, and a
        // refresh that dropped the session would leave nothing to `COMMIT`.
        let session = workspace
            .read_with(cx, |workspace, _| {
                workspace.profile().and_then(|profile| profile.connection())
            })
            .expect("the profile is connected");
        let run = |sql: &str| {
            session
                .query(sql, &CancelToken::default(), Fetch::default())
                .expect("the statement should run");
        };
        run("BEGIN");
        run("CREATE TABLE not_committed (id INTEGER)");
        workspace.update(cx, |workspace, _| {
            if let Some(profile) = workspace.profile_mut() {
                profile.in_transaction = true;
            }
        });

        cx.simulate_keystrokes("secondary-shift-r");
        settle(cx);
        assert_eq!(relation_names(&workspace, cx), ["made_elsewhere"]);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the profile is still open");
            assert_eq!(Some(profile.generation), generation, "it reconnected");
            assert!(!profile.refreshing_schema());
            assert_eq!(
                profile.session.notice.as_deref(),
                Some("Schema refreshed."),
                "the read beside the session failed"
            );
        });

        run("COMMIT");
        cx.simulate_keystrokes("secondary-shift-r");
        settle(cx);
        let mut names = relation_names(&workspace, cx);
        names.sort();
        assert_eq!(names, ["made_elsewhere", "not_committed"]);

        quit(workspace, cx);
    });
}

fn history_view(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> crate::palette::HistoryView {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .palette
            .as_ref()
            .expect("the palette is up")
            .read(cx)
            .history()
    })
}

#[gpui::test]
fn the_history_scope_filters_and_sorts_its_runs_from_the_keyboard(cx: &mut TestAppContext) {
    use crate::palette::{HistoryOrder, HistoryStatus, HistoryView};
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("select 1");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let last = &workspace.profile().unwrap().session.history[0];
            assert_eq!((last.sql.as_str(), last.rows), ("select 1", Some(1)));
            assert!(last.at.is_some() && last.ms.is_some() && last.error.is_none());
        });

        // Outside History the chords do nothing.
        cx.simulate_keystrokes("secondary-p");
        settle(cx);
        cx.simulate_keystrokes("secondary-k f secondary-k o");
        settle(cx);
        assert_eq!(history_view(&workspace, cx), HistoryView::default());

        cx.simulate_keystrokes("tab tab");
        settle(cx);
        assert!(palette_mode(&workspace, cx) == Some(PaletteMode::Search(Scope::History)));
        cx.simulate_keystrokes("secondary-k f secondary-k o secondary-k o");
        settle(cx);
        assert_eq!(
            history_view(&workspace, cx),
            HistoryView {
                status: HistoryStatus::Succeeded,
                order: HistoryOrder::MostRows,
            }
        );

        // A palette opened again starts unfiltered.
        cx.simulate_keystrokes("escape");
        settle(cx);
        cx.simulate_keystrokes("secondary-p");
        settle(cx);
        assert_eq!(history_view(&workspace, cx), HistoryView::default());

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_count_whose_tab_closed_under_it_still_leaves_its_outcome_in_the_history(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.executor().allow_parking();
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        cx.simulate_keystrokes("down down enter");
        settle(cx);
        let object = workspace.read_with(cx, |workspace, _| {
            match workspace.profile().unwrap().session.active {
                crate::session::Tab::Object(id) => id,
                crate::session::Tab::Query(_) => panic!("the table did not open"),
            }
        });

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.count_rows(object, cx);
                workspace.close_object(object, window, cx);
            })
        });
        settle(cx);

        workspace.read_with(cx, |workspace, _| {
            let last = &workspace.profile().unwrap().session.history[0];
            assert!(last.sql.to_lowercase().contains("count"), "{}", last.sql);
            assert!(last.ms.is_some(), "the count's outcome was dropped");
        });
        let profile =
            workspace.read_with(cx, |workspace, _| workspace.profile().unwrap().id.clone());
        assert!(store::history(&profile)[0].ms.is_some());

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_new_folder_takes_its_name_from_the_keyboard_and_hands_it_back(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let leaf =
                    workspace.profile().unwrap().session.explorer_leaves["relation-main\0birds"];
                let member = workspace.member_at(leaf.target);
                assert!(member.is_some(), "the table is nameable");
                workspace.new_folder(member, window, cx);
            });
        });
        settle(cx);
        let naming = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let session = &workspace.read(cx).profile().unwrap().session;
                session
                    .query_rename
                    .as_ref()
                    .map(|rename| rename.input.focus_handle(cx).is_focused(window))
            })
        };
        assert_eq!(
            naming(cx),
            Some(true),
            "the folder's field has the keyboard"
        );

        cx.simulate_input("Birds");
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(naming(cx), None);
        assert!(editor_has_keyboard(&workspace, cx));
        cx.update(|_, cx| {
            let profile = workspace.read(cx).profile().unwrap();
            let folders = &profile.shortcuts.folders;
            assert_eq!(folders.len(), 1);
            assert_eq!(folders[0].name, "Birds");
            assert_eq!(folders[0].members.len(), 1);
            let tree = profile.session.explorer_tree.read(cx);
            let row = tree.entry(0).expect("the folder leads the sidebar");
            assert_eq!(row.item().label.as_ref(), "Birds");
            assert_eq!(row.item().children[0].label.as_ref(), "birds");
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_folder_keeps_the_sidebar_drawn_while_the_catalog_is_away(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.update(|_, cx| {
            workspace.update(cx, |workspace, _| {
                let profile = workspace.profile_mut().unwrap();
                profile.catalog = CatalogState::Failed("gone".into());
                assert!(profile.session.saved_queries.is_empty());
                assert!(!super::forms::explorer_drawn(profile));

                profile.shortcuts.add_folder("Hot").unwrap();
                assert!(
                    super::forms::explorer_drawn(profile),
                    "its Remove and Unpin are reachable"
                );
            });
        });

        quit(workspace, cx);
    });
}

/// Where the sidebar draws `row`, by its id.
fn explorer_row(
    workspace: &Entity<Workspace>,
    row: &str,
    cx: &mut VisualTestContext,
) -> gpui::Bounds<gpui::Pixels> {
    let index = cx
        .update(|_, cx| {
            let tree = workspace
                .read(cx)
                .profile()
                .unwrap()
                .session
                .explorer_tree
                .clone();
            tree.read(cx).index_of(&row.to_string().into())
        })
        .expect("the row is listed");
    // `debug_bounds` takes a `'static` selector; a test's few rows can leak.
    let selector: &'static str = format!("explorer-row-{index}").leak();
    cx.debug_bounds(selector).expect("the row is drawn")
}

/// The id of the row listing `member` in `folder`, or among the pins.
fn shortcut_row(
    workspace: &Entity<Workspace>,
    member: &explorer::Member,
    folder: Option<&str>,
    cx: &mut VisualTestContext,
) -> String {
    cx.update(|_, cx| {
        let session = &workspace.read(cx).profile().unwrap().session;
        let listed = explorer::Shortcut::Member {
            member: member.clone(),
            folder: folder.map(str::to_string),
            missing: false,
        };
        session
            .explorer_shortcuts
            .iter()
            .find(|(_, shortcut)| **shortcut == listed)
            .map(|(row, _)| row.clone())
            .expect("the shortcut is listed")
    })
}

/// A left-button drag from `from` to `to`, frame by frame, as the pointer
/// would make it.
fn drag(
    cx: &mut VisualTestContext,
    from: gpui::Point<gpui::Pixels>,
    to: gpui::Point<gpui::Pixels>,
) {
    let none = gpui::Modifiers::default();
    cx.simulate_mouse_down(from, gpui::MouseButton::Left, none);
    for at in [from + gpui::point(gpui::px(0.), gpui::px(8.)), to] {
        cx.simulate_mouse_move(at, gpui::MouseButton::Left, none);
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
    cx.simulate_mouse_up(to, gpui::MouseButton::Left, none);
    settle(cx);
}

#[gpui::test]
fn a_shortcut_dragged_between_folders_moves_and_one_from_the_schemas_is_added(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let table = "relation-main\0birds";
        let birds = cx.update(|_, cx| {
            workspace.update(cx, |workspace, cx| {
                let leaf = workspace.profile().unwrap().session.explorer_leaves[table];
                let birds = workspace.member_at(leaf.target).unwrap();
                let shortcuts = &mut workspace.profile_mut().unwrap().shortcuts;
                shortcuts.add_folder("Hot").unwrap();
                shortcuts.add_folder("Cold").unwrap();
                workspace.toggle_in_folder("Hot".into(), birds.clone(), cx);
                birds
            })
        });
        settle(cx);
        let held = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let shortcuts = &workspace.read(cx).profile().unwrap().shortcuts;
                ["Hot", "Cold"].map(|name| shortcuts.holds(name, &birds))
            })
        };
        let in_hot = shortcut_row(&workspace, &birds, Some("Hot"), cx);

        let from = explorer_row(&workspace, &in_hot, cx).center();
        let to = explorer_row(&workspace, &explorer::folder_row("Cold"), cx).center();
        drag(cx, from, to);
        assert_eq!(
            held(cx),
            [false, true],
            "out of one folder into another moves"
        );

        let from = explorer_row(&workspace, table, cx).center();
        let to = explorer_row(&workspace, &explorer::folder_row("Hot"), cx).center();
        drag(cx, from, to);
        assert_eq!(held(cx), [true, true], "from the schemas it is added");
        assert!(
            sidebar_has_keyboard(&workspace, cx),
            "the sidebar keeps the keyboard"
        );
        assert!(
            cx.update(|_, cx| workspace
                .read(cx)
                .profile()
                .unwrap()
                .session
                .objects
                .is_empty()),
            "a drag opens nothing"
        );

        // Dropped where nothing takes it, a drag changes nothing.
        let from = explorer_row(&workspace, table, cx).center();
        drag(cx, from, from + gpui::point(gpui::px(0.), gpui::px(2_000.)));
        assert_eq!(held(cx), [true, true]);

        let at = explorer_row(&workspace, table, cx).center();
        cx.simulate_click(at, gpui::Modifiers::default());
        settle(cx);
        assert_eq!(
            cx.update(|_, cx| workspace.read(cx).profile().unwrap().session.objects.len()),
            1,
            "a click still opens the row"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_pin_dropped_on_the_top_half_of_another_goes_before_it(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (name text); create table fish (name text);",
        );
        let [birds, fish] = cx.update(|_, cx| {
            workspace.update(cx, |workspace, cx| {
                ["birds", "fish"].map(|name| {
                    let leaf = workspace.profile().unwrap().session.explorer_leaves
                        [&explorer::relation_row("main", name)];
                    let member = workspace.member_at(leaf.target).unwrap();
                    workspace.toggle_pin(explorer::Pin::Member(member.clone()), cx);
                    member
                })
            })
        });
        settle(cx);

        let from = explorer_row(&workspace, &shortcut_row(&workspace, &fish, None, cx), cx);
        let onto = explorer_row(&workspace, &shortcut_row(&workspace, &birds, None, cx), cx);
        drag(
            cx,
            from.center(),
            onto.origin + gpui::point(gpui::px(20.), gpui::px(2.)),
        );
        let pins = cx.update(|_, cx| workspace.read(cx).profile().unwrap().shortcuts.pins.clone());
        assert_eq!(
            pins,
            [explorer::Pin::Member(fish), explorer::Pin::Member(birds)]
        );

        quit(workspace, cx);
    });
}

/// Through the real restore and the real save: what `Profile::stored` writes
/// for a plugin profile is the user's string, never its password, and it
/// restores to the same connection.
#[gpui::test]
fn an_odbc_profile_saves_and_restores_as_it_was(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let odbc = store::tests::odbc_plugin();
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        // A SQLite profile in front, since launching connects the active one
        // and a plugin profile would reach for the keychain.
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").unwrap();
        let strings = [
            "Db2Dev",
            "DRIVER={PostgreSQL Unicode}; Server = pg.internal ;Port=5432;Opts={a;b}}c}",
        ];
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\ncheck_for_updates = false\n\
                 [[profiles]]\nid = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = {:?}\n\
                 [[profiles]]\nid = \"dsn\"\nname = \"DSN\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"a@b.example\"\nengine = \"plugin:odbc\"\nspeaks = \"generic\"\n\
                 connection = {:?}\nstatement_timeout = 30\n\
                 [[profiles]]\nid = \"tunnel\"\nname = \"Tunnel\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"u\"\nengine = \"plugin:odbc\"\nspeaks = \"postgres\"\n\
                 connection = {:?}\n\
                 [profiles.ssh]\nhost = \"bastion.example\"\nport = 2222\nuser = \"me\"\n",
                database.display().to_string(),
                strings[0],
                strings[1],
            ),
        )
        .unwrap();

        let (workspace, cx) = launch_with(cx, vec![odbc]);
        let external = |workspace: &Workspace| {
            workspace
                .profiles
                .iter()
                .filter(|profile| matches!(profile.config, ConnectionConfig::External(_)))
                .map(|profile| profile.config.clone())
                .collect::<Vec<_>>()
        };
        let restored = cx.update(|_, cx| external(workspace.read(cx)));
        let expected = |connection: &str, speaks, user: &str, timeout, ssh| {
            ConnectionConfig::External(ExternalConfig {
                plugin: odbc,
                speaks,
                connection: connection.into(),
                user: user.into(),
                password: String::new(),
                statement_timeout: timeout,
                ssm: None,
                ssh,
            })
        };
        assert_eq!(
            restored,
            [
                expected(strings[0], Engine::Generic, "a@b.example", 30, None),
                expected(
                    strings[1],
                    Engine::Postgres,
                    "u",
                    0,
                    Some(SshTunnel {
                        host: "bastion.example".into(),
                        port: Some(2222),
                        user: "me".into(),
                        identity_file: None,
                        run_as: None,
                        passphrase: String::new(),
                    })
                ),
            ]
        );

        workspace.update(cx, |workspace, cx| {
            for profile in &mut workspace.profiles {
                if let ConnectionConfig::External(config) = &mut profile.config {
                    config.password = "hunter2".into();
                }
            }
            workspace.remember_profiles(cx);
        });
        let saved = std::fs::read_to_string(directory.join("profiles.toml")).unwrap();
        assert!(!saved.contains("hunter2"), "{saved}");
        let list = store::decode_profiles(&saved, &[odbc]).unwrap();
        let written = list
            .profiles
            .iter()
            .filter_map(|stored| {
                Some((
                    stored.engine.as_deref()?,
                    stored.speaks.as_deref()?,
                    stored.connection.as_deref()?,
                ))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            written,
            [
                ("plugin:odbc", "generic", strings[0]),
                ("plugin:odbc", "postgres", strings[1]),
            ]
        );

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.profiles.clear();
                for stored in list.profiles {
                    workspace.restore_profile(stored, window, cx);
                }
            });
        });
        assert_eq!(cx.update(|_, cx| external(workspace.read(cx))), restored);

        quit(workspace, cx);
    });
}

/// Through the real restore, the edit form and the real save: a password
/// command comes back as written, disables the Password field while it is
/// there, and is dropped from an engine whose login never runs one.
#[gpui::test]
fn a_password_command_restores_into_the_edit_form_and_saves_back(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        // A SQLite profile in front, since launching connects the active one
        // and a server profile would reach for its login.
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").unwrap();
        let command = "aws rds generate-db-auth-token --hostname %HOSTNAME% --username %USERNAME%";
        let server = |id: &str, engine: &str| {
            format!(
                "[[profiles]]\nid = \"{id}\"\nname = \"{id}\"\nhost = \"db.internal\"\n\
                 database = \"d\"\nuser = \"u\"\nengine = \"{engine}\"\n\
                 password_command = {command:?}\npassword_expires = 900\n"
            )
        };
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\ncheck_for_updates = false\n\
                 [[profiles]]\nid = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = {:?}\n{}{}",
                database.display().to_string(),
                server("rds", "postgres"),
                server("mongo", "mongodb"),
            ),
        )
        .unwrap();

        let (workspace, cx) = launch(cx);
        let rds = cx.update(|_, cx| {
            workspace
                .read(cx)
                .profiles
                .iter()
                .position(|profile| profile.id == "rds")
                .unwrap()
        });
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.edit_profile(rds, window, cx));
        });
        settle(cx);
        cx.update(|_, cx| {
            let form = workspace.read(cx).form.as_ref().expect("the form is open");
            assert_eq!(form.password_command.read(cx).value(), command);
            assert_eq!(form.password_expires.read(cx).value(), "900");
            let password = form.password.read(cx).presentation();
            assert!(password.is_disabled());
            assert_eq!(password.placeholder(), "From the command");
            let (_, config) = form.config(cx).unwrap();
            assert_eq!(
                config.password_command(),
                Some(&crate::db::PasswordCommand {
                    command: command.into(),
                    expires: 900,
                })
            );
            assert_eq!(config.password(), Some(""));
        });

        // Cleared, the Password field is the profile's own again.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let form = workspace.form.as_ref().unwrap();
                form.password_command
                    .update(cx, |input, cx| input.set_value("", window, cx));
                cx.notify();
            });
        });
        settle(cx);
        cx.update(|_, cx| {
            let form = workspace.read(cx).form.as_ref().unwrap();
            let password = form.password.read(cx).presentation();
            assert!(!password.is_disabled());
            assert_eq!(password.placeholder(), "Unchanged");
            assert_eq!(form.config(cx).unwrap().1.password_command(), None);
        });

        cx.update(|_, cx| workspace.update(cx, |workspace, cx| workspace.remember_profiles(cx)));
        let saved = std::fs::read_to_string(directory.join("profiles.toml")).unwrap();
        let list = store::decode_profiles(&saved, &[]).unwrap();
        let written = |id: &str| {
            let stored = list.profiles.iter().find(|stored| stored.id == id).unwrap();
            (stored.password_command.as_deref(), stored.password_expires)
        };
        assert_eq!(written("rds"), (Some(command), Some(900)));
        assert_eq!(written("mongo"), (None, None));

        quit(workspace, cx);
    });
}

/// A duplicate has no Keychain entry of its own, so its Password field carries
/// the original's, for an ODBC profile as for a server.
#[gpui::test]
fn duplicating_an_odbc_profile_keeps_its_password(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\ncheck_for_updates = false\n\
                 [[profiles]]\nid = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = {:?}\n\
                 [[profiles]]\nid = \"dsn\"\nname = \"DSN\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"a@b.example\"\nengine = \"plugin:odbc\"\nspeaks = \"generic\"\n\
                 connection = \"Db2Dev\"\n",
                database.display().to_string(),
            ),
        )
        .unwrap();
        let (workspace, cx) = launch_with(cx, vec![store::tests::odbc_plugin()]);
        let password = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let profile = workspace
                    .profiles
                    .iter()
                    .find(|profile| profile.id == "dsn")
                    .unwrap();
                let form = ConnectionForm::duplicating(
                    profile,
                    "DSN copy".into(),
                    [Some("hunter2".into()), None],
                    window,
                    cx,
                );
                form.password.read(cx).unmask_value().to_string()
            })
        });
        assert_eq!(password, "hunter2");
        quit(workspace, cx);
    });
}

/// What Save and Test read the form as refuses Run as beside a host that is
/// no socket directory, as a connect would, and leaves a blank one beside a
/// service to the connect, since the service may name a socket directory.
#[gpui::test]
fn the_forms_save_and_test_refuse_run_as_without_a_socket_directory(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").unwrap();
        let postgres = |id: &str, host: &str, service: &str| {
            format!(
                "[[profiles]]\nid = \"{id}\"\nname = \"{id}\"\nhost = \"{host}\"\n\
                 database = \"d\"\nuser = \"u\"\nengine = \"postgres\"\n{service}\
                 [profiles.ssh]\nhost = \"bastion\"\nrun_as = \"postgres\"\n"
            )
        };
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\ncheck_for_updates = false\n\
                 [[profiles]]\nid = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = {:?}\n{}{}{}",
                database.display().to_string(),
                postgres("tcp", "db.internal", ""),
                postgres("socket", "/var/run/postgresql", ""),
                postgres("service", "", "service = \"socketed\"\n"),
            ),
        )
        .unwrap();
        let (workspace, cx) = launch(cx);
        let read_as = |cx: &mut VisualTestContext, id: &str| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let profile = workspace
                        .profiles
                        .iter()
                        .find(|profile| profile.id == id)
                        .unwrap();
                    let form = ConnectionForm::duplicating(
                        profile,
                        "Copy".into(),
                        [None, None],
                        window,
                        cx,
                    );
                    form.config(cx).map(|_| ())
                })
            })
        };
        let refused = read_as(cx, "tcp").unwrap_err();
        assert!(refused.starts_with("Run as needs Host"), "{refused}");
        assert_eq!(read_as(cx, "socket"), Ok(()));
        assert_eq!(read_as(cx, "service"), Ok(()));
        quit(workspace, cx);
    });
}

/// Turning the tunnel off with the keyboard in Key passphrase takes that row
/// away, so the keyboard goes to Password rather than nowhere, and typing
/// lands there.
#[gpui::test]
fn turning_the_tunnel_off_from_key_passphrase_hands_the_keyboard_to_password(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_connection_in(None, window, cx);
            });
        });
        settle(cx);
        let toggle_tunnel = |cx: &mut VisualTestContext| {
            let enabled =
                workspace.read_with(cx, |workspace, _| workspace.form.as_ref().unwrap().ssh);
            let switch = cx
                .debug_bounds(if enabled {
                    "tunnel-kind-0"
                } else {
                    "ssh-tunnel"
                })
                .expect("the switch is drawn")
                .center();
            cx.simulate_click(switch, gpui::Modifiers::default());
            settle(cx);
        };
        let field = |cx: &mut VisualTestContext,
                     field: fn(&ConnectionForm) -> &Entity<InputState>| {
            cx.update(|_, cx| field(workspace.read(cx).form.as_ref().unwrap()).clone())
        };

        toggle_tunnel(cx);
        let passphrase = field(cx, |form| &form.ssh_passphrase);
        cx.update(|window, cx| passphrase.focus_handle(cx).focus(window, cx));
        settle(cx);
        cx.simulate_input("correct horse");
        assert_eq!(
            cx.update(|_, cx| passphrase.read(cx).unmask_value().to_string()),
            "correct horse"
        );

        toggle_tunnel(cx);
        let password = field(cx, |form| &form.password);
        assert!(!workspace.read_with(cx, |workspace, _| workspace.form.as_ref().unwrap().ssh));
        assert!(cx.update(|window, cx| password.focus_handle(cx).is_focused(window)));
        cx.simulate_input("hunter2");
        assert_eq!(
            cx.update(|_, cx| password.read(cx).unmask_value().to_string()),
            "hunter2"
        );

        quit(workspace, cx);
    });
}

/// The ODBC tile is picked from the engine picker by keyboard, and the
/// keyboard goes on working through both field-set changes: the Connection
/// field takes it, and Host takes it back once Postgres unmounts that field.
#[gpui::test]
fn picking_the_odbc_tile_puts_the_keyboard_in_its_connection_string(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_with(cx, vec![store::tests::odbc_plugin()]);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_connection_in(None, window, cx);
            });
        });
        settle(cx);
        let has_keyboard =
            |cx: &mut VisualTestContext, field: fn(&ConnectionForm) -> &Entity<InputState>| {
                cx.update(|window, cx| {
                    let workspace = workspace.read(cx);
                    let form = workspace.form.as_ref().expect("the form is open");
                    field(form).focus_handle(cx).is_focused(window)
                })
            };
        let pick = |cx: &mut VisualTestContext, typed: &str| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.open_palette(PaletteMode::Engine, window, cx);
                });
            });
            settle(cx);
            cx.simulate_input(typed);
            cx.simulate_keystrokes("enter");
            settle(cx);
        };

        pick(cx, "odbc");
        assert!(palette_mode(&workspace, cx).is_none());
        workspace.read_with(cx, |workspace, _| {
            let form = workspace.form.as_ref().unwrap();
            assert_eq!(form.target().fields(), crate::db::Fields::ConnectionString);
            assert_eq!(form.engine, Engine::Generic);
            assert_eq!(form.mode, crate::sql::Mode::ReadOnly);
        });
        assert!(has_keyboard(cx, |form| &form.connection));
        cx.simulate_input("Db2Dev");
        let typed = cx.update(|_, cx| {
            let workspace = workspace.read(cx);
            let form = workspace.form.as_ref().unwrap();
            form.connection.read(cx).value().to_string()
        });
        assert_eq!(typed, "Db2Dev");

        pick(cx, "postgres");
        workspace.read_with(cx, |workspace, _| {
            let form = workspace.form.as_ref().unwrap();
            assert_eq!(form.target(), crate::db::Target::built_in(Engine::Postgres));
        });
        assert!(has_keyboard(cx, |form| &form.host));

        quit(workspace, cx);
    });
}

/// Read-only picked while a connect is under way finds no connection to hold
/// (`set_mode`), so the session that lands is opened again under it rather
/// than left as the Read-write it set out as, with Read-only on screen.
#[gpui::test]
fn a_mode_picked_while_connecting_is_the_one_the_session_holds(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let echo = crate::db::echo::plugin();
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\ncheck_for_updates = false\n\
                 [[profiles]]\nid = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = {:?}\n\
                 [[profiles]]\nid = \"echo\"\nname = \"Echo\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"plugin:echo\"\nspeaks = \"postgres\"\n\
                 connection = \"DSN=echo\"\n",
                database.display().to_string(),
            ),
        )
        .unwrap();

        let (workspace, cx) = launch_with(cx, vec![echo]);
        workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| matches!(profile.config, ConnectionConfig::External(_)))
                .expect("the echo profile is restored");
            assert_eq!(workspace.profiles[index].mode, crate::sql::Mode::ReadWrite);
            // Set so the connect never reaches for the keychain.
            if let ConnectionConfig::External(config) = &mut workspace.profiles[index].config {
                config.password = "x".into();
            }
            workspace.activate(index, cx);
            workspace.set_mode(crate::sql::Mode::ReadOnly, cx);
        });
        cx.run_until_parked();

        let connection = cx
            .update(|_, cx| {
                workspace
                    .read(cx)
                    .profile()
                    .and_then(|profile| profile.connection())
            })
            .expect("the echo profile connects");
        let last = connection
            .query("last", &CancelToken::default(), Fetch::default())
            .unwrap();
        assert_eq!(
            last.rows[0].get(0),
            Some("SET default_transaction_read_only = on")
        );

        quit(workspace, cx);
    });
}

/// A table whose own statement the gate cannot read says so where its rows
/// would be, rather than spinning, and a visit does not ask again.
#[gpui::test]
fn a_relation_whose_query_dbdelve_cannot_read_says_so_once(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            "active = \"echo\"\n\
             [settings]\ncheck_for_updates = false\n\
             [[profiles]]\nid = \"echo\"\nname = \"Echo\"\nhost = \"\"\ndatabase = \"\"\n\
             user = \"\"\nengine = \"plugin:echo\"\nspeaks = \"mysql\"\n\
             connection = \"DSN=echo\"\n",
        )
        .unwrap();

        let (workspace, cx) = launch_with(cx, vec![crate::db::echo::plugin()]);
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| matches!(profile.config, ConnectionConfig::External(_)))
                .unwrap();
            if let ConnectionConfig::External(config) = &mut workspace.profiles[index].config {
                config.password = "x".into();
            }
            workspace.activate(index, cx);
        });
        cx.run_until_parked();

        // `x``y` is one name to MySQL and a name and an alias to the grammar.
        let visit = |id: Option<u64>, cx: &mut VisualTestContext| {
            let id = cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let id = id.unwrap_or_else(|| {
                        let opened = crate::session::OpenedObject::Relation {
                            schema: "s".into(),
                            name: "x`y".into(),
                            kind: Default::default(),
                            filter: String::new(),
                            filters: Vec::new(),
                        };
                        workspace.open_object(opened, window, cx).unwrap()
                    });
                    workspace.activate_tab(crate::session::Tab::Object(id), window, cx);
                    id
                })
            });
            cx.run_until_parked();
            cx.update(|_, cx| {
                let profile = workspace.read(cx).profile().unwrap();
                let tab = profile.session.objects.iter().find(|tab| tab.id == id);
                let ObjectBody::Relation { query, .. } = &tab.unwrap().body else {
                    panic!("x`y opened as its rows");
                };
                let QueryState::Failed(error) = query else {
                    panic!("the refused tab is not failed");
                };
                assert!(error.message.contains("its own query"), "{}", error.message);
                (id, profile.session.last_structure_request)
            })
        };
        let (id, asked) = visit(None, cx);
        assert_eq!(visit(Some(id), cx), (id, asked));

        quit(workspace, cx);
    });
}

/// A bar edit Generic refuses is put back, so the bars the tab saves stay the
/// ones its filter ran from.
#[gpui::test]
fn a_refused_bar_edit_leaves_the_bars_as_they_ran(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n\
                 [settings]\ncheck_for_updates = false\n\
                 [[profiles]]\nid = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = {:?}\n\
                 [[profiles]]\nid = \"echo\"\nname = \"Echo\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"plugin:echo\"\nspeaks = \"generic\"\n\
                 connection = \"DSN=echo\"\n",
                database.display().to_string(),
            ),
        )
        .unwrap();

        let (workspace, cx) = launch_with(cx, vec![crate::db::echo::plugin()]);
        // The connect and the tab's query run on threads the scheduler does
        // not own; echo answers both at once, so each wait below ends.
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| matches!(profile.config, ConnectionConfig::External(_)))
                .unwrap();
            if let ConnectionConfig::External(config) = &mut workspace.profiles[index].config {
                config.password = "x".into();
            }
            workspace.activate(index, cx);
        });
        cx.run_until_parked();

        let bar = |column: &str, operator, value: &str| crate::filter::FilterBar {
            column: Some(column.into()),
            operator,
            value: value.into(),
            ..Default::default()
        };
        let ran = vec![
            bar("name", crate::filter::Operator::Contains, "ab"),
            bar("id", crate::filter::Operator::Equals, "50%"),
        ];
        let id = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let opened = crate::session::OpenedObject::Relation {
                    schema: "public".into(),
                    name: "accounts".into(),
                    kind: Default::default(),
                    filter: crate::filter::derived_filter(Engine::Generic, &ran, &[]),
                    filters: ran.clone(),
                };
                let id = workspace.open_object(opened, window, cx).unwrap();
                workspace.activate_tab(crate::session::Tab::Object(id), window, cx);
                id
            })
        });
        cx.run_until_parked();

        let saved = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let workspace = workspace.read(cx);
                let profile = workspace.profile().unwrap();
                let tab = profile
                    .session
                    .objects
                    .iter()
                    .find(|tab| tab.id == id)
                    .unwrap();
                let notice = profile.session.shown_notice().map(str::to_string);
                (tab.filters(Engine::Generic), notice)
            })
        };
        let as_ran: Vec<_> = ran.iter().map(crate::filter::stored_filter).collect();

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let input = workspace.filter_rows_mut(id).unwrap()[0].value.clone();
                input.update(cx, |input, cx| input.set_value("a%b", window, cx));
                workspace.submit_filter(id, &input, cx);
            })
        });
        let (bars, notice) = saved(cx);
        assert_eq!(bars, as_ran);
        assert!(notice.is_some_and(|notice| notice.contains("LIKE")));

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let action = crate::actions::SetFilterOperator {
                    row: 1,
                    operator: crate::filter::Operator::Contains,
                };
                workspace.set_filter_operator(&action, window, cx);
            })
        });
        assert_eq!(saved(cx).0, as_ran);

        quit(workspace, cx);
    });
}

#[gpui::test]
fn pinning_leaves_every_row_its_height(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let table = explorer::relation_row("main", "birds");
        let unpinned = explorer_row(&workspace, &table, cx).size.height;

        cx.update(|_, cx| {
            workspace.update(cx, |workspace, cx| {
                let leaf = workspace.profile().unwrap().session.explorer_leaves[&table];
                let birds = workspace.member_at(leaf.target).unwrap();
                workspace.toggle_pin(explorer::Pin::Member(birds), cx);
            })
        });
        settle(cx);

        assert_eq!(explorer_row(&workspace, &table, cx).size.height, unpinned);
        assert_eq!(
            explorer_row(&workspace, explorer::PINNED_GROUP, cx)
                .size
                .height,
            unpinned,
            "the Pinned group, listed first, is the row every row is sized by"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_service_launch_does_not_reuse_a_profiles_explicit_port(cx: &mut TestAppContext) {
    // PGSERVICE must be set before the process starts, not raced against
    // every other test that reads the environment.
    const CHILD: &str = "DBDELVE_SERVICE_PORT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "workspace::e2e::a_service_launch_does_not_reuse_a_profiles_explicit_port",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("PGSERVICE", "startup-port")
            .env_remove("PGSERVICEFILE")
            .env_remove("PGSYSCONFDIR")
            .status()
            .expect("the child test should run");
        assert!(status.success());
        return;
    }

    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            "active = \"explicit\"\n\
             [settings]\n\
             check_for_updates = false\n\
             [[profiles]]\n\
             id = \"explicit\"\n\
             name = \"Explicit port\"\n\
             host = \"\"\n\
             port = 5432\n\
             database = \"\"\n\
             user = \"\"\n\
             service = \"startup-port\"\n\
             [[profiles]]\n\
             id = \"inherited\"\n\
             name = \"Service port\"\n\
             host = \"\"\n\
             database = \"\"\n\
             user = \"\"\n\
             service = \"startup-port\"\n",
        )
        .unwrap();
        let (workspace, cx) = launch(cx);
        let active =
            workspace.read_with(cx, |workspace, _| workspace.profile().unwrap().id.clone());
        quit(workspace, cx);
        assert_eq!(active, "inherited");
    });
}

#[gpui::test]
fn a_click_on_an_empty_table_takes_the_keyboard_for_a_paste(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_on_sqlite_holding(cx, "create table nests (id integer primary key);");
        cx.executor().allow_parking();
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("id\n5\n".into()));
        let nests = explorer_row(&workspace, "relation-main\0nests", cx).center();
        cx.simulate_click(nests, gpui::Modifiers::default());
        settle(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus.focus(window, cx)
        });
        settle(cx);

        let empty = cx
            .debug_bounds("empty-results")
            .expect("the message is drawn");
        cx.simulate_click(empty.center(), gpui::Modifiers::default());
        settle(cx);
        cx.simulate_keystrokes("secondary-v");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_ways_to_import_show_only_where_rows_can_be_imported(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key); insert into birds values (1);
             create table nests (id integer primary key);",
        );
        cx.executor().allow_parking();
        let drawn = |selector: &'static str, cx: &mut VisualTestContext| {
            cx.debug_bounds(selector).is_some()
        };
        let open = |name: &str, cx: &mut VisualTestContext| {
            let row: &'static str = format!("relation-main\0{name}").leak();
            let row = explorer_row(&workspace, row, cx).center();
            cx.simulate_click(row, gpui::Modifiers::default());
            settle(cx);
        };
        assert!(!drawn("import-rows", cx), "a query tab imports nothing");

        open("birds", cx);
        assert!(drawn("import-rows", cx));
        assert!(
            !drawn("paste-rows", cx),
            "a table with rows needs no invitation"
        );
        let menu_offers_paste = cx.update(|_, cx| {
            let session = &workspace.read(cx).profile().unwrap().session;
            session
                .active_results()
                .unwrap()
                .read(cx)
                .delegate()
                .takes_imports()
        });
        assert!(menu_offers_paste);

        open("nests", cx);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("id\n5\n".into()));
        let paste = cx
            .debug_bounds("paste-rows")
            .expect("an empty table invites rows");
        cx.simulate_click(paste.center(), gpui::Modifiers::default());
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));
        cx.simulate_keystrokes("escape");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);

        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::ReadOnly, cx)
        });
        settle(cx);
        assert!(!drawn("paste-rows", cx), "read-only has nothing to invite");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn file_exports_from_the_menu_ask_for_the_right_file(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_on_sqlite_holding(cx, "create table birds (id integer primary key);");
        cx.executor().allow_parking();
        cx.simulate_input("select * from birds");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);

        for (downs, name) in [(3, "results.csv"), (4, "results.json"), (5, "results.sql")] {
            super::exports::SUGGESTED_NAMES.with_borrow_mut(Vec::clear);
            let bounds = cx
                .debug_bounds("export-results")
                .expect("Export button is drawn");
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            settle(cx);
            for _ in 0..downs {
                cx.simulate_keystrokes("down");
            }
            press(cx, "enter");
            cx.simulate_new_path_selection(|_| None);
            settle(cx);
            let asked = super::exports::SUGGESTED_NAMES.with_borrow(Clone::clone);
            assert_eq!(asked, [name], "{downs} downs");
        }

        quit(workspace, cx);
    });
}

#[gpui::test]
fn copy_results_formats_work_from_menu_and_palette_and_restore_focus(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, note text);
             insert into birds values (1, null), (2, ''), (3, 'NULL');",
        );
        cx.executor().allow_parking();
        let sql = "select * from birds order by id";
        cx.simulate_input(sql);
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);

        let read_clipboard = |cx: &mut VisualTestContext| {
            let item = cx.read_from_clipboard().expect("copy writes the clipboard");
            let metadata = item.entries().iter().find_map(|entry| match entry {
                gpui::ClipboardEntry::String(string) => string.metadata.clone(),
                _ => None,
            });
            (item.text().unwrap(), metadata)
        };
        let expected = |format: crate::export::ClipboardFormat, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                let grid = workspace
                    .profile()
                    .unwrap()
                    .session
                    .active_results()
                    .unwrap()
                    .read(cx)
                    .delegate();
                format.render(Engine::Sqlite, grid.result()).unwrap()
            })
        };
        let open_menu = |cx: &mut VisualTestContext| {
            let bounds = cx
                .debug_bounds("export-results")
                .expect("Export button is drawn");
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            settle(cx);
            cx.update(|window, _| {
                assert!(
                    window
                        .context_stack()
                        .iter()
                        .any(|context| context.contains("PopupMenu"))
                )
            });
        };

        for object_tab in [false, true] {
            if object_tab {
                let row = explorer_row(&workspace, "relation-main\0birds", cx).center();
                cx.simulate_click(row, gpui::Modifiers::default());
                settle(cx);
            }
            open_menu(cx);
            cx.simulate_keystrokes("down");
            press(cx, "enter");
            assert_eq!(
                read_clipboard(cx).0,
                expected(crate::export::ClipboardFormat::Tsv, cx)
            );

            for (ix, format) in crate::export::ClipboardFormat::ALL.into_iter().enumerate() {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string("before menu".into()));
                open_menu(cx);
                cx.simulate_keystrokes("down down");
                settle(cx);
                // The menu hangs off the right edge, so its submenu opens leftwards.
                cx.simulate_keystrokes("left");
                settle(cx);
                for _ in 0..ix {
                    cx.simulate_keystrokes("down");
                }
                press(cx, "enter");
                let (text, metadata) = read_clipboard(cx);
                assert_eq!(text, expected(format, cx), "menu {format:?}");
                let metadata = metadata.expect("all successful table copies carry original rows");
                let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
                assert_eq!(metadata["rows"][0]["note"], serde_json::Value::Null);
                assert_eq!(metadata["rows"][1]["note"], "");
                assert_eq!(metadata["rows"][2]["note"], "NULL");
                assert!(metadata["columns"][0]["type"].is_string());
                let (_, bytes, _) = crate::import_rows::pasted(
                    &text,
                    Some(&serde_json::to_string(&metadata).unwrap()),
                    false,
                )
                .unwrap();
                let pasted: Vec<serde_json::Value> = String::from_utf8(bytes)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(pasted, metadata["rows"].as_array().unwrap().clone());
                settle(cx);
                cx.update(|window, _| {
                    assert!(
                        !window
                            .context_stack()
                            .iter()
                            .any(|context| context.contains("PopupMenu"))
                    )
                });

                cx.write_to_clipboard(gpui::ClipboardItem::new_string("before palette".into()));
                cx.simulate_keystrokes("secondary-shift-p");
                settle(cx);
                cx.simulate_input(&format!("Copy results as {}", format.label()));
                press(cx, "enter");
                assert_eq!(read_clipboard(cx).0, text, "palette {format:?}");
            }
            open_menu(cx);
            press(cx, "escape");
            cx.update(|window, cx| {
                assert!(
                    !window
                        .context_stack()
                        .iter()
                        .any(|context| context.contains("PopupMenu"))
                );
                assert!(
                    window.focused(cx).is_some(),
                    "Escape returns a live dispatch path"
                );
            });
            // The existing copy action still defaults to TSV after a table format was used.
            cx.simulate_keystrokes("secondary-k r");
            settle(cx);
            cx.update(|window, cx| {
                window.dispatch_action(Box::new(crate::actions::CopyResults), cx);
            });
            assert_eq!(
                read_clipboard(cx).0,
                expected(crate::export::ClipboardFormat::Tsv, cx)
            );
            if !object_tab {
                assert_eq!(buffer(&workspace, cx), sql);
            } else {
                let grid = workspace.read_with(cx, |workspace, _| {
                    workspace
                        .profile()
                        .unwrap()
                        .session
                        .active_results()
                        .unwrap()
                        .clone()
                });
                grid.update(cx, |grid, cx| {
                    assert!(grid.delegate_mut().set_pending(
                        0,
                        1,
                        crate::result_grid::NewValue::Value("pending".into())
                    ));
                    cx.notify();
                });
                workspace.update(cx, |_, cx| cx.notify());
                settle(cx);
                assert!(cx.debug_bounds("export-results").is_none());
                for format in crate::export::ClipboardFormat::ALL {
                    workspace.update(cx, |workspace, cx| workspace.copy_results_as(format, cx));
                    assert_eq!(read_clipboard(cx).0, expected(format, cx));
                    assert!(!read_clipboard(cx).0.contains("pending"));
                }
                grid.update(cx, |grid, _| grid.delegate_mut().discard_pending());
            }
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn refused_table_copies_preserve_the_clipboard(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let grid = workspace.read_with(cx, |workspace, _| {
            workspace
                .profile()
                .unwrap()
                .session
                .active_results()
                .unwrap()
                .clone()
        });
        let column = crate::db::Column {
            name: "v".into(),
            data_type: Some("text".into()),
        };
        let result = crate::db::QueryResult {
            columns: vec![column.clone()],
            rows: crate::db::rows(vec![vec![Some(
                "x".repeat(crate::result_grid::COPIED_ROWS_TEXT_LIMIT),
            )]]),
            ..Default::default()
        };
        grid.update(cx, |grid, _| {
            *grid.delegate_mut() =
                crate::result_grid::ResultGrid::new(result, crate::sql::Mode::ReadOnly);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("previous clipboard".into()));
        for format in crate::export::ClipboardFormat::ALL.into_iter().skip(2) {
            workspace.update(cx, |workspace, cx| workspace.copy_results_as(format, cx));
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().as_deref(),
                Some("previous clipboard")
            );
            workspace.read_with(cx, |workspace, _| {
                assert_eq!(
                    workspace.profile().unwrap().session.notice.as_deref(),
                    Some("Table output exceeds 16 MiB. Copy as TSV/CSV or export to a file.")
                );
            });
        }
        let width = (crate::result_grid::COPIED_ROWS_TEXT_LIMIT - 24) / 5;
        let value = format!("{}\u{301}", "x".repeat(width));
        grid.update(cx, |grid, _| {
            *grid.delegate_mut() = crate::result_grid::ResultGrid::new(
                crate::db::QueryResult {
                    columns: vec![column.clone()],
                    rows: crate::db::rows(vec![vec![Some(value.clone())]]),
                    ..Default::default()
                },
                crate::sql::Mode::ReadOnly,
            );
        });
        workspace.update(cx, |workspace, cx| {
            workspace.copy_results_as(crate::export::ClipboardFormat::AsciiTable, cx);
        });
        let item = cx.read_from_clipboard().unwrap();
        assert_eq!(
            item.text().unwrap().len(),
            crate::result_grid::COPIED_ROWS_TEXT_LIMIT
        );
        let metadata = item
            .entries()
            .iter()
            .find_map(|entry| match entry {
                gpui::ClipboardEntry::String(string) => string.metadata.as_deref(),
                _ => None,
            })
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_str(metadata).unwrap();
        assert_eq!(metadata["rows"][0]["v"], value);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("previous clipboard".into()));
        for result in [
            crate::result_grid::ResultGrid::new(
                crate::db::QueryResult::default(),
                crate::sql::Mode::ReadOnly,
            ),
            crate::result_grid::ResultGrid::new(
                crate::db::QueryResult {
                    columns: vec![column.clone()],
                    capped_from: Some(10),
                    ..Default::default()
                },
                crate::sql::Mode::ReadOnly,
            ),
            crate::result_grid::ResultGrid::new(
                crate::db::QueryResult {
                    columns: vec![column],
                    ..Default::default()
                },
                crate::sql::Mode::ReadOnly,
            )
            .streaming(0),
        ] {
            grid.update(cx, |grid, _| *grid.delegate_mut() = result);
            for format in crate::export::ClipboardFormat::ALL {
                workspace.update(cx, |workspace, cx| workspace.copy_results_as(format, cx));
                assert_eq!(
                    cx.read_from_clipboard().unwrap().text().as_deref(),
                    Some("previous clipboard")
                );
            }
        }
        quit(workspace, cx);
    });
}

fn disconnect(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.dispatch_action(Box::new(Disconnect), cx));
    settle(cx);
}

/// What the palette up lists, whatever scope each row is in.
fn palette_labels(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    workspace.read_with(cx, |workspace, cx| {
        let palette = workspace.palette.as_ref().expect("the palette is up");
        palette
            .read(cx)
            .labels()
            .into_iter()
            .map(str::to_string)
            .collect()
    })
}

fn buffers_of(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.profiles[0]
            .session
            .queries
            .iter()
            .map(|tab| tab.editor.read(cx).value().to_string())
            .collect()
    })
}

#[gpui::test]
fn disconnect_leads_home_and_a_relaunch_stays_there(cx: &mut TestAppContext) {
    // CI's live job sets `PG*`, whose profile would be put in front on launch.
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("select 1");
        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        let offered = palette_labels(&workspace, cx);
        assert!(offered.iter().any(|label| label == "Disconnect"));
        assert!(offered.iter().any(|label| label == "Disconnect from E2E"));
        cx.simulate_keystrokes("escape");
        settle(cx);

        disconnect(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().is_none());
            assert!(matches!(workspace.profiles[0].state, ProfileState::Idle));
        });
        assert_eq!(buffers_of(&workspace, cx), ["select 1"]);
        assert_eq!(store::peek_profiles(&[]).unwrap().active, None);
        cx.update(|window, cx| {
            assert!(
                window.focused(cx).is_some(),
                "the welcome surface takes the keyboard"
            )
        });

        // Home lists the connections and nothing that needs one in front.
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(crate::actions::SwitchConnection), cx)
        });
        settle(cx);
        assert!(palette_mode(&workspace, cx) == Some(PaletteMode::Search(Scope::Connections)));
        let offered = palette_labels(&workspace, cx);
        assert!(offered.iter().any(|label| label == "E2E"));
        assert!(!offered.iter().any(|label| label.starts_with("Disconnect")));
        cx.simulate_keystrokes("escape");
        settle(cx);

        // A second launch over the same home, as `launch` builds it.
        drop(workspace);
        let mut relaunched = None;
        let (_, cx) = cx.cx.add_window_view(|window, cx| {
            let entity = cx.new(|cx| Workspace::new(Vec::new(), window, cx));
            relaunched = Some(entity.clone());
            Root::new(entity, window, cx)
        });
        let workspace = relaunched.expect("the window builds the workspace");
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().is_none());
            assert!(matches!(workspace.profiles[0].state, ProfileState::Idle));
        });
        assert_eq!(buffers_of(&workspace, cx), ["select 1"]);

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_connection_picked_on_the_welcome_surface_comes_in_front_connected(cx: &mut TestAppContext) {
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        disconnect(cx);
        let row = cx
            .debug_bounds("welcome-connection-0")
            .expect("the connection is listed");
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the picked one is in front");
            assert!(matches!(profile.state, ProfileState::Connected(_)));
        });
        assert_eq!(
            store::peek_profiles(&[]).unwrap().active.as_deref(),
            Some("e2e")
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn disconnect_is_refused_while_a_grid_holds_unapplied_edits(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text); \
             insert into birds values (1, 'wren');",
        );
        cx.simulate_input("select * from birds");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        grid.update(cx, |grid, _| {
            assert!(grid.delegate_mut().set_pending(
                0,
                1,
                crate::result_grid::NewValue::Value("robin".into())
            ));
        });

        disconnect(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection stays in front");
            assert!(matches!(profile.state, ProfileState::Connected(_)));
            let notice = profile.session.notice.as_deref().unwrap_or_default();
            assert!(
                notice.ends_with("so the connection was not closed."),
                "{notice}"
            );
        });

        grid.update(cx, |grid, _| grid.delegate_mut().discard_pending());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn enter_on_text_that_spells_no_bytes_keeps_the_input_open_and_says_why(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table blobs (id integer primary key, b blob); \
             insert into blobs values (1, x'6869');",
        );
        cx.simulate_input("select * from blobs");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        press(cx, "right");
        press(cx, "enter");
        assert!(!grid_has_keyboard(&workspace, cx), "the input has it");

        cx.simulate_input("zz");
        press(cx, "enter");
        assert!(
            !grid_has_keyboard(&workspace, cx),
            "the input is still open"
        );
        workspace.read_with(cx, |workspace, cx| {
            let session = &workspace.profile().expect("a connection in front").session;
            assert_eq!(
                session.shown_notice(),
                Some("This is not hex: pairs of digits 0-9 and A-F, so it was not staged.")
            );
            // A refusal of the text, not of the mode.
            assert!(session.pending_run.is_none());
            let grid = session.active_results().expect("rows on screen");
            assert!(!grid.read(cx).delegate().has_pending());
        });

        press(cx, "escape");
        assert!(grid_has_keyboard(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_new_parse_as_form_closes_the_input_on_its_column_and_gives_the_grid_the_keyboard(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table blobs (id integer primary key, b blob); \
             insert into blobs values (1, x'6869');",
        );
        cx.simulate_input("select * from blobs");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        press(cx, "right");
        press(cx, "enter");
        assert!(!grid_has_keyboard(&workspace, cx), "the input has it");

        let grid = workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().expect("a connection in front").session;
            session.active_results().expect("rows on screen").clone()
        });
        grid.update_in(cx, |table, window, cx| {
            crate::result_grid::parse_as(table, 1, crate::result_grid::BinaryForm::Text, window, cx)
        });
        settle(cx);
        assert!(grid_has_keyboard(&workspace, cx));
        assert!(!grid.read_with(cx, |grid, _| grid.delegate().has_pending()));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_new_parse_as_form_refills_a_row_panel_draft_typed_in_the_old_one(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table blobs (id integer primary key, b blob); \
             insert into blobs values (1, x'4142');",
        );
        cx.simulate_input("select * from blobs");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        press(cx, "right");
        press(cx, "secondary-shift-e");
        let field = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                let inputs = workspace.row_panel.inputs.as_ref().expect("Edit mode");
                let field = inputs.fields[1].as_ref().expect("an input on the blob");
                (
                    field.input.clone(),
                    field.input.read(cx).value().to_string(),
                )
            })
        };
        let (input, seeded) = field(&workspace, cx);
        assert_eq!(seeded, "x'4142'");

        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.set_value("4142!", window, cx));
        });
        settle(cx);
        assert_eq!(field(&workspace, cx).1, "4142!", "a draft is kept");

        let grid = workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().expect("a connection in front").session;
            session.active_results().expect("rows on screen").clone()
        });
        grid.update_in(cx, |table, window, cx| {
            crate::result_grid::parse_as(table, 1, crate::result_grid::BinaryForm::Text, window, cx)
        });
        settle(cx);
        assert_eq!(field(&workspace, cx).1, "AB");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn disconnect_is_refused_while_an_export_runs(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        workspace.update(cx, |workspace, _| {
            let profile = workspace.profile_mut().expect("a connection in front");
            profile.session.exporting = Some(crate::session::Exporting {
                file: "birds.csv".into(),
                started: std::time::Instant::now(),
                progress: Default::default(),
            });
        });

        disconnect(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection stays in front");
            assert!(matches!(profile.state, ProfileState::Connected(_)));
            let notice = profile.session.notice.as_deref().unwrap_or_default();
            assert_eq!(
                notice,
                "An export to birds.csv is running, so the connection was not closed."
            );
        });

        workspace.update(cx, |workspace, _| {
            workspace.profile_mut().expect("in front").session.exporting = None;
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn disconnect_stops_what_a_relation_tab_holds_the_connection_for(cx: &mut TestAppContext) {
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let row = explorer_row(&workspace, "relation-main\0birds", cx).center();
        cx.simulate_click(row, gpui::Modifiers::default());
        settle(cx);
        // A count still out, which a run left going is the same as: a task
        // holding a clone of the connection until the server answers.
        workspace.update(cx, |workspace, _| {
            let profile = workspace.profile_mut().expect("a profile is open");
            let tab = profile
                .session
                .objects
                .first_mut()
                .expect("the table opened");
            let ObjectBody::Relation { count, .. } = &mut tab.body else {
                panic!("a relation tab");
            };
            *count = crate::session::RowCount::Counting {
                filter: String::new(),
                started: std::time::Instant::now(),
                cancel: CancelToken::default(),
                cancelling: false,
            };
        });

        disconnect(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().is_none());
            let tab = &workspace.profiles[0].session.objects[0];
            assert!(matches!(
                &tab.body,
                ObjectBody::Relation {
                    count: crate::session::RowCount::Counting {
                        cancelling: true,
                        ..
                    },
                    ..
                }
            ));
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn disconnect_closes_an_import_dialog_that_is_not_running(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let file = store::dbdelve_directory().unwrap().join("birds.csv");
        std::fs::write(&file, "name\nwren\n").unwrap();
        let profile = workspace.update_in(cx, |workspace, window, cx| {
            let profile = workspace.profile().unwrap().id.clone();
            workspace.open_import_file(
                profile.clone(),
                "main".into(),
                "birds".into(),
                file,
                window,
                cx,
            );
            profile
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("import-rows"));

        disconnect(cx);
        workspace.read_with(cx, |workspace, _| {
            let found = workspace.profiles.iter().find(|found| found.id == profile);
            assert!(found.unwrap().session.import_rows.is_none());
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn files_opened_while_connecting_are_not_run_by_a_later_connect(cx: &mut TestAppContext) {
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        settle(cx);
        cx.executor().allow_parking();
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("birds.csv");
        std::fs::write(&file, "name\nwren\n").unwrap();
        // In one update, so the connect the file starts cannot land between.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_data_files(vec![file], window, cx);
            assert!(!workspace.runs_on_connect.is_empty());
            workspace.disconnect(&Disconnect, window, cx);
            assert!(workspace.runs_on_connect.is_empty());
        });
        settle(cx);

        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the files profile is in front");
            assert!(matches!(profile.state, ProfileState::Connected(_)));
        });
        query_tab(&workspace, cx, |tab, _, cx| {
            assert!(matches!(tab.query, QueryState::Idle));
            assert!(
                !tab.editor.read(cx).value().is_empty(),
                "the statement stays"
            );
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_default_chip_puts_a_typed_then_emptied_insert_field_back_out_of_the_statement(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text, wing text);",
        );
        workspace.update_in(cx, |workspace, window, cx| {
            let leaf = workspace.profile().unwrap().session.explorer_leaves
                [&explorer::relation_row("main", "birds")];
            workspace.open_explorer_target(leaf.target, window, cx);
        });
        settle(cx);
        let open_form = |cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.new_row(&crate::actions::NewRow, window, cx);
            });
            settle(cx);
        };
        let input = |index: usize, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let form = workspace.profile().unwrap().session.insert_form.as_ref();
                form.unwrap().fields[index].input.clone()
            })
        };
        let focus = |index: usize, cx: &mut VisualTestContext| {
            let input = input(index, cx);
            cx.update(|window, cx| input.focus_handle(cx).focus(window, cx));
            settle(cx);
        };
        let typing_in = |index: usize, cx: &mut VisualTestContext| {
            let input = input(index, cx);
            cx.update(|window, cx| input.focus_handle(cx).is_focused(window))
        };
        let state = |index: usize, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                let form = workspace.profile().unwrap().session.insert_form.as_ref();
                let field = &form.unwrap().fields[index];
                let input = field.input.read(cx);
                (
                    field.nulled,
                    field.touched,
                    input.value().to_string(),
                    input.presentation().placeholder().to_string(),
                )
            })
        };
        let statement = |cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.confirm_new_row(window, cx);
            });
            settle(cx);
            workspace.read_with(cx, |workspace, _| {
                let review = workspace.profile().unwrap().session.apply_review.as_ref();
                review.unwrap().sql.clone()
            })
        };
        let left_out = || (false, false, String::new(), "DEFAULT".to_string());
        let nulled = || (true, false, String::new(), "NULL".to_string());
        let typed = |text: &str| (false, true, text.to_string(), "empty string".to_string());

        open_form(cx);
        focus(0, cx);
        cx.simulate_input("7");
        settle(cx);
        assert_eq!(state(0, cx), typed("7"));
        cx.simulate_keystrokes("backspace");
        settle(cx);
        assert_eq!(state(0, cx), typed(""), "emptying keeps the field a value");

        // Each field is its DEFAULT, its NULL, then its box.
        cx.simulate_keystrokes("shift-tab shift-tab");
        settle(cx);
        assert!(!typing_in(0, cx));
        press(cx, "enter");
        assert_eq!(state(0, cx), left_out(), "DEFAULT puts the field back out");
        cx.simulate_keystrokes("tab");
        settle(cx);
        press(cx, "space");
        assert_eq!(state(0, cx), nulled());
        cx.simulate_keystrokes("tab");
        settle(cx);
        assert!(typing_in(0, cx));
        cx.simulate_input("9");
        settle(cx);
        assert_eq!(state(0, cx), typed("9"), "typing turns the NULL off");
        cx.simulate_keystrokes("shift-tab");
        settle(cx);
        press(cx, "space");
        assert_eq!(state(0, cx), nulled(), "NULL clears the box");
        press(cx, "space");
        assert_eq!(state(0, cx), left_out());

        cx.simulate_keystrokes("tab tab tab tab");
        settle(cx);
        assert!(typing_in(1, cx));
        cx.simulate_input("robin");
        cx.simulate_keystrokes("tab tab tab");
        settle(cx);
        assert!(typing_in(2, cx));
        cx.simulate_input("x");
        cx.simulate_keystrokes("backspace");
        settle(cx);
        assert_eq!(state(1, cx), typed("robin"));
        assert_eq!(state(2, cx), typed(""));
        let sql = statement(cx);
        assert!(!sql.contains("\"id\""), "{sql}");
        assert!(sql.contains("\"name\"") && sql.contains("'robin'"), "{sql}");
        assert!(sql.contains("\"wing\"") && sql.contains("''"), "{sql}");

        workspace.update(cx, |workspace, cx| workspace.close_apply_review(cx));
        open_form(cx);
        focus(1, cx);
        cx.simulate_input("robin");
        cx.simulate_keystrokes("shift-tab");
        settle(cx);
        press(cx, "space");
        assert_eq!(state(1, cx), nulled());
        let sql = statement(cx);
        assert!(sql.contains("NULL") && !sql.contains("robin"), "{sql}");

        quit(workspace, cx);
    });
}

const DIAGRAM_SCHEMA: &str = "create table birds (id integer primary key, name text, latin text);
     create table sightings (id integer primary key, bird integer not null references birds(id), seen_at text);";

/// The diagram of `main`, opened from the palette and read.
fn show_diagram(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<crate::diagram_view::DiagramView> {
    cx.simulate_keystrokes("secondary-shift-p");
    settle(cx);
    cx.simulate_input("Show diagram of main");
    settle(cx);
    cx.simulate_keystrokes("enter");
    settle(cx);
    cx.update(|_, cx| {
        match workspace
            .read(cx)
            .profile()
            .and_then(|profile| profile.session.active_object())
            .map(|tab| &tab.body)
        {
            Some(ObjectBody::Diagram { view, side, .. }) => {
                assert!(side.is_none(), "the read landed and let its connection go");
                view.clone()
            }
            _ => panic!("the diagram is in front"),
        }
    })
}

/// The tab in front, as `(name, is structure)`, when it is a relation's.
fn front_relation(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<(String, bool)> {
    cx.update(|_, cx| {
        let tab = workspace.read(cx).profile()?.session.active_object()?;
        match tab.body {
            ObjectBody::Relation { .. } => Some((tab.name.clone(), false)),
            ObjectBody::Structure { .. } => Some((tab.name.clone(), true)),
            _ => None,
        }
    })
}

#[gpui::test]
fn the_palette_opens_a_diagram_that_takes_the_keyboard_and_escape_clears_its_focus(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        let view = show_diagram(&workspace, cx);
        assert_eq!(
            cx.update(|_, cx| {
                let tab = workspace.read(cx).profile()?.session.active_object()?;
                Some((tab.label().to_string(), tab.name.clone()))
            }),
            Some(("main".to_string(), String::new())),
            "the strip names the tab after its schema, which its stored name leaves out"
        );
        let keyboard = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| view.read(cx).focus_handle().is_focused(window))
        };
        assert!(keyboard(cx), "the canvas holds the keyboard");

        let birds = cx
            .update(|_, cx| view.read(cx).table_on_screen("birds"))
            .expect("the diagram drew the birds");
        cx.simulate_click(birds, gpui::Modifiers::default());
        settle(cx);
        assert!(
            cx.update(|_, cx| view.read(cx).has_focus()),
            "a click focuses a card"
        );

        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(
            !cx.update(|_, cx| view.read(cx).has_focus()),
            "escape clears it"
        );
        assert!(keyboard(cx), "and leaves the keyboard on the canvas");

        quit(workspace, cx);
    });
}

/// Zooms the diagram under the pointer at `at` with the wheel, as many
/// events as `steps`.
fn wheel_zoom(at: gpui::Point<gpui::Pixels>, steps: usize, cx: &mut VisualTestContext) {
    for _ in 0..steps {
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: at,
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(-40.0))),
            modifiers: gpui::Modifiers::secondary_key(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        settle(cx);
    }
}

#[gpui::test]
fn a_wheel_zoom_is_written_once_the_wheel_stops_and_a_quit_writes_only_what_is_pending(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        let view = show_diagram(&workspace, cx);
        let path = store::dbdelve_directory().unwrap().join("profiles.toml");
        let written = || std::fs::read_to_string(&path).expect("profiles.toml is there");
        let unsaved = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| workspace.profiles_unsaved)
        };
        let before = written();
        let fitted = cx.update(|_, cx| view.read(cx).zoom());
        let birds = cx
            .update(|_, cx| view.read(cx).table_on_screen("birds"))
            .expect("the diagram drew the birds");

        wheel_zoom(birds, 3, cx);
        assert_ne!(cx.update(|_, cx| view.read(cx).zoom()), fitted);
        assert_eq!(written(), before, "not written on every event");
        assert!(unsaved(cx));
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(600));
        settle(cx);
        assert_ne!(written(), before, "written once the wheel stops");
        assert!(!unsaved(cx));

        // Another instance saved since; with nothing pending here, quitting
        // leaves its file alone.
        let theirs = format!("{}\n# theirs\n", written());
        std::fs::write(&path, &theirs).unwrap();
        quit(workspace, cx);
        assert_eq!(written(), theirs);
    });
}

#[gpui::test]
fn a_quit_before_the_wheel_has_stopped_writes_the_zoom(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        let view = show_diagram(&workspace, cx);
        let path = store::dbdelve_directory().unwrap().join("profiles.toml");
        let before = std::fs::read_to_string(&path).unwrap();
        let birds = cx
            .update(|_, cx| view.read(cx).table_on_screen("birds"))
            .expect("the diagram drew the birds");
        wheel_zoom(birds, 2, cx);
        quit(workspace, cx);
        assert_ne!(std::fs::read_to_string(&path).unwrap(), before);
    });
}

#[gpui::test]
fn a_diagram_behind_another_tab_lets_its_render_caches_go_and_draws_again_when_shown(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        let view = show_diagram(&workspace, cx);
        let caches =
            |cx: &mut VisualTestContext| cx.update(|_, cx| view.read(cx).holds_render_caches());
        assert!(caches(cx), "drawn from its caches");

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_relation_view(
                "main".into(),
                "birds".into(),
                crate::db::RelationKind::Table,
                false,
                window,
                cx,
            );
        });
        settle(cx);
        assert!(!caches(cx), "let go behind the birds' rows");

        show_diagram(&workspace, cx);
        assert!(caches(cx), "built again when shown");
        let sightings = cx
            .update(|_, cx| view.read(cx).table_on_screen("sightings"))
            .expect("the diagram drew the sightings");
        cx.simulate_click(sightings, gpui::Modifiers::default());
        settle(cx);
        assert_eq!(
            cx.update(|_, cx| view.read(cx).focused_name()).as_deref(),
            Some("sightings"),
            "and a click finds its card"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_diagram_read_outlived_by_a_reconnect_is_read_again_for_the_tab_in_front(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_diagram("main".into(), None, window, cx);
            // A reconnect landed while the read was out, and its own read
            // was refused because this one still held the slot.
            workspace.profile_mut().unwrap().generation += 1;
        });
        settle(cx);
        let view = diagram_in_front(&workspace, cx).0;
        assert!(
            cx.update(|_, cx| view.read(cx).laid_out()).is_some(),
            "the old answer was dropped and the diagram read again"
        );
        quit(workspace, cx);
    });
}

/// A reconnect lets go of a read still out from the connection it replaces,
/// which could stall there and hold the slot the new connection's read needs.
#[gpui::test]
fn a_reconnect_cancels_a_diagram_read_still_out_and_reads_again(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        let held = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_diagram("main".into(), None, window, cx);
            let index = workspace.active.unwrap();
            workspace.reconnect(index, cx);
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            matches!(tab.body, ObjectBody::Diagram { side: Some(_), .. })
        });
        assert!(!held, "the old read let its slot go");
        settle(cx);
        let view = diagram_in_front(&workspace, cx).0;
        assert!(
            cx.update(|_, cx| view.read(cx).laid_out()).is_some(),
            "and the diagram was read on the new connection"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_double_click_opens_a_diagram_cards_table_and_the_keyboard_the_focused_ones(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        let view = show_diagram(&workspace, cx);
        let at = |name: &str, cx: &mut VisualTestContext| {
            cx.update(|_, cx| view.read(cx).table_on_screen(name))
                .expect("the diagram drew the table")
        };

        let birds = at("birds", cx);
        cx.simulate_click(birds, gpui::Modifiers::default());
        cx.simulate_event(gpui::MouseDownEvent {
            position: birds,
            modifiers: gpui::Modifiers::default(),
            button: gpui::MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: birds,
            modifiers: gpui::Modifiers::default(),
            button: gpui::MouseButton::Left,
            click_count: 2,
        });
        settle(cx);
        assert_eq!(
            front_relation(&workspace, cx),
            Some(("birds".to_string(), false)),
            "a double-click opens the table's rows"
        );

        let view = show_diagram(&workspace, cx);
        let sightings = cx
            .update(|_, cx| view.read(cx).table_on_screen("sightings"))
            .expect("the diagram drew the sightings");
        cx.simulate_click(sightings, gpui::Modifiers::default());
        settle(cx);
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        assert_eq!(
            front_relation(&workspace, cx),
            Some(("sightings".to_string(), true)),
            "Run Query's chord gives way to the diagram's Open Structure"
        );

        let view = show_diagram(&workspace, cx);
        assert!(
            cx.update(|_, cx| view.read(cx).has_focus()),
            "the diagram kept its focus while away"
        );
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(
            front_relation(&workspace, cx),
            Some(("sightings".to_string(), false)),
            "enter opens the focused table's rows"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_zoom_chords_zoom_a_diagram_and_its_views_and_keys_only_lay_it_out_again(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            &format!("{DIAGRAM_SCHEMA} create table weather (id integer primary key);"),
        );
        let view = show_diagram(&workspace, cx);
        let zoom = |cx: &mut VisualTestContext| cx.update(|_, cx| view.read(cx).zoom());
        let fitted = zoom(cx);

        cx.simulate_keystrokes("secondary-=");
        settle(cx);
        assert!(zoom(cx) > fitted, "secondary-= zooms the diagram in");
        cx.simulate_keystrokes("secondary-0");
        settle(cx);
        assert_eq!(zoom(cx), fitted, "secondary-0 fits it again");

        let laid_out = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| view.read(cx).laid_out())
                .expect("the diagram is laid out")
        };
        let sightings = cx
            .update(|_, cx| view.read(cx).table_on_screen("sightings"))
            .expect("the diagram drew the sightings");
        cx.simulate_click(sightings, gpui::Modifiers::default());
        settle(cx);
        view.update(cx, |view, cx| view.show_neighbourhood(true, cx));
        settle(cx);
        let (mut names, _) = laid_out(cx);
        names.sort();
        assert_eq!(
            names,
            ["birds", "sightings"],
            "the neighbourhood leaves the weather out"
        );
        assert!(
            cx.update(|_, cx| view.read(cx).has_focus()),
            "on the table it centres on"
        );

        view.update(cx, |view, cx| view.show_neighbourhood(false, cx));
        settle(cx);
        assert_eq!(laid_out(cx).0.len(), 3, "the schema view is back whole");

        view.update(cx, |view, cx| view.toggle_keys_only(cx));
        settle(cx);
        assert!(laid_out(cx).1, "keys only lays the schema out again");
        assert!(
            cx.update(|_, cx| view.read(cx).has_focus()),
            "keeping the focus on its table"
        );

        quit(workspace, cx);
    });
}

/// The schema's layout at the new setting is overtaken by the neighbourhood,
/// so the schema laid out at the old one must not come back from the stash.
#[gpui::test]
fn keys_only_switched_just_before_a_neighbourhood_holds_back_on_the_schema(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            &format!("{DIAGRAM_SCHEMA} create table weather (id integer primary key);"),
        );
        let view = show_diagram(&workspace, cx);
        let sightings = cx
            .update(|_, cx| view.read(cx).table_on_screen("sightings"))
            .expect("the diagram drew the sightings");
        cx.simulate_click(sightings, gpui::Modifiers::default());
        settle(cx);
        view.update(cx, |view, cx| {
            view.toggle_keys_only(cx);
            view.show_neighbourhood(true, cx);
        });
        settle(cx);
        view.update(cx, |view, cx| view.show_neighbourhood(false, cx));
        settle(cx);
        let (names, keys_only) = cx
            .update(|_, cx| view.read(cx).laid_out())
            .expect("the diagram is laid out");
        assert_eq!(names.len(), 3, "the schema view is back");
        assert!(keys_only, "laid out at the setting the switch shows");

        quit(workspace, cx);
    });
}

/// A card of many columns zoomed out short of name blocks drew its key rows
/// alone, with Keys only off, and its bottom edge could not open it.
#[gpui::test]
fn zoomed_out_a_card_draws_its_columns_and_its_bottom_edge_opens_it(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let notes: Vec<String> = (0..38).map(|note| format!("note_{note} text")).collect();
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            &format!(
                "{DIAGRAM_SCHEMA} create table users (id integer primary key, \
                 email text unique, bird integer references birds(id), {});",
                notes.join(", ")
            ),
        );
        let view = show_diagram(&workspace, cx);
        assert!(
            !cx.update(|_, cx| view.read(cx).laid_out()).unwrap().1,
            "a small schema is laid out with every column"
        );
        let zoom = |cx: &mut VisualTestContext| cx.update(|_, cx| view.read(cx).zoom());
        let users = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| view.read(cx).table_bounds("users"))
                .expect("the diagram drew the users")
        };
        for _ in 0..40 {
            let delta = match zoom(cx) {
                zoom if zoom >= 0.8 => -40.0,
                zoom if zoom < 0.6 => 40.0,
                _ => break,
            };
            let position = users(cx).center();
            cx.simulate_event(gpui::ScrollWheelEvent {
                position,
                delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(delta))),
                modifiers: gpui::Modifiers::secondary_key(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            settle(cx);
        }
        let zoomed = zoom(cx);
        assert!((0.6..0.8).contains(&zoomed), "{zoomed}");
        assert_eq!(
            cx.update(|_, cx| view.read(cx).rows_drawn("users")),
            Some(crate::diagram::geometry::AUTO_ROWS - 1),
            "every column up to the cap, not the three keys"
        );

        let before = users(cx);
        let edge = gpui::point(before.center().x, before.bottom() - gpui::px(1.0));
        let to = gpui::point(edge.x, edge.y + gpui::px(3.0 * 24.0 * zoomed));
        let left = gpui::MouseButton::Left;
        cx.simulate_mouse_down(edge, left, gpui::Modifiers::default());
        cx.simulate_mouse_move(to, left, gpui::Modifiers::default());
        cx.simulate_mouse_up(to, left, gpui::Modifiers::default());
        settle(cx);
        let after = users(cx);
        assert_eq!(after.origin, before.origin, "a resize, not a move");
        assert!(
            after.size.height > before.size.height + gpui::px(2.0 * 24.0 * zoomed),
            "the bottom edge opened the card: {before:?} to {after:?}"
        );

        quit(workspace, cx);
    });
}

/// From the sidebar, the chord that moves between a buffer and its rows
/// puts the keyboard on a diagram's canvas, which has neither.
#[gpui::test]
fn toggle_results_focus_puts_the_keyboard_on_a_diagram_from_the_sidebar(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        let view = show_diagram(&workspace, cx);
        let canvas = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| view.read(cx).focus_handle().is_focused(window))
        };
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        assert!(!canvas(cx), "the sidebar took the keyboard");
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        assert!(canvas(cx), "the canvas has it back");

        quit(workspace, cx);
    });
}

/// The diagram tab in front, once read, and how many diagram tabs are open.
fn diagram_in_front(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (Entity<crate::diagram_view::DiagramView>, usize) {
    cx.update(|_, cx| {
        let session = &workspace.read(cx).profile().unwrap().session;
        let open = (session.objects.iter())
            .filter(|tab| matches!(tab.body, ObjectBody::Diagram { .. }))
            .count();
        match session.active_object().map(|tab| &tab.body) {
            Some(ObjectBody::Diagram { view, side, .. }) => {
                assert!(side.is_none(), "the read landed");
                (view.clone(), open)
            }
            _ => panic!("a diagram is in front"),
        }
    })
}

/// A right-click on a sidebar row, then the `nth` item of the menu it opens.
fn pick_from_row_menu(
    workspace: &Entity<Workspace>,
    row: &str,
    nth: usize,
    cx: &mut VisualTestContext,
) {
    let at = explorer_row(workspace, row, cx).center();
    let none = gpui::Modifiers::default();
    cx.simulate_mouse_down(at, gpui::MouseButton::Right, none);
    cx.simulate_mouse_up(at, gpui::MouseButton::Right, none);
    settle(cx);
    for _ in 0..nth {
        cx.simulate_keystrokes("down");
    }
    cx.simulate_keystrokes("enter");
    settle(cx);
}

#[gpui::test]
fn the_sidebar_shows_a_schemas_diagram_and_a_tables_neighbourhood_in_one_tab(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            &format!("{DIAGRAM_SCHEMA} create table weather (id integer primary key);"),
        );
        let names = |view: &Entity<crate::diagram_view::DiagramView>,
                     cx: &mut VisualTestContext| {
            let (mut names, _) = cx
                .update(|_, cx| view.read(cx).laid_out())
                .expect("the diagram is laid out");
            names.sort();
            names
        };

        pick_from_row_menu(&workspace, &explorer::schema_row("main"), 1, cx);
        let (view, open) = diagram_in_front(&workspace, cx);
        assert_eq!(open, 1);
        assert_eq!(names(&view, cx), ["birds", "sightings", "weather"]);

        // Open Data, Open Structure, Show in Diagram.
        pick_from_row_menu(&workspace, &explorer::relation_row("main", "birds"), 3, cx);
        let (view, open) = diagram_in_front(&workspace, cx);
        assert_eq!(open, 1, "the schema's diagram is reused");
        assert_eq!(
            names(&view, cx),
            ["birds", "sightings"],
            "showing the table's neighbourhood"
        );
        assert_eq!(
            cx.update(|_, cx| view.read(cx).focused_name())
                .as_deref()
                .map(|name| name.to_string()),
            Some("birds".to_string())
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_structure_tab_shows_its_tables_neighbourhood_and_a_double_click_there_opens_a_neighbour(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            &format!("{DIAGRAM_SCHEMA} create table weather (id integer primary key);"),
        );
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_relation_view(
                "main".into(),
                "sightings".into(),
                crate::db::RelationKind::Table,
                true,
                window,
                cx,
            );
        });
        settle(cx);
        let button = cx
            .debug_bounds("show-diagram")
            .expect("the structure offers the diagram");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        settle(cx);
        let (view, open) = diagram_in_front(&workspace, cx);
        assert_eq!(open, 1);
        let (mut names, _) = cx
            .update(|_, cx| view.read(cx).laid_out())
            .expect("the diagram is laid out");
        names.sort();
        assert_eq!(names, ["birds", "sightings"], "the neighbourhood");
        assert_eq!(
            cx.update(|_, cx| view.read(cx).focused_name())
                .map(|name| name.to_string()),
            Some("sightings".to_string()),
            "centred on the table"
        );
        assert!(
            cx.update(|window, cx| view.read(cx).focus_handle().is_focused(window)),
            "and the canvas holds the keyboard"
        );

        let birds = cx
            .update(|_, cx| view.read(cx).table_on_screen("birds"))
            .expect("the neighbourhood drew the birds");
        let none = gpui::Modifiers::default();
        cx.simulate_click(birds, none);
        cx.simulate_event(gpui::MouseDownEvent {
            position: birds,
            modifiers: none,
            button: gpui::MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: birds,
            modifiers: none,
            button: gpui::MouseButton::Left,
            click_count: 2,
        });
        settle(cx);
        assert_eq!(
            front_relation(&workspace, cx),
            Some(("birds".to_string(), false)),
            "a double-click on a neighbour opens it, as in the schema view"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn keys_only_switched_while_a_diagram_is_read_again_holds_once_the_read_lands(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        let view = show_diagram(&workspace, cx);
        let read_again = |cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, cx| {
                let session = &mut workspace.profile_mut().unwrap().session;
                let crate::session::Tab::Object(id) = session.active else {
                    panic!("the diagram is in front");
                };
                let tab = session.objects.iter_mut().find(|tab| tab.id == id);
                if let Some(ObjectBody::Diagram { stale, .. }) = tab.map(|tab| &mut tab.body) {
                    *stale = true;
                }
                workspace.load_diagram(id, cx);
            });
        };
        let shown = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let view = view.read(cx);
                (
                    view.keys_only(),
                    view.laid_out().map(|(_, keys_only)| keys_only),
                )
            })
        };

        read_again(cx);
        view.update(cx, |view, cx| view.toggle_keys_only(cx));
        settle(cx);
        assert_eq!(
            shown(cx),
            (Some(true), Some(true)),
            "the switch and the cards agree"
        );

        let birds = cx
            .update(|_, cx| view.read(cx).table_on_screen("birds"))
            .expect("the diagram drew the birds");
        cx.simulate_click(birds, gpui::Modifiers::default());
        settle(cx);
        view.update(cx, |view, cx| view.show_neighbourhood(true, cx));
        settle(cx);
        read_again(cx);
        view.update(cx, |view, cx| view.toggle_keys_only(cx));
        settle(cx);
        assert_eq!(shown(cx), (Some(false), Some(false)), "in a neighbourhood");
        view.update(cx, |view, cx| view.show_neighbourhood(false, cx));
        settle(cx);
        assert_eq!(
            shown(cx),
            (Some(false), Some(false)),
            "and in the schema view it goes back to"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_refresh_reads_a_diagram_again_and_keeps_the_cards_the_user_moved(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            &format!("{DIAGRAM_SCHEMA} create table weather (id integer primary key);"),
        );
        let view = show_diagram(&workspace, cx);
        let origin = |name: &str, cx: &mut VisualTestContext| {
            cx.update(|_, cx| view.read(cx).table_origin(name))
        };
        let names = |cx: &mut VisualTestContext| {
            let (mut names, _) = cx
                .update(|_, cx| view.read(cx).laid_out())
                .expect("the diagram is laid out");
            names.sort();
            names
        };
        let change = |sql: &str| {
            let database = store::dbdelve_directory()
                .expect("the test home has a data directory")
                .join("e2e.db");
            rusqlite::Connection::open(database)
                .and_then(|connection| connection.execute_batch(sql))
                .expect("the schema change must apply");
        };

        let before = origin("birds", cx);
        let birds = cx
            .update(|_, cx| view.read(cx).table_on_screen("birds"))
            .expect("the diagram drew the birds");
        let to = gpui::point(birds.x + gpui::px(40.0), birds.y + gpui::px(160.0));
        let left = gpui::MouseButton::Left;
        cx.simulate_mouse_down(birds, left, gpui::Modifiers::default());
        cx.simulate_mouse_move(to, left, gpui::Modifiers::default());
        cx.simulate_mouse_up(to, left, gpui::Modifiers::default());
        settle(cx);
        let dragged = origin("birds", cx);
        assert_ne!(dragged, before, "the drag moved the birds");
        let sightings = cx
            .update(|_, cx| view.read(cx).table_on_screen("sightings"))
            .expect("the diagram drew the sightings");
        cx.simulate_click(sightings, gpui::Modifiers::default());
        settle(cx);

        change(
            "create table nests (id integer primary key, bird integer references birds(id));
             drop table weather;",
        );
        cx.simulate_keystrokes("secondary-shift-r");
        settle(cx);
        assert_eq!(
            names(cx),
            ["birds", "nests", "sightings"],
            "the diagram in front is read again"
        );
        assert_eq!(origin("birds", cx), dragged, "the card moved stays put");
        assert_eq!(
            cx.update(|_, cx| view.read(cx).focused_name()).as_deref(),
            Some("sightings"),
            "and the focus stays on its table"
        );

        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(
            front_relation(&workspace, cx),
            Some(("sightings".to_string(), false))
        );
        change("create table eggs (id integer primary key, nest integer references nests(id));");
        cx.simulate_keystrokes("secondary-shift-r");
        settle(cx);
        show_diagram(&workspace, cx);
        assert_eq!(
            names(cx),
            ["birds", "eggs", "nests", "sightings"],
            "one behind another tab is read again when it is shown"
        );
        assert_eq!(origin("birds", cx), dragged);
        let view = show_diagram(&workspace, cx);
        view.update(cx, |view, cx| view.toggle_keys_only(cx));
        settle(cx);
        assert_eq!(
            cx.update(|_, cx| view.read(cx).table_origin("birds")),
            dragged,
            "keys only lays the rest out again around it"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_diagram_card_resizes_from_its_corner_and_a_double_click_there_gives_its_size_back(
    cx: &mut TestAppContext,
) {
    use crate::diagram::geometry::{CARD_WIDTH, card_height};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        let view = show_diagram(&workspace, cx);
        let size = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| view.read(cx).table_size("birds"))
                .expect("the diagram drew the birds")
        };
        let corner = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| view.read(cx).table_corner_on_screen("birds"))
                .expect("the diagram drew the birds")
        };
        assert_eq!(size(cx), (CARD_WIDTH, card_height(3)));
        let zoom = cx.update(|_, cx| view.read(cx).zoom());

        let from = corner(cx);
        let to = gpui::point(
            from.x + gpui::px(80.0 * zoom),
            from.y - gpui::px(30.0 * zoom),
        );
        let left = gpui::MouseButton::Left;
        cx.simulate_mouse_down(from, left, gpui::Modifiers::default());
        cx.simulate_mouse_move(to, left, gpui::Modifiers::default());
        cx.simulate_mouse_up(to, left, gpui::Modifiers::default());
        settle(cx);
        let (width, height) = size(cx);
        assert!((width - (CARD_WIDTH + 80.0)).abs() < 0.5, "wider: {width}");
        assert_eq!(height, card_height(2), "a row shorter, in whole rows");
        assert!(
            !cx.update(|_, cx| view.read(cx).has_focus()),
            "a resize focuses nothing"
        );

        // Keys only lists the birds' one key, which is shorter still.
        view.update(cx, |view, cx| view.toggle_keys_only(cx));
        settle(cx);
        assert_eq!(
            size(cx),
            (width, card_height(1)),
            "keys only keeps the width"
        );
        view.update(cx, |view, cx| view.toggle_keys_only(cx));
        settle(cx);
        assert_eq!(size(cx), (width, height), "and every column the height");

        let corner = corner(cx);
        cx.simulate_click(corner, gpui::Modifiers::default());
        cx.simulate_event(gpui::MouseDownEvent {
            position: corner,
            modifiers: gpui::Modifiers::default(),
            button: left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: corner,
            modifiers: gpui::Modifiers::default(),
            button: left,
            click_count: 2,
        });
        settle(cx);
        assert_eq!(size(cx), (CARD_WIDTH, card_height(3)), "its own size back");
        assert_eq!(
            front_relation(&workspace, cx),
            None,
            "a double-click on the corner opens no table"
        );

        quit(workspace, cx);
    });
}

/// A second launch over the same home, as `launch` builds it, beside the
/// first window.
fn relaunch(cx: &mut VisualTestContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let mut relaunched = None;
    let (_, cx) = cx.cx.add_window_view(|window, cx| {
        let entity = cx.new(|cx| Workspace::new(Vec::new(), window, cx));
        relaunched = Some(entity.clone());
        Root::new(entity, window, cx)
    });
    settle(cx);
    (relaunched.expect("the window builds the workspace"), cx)
}

/// The diagram in front, once it is read.
fn front_diagram(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<crate::diagram_view::DiagramView> {
    cx.update(|_, cx| {
        match workspace
            .read(cx)
            .profile()
            .and_then(|profile| profile.session.active_object())
            .map(|tab| &tab.body)
        {
            Some(ObjectBody::Diagram { view, side, .. }) => {
                assert!(side.is_none(), "the read landed");
                view.clone()
            }
            _ => panic!("the diagram came back in front"),
        }
    })
}

#[gpui::test]
fn a_diagram_tab_comes_back_after_a_relaunch_as_it_was_left(cx: &mut TestAppContext) {
    // CI's live job sets `PG*`, whose profile would be put in front on launch.
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            &format!("{DIAGRAM_SCHEMA} create table weather (id integer primary key);"),
        );
        let view = show_diagram(&workspace, cx);
        let left = gpui::MouseButton::Left;
        let none = gpui::Modifiers::default();
        let birds = cx
            .update(|_, cx| view.read(cx).table_on_screen("birds"))
            .expect("the diagram drew the birds");
        let to = gpui::point(birds.x + gpui::px(40.0), birds.y + gpui::px(160.0));
        cx.simulate_mouse_down(birds, left, none);
        cx.simulate_mouse_move(to, left, none);
        cx.simulate_mouse_up(to, left, none);
        let corner = cx
            .update(|_, cx| view.read(cx).table_corner_on_screen("sightings"))
            .expect("the diagram drew the sightings");
        let to = gpui::point(corner.x + gpui::px(60.0), corner.y - gpui::px(30.0));
        cx.simulate_mouse_down(corner, left, none);
        cx.simulate_mouse_move(to, left, none);
        cx.simulate_mouse_up(to, left, none);
        settle(cx);
        cx.simulate_keystrokes("secondary-=");
        settle(cx);
        let state = |view: &Entity<crate::diagram_view::DiagramView>,
                     cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let view = view.read(cx);
                (
                    view.table_origin("birds").expect("the birds are drawn"),
                    view.table_size("sightings")
                        .expect("the sightings are drawn"),
                    view.zoom(),
                )
            })
        };
        let (origin, size, zoom) = state(&view, cx);
        let arranged = store::read_diagram("e2e", None, "main");
        assert_eq!(
            arranged
                .iter()
                .map(|card| card.name.as_str())
                .collect::<Vec<_>>(),
            ["birds", "sightings"],
            "the moved and the sized card, in a file of their own"
        );
        let saved =
            std::fs::read_to_string(store::dbdelve_directory().unwrap().join("profiles.toml"))
                .unwrap();
        assert!(saved.contains("diagram = true"), "{saved}");
        assert!(
            !saved.contains("birds"),
            "no card in profiles.toml:\n{saved}"
        );

        drop(workspace);
        let (workspace, cx) = relaunch(cx);
        let view = front_diagram(&workspace, cx);
        let (restored_origin, restored_size, restored_zoom) = state(&view, cx);
        let near = |a: f32, b: f32| (a - b).abs() < 0.01;
        assert!(
            near(restored_origin.0, origin.0) && near(restored_origin.1, origin.1),
            "{restored_origin:?} is where {origin:?} was left"
        );
        assert!(near(restored_size.0, size.0) && restored_size.1 == size.1);
        assert!(near(restored_zoom, zoom), "{restored_zoom} was {zoom}");

        let sightings = cx
            .update(|_, cx| view.read(cx).table_on_screen("sightings"))
            .expect("the diagram drew the sightings");
        cx.simulate_click(sightings, none);
        settle(cx);
        view.update(cx, |view, cx| view.show_neighbourhood(true, cx));
        settle(cx);

        drop(workspace);
        let (workspace, cx) = relaunch(cx);
        let view = front_diagram(&workspace, cx);
        let (mut names, _) = cx
            .update(|_, cx| view.read(cx).laid_out())
            .expect("the diagram is laid out");
        names.sort();
        assert_eq!(names, ["birds", "sightings"], "back in the neighbourhood");
        assert_eq!(
            cx.update(|_, cx| view.read(cx).focused_name()).as_deref(),
            Some("sightings"),
            "centred on its table"
        );
        view.update(cx, |view, cx| view.show_neighbourhood(false, cx));
        settle(cx);
        let origin_now = cx
            .update(|_, cx| view.read(cx).table_origin("birds"))
            .unwrap();
        assert!(
            near(origin_now.0, origin.0) && near(origin_now.1, origin.1),
            "the schema view behind it kept the moved card"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn secondary_f_finds_a_table_in_a_diagram_and_escape_puts_the_field_away(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            &format!("{DIAGRAM_SCHEMA} create table weather (id integer primary key);"),
        );
        let view = show_diagram(&workspace, cx);
        let canvas = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| view.read(cx).focus_handle().is_focused(window))
        };
        let focused = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| view.read(cx).focused_name())
                .map(|name| name.to_string())
        };

        cx.simulate_keystrokes("secondary-f");
        settle(cx);
        assert!(
            cx.update(|_, cx| view.read(cx).finding()),
            "the field opens"
        );
        assert!(!canvas(cx), "and takes the keyboard");
        cx.simulate_input("sight");
        settle(cx);
        assert_eq!(
            focused(cx).as_deref(),
            Some("sightings"),
            "typing jumps to the match"
        );

        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(
            !cx.update(|_, cx| view.read(cx).finding()),
            "escape closes the field"
        );
        assert!(canvas(cx), "handing the keyboard back to the canvas");
        assert_eq!(
            focused(cx).as_deref(),
            Some("sightings"),
            "with the match in focus"
        );

        cx.simulate_keystrokes("secondary-f");
        settle(cx);
        cx.simulate_input("i");
        settle(cx);
        let first = focused(cx);
        cx.simulate_keystrokes("down");
        settle(cx);
        assert_ne!(focused(cx), first, "down moves to the next match");
        cx.simulate_keystrokes("up");
        settle(cx);
        assert_eq!(focused(cx), first, "and up back");

        cx.simulate_keystrokes("backspace");
        cx.simulate_input("weath");
        settle(cx);
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert!(
            !cx.update(|_, cx| view.read(cx).finding()),
            "enter picks the match"
        );
        assert_eq!(focused(cx).as_deref(), Some("weather"));
        assert_eq!(
            front_relation(&workspace, cx),
            None,
            "and opens nothing yet"
        );
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(
            front_relation(&workspace, cx),
            Some(("weather".to_string(), false)),
            "the next enter opens it"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_arrow_keys_walk_a_diagrams_cards_and_leave_the_find_field_its_own(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        let view = show_diagram(&workspace, cx);
        let focused = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| view.read(cx).focused_name())
                .map(|name| name.to_string())
        };
        assert_eq!(focused(cx), None);

        cx.simulate_keystrokes("down");
        settle(cx);
        assert!(
            focused(cx).is_some(),
            "with nothing in focus, an arrow focuses the card nearest the middle"
        );

        cx.simulate_keystrokes("left");
        settle(cx);
        assert_eq!(
            focused(cx).as_deref(),
            Some("birds"),
            "the referenced table"
        );
        cx.simulate_keystrokes("right");
        settle(cx);
        assert_eq!(
            focused(cx).as_deref(),
            Some("sightings"),
            "the table that references it"
        );
        cx.simulate_keystrokes("right");
        settle(cx);
        assert_eq!(
            focused(cx).as_deref(),
            Some("sightings"),
            "nothing further that way keeps the focus"
        );

        cx.simulate_keystrokes("secondary-f");
        settle(cx);
        cx.simulate_input("birds");
        settle(cx);
        assert_eq!(focused(cx).as_deref(), Some("birds"));
        cx.simulate_keystrokes("right");
        settle(cx);
        assert_eq!(
            focused(cx).as_deref(),
            Some("birds"),
            "an arrow in the field, even at the end of its text, never walks the cards"
        );
        cx.simulate_keystrokes("escape");
        settle(cx);
        cx.simulate_keystrokes("right");
        settle(cx);
        assert_eq!(
            focused(cx).as_deref(),
            Some("sightings"),
            "and the canvas takes it back with the keyboard"
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_diagrams_minimap_shows_once_it_overflows_and_a_press_on_it_moves_the_view(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            &format!("{DIAGRAM_SCHEMA} create table weather (id integer primary key);"),
        );
        let view = show_diagram(&workspace, cx);
        let middle = |cx: &mut VisualTestContext| cx.update(|_, cx| view.read(cx).minimap_middle());
        assert_eq!(
            middle(cx),
            None,
            "no minimap while the whole scene is in view"
        );

        for _ in 0..12 {
            cx.simulate_keystrokes("secondary-=");
        }
        settle(cx);
        // Off the middle of the scene, onto the birds.
        cx.simulate_keystrokes("secondary-f");
        cx.simulate_input("birds");
        cx.simulate_keystrokes("enter");
        settle(cx);
        let at = middle(cx).expect("zoomed in, the minimap shows");
        let before = cx.update(|_, cx| view.read(cx).off_middle());
        cx.simulate_click(at, gpui::Modifiers::default());
        settle(cx);
        let after = cx.update(|_, cx| view.read(cx).off_middle());
        assert!(
            after < 1.0 && after < before,
            "a press there centres the view on it: {before} -> {after}"
        );
        assert!(
            cx.update(|_, cx| view.read(cx).has_focus()),
            "and leaves the focus alone"
        );

        quit(workspace, cx);
    });
}

/// Opens the diagram toolbar's Export menu and picks its item `downs` down.
fn pick_diagram_export(downs: usize, cx: &mut VisualTestContext) {
    let bounds = cx
        .debug_bounds("diagram-export")
        .expect("the toolbar draws Export");
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    settle(cx);
    for _ in 0..downs {
        cx.simulate_keystrokes("down");
    }
    press(cx, "enter");
}

/// Waits for an export running off the UI thread to write `path`.
fn exported_file(path: &std::path::Path, cx: &mut VisualTestContext) -> Vec<u8> {
    for _ in 0..500 {
        settle(cx);
        if path.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::fs::read(path).expect("the file is written")
}

#[gpui::test]
fn the_diagram_export_menu_writes_the_drawing_to_the_file_picked(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        cx.executor().allow_parking();
        show_diagram(&workspace, cx);
        let directory = store::dbdelve_directory().unwrap();

        super::exports::SUGGESTED_NAMES.with_borrow_mut(Vec::clear);
        pick_diagram_export(2, cx);
        let path = directory.join("main.svg");
        let chosen = path.clone();
        cx.simulate_new_path_selection(|_| Some(chosen));
        let svg = String::from_utf8(exported_file(&path, cx)).unwrap();
        assert_eq!(
            super::exports::SUGGESTED_NAMES.with_borrow(Clone::clone),
            ["main.svg"],
            "named after the schema"
        );
        assert!(svg.starts_with("<?xml"), "{svg}");
        assert!(svg.contains(">birds</text>") && svg.contains(">sightings</text>"));
        settle(cx);
        let notice =
            cx.update(|_, cx| workspace.read(cx).profile().unwrap().session.notice.clone());
        assert_eq!(
            notice,
            Some(format!(
                "Exported the diagram as SVG to {}.",
                path.display()
            ))
        );

        pick_diagram_export(3, cx);
        let path = directory.join("main.png");
        let chosen = path.clone();
        cx.simulate_new_path_selection(|_| Some(chosen));
        let png = exported_file(&path, cx);
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "a PNG");
        // The extension typed decides, as it does for a result's export.
        pick_diagram_export(2, cx);
        let path = directory.join("typed.png");
        let chosen = path.clone();
        cx.simulate_new_path_selection(|_| Some(chosen));
        assert!(exported_file(&path, cx).starts_with(b"\x89PNG"));
        pick_diagram_export(4, cx);
        let path = directory.join("main.mmd");
        let chosen = path.clone();
        cx.simulate_new_path_selection(|_| Some(chosen));
        let text = String::from_utf8(exported_file(&path, cx)).unwrap();
        assert!(text.starts_with("erDiagram\n"), "{text}");

        // The palette asks for the same file, and a neighbourhood is named
        // after the table in its middle too.
        let view = show_diagram(&workspace, cx);
        view.update(cx, |view, cx| view.centre_on("sightings", cx));
        settle(cx);
        super::exports::SUGGESTED_NAMES.with_borrow_mut(Vec::clear);
        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        cx.simulate_input("Export diagram as SVG");
        settle(cx);
        cx.simulate_keystrokes("enter");
        cx.simulate_new_path_selection(|_| None);
        settle(cx);
        assert_eq!(
            super::exports::SUGGESTED_NAMES.with_borrow(Clone::clone),
            ["main-sightings.svg"]
        );

        quit(workspace, cx);
    });
}

#[gpui::test]
fn copy_as_mermaid_puts_the_diagram_on_the_clipboard_from_the_menu_and_the_palette(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, DIAGRAM_SCHEMA);
        let view = show_diagram(&workspace, cx);
        let expected = "erDiagram
    \"birds\" {
        INTEGER id PK
        TEXT name
        TEXT latin
    }
    \"sightings\" {
        INTEGER id PK
        INTEGER bird FK
        TEXT seen_at
    }
    \"birds\" ||..o{ \"sightings\" : \"bird\"
";
        let copied = |cx: &mut VisualTestContext| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .expect("the copy wrote the clipboard")
        };

        pick_diagram_export(1, cx);
        assert_eq!(copied(cx), expected);
        assert_eq!(
            cx.update(|_, cx| workspace.read(cx).profile().unwrap().session.notice.clone()),
            Some("Copied the diagram as Mermaid.".to_string())
        );
        assert!(
            cx.update(|window, cx| view.read(cx).focus_handle().is_focused(window)),
            "the menu hands the keyboard back to the canvas"
        );

        cx.write_to_clipboard(gpui::ClipboardItem::new_string(String::new()));
        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        cx.simulate_input("Copy diagram as Mermaid");
        settle(cx);
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(copied(cx), expected);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_selection_preserves_fields_and_moves_focus(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_connection_in(None, window, cx)
            })
        });
        settle(cx);
        let select = |cx: &mut VisualTestContext, id: &'static str| {
            let bounds = cx.debug_bounds(id).expect("tunnel option drawn");
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            settle(cx);
        };
        select(cx, "tunnel-kind-2");
        let target =
            cx.update(|_, cx| workspace.read(cx).form.as_ref().unwrap().ssm_target.clone());
        assert!(cx.update(|window, cx| target.focus_handle(cx).is_focused(window)));
        cx.simulate_input("i-0123456789abcdef0");
        select(cx, "ssh-tunnel");
        assert!(workspace.read_with(cx, |workspace, _| {
            let form = workspace.form.as_ref().unwrap();
            form.ssh && !form.ssm
        }));
        select(cx, "tunnel-kind-2");
        assert_eq!(
            cx.update(|_, cx| target.read(cx).value().to_string()),
            "i-0123456789abcdef0"
        );
        select(cx, "tunnel-kind-0");
        assert!(workspace.read_with(cx, |workspace, _| {
            let form = workspace.form.as_ref().unwrap();
            !form.ssh && !form.ssm
        }));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_form_reopens_with_the_same_route(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |_, cx| {
                let config = ConnectionConfig::Postgres(crate::db::PostgresConfig {
                    server: crate::db::ServerConfig {
                        host: "private.example.test".into(),
                        user: "reader".into(),
                        ssm: Some(crate::db::SsmTunnel {
                            target: "i-0123456789abcdef0".into(),
                            profile: Some("production".into()),
                            region: Some("ap-northeast-1".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ..Default::default()
                });
                let mut form =
                    crate::connection_form::ConnectionForm::new(Some(&config), window, cx);
                assert!(form.ssm && !form.ssh);
                let (_, reopened) = form.config(cx).unwrap();
                assert_eq!(reopened.ssm(), config.ssm());
                assert_eq!(reopened.server().unwrap().host, "private.example.test");
                form.ssm_target
                    .update(cx, |input, cx| input.set_value("", window, cx));
                assert!(form.config(cx).is_err());
                form.ssm = false;
                assert!(form.config(cx).unwrap().1.ssm().is_none());
            });
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_setup_checks_missing_tools_and_discards_edited_paths(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_connection_in(None, window, cx);
                let form = workspace.form.as_mut().unwrap();
                form.ssm = true;
                form.ssm_aws_cli.update(cx, |input, cx| {
                    input.set_value(
                        std::env::temp_dir()
                            .join("dbdelve-missing/aws")
                            .to_string_lossy()
                            .into_owned(),
                        window,
                        cx,
                    )
                });
                form.ssm_plugin
                    .update(cx, |input, cx| input.set_value("relative-path", window, cx));
            })
        });
        settle(cx);
        assert!(workspace.read_with(cx, |workspace, _| {
            workspace.form.as_ref().unwrap().ssm_checking
        }));
        // Edit while the delayed check still owns the old paths.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let input = workspace.form.as_ref().unwrap().ssm_aws_cli.clone();
                input.update(cx, |input, cx| {
                    input.set_value("changed-relative-path", window, cx)
                });
            })
        });
        settle(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        settle(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        settle(cx);
        assert!(workspace.read_with(cx, |workspace, _| {
            matches!(
                workspace.form.as_ref().unwrap().ssm_dependencies,
                Some([
                    crate::db::ssm::dependencies::Status::Unavailable(_),
                    crate::db::ssm::dependencies::Status::Unavailable(_)
                ])
            )
        }));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_setup_recheck_preserves_detected_path_layout(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        use crate::db::ssm::dependencies::Status;
        let (workspace, cx) = launch(cx);
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1800.)));
        let results = [
            Status::Found("/previous/aws".into()),
            Status::Found("/previous/session-manager-plugin".into()),
        ];
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_connection_in(None, window, cx);
                let form = workspace.form.as_mut().unwrap();
                form.opened = std::time::Instant::now() - std::time::Duration::from_secs(2);
                form.ssm = true;
                form.ssm_advanced = true;
                // Invalid explicit paths keep this test independent of local tools.
                for input in [&form.ssm_aws_cli, &form.ssm_plugin] {
                    input.update(cx, |input, cx| input.set_value("relative-path", window, cx));
                }
            });
        });
        settle(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        settle(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.form.as_mut().unwrap().ssm_dependencies = Some(results.clone());
            cx.notify();
        });
        settle(cx);
        assert!(
            cx.debug_bounds("ssm-path-0: Detected: /previous/aws")
                .is_some()
        );
        let before = [
            cx.debug_bounds("ssm-executable-0").unwrap(),
            cx.debug_bounds("ssm-executable-1").unwrap(),
        ];
        let button = cx.debug_bounds("ssm-check-again").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        settle(cx);
        assert!(workspace.read_with(cx, |workspace, _| {
            workspace.form.as_ref().unwrap().ssm_checking
        }));
        for selector in ["ssm-path-0: Detecting…", "ssm-path-1: Detecting…"] {
            assert!(cx.debug_bounds(selector).is_some());
        }
        assert!(
            cx.debug_bounds("ssm-path-0: Detected: /previous/aws")
                .is_none()
        );
        assert_eq!(
            before,
            [
                cx.debug_bounds("ssm-executable-0").unwrap(),
                cx.debug_bounds("ssm-executable-1").unwrap()
            ]
        );
        assert!(workspace.read_with(cx, |workspace, _| {
            workspace.form.as_ref().unwrap().ssm_dependencies == Some(results)
        }));
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        settle(cx);
        assert!(workspace.read_with(cx, |workspace, _| {
            let form = workspace.form.as_ref().unwrap();
            !form.ssm_checking
                && matches!(
                    form.ssm_dependencies,
                    Some([Status::Unavailable(_), Status::Unavailable(_)])
                )
        }));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_setup_first_expansion_keeps_detail_rows_through_detection(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        use crate::db::ssm::dependencies::Status;
        let (workspace, cx) = launch(cx);
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1800.)));
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_connection_in(None, window, cx);
                let form = workspace.form.as_mut().unwrap();
                form.opened = std::time::Instant::now() - std::time::Duration::from_secs(2);
                form.ssm = true;
                for input in [&form.ssm_aws_cli, &form.ssm_plugin] {
                    input.update(cx, |input, cx| input.set_value("relative-path", window, cx));
                }
            });
        });
        settle(cx);
        let advanced = cx.debug_bounds("ssm-advanced").unwrap();
        cx.simulate_click(advanced.center(), gpui::Modifiers::default());
        settle(cx);
        for selector in ["ssm-path-0: Detecting…", "ssm-path-1: Detecting…"] {
            assert!(cx.debug_bounds(selector).is_some());
        }
        let before = [
            cx.debug_bounds("ssm-executable-0").unwrap(),
            cx.debug_bounds("ssm-executable-1").unwrap(),
        ];
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        settle(cx);
        assert!(workspace.read_with(cx, |workspace, _| {
            !workspace.form.as_ref().unwrap().ssm_checking
        }));
        for results in [
            [Status::Unavailable("invalid path".into()), Status::Missing],
            [
                Status::Found("/aws".into()),
                Status::Found("/plugin".into()),
            ],
        ] {
            workspace.update(cx, |workspace, cx| {
                workspace.form.as_mut().unwrap().ssm_dependencies = Some(results);
                cx.notify();
            });
            settle(cx);
            // Missing tools add setup guidance above Advanced; the path rows
            // themselves must retain their size.
            assert_eq!(
                before.map(|bounds| bounds.size),
                [
                    cx.debug_bounds("ssm-executable-0").unwrap().size,
                    cx.debug_bounds("ssm-executable-1").unwrap().size
                ]
            );
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_setup_long_paths_keep_one_line_at_larger_ui_fonts(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        use crate::db::ssm::dependencies::Status;
        let (workspace, cx) = launch(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_connection_in(None, window, cx);
                let form = workspace.form.as_mut().unwrap();
                form.opened = std::time::Instant::now() - std::time::Duration::from_secs(2);
                form.ssm = true;
                form.ssm_advanced = true;
                form.ssm_dependencies = Some([
                    Status::Found("/aws".into()),
                    Status::Found("/plugin".into()),
                ]);
            });
        });
        for font_size in [13., 15., 17.] {
            workspace.update(cx, |workspace, cx| {
                workspace.set_font_size(super::FontSlot::Chrome, font_size, cx);
            });
            for width in [480., 1200.] {
                cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(2400.)));
                workspace.update(cx, |workspace, cx| {
                    let form = workspace.form.as_mut().unwrap();
                    form.ssm_checking = false;
                    form.ssm_dependencies = Some([
                        Status::Found("/aws".into()),
                        Status::Found("/plugin".into()),
                    ]);
                    cx.notify();
                });
                settle(cx);
                let short = [
                    cx.debug_bounds("ssm-executable-0").unwrap(),
                    cx.debug_bounds("ssm-executable-1").unwrap(),
                ];
                workspace.update(cx, |workspace, cx| {
                    let long = format!(
                        "/Applications/{}/aws",
                        "A very long installation directory/".repeat(20)
                    );
                    workspace.form.as_mut().unwrap().ssm_dependencies = Some([
                        Status::Found(long.clone().into()),
                        Status::Found(long.into()),
                    ]);
                    cx.notify();
                });
                settle(cx);
                assert_eq!(
                    short,
                    [
                        cx.debug_bounds("ssm-executable-0").unwrap(),
                        cx.debug_bounds("ssm-executable-1").unwrap()
                    ],
                    "long paths at font {font_size}, width {width}"
                );
                workspace.update(cx, |workspace, cx| {
                    workspace.form.as_mut().unwrap().ssm_checking = true;
                    cx.notify();
                });
                settle(cx);
                assert_eq!(
                    short,
                    [
                        cx.debug_bounds("ssm-executable-0").unwrap(),
                        cx.debug_bounds("ssm-executable-1").unwrap()
                    ],
                    "detecting at font {font_size}, width {width}"
                );
            }
        }
        workspace.update(cx, |workspace, cx| {
            workspace.set_font_size(
                super::FontSlot::Chrome,
                crate::theme::layout::BODY_FONT_SIZE,
                cx,
            );
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_setup_advanced_collapse_restores_focus_and_keeps_paths(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1800.)));
        let (input, target) = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_connection_in(None, window, cx);
                let form = workspace.form.as_mut().unwrap();
                form.ssm = true;
                form.ssm_advanced = true;
                // Keep this focus test independent of installed tools.
                form.ssm_dependencies = Some([
                    crate::db::ssm::dependencies::Status::Missing,
                    crate::db::ssm::dependencies::Status::Missing,
                ]);
                form.ssm_aws_cli.update(cx, |input, cx| {
                    input.set_value("keep-this-path", window, cx)
                });
                (form.ssm_aws_cli.clone(), form.ssm_target.clone())
            })
        });
        settle(cx);
        cx.update(|window, cx| window.focus(&input.focus_handle(cx), cx));
        let bounds = cx.debug_bounds("ssm-advanced").expect("Advanced drawn");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        settle(cx);
        assert!(cx.update(|window, cx| target.focus_handle(cx).is_focused(window)));
        assert_eq!(
            cx.update(|_, cx| input.read(cx).value().to_string()),
            "keep-this-path"
        );
        assert!(!workspace.read_with(cx, |workspace, _| {
            workspace.form.as_ref().unwrap().ssm_advanced
        }));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_picker_selection_rechecks_tools_but_cancel_keeps_the_result(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        use crate::db::ssm::dependencies::Status;
        let (workspace, cx) = launch(cx);
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1800.)));
        let input = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_connection_in(None, window, cx);
                let form = workspace.form.as_mut().unwrap();
                form.ssm = true;
                form.ssm_dependencies = Some([Status::Missing, Status::Missing]);
                form.ssm_aws_cli.clone()
            })
        });
        settle(cx);
        let choose = |cx: &mut VisualTestContext| {
            let bounds = cx.debug_bounds("ssm-choose-0").unwrap();
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            settle(cx);
            assert!(cx.did_prompt_for_paths());
        };
        choose(cx);
        cx.simulate_path_prompt_response(|_| None);
        settle(cx);
        assert!(cx.update(|_, cx| input.read(cx).value().is_empty()));
        assert!(workspace.read_with(cx, |workspace, _| {
            let form = workspace.form.as_ref().unwrap();
            !form.ssm_checking && form.ssm_dependencies == Some([Status::Missing, Status::Missing])
        }));
        choose(cx);
        let selected = std::env::temp_dir().join("dbdelve-picker-test/missing-aws");
        cx.simulate_path_prompt_response(|_| Some(vec![selected.clone()]));
        settle(cx);
        assert_eq!(
            cx.update(|_, cx| input.read(cx).value().to_string()),
            selected.to_string_lossy()
        );
        assert!(
            workspace.read_with(cx, |workspace, _| {
                let form = workspace.form.as_ref().unwrap();
                form.ssm_checking && form.ssm_dependencies.is_none()
            }),
            "choosing a path must invalidate the previous tool result"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_wrong_executable_offers_picker_before_installation_guide(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        use crate::db::ssm::dependencies::Status;
        let (workspace, cx) = launch(cx);
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1800.)));
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_connection_in(None, window, cx);
                let form = workspace.form.as_mut().unwrap();
                form.opened = std::time::Instant::now() - std::time::Duration::from_secs(2);
                form.ssm = true;
                form.ssm_dependencies = Some([
                    Status::WrongExecutable("Choose the aws executable.".into()),
                    Status::Unavailable("Version check timed out.".into()),
                ]);
            });
        });
        settle(cx);
        assert!(cx.debug_bounds("ssm-status-0: Wrong executable").is_some());
        assert!(cx.debug_bounds("ssm-status-1: Couldn’t run").is_some());
        let picker = cx.debug_bounds("ssm-choose-0").unwrap();
        let guide = cx.debug_bounds("ssm-install-0").unwrap();
        assert!(picker.origin.x < guide.origin.x);
        assert_eq!(picker.center().y, guide.center().y);
        cx.simulate_click(picker.center(), gpui::Modifiers::default());
        settle(cx);
        assert!(cx.did_prompt_for_paths());
        cx.simulate_path_prompt_response(|_| None);
        settle(cx);
        assert!(cx.debug_bounds("ssm-status-0: Wrong executable").is_some());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_draft_apply_on_a_table_keeps_its_rows_on_screen(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| {
            assert!(
                grid.delegate_mut()
                    .set_pending(0, 1, NewValue::Value("robin".into()))
            );
        });
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply edits", true, None))
        );
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "nothing is sent before Run"
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        // Sent and not yet answered: the rows and the edit are where they were.
        assert!(matches!(
            front_state(&workspace, cx),
            QueryState::Running { .. }
        ));
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 2);
            assert_eq!(grid.delegate().pending_count(), 1);
        });

        settle(cx);
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 2);
            assert_eq!(grid.delegate().cell(0, 1), Some("robin"));
            assert_eq!(grid.delegate().pending_count(), 0);
        });
        assert_eq!(front_review(&workspace, cx), None, "the review is gone");
        assert!(matches!(
            front_state(&workspace, cx),
            QueryState::Complete { .. }
        ));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_failed_apply_keeps_the_draft(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| {
            // A NULL into a NOT NULL column, which the table refuses.
            assert!(grid.delegate_mut().set_pending(0, 1, NewValue::Null));
        });
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 2);
            assert_eq!(grid.delegate().pending_count(), 1);
        });
        assert!(
            !matches!(front_state(&workspace, cx), QueryState::Failed(_)),
            "an error pane would cover the rows the edit is staged on"
        );
        let (_, _, error) = front_review(&workspace, cx).expect("the review stays");
        let error = error.expect("the review says why");
        assert!(error.contains("NOT NULL"), "{error}");
        assert!(
            cx.debug_bounds("apply-review-error").is_some(),
            "the review draws it"
        );
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx).and_then(|(_, _, error)| error),
            Some(error),
            "run again, it is the constraint's once and not twice"
        );

        workspace.update(cx, |workspace, cx| workspace.close_apply_review(cx));
        settle(cx);
        grid.update(cx, |grid, _| {
            assert!(
                grid.delegate_mut()
                    .set_pending(0, 1, NewValue::Value("robin".into()))
            );
        });
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply edits", true, None))
        );
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        grid.read_with(cx, |grid, _| assert_eq!(grid.delegate().pending_count(), 0));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_edit_staged_while_an_apply_is_in_flight_stays_staged(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        run_in_front(&workspace, cx, "select * from birds order by id");
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        let stage = |cx: &mut VisualTestContext, row: usize, name: &str| {
            grid.update(cx, |grid, _| {
                assert!(grid.delegate_mut().set_pending(
                    row,
                    1,
                    NewValue::Value(name.to_string().into())
                ));
            });
        };
        stage(cx, 0, "robin");
        // SQLite does not review a query tab's edits, so the append sends at
        // once. Not through the action, which would run the executor to the
        // answer before the test can stage anything.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.apply_edits(&crate::actions::ApplyEdits, window, cx)
            })
        });
        assert!(
            matches!(front_state(&workspace, cx), QueryState::Running { .. }),
            "the batch is in flight"
        );
        stage(cx, 1, "jay");
        stage(cx, 0, "finch");
        settle(cx);

        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("tit"),
            "only the batch went"
        );
        grid.read_with(cx, |grid, _| {
            let grid = grid.delegate();
            assert_eq!(grid.result().rows.len(), 2);
            assert_eq!(grid.pending_count(), 2);
            assert_eq!(
                grid.pending_value(1, 1),
                Some(&NewValue::Value("jay".into()))
            );
            assert_eq!(
                grid.pending_value(0, 1),
                Some(&NewValue::Value("finch".into()))
            );
        });
        assert!(matches!(
            front_state(&workspace, cx),
            QueryState::Complete { .. }
        ));
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.profile().unwrap().session.shown_notice(),
                Some("The change was applied. The rows were not read again: this tab has unapplied edits.")
            );
        });

        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("finch")
        );
        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("jay")
        );
        grid.read_with(cx, |grid, _| {
            let grid = grid.delegate();
            assert_eq!(grid.pending_count(), 0);
            assert_eq!(grid.cell(0, 1), Some("finch"));
            assert_eq!(grid.cell(1, 1), Some("jay"), "the re-read ran");
        });
        quit(workspace, cx);
    });
}

/// The notice the session holds for a tab, whichever tab is in front.
fn tab_notice(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<(Tab, String)> {
    workspace.read_with(cx, |workspace, _| {
        workspace.profile().unwrap().session.tab_notice.clone()
    })
}

#[gpui::test]
fn a_failed_apply_from_a_query_tab_keeps_the_draft(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        run_in_front(&workspace, cx, "select * from birds order by id");
        let tab = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        grid.update(cx, |grid, _| {
            // A NULL into a NOT NULL column, which the table refuses.
            assert!(grid.delegate_mut().set_pending(0, 1, NewValue::Null));
        });
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);

        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 2);
            assert_eq!(grid.delegate().pending_count(), 1);
        });
        assert!(
            matches!(front_state(&workspace, cx), QueryState::Complete { .. }),
            "the tab is as it was, not an error pane over the rows"
        );
        let (noticed, message) = tab_notice(&workspace, cx).expect("the failure is said");
        assert_eq!(noticed, tab);
        assert!(message.contains("NOT NULL"), "{message}");
        assert!(
            buffer(&workspace, cx).contains("UPDATE"),
            "{}",
            buffer(&workspace, cx)
        );
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_reviewed_delete_leaves_other_rows_edits_staged(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| {
            assert!(
                grid.delegate_mut()
                    .set_pending(0, 1, NewValue::Value("robin".into()))
            );
            grid.delegate_mut().select_row(1);
        });
        cx.dispatch_action(crate::actions::DeleteRow);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Delete row", false, None))
        );
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            on_disk("select count(*) from birds").as_deref(),
            Some("1"),
            "row 2 is gone"
        );
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 2);
            assert_eq!(
                grid.delegate().pending_value(0, 1),
                Some(&NewValue::Value("robin".into())),
                "the other row's edit is not the delete's to drop"
            );
            assert_eq!(grid.delegate().pending_count(), 1);
        });
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.profile().unwrap().session.shown_notice(),
                Some("The change was applied. The rows were not read again: this tab has unapplied edits.")
            );
        });
        quit(workspace, cx);
    });
}

/// `BIRDS` with a number beside the name, which a typed `07` and the server's
/// `7` tell apart.
const COUNTED_BIRDS: &str = "create table birds (id integer primary key, name text not null, n integer); \
                             insert into birds values (1, 'wren', 1), (2, 'tit', 2);";

/// The edit mode of relation tab `id` of the profile in front.
fn edit_mode_of(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: u64,
) -> crate::session::EditMode {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .profile()
            .unwrap()
            .session
            .edit_mode(Tab::Object(id))
    })
}

/// The tab in front's grid cell, the server's value as the grid holds it.
fn cell_of(
    grid: &Entity<gpui_component::table::TableState<crate::result_grid::ResultGrid>>,
    cx: &mut VisualTestContext,
    row: usize,
    col: usize,
) -> Option<String> {
    grid.read_with(cx, |grid, _| {
        grid.delegate().cell(row, col).map(str::to_string)
    })
}

#[gpui::test]
fn saving_in_row_mode_sends_the_rows_update_without_a_dialog(cx: &mut TestAppContext) {
    use crate::{result_grid::NewValue, session::EditMode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, COUNTED_BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        workspace.update(cx, |workspace, cx| {
            workspace.set_edit_mode(EditMode::Row, cx)
        });
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Row);

        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| {
            let grid = grid.delegate_mut();
            assert!(grid.set_pending(0, 1, NewValue::Value("robin".into())));
            assert!(grid.set_pending(0, 2, NewValue::Value("07".into())));
        });
        // Not through the action, which runs the executor to the answer before
        // the test can look at the send in flight.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.apply_edits(&crate::actions::ApplyEdits, window, cx)
            })
        });
        assert_eq!(front_review(&workspace, cx), None, "no dialog");
        assert!(matches!(
            front_state(&workspace, cx),
            QueryState::Running { .. }
        ));
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 2);
            assert_eq!(grid.delegate().pending_count(), 2, "kept until answered");
        });

        settle(cx);
        assert_eq!(
            on_disk("select name || ':' || n from birds where id = 1").as_deref(),
            Some("robin:7")
        );
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 2);
            assert_eq!(grid.delegate().pending_count(), 0);
        });
        assert_eq!(cell_of(&grid, cx, 0, 1).as_deref(), Some("robin"));
        assert_eq!(
            cell_of(&grid, cx, 0, 2).as_deref(),
            Some("7"),
            "the server's value, not the typed one"
        );
        assert_eq!(cell_of(&grid, cx, 1, 1).as_deref(), Some("tit"));
        assert!(matches!(
            front_state(&workspace, cx),
            QueryState::Complete { .. }
        ));
        assert_eq!(front_review(&workspace, cx), None);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().unwrap();
            assert!(profile.session.history[0].sql.starts_with("UPDATE"));
        });
        let notice = shown_notice(&workspace, cx).expect("the send is on the status line");
        assert!(notice.starts_with("1 row updated: UPDATE"), "{notice}");
        quit(workspace, cx);
    });
}

type Grid = Entity<gpui_component::table::TableState<crate::result_grid::ResultGrid>>;

/// Opens `birds` in Row mode, as `COUNTED_BIRDS` or a variation of it holds it.
fn open_in_row_mode(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (u64, Grid) {
    let id = open_own_table(workspace, cx, "birds");
    workspace.update(cx, |workspace, cx| {
        workspace.set_edit_mode(crate::session::EditMode::Row, cx)
    });
    assert_eq!(
        edit_mode_of(workspace, cx, id),
        crate::session::EditMode::Row
    );
    let grid = relation_grid(workspace, cx, id);
    (id, grid)
}

fn stage(
    grid: &Grid,
    cx: &mut VisualTestContext,
    row: usize,
    col: usize,
    value: crate::result_grid::NewValue,
) {
    grid.update(cx, |grid, _| {
        assert!(grid.delegate_mut().set_pending(row, col, value));
    });
}

/// Save as the keystroke does it, settled.
fn save(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    let _ = workspace;
    cx.dispatch_action(crate::actions::ApplyEdits);
    settle(cx);
}

/// How many lines the profile's history file holds, a run being one line as it
/// is sent and one as it lands.
fn history_lines(profile: &str) -> usize {
    let path = store::dbdelve_directory()
        .expect("the test home has a data directory")
        .join("queries")
        .join(profile)
        .join(".history.jsonl");
    std::fs::read_to_string(path).map_or(0, |text| text.lines().count())
}

fn history_sql(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    workspace.read_with(cx, |workspace, _| {
        let profile = workspace.profile().unwrap();
        profile
            .session
            .history
            .iter()
            .map(|entry| entry.sql.clone())
            .collect()
    })
}

#[gpui::test]
fn a_row_send_is_recorded_in_history(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);

        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        save(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Value("jay".into()));
        save(&workspace, cx);

        let held = history_sql(&workspace, cx);
        assert_eq!(held.len(), 2, "{held:?}");
        assert!(
            held[0].starts_with("UPDATE") && held[0].contains("'jay'"),
            "{held:?}"
        );
        assert!(held[1].contains("'robin'"), "{held:?}");
        let on_file: Vec<String> = store::history("e2e")
            .into_iter()
            .map(|entry| entry.sql)
            .collect();
        assert_eq!(on_file, held, "newest first, as the file reads back");
        let notice = shown_notice(&workspace, cx).expect("the last send is on the status line");
        assert!(notice.starts_with("1 row updated: UPDATE"), "{notice}");
        assert!(notice.contains("'jay'"), "{notice}");

        // The server refuses it: sent, so recorded, and with its failure.
        stage(&grid, cx, 0, 1, NewValue::Null);
        save(&workspace, cx);
        let held = history_sql(&workspace, cx);
        assert_eq!(held.len(), 3);
        assert!(held[0].contains("NULL"), "{held:?}");
        let failed = store::history("e2e").remove(0);
        assert!(
            failed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("NOT NULL")),
            "{failed:?}"
        );
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("jay")
        );

        // The text of the first send again: one entry in the list, and its own
        // lines in the file.
        let lines = history_lines("e2e");
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        save(&workspace, cx);
        assert_eq!(history_lines("e2e"), lines + 2);
        let held = history_sql(&workspace, cx);
        assert_eq!(held.len(), 3, "one entry per distinct text");
        assert!(held[0].contains("'robin'"), "{held:?}");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_row_that_is_gone_after_its_update_says_so_and_rereads(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let schema = format!(
            "{BIRDS} create trigger gone after update on birds when new.name = 'ghost' \
             begin delete from birds where id = new.id; end;"
        );
        let (workspace, cx) = launch_on_sqlite_holding(cx, &schema);
        let (_, grid) = open_in_row_mode(&workspace, cx);

        stage(&grid, cx, 0, 1, NewValue::Value("ghost".into()));
        save(&workspace, cx);

        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("The row was not found after the update.")
        );
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 1, "read again");
            assert_eq!(grid.delegate().pending_count(), 0);
        });
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("1"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn saving_twice_sends_one_update(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let schema = format!(
            "{BIRDS} create table counter (n integer); insert into counter values (0); \
             create trigger counted after update on birds begin update counter set n = n + 1; end;"
        );
        let (workspace, cx) = launch_on_sqlite_holding(cx, &schema);
        let (_, grid) = open_in_row_mode(&workspace, cx);

        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        // Not through the action, which runs the executor to the answer
        // between the two: the second Save has to find the first in flight.
        let press = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.apply_edits(&crate::actions::ApplyEdits, window, cx)
                })
            })
        };
        press(cx);
        assert!(matches!(
            front_state(&workspace, cx),
            QueryState::Running { .. }
        ));
        press(cx);
        settle(cx);
        assert_eq!(on_disk("select n from counter").as_deref(), Some("1"));
        grid.read_with(cx, |grid, _| assert_eq!(grid.delegate().pending_count(), 0));

        // Landed: the row holds no edits, so another Save sends nothing.
        save(&workspace, cx);
        assert_eq!(on_disk("select n from counter").as_deref(), Some("1"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn save_in_row_mode_with_nothing_staged_sends_nothing(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (_, _grid) = open_in_row_mode(&workspace, cx);
        let (notice, history, lines) = (
            shown_notice(&workspace, cx),
            history_sql(&workspace, cx),
            history_lines("e2e"),
        );

        save(&workspace, cx);

        assert_eq!(front_review(&workspace, cx), None);
        assert_eq!(shown_notice(&workspace, cx), notice);
        assert_eq!(history_sql(&workspace, cx), history);
        assert_eq!(history_lines("e2e"), lines);
        assert!(matches!(
            front_state(&workspace, cx),
            QueryState::Complete { .. }
        ));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn save_sends_the_rings_row_first_and_one_row_at_a_time(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);

        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        stage(&grid, cx, 1, 1, NewValue::Value("jay".into()));
        grid.update(cx, |grid, _| grid.delegate_mut().select_row(1));

        save(&workspace, cx);
        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("jay")
        );
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().pending_count(), 1);
            assert_eq!(grid.delegate().first_pending_row(), Some(0));
        });

        save(&workspace, cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        grid.read_with(cx, |grid, _| assert_eq!(grid.delegate().pending_count(), 0));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn draft_to_row_is_refused_while_the_draft_holds_edits(cx: &mut TestAppContext) {
    use crate::{result_grid::NewValue, session::EditMode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        let go_row = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_edit_mode(EditMode::Row, cx)
            })
        };

        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        stage(&grid, cx, 1, 1, NewValue::Value("jay".into()));
        go_row(&workspace, cx);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Draft);
        grid.read_with(cx, |grid, _| assert_eq!(grid.delegate().pending_count(), 2));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("The draft has 2 edits, so the view was not switched to Row mode.")
        );

        grid.update(cx, |grid, _| grid.delegate_mut().discard_row(1));
        go_row(&workspace, cx);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Draft);
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("The draft has 1 edit, so the view was not switched to Row mode.")
        );

        grid.update(cx, |grid, _| grid.delegate_mut().discard_pending());
        workspace.update(cx, |workspace, _| workspace.clear_notice());
        go_row(&workspace, cx);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Row);
        assert_eq!(shown_notice(&workspace, cx), None);

        // The mode the view already has: nothing is said and nothing changes.
        go_row(&workspace, cx);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Row);
        assert_eq!(shown_notice(&workspace, cx), None);
        quit(workspace, cx);
    });
}

/// The edit mode `views::view_edit_mode` answers for the tab in front.
fn front_edit_mode(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<crate::session::EditMode> {
    workspace.read_with(cx, |workspace, _| {
        crate::views::view_edit_mode(workspace.profile().unwrap())
    })
}

#[gpui::test]
fn the_status_bar_switches_a_tables_edit_mode(cx: &mut TestAppContext) {
    use crate::{result_grid::NewValue, session::EditMode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let query_tab = workspace.read_with(cx, |workspace, _| {
            Tab::Query(workspace.profile().unwrap().session.queries[0].id)
        });
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        assert_eq!(drawn(cx, &["edit-mode"]), [true]);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Draft));

        pick_edit_mode(&workspace, cx, EditMode::Row);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Row));
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        save(&workspace, cx);
        assert_eq!(front_review(&workspace, cx), None, "no dialog");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin"),
            "the chip's choice reaches the send"
        );

        // The mode the view already has: nothing changes and nothing is said.
        workspace.update(cx, |workspace, _| workspace.clear_notice());
        pick_edit_mode(&workspace, cx, EditMode::Row);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Row));
        assert_eq!(shown_notice(&workspace, cx), None);

        pick_edit_mode(&workspace, cx, EditMode::Draft);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Draft));

        // A draft is never sent by switching: the Row chip refuses and says so.
        stage(&grid, cx, 0, 1, NewValue::Value("jay".into()));
        stage(&grid, cx, 1, 1, NewValue::Value("owl".into()));
        pick_edit_mode(&workspace, cx, EditMode::Row);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Draft));
        grid.read_with(cx, |grid, _| assert_eq!(grid.delegate().pending_count(), 2));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("tit")
        );
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("The draft has 2 edits, so the view was not switched to Row mode.")
        );

        // A query tab has no rows to choose a mode for.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_tab(query_tab, window, cx)
            });
        });
        settle(cx);
        assert_eq!(drawn(cx, &["edit-mode"]), [false]);
        assert_eq!(front_edit_mode(&workspace, cx), None);
        quit(workspace, cx);
    });
}

/// `profiles.toml` as the window last saved it.
fn saved_profiles_text() -> String {
    std::fs::read_to_string(
        store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("profiles.toml"),
    )
    .expect("profiles.toml reads")
}

#[gpui::test]
fn a_new_table_tab_starts_in_the_default_edit_mode(cx: &mut TestAppContext) {
    use crate::session::EditMode;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text); \
             create table nests (id integer primary key, owner text);",
        );
        let set_default = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext, mode| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_default_edit_mode(mode, cx)
            })
        };
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.settings.edit_mode),
            EditMode::Classic
        );
        let birds = open_own_table(&workspace, cx, "birds");
        assert_eq!(edit_mode_of(&workspace, cx, birds), EditMode::Classic);

        set_default(&workspace, cx, EditMode::Row);
        assert_eq!(
            edit_mode_of(&workspace, cx, birds),
            EditMode::Classic,
            "each open tab holds its own"
        );
        assert!(saved_profiles_text().contains("edit_mode = \"row\""));
        let nests = open_own_table(&workspace, cx, "nests");
        assert_eq!(edit_mode_of(&workspace, cx, nests), EditMode::Row);

        // The default it already holds: nothing is written.
        let before = saved_profiles_text();
        let path = store::dbdelve_directory().unwrap().join("profiles.toml");
        std::fs::write(&path, format!("{before}# marker\n")).unwrap();
        set_default(&workspace, cx, EditMode::Row);
        assert!(saved_profiles_text().ends_with("# marker\n"), "no save");

        set_default(&workspace, cx, EditMode::Draft);
        assert!(saved_profiles_text().contains("edit_mode = \"draft\""));
        set_default(&workspace, cx, EditMode::Classic);
        assert!(!saved_profiles_text().contains("edit_mode"));
        quit(workspace, cx);
    });
}

/// What the palette lists with nothing typed, closed again.
fn palette_listing(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    cx.simulate_keystrokes("secondary-shift-p");
    settle(cx);
    let labels = palette_labels(workspace, cx);
    cx.simulate_keystrokes("escape");
    settle(cx);
    labels
}

#[gpui::test]
fn the_palette_switches_a_tables_edit_mode(cx: &mut TestAppContext) {
    use crate::{result_grid::NewValue, session::EditMode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let has = |rows: &[String], label: &str| rows.iter().any(|row| row == label);

        let rows = palette_listing(&workspace, cx);
        assert!(!has(&rows, "Edit in Row mode"), "{rows:?}");
        assert!(!has(&rows, "Edit in Draft mode"), "{rows:?}");

        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        let rows = palette_listing(&workspace, cx);
        assert!(has(&rows, "Edit in Row mode"), "{rows:?}");
        assert!(!has(&rows, "Edit in Draft mode"), "{rows:?}");

        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        cx.simulate_input("Edit in Row mode");
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Row));
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Row);

        let rows = palette_listing(&workspace, cx);
        assert!(has(&rows, "Edit in Draft mode"), "{rows:?}");
        assert!(!has(&rows, "Edit in Row mode"), "{rows:?}");
        assert!(!has(&rows, "Save row"), "nothing is staged: {rows:?}");

        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let rows = palette_listing(&workspace, cx);
        assert!(has(&rows, "Save row"), "{rows:?}");
        assert!(!has(&rows, "Apply edits"), "{rows:?}");

        grid.update(cx, |grid, _| grid.delegate_mut().discard_pending());
        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        cx.simulate_input("Edit in Draft mode");
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Draft));

        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let rows = palette_listing(&workspace, cx);
        assert!(has(&rows, "Apply edits"), "{rows:?}");
        assert!(!has(&rows, "Save row"), "{rows:?}");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_table_without_a_key_stays_read_only_in_row_mode(cx: &mut TestAppContext) {
    use crate::{result_grid::NewValue, session::EditMode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table logs (note text, at text); \
             insert into logs values ('a', 'x');",
        );
        let id = open_own_table(&workspace, cx, "logs");
        let grid = relation_grid(&workspace, cx, id);
        // Whether any cell takes an edit, and what the grid says about why not.
        let staged = |cx: &mut VisualTestContext| {
            grid.update(cx, |grid, _| {
                let grid = grid.delegate_mut();
                let staged: Vec<bool> = (0..2)
                    .map(|col| grid.set_pending(0, col, NewValue::Value("b".into())))
                    .collect();
                (
                    staged,
                    grid.read_only_reason().map(str::to_string),
                    grid.pending_count(),
                )
            })
        };
        let in_draft = staged(cx);
        assert_eq!(in_draft, (vec![false, false], None, 0));

        workspace.update(cx, |workspace, cx| {
            workspace.set_edit_mode(EditMode::Row, cx)
        });
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Row);
        assert_eq!(staged(cx), in_draft, "Row mode adds no way to stage");
        save(&workspace, cx);
        assert_eq!(front_review(&workspace, cx), None);
        assert_eq!(on_disk("select note from logs").as_deref(), Some("a"));
        assert_eq!(history_sql(&workspace, cx), Vec::<String>::new());

        workspace.update(cx, |workspace, cx| {
            workspace.set_edit_mode(EditMode::Draft, cx)
        });
        assert_eq!(staged(cx), in_draft, "and Draft is as it was");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn delete_row_and_new_row_keep_their_review_and_form_in_a_row_view(cx: &mut TestAppContext) {
    use crate::session::EditMode;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, grid) = open_in_row_mode(&workspace, cx);
        grid.update(cx, |grid, _| grid.delegate_mut().select_row(1));

        cx.dispatch_action(crate::actions::DeleteRow);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Delete row", false, None))
        );
        assert!(!matches!(
            front_state(&workspace, cx),
            QueryState::Running { .. }
        ));
        assert_eq!(
            on_disk("select count(*) from birds").as_deref(),
            Some("2"),
            "read before anything is sent"
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("1"));
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 1)
        });
        assert_eq!(front_review(&workspace, cx), None);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Row);

        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let form = workspace.profile().unwrap().session.insert_form.as_ref();
            assert_eq!(form.map(|form| form.tab), Some(Tab::Object(id)));
        });
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("1"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_row_save_does_not_send_over_an_open_review(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        grid.update(cx, |grid, _| grid.delegate_mut().select_row(1));
        cx.dispatch_action(crate::actions::DeleteRow);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Delete row", false, None))
        );

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.apply_edits(&crate::actions::ApplyEdits, window, cx)
            })
        });
        settle(cx);

        // Nothing went to the server, the review is not given an `UPDATE`'s
        // outcome and the edit stays.
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Delete row", false, None))
        );
        grid.read_with(cx, |grid, _| assert_eq!(grid.delegate().pending_count(), 1));
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("The row was not sent: a statement is waiting in review.")
        );
        quit(workspace, cx);
    });
}

const THREE_BIRDS: &str = "create table birds (id integer primary key, name text not null, n integer); \
     insert into birds values (1, 'wren', 1), (2, 'tit', 2), (3, 'kite', 3);";

/// The grid has the keyboard, as it does once a tab opens or a cell's input
/// closes.
fn focus_grid(grid: &Grid, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        use gpui::Focusable;
        grid.focus_handle(cx).focus(window, cx)
    });
    settle(cx);
}

/// Puts the ring on a cell as a pointer would reach it from the library's side:
/// the row through the table's own selection, the column through the grid.
fn ring_on(grid: &Grid, cx: &mut VisualTestContext, row: usize, col: usize) {
    grid.update(cx, |table, cx| table.set_selected_row(row, cx));
    grid.update(cx, |table, _| table.delegate_mut().select_col(col));
    settle(cx);
}

fn ring_of(grid: &Grid, cx: &mut VisualTestContext) -> Option<(usize, usize)> {
    grid.read_with(cx, |grid, _| grid.delegate().active())
}

fn pending_of(grid: &Grid, cx: &mut VisualTestContext) -> usize {
    grid.read_with(cx, |grid, _| grid.delegate().pending_count())
}

fn state_kind(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> &'static str {
    match front_state(workspace, cx) {
        QueryState::Idle => "idle",
        QueryState::Running { .. } => "running",
        QueryState::Complete { .. } => "complete",
        QueryState::Failed(_) => "failed",
        QueryState::Explained { .. } => "explained",
    }
}

#[gpui::test]
fn leaving_an_edited_row_sends_its_update(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);

        // An edit typed into the cell, carried down with the arrow.
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("robin");
        settle(cx);
        press(cx, "down");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(ring_of(&grid, cx), Some((1, 1)));
        assert_eq!(cell_of(&grid, cx, 0, 1).as_deref(), Some("robin"));
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(front_review(&workspace, cx), None, "no dialog");
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().unwrap();
            assert!(profile.session.history[0].sql.starts_with("UPDATE"));
        });

        // A staged edit, left by the arrow on the grid. The input the carried
        // edit opened on row 1 is put away first.
        press(cx, "escape");
        stage(&grid, cx, 1, 1, NewValue::Value("jay".into()));
        press(cx, "down");
        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("jay")
        );
        assert_eq!(ring_of(&grid, cx), Some((2, 1)));

        // A staged edit, left by the pointer.
        stage(&grid, cx, 2, 1, NewValue::Value("owl".into()));
        grid.update(cx, |table, cx| table.set_selected_row(0, cx));
        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 3").as_deref(),
            Some("owl")
        );
        assert_eq!(ring_of(&grid, cx).map(|(row, _)| row), Some(0));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn draft_mode_sends_nothing_when_the_row_is_left(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        assert_eq!(
            edit_mode_of(&workspace, cx, id),
            crate::session::EditMode::Draft
        );
        let grid = relation_grid(&workspace, cx, id);
        let before = state_kind(&workspace, cx);

        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        press(cx, "down");
        assert_eq!(ring_of(&grid, cx).map(|(row, _)| row), Some(1));
        grid.update(cx, |table, cx| table.set_selected_row(2, cx));
        settle(cx);
        grid.update(cx, |table, cx| table.set_selected_row(0, cx));
        settle(cx);

        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        assert_eq!(pending_of(&grid, cx), 1);
        assert_eq!(shown_notice(&workspace, cx), None);
        assert_eq!(state_kind(&workspace, cx), before);
        assert_eq!(history_sql(&workspace, cx).len(), 0);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn moving_within_a_row_sends_nothing(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));

        for key in ["right", "tab", "left"] {
            press(cx, key);
            assert_eq!(
                ring_of(&grid, cx).map(|(row, _)| row),
                Some(0),
                "{key} stays on the row"
            );
            assert_eq!(
                on_disk("select name from birds where id = 1").as_deref(),
                Some("wren"),
                "{key} sent the row"
            );
            assert_eq!(pending_of(&grid, cx), 1, "{key}");
        }

        // An input committed with the value it was opened on stages nothing, so
        // the row it leaves is not sent.
        grid.update(cx, |table, _| table.delegate_mut().discard_pending());
        ring_on(&grid, cx, 1, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        press(cx, "enter");
        assert_eq!(pending_of(&grid, cx), 0);
        press(cx, "down");
        assert_eq!(ring_of(&grid, cx).map(|(row, _)| row), Some(2));
        assert_eq!(history_sql(&workspace, cx).len(), 0);
        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("tit")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn scrolling_and_window_blur_send_nothing(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let schema = "create table birds (id integer primary key, name text not null, n integer); \
             with recursive s(i) as (select 1 union all select i + 1 from s where i < 60) \
             insert into birds select i, 'bird' || i, i from s;";
        let (workspace, cx) = launch_on_sqlite_holding(cx, schema);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let history = history_sql(&workspace, cx);

        let over = cx
            .debug_bounds("row-number-3")
            .expect("a row of the grid is drawn")
            .center();
        let offset = |grid: &Grid, cx: &mut VisualTestContext| {
            grid.read_with(cx, |table, _| {
                table
                    .vertical_scroll_handle
                    .0
                    .borrow()
                    .base_handle
                    .offset()
                    .y
            })
        };
        let top = offset(&grid, cx);
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: over,
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(-600.))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        settle(cx);
        assert!(offset(&grid, cx) < top, "the wheel did not scroll the grid");
        cx.deactivate_window();
        settle(cx);

        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("bird1")
        );
        assert_eq!(pending_of(&grid, cx), 1);
        assert_eq!(ring_of(&grid, cx), Some((0, 1)));
        assert_eq!(history_sql(&workspace, cx), history);
        assert_eq!(shown_notice(&workspace, cx), None);
        quit(workspace, cx);
    });
}

/// The refusal a `NOT NULL` column gives a staged `NULL`, which the server
/// sends back whole.
fn refused_row(grid: &Grid, cx: &mut VisualTestContext) -> Option<(usize, String)> {
    grid.read_with(cx, |grid, _| {
        grid.delegate()
            .send_failure()
            .map(|(row, message)| (row, message.to_string()))
    })
}

#[gpui::test]
fn a_failed_row_send_keeps_the_cursor_and_the_edits(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Null);
        press(cx, "down");

        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        assert_eq!(ring_of(&grid, cx), Some((0, 1)), "the ring is back");
        assert_eq!(pending_of(&grid, cx), 1, "the edit is still staged");
        let (row, message) = refused_row(&grid, cx).expect("the row is marked");
        assert_eq!(row, 0);
        assert!(message.contains("NOT NULL"), "{message}");
        let notice = shown_notice(&workspace, cx).expect("the status line says why");
        assert!(notice.contains("NOT NULL"), "{notice}");
        assert_ne!(state_kind(&workspace, cx), "failed");

        // Fixed, the row goes, and the mark with it.
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        assert_eq!(refused_row(&grid, cx), None, "editing the row clears it");
        press(cx, "down");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(refused_row(&grid, cx), None);
        assert_eq!(ring_of(&grid, cx).map(|(row, _)| row), Some(1));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_failed_row_send_does_not_pull_the_cursor_back_from_a_later_row(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Null);

        // On to row 2 before the refusal comes back.
        grid.update(cx, |table, cx| table.set_selected_row(1, cx));
        grid.update(cx, |table, cx| table.set_selected_row(2, cx));
        settle(cx);

        assert_eq!(ring_of(&grid, cx).map(|(row, _)| row), Some(2));
        assert_eq!(pending_of(&grid, cx), 1);
        let (row, message) = refused_row(&grid, cx).expect("the row is still marked");
        assert_eq!(row, 0);
        assert!(message.contains("NOT NULL"), "{message}");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn escape_reverts_the_dirty_row(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Null);
        press(cx, "down");
        assert!(refused_row(&grid, cx).is_some());
        stage(&grid, cx, 2, 1, NewValue::Value("owl".into()));
        assert_eq!(pending_of(&grid, cx), 2);

        press(cx, "escape");
        assert_eq!(refused_row(&grid, cx), None);
        assert_eq!(pending_of(&grid, cx), 1, "row 2's edit stays");
        grid.read_with(cx, |grid, _| {
            assert!(!grid.delegate().row_has_pending(0));
            assert!(grid.delegate().row_has_pending(2));
        });
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        assert_eq!(ring_of(&grid, cx).map(|(row, _)| row), Some(0));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_refused_rows_mark_and_escape_follow_the_row_into_a_client_sort(cx: &mut TestAppContext) {
    use crate::{actions::SortColumn, result_grid::NewValue};

    store::tests::with_home(|| {
        // Byte order puts a capital ahead of every lowercase letter and the
        // client's sort does not, so entering memory moves `Wren` down a row.
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text not null, n integer); \
             insert into birds values (1, 'Wren', 1), (2, 'tit', 2), (3, 'Kite', 3);",
        );
        let (_, grid) = open_in_row_mode(&workspace, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.sort_column(&SortColumn { column: 1 }, window, cx)
        });
        settle_through(cx);
        assert_eq!(cell_of(&grid, cx, 1, 1).as_deref(), Some("Wren"));

        focus_grid(&grid, cx);
        ring_on(&grid, cx, 1, 1);
        stage(&grid, cx, 1, 1, NewValue::Null);
        press(cx, "down");
        assert_eq!(refused_row(&grid, cx).map(|(row, _)| row), Some(1));

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_view_sorting(true, window, cx)
        });
        settle(cx);
        assert_eq!(
            cell_of(&grid, cx, 2, 1).as_deref(),
            Some("Wren"),
            "it moved"
        );
        assert_eq!(
            refused_row(&grid, cx).map(|(row, _)| row),
            Some(2),
            "the mark is on the row the server refused"
        );
        assert_eq!(pending_of(&grid, cx), 1);

        // Esc on the moved row puts that row back, and no other.
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 2, 1);
        press(cx, "escape");
        assert_eq!(refused_row(&grid, cx), None);
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("Wren")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn escape_reverts_nothing_in_a_draft_view(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        press(cx, "escape");
        assert_eq!(pending_of(&grid, cx), 1);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_row_send_under_read_only_raises_the_mode_prompt_and_keeps_the_edits(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let prompt = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace.profile().unwrap().session.pending_run.is_some()
            })
        };

        // Lowering the mode sends nothing and keeps the row.
        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::ReadOnly, cx)
        });
        settle(cx);
        assert_eq!(pending_of(&grid, cx), 1);
        assert!(!prompt(&workspace, cx));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );

        // Leaving it asks, and the edit and the ring stay where they were.
        press(cx, "down");
        assert!(prompt(&workspace, cx), "the mode prompt is up");
        assert_eq!(ring_of(&grid, cx).map(|(row, _)| row), Some(0));
        assert_eq!(pending_of(&grid, cx), 1);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );

        workspace.update(cx, |workspace, cx| workspace.cancel_pending_run(cx));
        settle(cx);
        assert_eq!(pending_of(&grid, cx), 1, "cancelling leaves the row staged");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_second_row_left_while_the_first_is_saving_stays_staged(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        stage(&grid, cx, 1, 1, NewValue::Value("jay".into()));

        // One turn: row 0 is left and its send is out when row 1 is left.
        grid.update(cx, |table, cx| table.set_selected_row(1, cx));
        grid.update(cx, |table, cx| table.set_selected_row(2, cx));
        assert_eq!(state_kind(&workspace, cx), "running");
        grid.read_with(cx, |grid, _| {
            assert!(grid.delegate().row_has_pending(1), "row 1 was not dropped");
        });
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("The previous row is still being saved.")
        );

        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("tit")
        );
        grid.read_with(cx, |grid, _| {
            assert!(!grid.delegate().row_has_pending(0));
            assert!(grid.delegate().row_has_pending(1));
        });

        save(&workspace, cx);
        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("jay")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_edit_typed_while_its_row_is_saving_stays_staged(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        grid.update(cx, |table, cx| table.set_selected_row(1, cx));
        assert_eq!(state_kind(&workspace, cx), "running");
        stage(&grid, cx, 0, 2, NewValue::Value("9".into()));

        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(cell_of(&grid, cx, 0, 1).as_deref(), Some("robin"));
        assert_eq!(pending_of(&grid, cx), 1, "the edit made mid-send stays");
        assert_eq!(
            on_disk("select n from birds where id = 1").as_deref(),
            Some("1")
        );

        save(&workspace, cx);
        assert_eq!(
            on_disk("select n from birds where id = 1").as_deref(),
            Some("9")
        );
        assert_eq!(pending_of(&grid, cx), 0);
        quit(workspace, cx);
    });
}

/// A click on the `id` cell of drawn row `row`, which is where the pointer
/// reaches the cell's own mouse-down listener.
fn click_cell_of_row(cx: &mut VisualTestContext, row: usize) {
    let selector: &'static str = Box::leak(format!("row-number-{row}").into_boxed_str());
    let gutter = cx.debug_bounds(selector).expect("the row is drawn");
    let at = gpui::point(gutter.right() + gpui::px(40.), gutter.center().y);
    cx.simulate_click(at, gpui::Modifiers::default());
    settle(cx);
}

#[gpui::test]
fn a_cell_being_typed_in_is_kept_when_its_row_is_clicked_away_from_in_row_mode(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("robin");
        settle(cx);
        assert_eq!(pending_of(&grid, cx), 0, "typed, never committed");

        click_cell_of_row(cx, 1);
        assert_eq!(ring_of(&grid, cx).map(|(row, _)| row), Some(1));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(pending_of(&grid, cx), 0);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_cell_being_typed_in_is_dropped_when_its_row_is_clicked_away_from_in_draft_mode(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("robin");
        settle(cx);

        click_cell_of_row(cx, 1);
        assert_eq!(ring_of(&grid, cx).map(|(row, _)| row), Some(1));
        assert_eq!(pending_of(&grid, cx), 0, "Draft stages nothing");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_refused_typed_edit_carried_down_returns_with_the_keyboard_and_the_mark_drawn(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let schema = "create table birds (id integer primary key, \
             name text not null check (name <> 'bad'), n integer); \
             insert into birds values (1, 'wren', 1), (2, 'tit', 2), (3, 'kite', 3);";
        let (workspace, cx) = launch_on_sqlite_holding(cx, schema);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("bad");
        settle(cx);
        // Down commits the text and opens the next row's cell, which the
        // refusal then puts away with the ring.
        press(cx, "down");
        settle(cx);

        assert_eq!(ring_of(&grid, cx), Some((0, 1)));
        assert_eq!(pending_of(&grid, cx), 1);
        assert!(refused_row(&grid, cx).is_some());
        assert!(
            cx.debug_bounds("row-send-failed-0").is_some(),
            "the refused row's number is drawn marked"
        );
        assert!(grid_holds_keyboard(&grid, cx));

        // Esc, with the keyboard back on the grid, puts the row back.
        press(cx, "escape");
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(refused_row(&grid, cx), None);
        assert!(cx.debug_bounds("row-send-failed-0").is_none());
        quit(workspace, cx);
    });
}

fn grid_holds_keyboard(grid: &Grid, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| {
        use gpui::Focusable;
        grid.focus_handle(cx).is_focused(window)
    })
}

const FIVE_BIRDS: &str = "create table birds (id integer primary key, name text not null); \
     insert into birds values (1, 'wren'), (2, 'tit'), (3, 'kite'), (4, 'owl'), (5, 'jay');";

/// What relation tab `id` would read next if asked again: its filter, sort,
/// row limit and page offset, and whether its rows are owed a refresh.
fn relation_view(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: u64,
) -> (String, Vec<crate::sql::SortKey>, Option<usize>, usize, bool) {
    workspace.read_with(cx, |workspace, _| {
        let tab = workspace
            .profile()
            .and_then(|profile| profile.session.objects.iter().find(|tab| tab.id == id))
            .expect("the relation tab is open");
        let ObjectBody::Relation {
            filter,
            sort,
            limit,
            offset,
            stale,
            ..
        } = &tab.body
        else {
            panic!("the tab is a relation");
        };
        (filter.clone(), sort.clone(), *limit, *offset, *stale)
    })
}

/// The action the discard prompt holds, if it is up.
fn discard_holds(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<crate::session::AfterEdits> {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .profile()?
            .session
            .pending_discard
            .as_ref()
            .map(|discard| discard.then.clone())
    })
}

/// Discard, as the prompt's button does it.
fn confirm_discard(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.confirm_discard_close(window, cx)
    });
    settle(cx);
}

/// `rows` of the first column of the grid, as it shows them.
fn first_column(grid: &Grid, cx: &mut VisualTestContext) -> Vec<String> {
    grid.read_with(cx, |grid, _| {
        let grid = grid.delegate();
        (0..grid.result().rows.len())
            .filter_map(|row| grid.cell(row, 0).map(str::to_string))
            .collect()
    })
}

#[gpui::test]
fn a_page_turn_over_a_draft_asks_first(cx: &mut TestAppContext) {
    use crate::{
        actions::{NextPage, SetRowLimit, SortColumn},
        result_grid::NewValue,
        session::AfterEdits,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, FIVE_BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        cx.dispatch_action(SetRowLimit { rows: Some(2) });
        settle(cx);
        let grid = relation_grid(&workspace, cx, id);
        assert_eq!(first_column(&grid, cx), ["1", "2"]);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let before = relation_view(&workspace, cx, id);

        cx.dispatch_action(NextPage);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        assert!(matches!(
            discard_holds(&workspace, cx),
            Some(AfterEdits::Requery { offset: 2, .. })
        ));
        assert_eq!(
            relation_view(&workspace, cx, id),
            before,
            "the tab still claims the page it shows"
        );
        assert_eq!(first_column(&grid, cx), ["1", "2"]);
        assert_eq!(pending_of(&grid, cx), 1);
        assert_eq!(state_kind(&workspace, cx), "complete", "nothing ran");

        // Cancel leaves it all as it was.
        press(cx, "escape");
        assert!(discard_holds(&workspace, cx).is_none());
        assert_eq!(relation_view(&workspace, cx, id), before);
        assert_eq!(pending_of(&grid, cx), 1);
        assert_eq!(first_column(&grid, cx), ["1", "2"]);

        // Discard drops the edit and turns the page it was raised for.
        cx.dispatch_action(NextPage);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        confirm_discard(&workspace, cx);
        assert!(discard_holds(&workspace, cx).is_none());
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(relation_view(&workspace, cx, id).3, 2);
        assert_eq!(first_column(&grid, cx), ["3", "4"]);
        // Answered twice, it acts once.
        confirm_discard(&workspace, cx);
        assert_eq!(relation_view(&workspace, cx, id).3, 2);
        assert_eq!(first_column(&grid, cx), ["3", "4"]);

        // A header sort and a row limit change ask as a page turn does.
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let before = relation_view(&workspace, cx, id);
        cx.dispatch_action(SortColumn { column: 1 });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        assert_eq!(relation_view(&workspace, cx, id), before);
        press(cx, "escape");
        assert_eq!(relation_view(&workspace, cx, id), before);
        assert_eq!(pending_of(&grid, cx), 1);

        cx.dispatch_action(SetRowLimit { rows: Some(3) });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        assert_eq!(relation_view(&workspace, cx, id), before);
        press(cx, "escape");
        assert_eq!(relation_view(&workspace, cx, id), before);
        assert_eq!(pending_of(&grid, cx), 1);
        assert_eq!(first_column(&grid, cx), ["3", "4"]);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn closing_a_tab_with_edits_asks_and_discard_closes_it(cx: &mut TestAppContext) {
    use crate::{actions::CloseTab, result_grid::NewValue, session::AfterEdits};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        cx.simulate_input("select * from birds");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        let (tab_id, grid) = query_tab(&workspace, cx, |tab, _, _| (tab.id, tab.results.clone()));
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));

        cx.dispatch_action(CloseTab);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        assert!(matches!(
            discard_holds(&workspace, cx),
            Some(AfterEdits::Close { deleting: None })
        ));
        let title = discard_holds(&workspace, cx).unwrap().discards().0;
        assert_eq!(title, "Close tab");

        press(cx, "escape");
        assert!(discard_holds(&workspace, cx).is_none());
        assert_eq!(pending_of(&grid, cx), 1, "the edit is kept");
        let open = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profile()
                    .unwrap()
                    .session
                    .query_tab(tab_id)
                    .is_some()
            })
        };
        assert!(open(cx), "the tab is kept");

        cx.dispatch_action(CloseTab);
        settle(cx);
        confirm_discard(&workspace, cx);
        assert!(!open(cx), "Discard closes the tab");
        quit(workspace, cx);
    });
}

fn sorting_of(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    tab: Tab,
) -> crate::session::Sorting {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .profile()
            .unwrap()
            .session
            .sorting(tab)
            .cloned()
            .expect("the tab has a sorting")
    })
}

/// Relation tab `id`'s row count, as `Some(filter, rows)` once counted.
fn counted(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: u64,
) -> Option<(String, u64)> {
    workspace.read_with(cx, |workspace, _| {
        let tab = workspace
            .profile()?
            .session
            .objects
            .iter()
            .find(|tab| tab.id == id)?;
        match &tab.body {
            ObjectBody::Relation {
                count: crate::session::RowCount::Counted(filter, rows),
                ..
            } => Some((filter.clone(), *rows)),
            _ => None,
        }
    })
}

fn set_counted(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, id: u64) {
    workspace.update(cx, |workspace, _| {
        let tab = workspace
            .profile_mut()
            .unwrap()
            .session
            .objects
            .iter_mut()
            .find(|tab| tab.id == id)
            .unwrap();
        let ObjectBody::Relation { count, .. } = &mut tab.body else {
            panic!("a relation tab");
        };
        *count = crate::session::RowCount::Counted(String::new(), 2);
    });
}

/// The bars of relation tab `id`: operator, joiner and the value each last
/// applied.
fn bars_of(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: u64,
) -> Vec<(crate::filter::Operator, crate::filter::Conjunction, String)> {
    workspace.update(cx, |workspace, _| {
        workspace
            .filter_rows_mut(id)
            .unwrap()
            .iter()
            .map(|row| (row.operator, row.conjunction, row.applied.clone()))
            .collect()
    })
}

#[gpui::test]
fn a_refresh_sort_filter_or_row_limit_over_a_draft_asks_first(cx: &mut TestAppContext) {
    use crate::{
        actions::{RefreshRelation, RunQuery, SortColumn},
        result_grid::NewValue,
        session::{AfterEdits, Sorting},
    };

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        let tab = Tab::Object(id);
        set_counted(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let before = relation_view(&workspace, cx, id);
        let sorting = sorting_of(&workspace, cx, tab);

        // Refresh, by its action and by Run on the object tab.
        for run in [false, true] {
            if run {
                cx.dispatch_action(RunQuery);
            } else {
                cx.dispatch_action(RefreshRelation);
            }
            settle(cx);
            assert_eq!(front_dialog(&workspace, cx), Some("discard"));
            assert!(matches!(
                discard_holds(&workspace, cx),
                Some(AfterEdits::Refresh)
            ));
            assert_eq!(relation_view(&workspace, cx, id), before);
            assert_eq!(counted(&workspace, cx, id), Some((String::new(), 2)));
            assert_eq!(sorting_of(&workspace, cx, tab), sorting);
            assert_eq!(pending_of(&grid, cx), 1);
            assert_eq!(state_kind(&workspace, cx), "complete");
            press(cx, "escape");
            assert_eq!(pending_of(&grid, cx), 1);
        }
        // Discard re-reads: a row changed on disk since is what lands.
        rusqlite::Connection::open(store::dbdelve_directory().unwrap().join("e2e.db"))
            .and_then(|db| db.execute("update birds set name = 'finch' where id = 1", []))
            .unwrap();
        cx.dispatch_action(RefreshRelation);
        settle(cx);
        confirm_discard(&workspace, cx);
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(cell_of(&grid, cx, 0, 1).as_deref(), Some("finch"));
        assert_eq!(
            counted(&workspace, cx, id),
            None,
            "the count is asked again"
        );

        // The view's sorting switched to the server, with a sort on screen.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_view_sorting(true, window, cx)
        });
        cx.dispatch_action(SortColumn { column: 1 });
        settle(cx);
        let held = sorting_of(&workspace, cx, tab);
        assert!(matches!(&held, Sorting::Client(keys) if keys.len() == 1));
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let before = relation_view(&workspace, cx, id);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_view_sorting(false, window, cx)
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        assert!(matches!(
            discard_holds(&workspace, cx),
            Some(AfterEdits::ViewSorting(false))
        ));
        assert_eq!(sorting_of(&workspace, cx, tab), held);
        assert_eq!(relation_view(&workspace, cx, id), before);
        assert_eq!(pending_of(&grid, cx), 1);
        press(cx, "escape");
        assert_eq!(sorting_of(&workspace, cx, tab), held);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_view_sorting(false, window, cx)
        });
        settle(cx);
        confirm_discard(&workspace, cx);
        assert_eq!(sorting_of(&workspace, cx, tab), Sorting::Server);
        assert_eq!(
            relation_view(&workspace, cx, id).1.len(),
            1,
            "the sort reached the server"
        );
        assert_eq!(pending_of(&grid, cx), 0);

        // A filter bar submitted with a value.
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let before = relation_view(&workspace, cx, id);
        let input = workspace.update_in(cx, |workspace, window, cx| {
            workspace.push_filter_row(id, window, cx);
            let input = workspace.filter_rows_mut(id).unwrap()[0].value.clone();
            input.update(cx, |input, cx| input.set_value("id = 2", window, cx));
            input
        });
        workspace.update(cx, |workspace, cx| workspace.submit_filter(id, &input, cx));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        assert!(matches!(
            discard_holds(&workspace, cx),
            Some(AfterEdits::Filter { .. })
        ));
        assert_eq!(relation_view(&workspace, cx, id), before);
        assert_eq!(
            grid.read_with(cx, |g, _| g.delegate().result().rows.len()),
            2
        );
        confirm_discard(&workspace, cx);
        assert_eq!(relation_view(&workspace, cx, id).0, "id = 2");
        assert_eq!(
            grid.read_with(cx, |g, _| g.delegate().result().rows.len()),
            1
        );
        assert_eq!(pending_of(&grid, cx), 0);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn cancelling_the_discard_prompt_puts_a_changed_filter_bar_back(cx: &mut TestAppContext) {
    use crate::{
        filter::{Conjunction, FilterBar, Operator},
        result_grid::NewValue,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let bar = FilterBar {
            column: Some("name".into()),
            operator: Operator::Equals,
            value: "wren".into(),
            ..Default::default()
        };
        let id = workspace.update_in(cx, |workspace, window, cx| {
            let opened = crate::session::OpenedObject::Relation {
                schema: "main".into(),
                name: "birds".into(),
                kind: Default::default(),
                filter: crate::filter::derived_filter(
                    Engine::Sqlite,
                    std::slice::from_ref(&bar),
                    &[],
                ),
                filters: vec![bar.clone()],
            };
            let id = workspace
                .open_object(opened, window, cx)
                .expect("the relation opens");
            workspace.activate_tab(Tab::Object(id), window, cx);
            id
        });
        settle(cx);
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let bars = bars_of(&workspace, cx, id);
        let view = relation_view(&workspace, cx, id);
        assert_eq!(bars, [(Operator::Equals, Conjunction::And, "wren".into())]);

        workspace.update_in(cx, |workspace, window, cx| {
            let action = crate::actions::SetFilterOperator {
                row: 0,
                operator: Operator::NotEquals,
            };
            workspace.set_filter_operator(&action, window, cx);
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        assert_eq!(relation_view(&workspace, cx, id), view);
        press(cx, "escape");
        assert_eq!(
            bars_of(&workspace, cx, id),
            bars,
            "the operator is put back"
        );
        assert_eq!(relation_view(&workspace, cx, id), view);
        assert_eq!(pending_of(&grid, cx), 1);

        // A value typed into the bar and sent.
        let input = workspace.update_in(cx, |workspace, window, cx| {
            let input = workspace.filter_rows_mut(id).unwrap()[0].value.clone();
            input.update(cx, |input, cx| input.set_value("tit", window, cx));
            input
        });
        workspace.update(cx, |workspace, cx| workspace.submit_filter(id, &input, cx));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        press(cx, "escape");
        assert_eq!(
            bars_of(&workspace, cx, id),
            bars,
            "the applied value is put back"
        );
        assert_eq!(relation_view(&workspace, cx, id), view);
        assert_eq!(pending_of(&grid, cx), 1);

        // Clearing the bars.
        cx.dispatch_action(crate::actions::ClearFilter);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        press(cx, "escape");
        assert_eq!(
            bars_of(&workspace, cx, id),
            bars,
            "the cleared bar is put back"
        );
        assert_eq!(relation_view(&workspace, cx, id), view);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_run_or_a_header_sort_on_a_query_tab_over_a_draft_asks_first(cx: &mut TestAppContext) {
    use crate::{
        actions::{RunQuery, SortColumn},
        result_grid::NewValue,
        session::AfterEdits,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        cx.simulate_input("select * from birds order by id");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let text = buffer(&workspace, cx);

        cx.dispatch_action(RunQuery);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        assert!(matches!(
            discard_holds(&workspace, cx),
            Some(AfterEdits::Run)
        ));
        assert_eq!(buffer(&workspace, cx), text);
        assert_eq!(pending_of(&grid, cx), 1);
        assert_eq!(state_kind(&workspace, cx), "complete");
        press(cx, "escape");
        assert_eq!(pending_of(&grid, cx), 1, "Cancel keeps the edit");
        assert_eq!(buffer(&workspace, cx), text);

        cx.dispatch_action(SortColumn { column: 1 });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        assert!(matches!(
            discard_holds(&workspace, cx),
            Some(AfterEdits::Sort(1))
        ));
        assert_eq!(buffer(&workspace, cx), text, "the buffer is not rewritten");
        assert_eq!(pending_of(&grid, cx), 1);
        press(cx, "escape");
        assert_eq!(buffer(&workspace, cx), text);

        cx.dispatch_action(SortColumn { column: 1 });
        settle(cx);
        confirm_discard(&workspace, cx);
        let sorted = buffer(&workspace, cx);
        assert!(sorted.to_lowercase().contains("name"), "{sorted}");
        assert_ne!(sorted, text);
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(state_kind(&workspace, cx), "complete");

        // Run again, and Discard runs it.
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        cx.dispatch_action(RunQuery);
        settle(cx);
        confirm_discard(&workspace, cx);
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(state_kind(&workspace, cx), "complete");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_requery_that_changes_nothing_over_a_draft_asks_nothing(cx: &mut TestAppContext) {
    use crate::{
        actions::{PreviousPage, SetRowLimit, SortColumn},
        result_grid::NewValue,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        let limit = relation_view(&workspace, cx, id).2;
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_view_sorting(true, window, cx)
        });
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let before = relation_view(&workspace, cx, id);

        cx.dispatch_action(SetRowLimit { rows: limit });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None, "the limit in force");
        cx.dispatch_action(PreviousPage);
        settle(cx);
        assert_eq!(
            front_dialog(&workspace, cx),
            None,
            "no page before the first"
        );

        // Sorted in memory: the edit moves with its row.
        cx.dispatch_action(SortColumn { column: 1 });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        assert_eq!(relation_view(&workspace, cx, id), before);
        assert_eq!(pending_of(&grid, cx), 1);
        grid.read_with(cx, |grid, _| {
            let grid = grid.delegate();
            let staged = (0..2)
                .find(|&row| grid.pending_value(row, 1).is_some())
                .expect("the edit is on a row");
            assert_eq!(grid.cell(staged, 0), Some("1"), "the row it was staged on");
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_requery_while_the_tabs_edits_are_being_sent_is_not_run(cx: &mut TestAppContext) {
    use crate::{
        actions::{ApplyEdits, SetRowLimit},
        result_grid::NewValue,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        cx.dispatch_action(ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        assert_eq!(state_kind(&workspace, cx), "running");
        let before = relation_view(&workspace, cx, id);

        // Not through the action, which runs the executor to the answer.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_row_limit(&SetRowLimit { rows: Some(1) }, window, cx)
        });
        // The apply's own review is still up until the send lands.
        assert_ne!(front_dialog(&workspace, cx), Some("discard"), "no prompt");
        assert!(discard_holds(&workspace, cx).is_none(), "nothing is parked");
        assert_eq!(relation_view(&workspace, cx, id), before);
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("This tab's edits are still being sent.")
        );

        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(state_kind(&workspace, cx), "complete");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_run_that_reaches_a_grid_with_edits_unguarded_is_not_sent(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        cx.simulate_input("select * from birds");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        let (tab_id, grid) = query_tab(&workspace, cx, |tab, _, _| (tab.id, tab.results.clone()));
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let rows = grid.read_with(cx, |grid, _| grid.delegate().result().rows.len());

        workspace.update(cx, |workspace, cx| {
            workspace.execute_sql("select 1".into(), Tab::Query(tab_id), cx)
        });
        settle(cx);
        assert_eq!(pending_of(&grid, cx), 1);
        assert_eq!(
            grid.read_with(cx, |grid, _| grid.delegate().result().rows.len()),
            rows
        );
        assert_eq!(state_kind(&workspace, cx), "complete");
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("The statement was not run: this tab has unapplied edits.")
        );

        // A question about the statement is not a replacement of the rows.
        cx.simulate_keystrokes("secondary-shift-enter");
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| {
            assert!(tab.plan.is_some(), "the Explain ran");
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn lowering_to_read_only_keeps_the_draft_and_an_apply_raises_the_mode_prompt(
    cx: &mut TestAppContext,
) {
    use crate::{actions::ApplyEdits, result_grid::NewValue, sql::Mode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));

        workspace.update(cx, |workspace, cx| workspace.set_mode(Mode::ReadOnly, cx));
        settle(cx);
        assert_eq!(pending_of(&grid, cx), 1, "the draft is kept");
        grid.update(cx, |grid, _| {
            assert!(
                !grid
                    .delegate_mut()
                    .set_pending(1, 1, NewValue::Value("owl".into())),
                "no further staging"
            );
        });

        cx.dispatch_action(ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().unwrap().session.pending_run.is_some());
        });
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 2);
            assert_eq!(grid.delegate().pending_count(), 1);
        });
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );

        // Cancelling the prompt leaves the draft staged.
        workspace.update(cx, |workspace, cx| workspace.cancel_pending_run(cx));
        settle(cx);
        assert_eq!(pending_of(&grid, cx), 1);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_stale_tab_with_edits_is_not_reloaded_on_a_visit(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        workspace.update(cx, |workspace, _| {
            let tab = workspace
                .profile_mut()
                .unwrap()
                .session
                .objects
                .iter_mut()
                .find(|tab| tab.id == id)
                .unwrap();
            let ObjectBody::Relation { stale, .. } = &mut tab.body else {
                panic!("a relation tab");
            };
            *stale = true;
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_tab(Tab::Object(id), window, cx)
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None, "no prompt");
        assert!(discard_holds(&workspace, cx).is_none());
        assert!(relation_view(&workspace, cx, id).4, "still owed its reload");
        assert_eq!(pending_of(&grid, cx), 1);
        assert_eq!(cell_of(&grid, cx, 0, 1).as_deref(), Some("wren"));
        quit(workspace, cx);
    });
}

/// What the tab's parked run, if it is a Row send, would resume once it runs:
/// `None` for no parked Row send, `Some(None)` for one that resumes nothing.
fn parked_row_send_action(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<Option<crate::session::AfterEdits>> {
    workspace.read_with(cx, |workspace, _| {
        let resume = workspace
            .profile()?
            .session
            .pending_run
            .as_ref()?
            .resume
            .as_ref()?;
        match resume.refresh.as_ref()? {
            crate::session::Refresh::Row { then, .. } => Some(then.clone()),
            _ => None,
        }
    })
}

fn resume_after_of(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<crate::session::Tab> {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .profile()?
            .session
            .resume_after
            .as_ref()
            .map(|(tab, _)| *tab)
    })
}

#[gpui::test]
fn a_page_turn_in_row_mode_saves_the_dirty_row_first(cx: &mut TestAppContext) {
    use crate::{
        actions::{NextPage, PreviousPage, SetRowLimit},
        result_grid::NewValue,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, FIVE_BIRDS);
        let (id, grid) = open_in_row_mode(&workspace, cx);
        cx.dispatch_action(SetRowLimit { rows: Some(2) });
        settle(cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        assert_eq!(first_column(&grid, cx), ["1", "2"]);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));

        // The row goes first and no prompt is raised over it.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.next_page(&NextPage, window, cx)
        });
        assert!(discard_holds(&workspace, cx).is_none(), "no prompt");
        assert_eq!(front_dialog(&workspace, cx), None);
        assert_eq!(state_kind(&workspace, cx), "running", "the row is sent");
        assert_eq!(relation_view(&workspace, cx, id).3, 0, "not turned yet");
        settle(cx);
        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(
            relation_view(&workspace, cx, id).3,
            2,
            "then the page turns"
        );
        assert_eq!(first_column(&grid, cx), ["3", "4"]);
        assert_eq!(pending_of(&grid, cx), 0);
        assert!(
            history_sql(&workspace, cx)[0].starts_with("UPDATE"),
            "the row's UPDATE is the newest entry"
        );
        assert_eq!(resume_after_of(&workspace, cx), None);

        // Back on page 1, a row the server refuses keeps the page.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.previous_page(&PreviousPage, window, cx)
        });
        settle(cx);
        assert_eq!(relation_view(&workspace, cx, id).3, 0);
        assert_eq!(first_column(&grid, cx), ["1", "2"]);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Null);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.next_page(&NextPage, window, cx)
        });
        settle(cx);
        settle(cx);
        assert_eq!(relation_view(&workspace, cx, id).3, 0, "the page stays");
        assert_eq!(first_column(&grid, cx), ["1", "2"]);
        assert_eq!(pending_of(&grid, cx), 1, "the edit is kept");
        assert!(refused_row(&grid, cx).is_some_and(|(row, _)| row == 0));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(resume_after_of(&workspace, cx), None);
        assert_eq!(discard_holds(&workspace, cx).map(|_| ()), None);

        // A send the mode stops resumes nothing, and neither Cancel nor a later
        // Run turns the page.
        grid.update(cx, |table, cx| {
            table.delegate_mut().discard_pending();
            table.refresh(cx);
        });
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::ReadOnly, cx)
        });
        settle(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.next_page(&NextPage, window, cx)
        });
        settle(cx);
        assert!(
            matches!(parked_row_send_action(&workspace, cx), Some(None)),
            "the parked send carries no action"
        );
        assert_eq!(relation_view(&workspace, cx, id).3, 0);
        assert_eq!(pending_of(&grid, cx), 1);
        workspace.update(cx, |workspace, cx| workspace.cancel_pending_run(cx));
        settle(cx);
        assert_eq!(relation_view(&workspace, cx, id).3, 0);
        assert_eq!(first_column(&grid, cx), ["1", "2"]);
        assert_eq!(pending_of(&grid, cx), 1);
        assert_eq!(resume_after_of(&workspace, cx), None);
        quit(workspace, cx);
    });
}

/// Settles through a landed send and what its release starts, which takes a
/// frame to reach render and another round of the executor to run.
fn settle_through(cx: &mut VisualTestContext) {
    for _ in 0..3 {
        settle(cx);
    }
}

/// No prompt of any kind is up over the tab.
fn assert_no_discard_prompt(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    assert!(discard_holds(workspace, cx).is_none(), "no discard prompt");
    assert_ne!(front_dialog(workspace, cx), Some("discard"));
}

const SIX_BIRDS: &str = "create table birds (id integer primary key, name text not null); \
     insert into birds values (1, 'wren'), (2, 'tit'), (3, 'kite'), (4, 'owl'), (5, 'jay'), \
     (6, 'crow');";

#[gpui::test]
fn row_mode_saves_the_dirty_row_before_a_sort_a_filter_a_refresh_or_a_row_limit(
    cx: &mut TestAppContext,
) {
    use crate::{
        actions::{RefreshRelation, SetRowLimit, SortColumn},
        result_grid::NewValue,
        session::Sorting,
    };

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, SIX_BIRDS);
        let (id, grid) = open_in_row_mode(&workspace, cx);
        let edit_first_row = |name: &str, cx: &mut VisualTestContext| -> String {
            let key = cell_of(&grid, cx, 0, 0).expect("a first row");
            stage(&grid, cx, 0, 1, NewValue::Value(name.into()));
            key
        };
        let name_of = |key: &str| on_disk(&format!("select name from birds where id = {key}"));

        // A header click on a view the server sorts, as a new tab's is.
        assert_eq!(sorting_of(&workspace, cx, Tab::Object(id)), Sorting::Server);
        let key = edit_first_row("alpha", cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.sort_column(&SortColumn { column: 1 }, window, cx)
        });
        assert_eq!(state_kind(&workspace, cx), "running", "the row goes first");
        settle_through(cx);
        assert_no_discard_prompt(&workspace, cx);
        assert_eq!(name_of(&key).as_deref(), Some("alpha"));
        assert_eq!(
            relation_view(&workspace, cx, id).1.len(),
            1,
            "then it sorts"
        );
        assert_eq!(pending_of(&grid, cx), 0);

        // A header click on a view sorted in memory.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_view_sorting(true, window, cx)
        });
        settle(cx);
        let held = sorting_of(&workspace, cx, Tab::Object(id));
        let key = edit_first_row("bravo", cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.sort_column(&SortColumn { column: 0 }, window, cx)
        });
        assert_eq!(state_kind(&workspace, cx), "running", "the row goes first");
        settle_through(cx);
        assert_no_discard_prompt(&workspace, cx);
        assert_eq!(name_of(&key).as_deref(), Some("bravo"));
        assert_ne!(sorting_of(&workspace, cx, Tab::Object(id)), held);
        assert!(matches!(
            sorting_of(&workspace, cx, Tab::Object(id)),
            Sorting::Client(keys) if keys.len() == 2
        ));
        assert_eq!(pending_of(&grid, cx), 0);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_view_sorting(false, window, cx)
        });
        settle_through(cx);

        // A filter bar submitted.
        let key = edit_first_row("charlie", cx);
        let input = workspace.update_in(cx, |workspace, window, cx| {
            workspace.push_filter_row(id, window, cx);
            let input = workspace.filter_rows_mut(id).unwrap()[0].value.clone();
            input.update(cx, |input, cx| input.set_value("id > 2", window, cx));
            input
        });
        workspace.update(cx, |workspace, cx| workspace.submit_filter(id, &input, cx));
        settle_through(cx);
        assert_no_discard_prompt(&workspace, cx);
        assert_eq!(name_of(&key).as_deref(), Some("charlie"));
        assert_eq!(relation_view(&workspace, cx, id).0, "id > 2");
        assert_eq!(
            grid.read_with(cx, |g, _| g.delegate().result().rows.len()),
            4
        );
        assert_eq!(pending_of(&grid, cx), 0);

        // A refresh, which reads what is on the server now.
        let key = edit_first_row("delta", cx);
        rusqlite::Connection::open(store::dbdelve_directory().unwrap().join("e2e.db"))
            .and_then(|db| db.execute("update birds set name = 'finch' where id = 3", []))
            .unwrap();
        cx.dispatch_action(RefreshRelation);
        settle_through(cx);
        assert_no_discard_prompt(&workspace, cx);
        assert_eq!(name_of(&key).as_deref(), Some("delta"));
        let shown: Vec<_> = (0..4)
            .filter_map(|row| cell_of(&grid, cx, row, 1))
            .collect();
        assert!(shown.contains(&"finch".to_string()), "{shown:?}");
        assert!(shown.contains(&"delta".to_string()), "{shown:?}");
        assert_eq!(pending_of(&grid, cx), 0);

        // A row limit change.
        let key = edit_first_row("echo", cx);
        cx.dispatch_action(SetRowLimit { rows: Some(2) });
        settle_through(cx);
        assert_no_discard_prompt(&workspace, cx);
        assert_eq!(name_of(&key).as_deref(), Some("echo"));
        assert_eq!(relation_view(&workspace, cx, id).2, Some(2));
        assert_eq!(
            grid.read_with(cx, |g, _| g.delegate().result().rows.len()),
            2
        );
        assert_eq!(pending_of(&grid, cx), 0);

        // A refused row cancels the action and keeps the row.
        stage(&grid, cx, 0, 1, NewValue::Null);
        let before = relation_view(&workspace, cx, id);
        cx.dispatch_action(SetRowLimit { rows: Some(3) });
        settle_through(cx);
        assert_eq!(relation_view(&workspace, cx, id), before);
        assert_eq!(pending_of(&grid, cx), 1);
        assert!(refused_row(&grid, cx).is_some());
        assert_eq!(resume_after_of(&workspace, cx), None);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn switching_tabs_in_row_mode_saves_the_dirty_row_first(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let query = workspace.read_with(cx, |workspace, _| {
            Tab::Query(workspace.profile().unwrap().session.queries[0].id)
        });
        let (id, grid) = open_in_row_mode(&workspace, cx);
        let table = Tab::Object(id);
        let front = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace.profile().unwrap().session.active
            })
        };
        let switch = |tab: Tab, workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.activate_tab(tab, window, cx)
            })
        };

        // The row is sent, and the other tab comes forward when it lands.
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        switch(query, &workspace, cx);
        assert_eq!(front(&workspace, cx), table, "not yet");
        assert_eq!(state_kind(&workspace, cx), "running");
        settle_through(cx);
        assert_no_discard_prompt(&workspace, cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(front(&workspace, cx), query);
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(resume_after_of(&workspace, cx), None);

        // A row the server refuses keeps the tab in front.
        switch(table, &workspace, cx);
        settle(cx);
        stage(&grid, cx, 0, 1, NewValue::Null);
        switch(query, &workspace, cx);
        settle_through(cx);
        assert_eq!(front(&workspace, cx), table);
        assert_eq!(pending_of(&grid, cx), 1);
        assert!(refused_row(&grid, cx).is_some_and(|(row, _)| row == 0));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(resume_after_of(&workspace, cx), None);
        quit(workspace, cx);
    });
}

fn table_tab_is_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, id: u64) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .profile()
            .unwrap()
            .session
            .objects
            .iter()
            .any(|tab| tab.id == id)
    })
}

#[gpui::test]
fn closing_a_row_mode_tab_saves_its_dirty_row_first(cx: &mut TestAppContext) {
    use crate::{actions::CloseTab, result_grid::NewValue};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, grid) = open_in_row_mode(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.close_tab(&CloseTab, window, cx)
        });
        assert_no_discard_prompt(&workspace, cx);
        assert!(table_tab_is_open(&workspace, cx, id), "not before the save");
        settle_through(cx);
        assert_no_discard_prompt(&workspace, cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert!(!table_tab_is_open(&workspace, cx, id), "then it closes");

        // A row the server refuses keeps the tab open with the row marked.
        let (id, grid) = open_in_row_mode(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Null);
        cx.dispatch_action(CloseTab);
        settle_through(cx);
        assert_no_discard_prompt(&workspace, cx);
        assert!(table_tab_is_open(&workspace, cx, id));
        assert_eq!(pending_of(&grid, cx), 1);
        assert!(refused_row(&grid, cx).is_some());

        // Esc puts the row back, and the tab then closes without a send.
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        press(cx, "escape");
        assert_eq!(pending_of(&grid, cx), 0);
        cx.dispatch_action(CloseTab);
        settle_through(cx);
        assert!(!table_tab_is_open(&workspace, cx, id));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn row_to_draft_saves_the_dirty_row_first(cx: &mut TestAppContext) {
    use crate::{result_grid::NewValue, session::EditMode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);

        // A row the server refuses leaves the view in Row.
        stage(&grid, cx, 0, 1, NewValue::Null);
        pick_edit_mode(&workspace, cx, EditMode::Draft);
        settle_through(cx);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Row));
        assert_eq!(pending_of(&grid, cx), 1);
        assert!(refused_row(&grid, cx).is_some());
        assert_no_discard_prompt(&workspace, cx);
        grid.update(cx, |table, cx| {
            table.delegate_mut().discard_pending();
            table.refresh(cx);
        });

        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        pick_edit_mode(&workspace, cx, EditMode::Draft);
        settle_through(cx);
        assert_no_discard_prompt(&workspace, cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Draft));
        assert_eq!(pending_of(&grid, cx), 0);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_draft_view_keeps_its_edits_across_a_tab_switch(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let query = workspace.read_with(cx, |workspace, _| {
            Tab::Query(workspace.profile().unwrap().session.queries[0].id)
        });
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let history = history_sql(&workspace, cx).len();

        for tab in [query, Tab::Object(id)] {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.activate_tab(tab, window, cx)
            });
            settle_through(cx);
            assert_no_discard_prompt(&workspace, cx);
            assert_eq!(pending_of(&grid, cx), 1);
        }
        assert_eq!(history_sql(&workspace, cx).len(), history, "nothing sent");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_reread_owed_after_one_rows_send_sends_no_other_dirty_row_and_leaves_the_tab_stale(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text not null); \
             insert into birds values (1, 'wren'), (2, 'tit'), (3, 'kite'); \
             create trigger vanish after update on birds when new.name = 'ghost' \
             begin delete from birds where id = new.id; end;",
        );
        let (id, grid) = open_in_row_mode(&workspace, cx);

        // Row 0 is gone once it is sent, so it cannot be read back and the
        // relation is owed a read.
        stage(&grid, cx, 0, 1, NewValue::Value("ghost".into()));
        stage(&grid, cx, 1, 1, NewValue::Value("jay".into()));
        let history = history_sql(&workspace, cx).len();
        save(&workspace, cx);
        settle_through(cx);

        // Nothing but the row the user saved went to the server: reading the
        // relation again would have sent the other row with no act of theirs.
        assert_no_discard_prompt(&workspace, cx);
        assert_eq!(on_disk("select name from birds where id = 1"), None);
        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("tit"),
            "the other row was not sent"
        );
        assert_eq!(history_sql(&workspace, cx).len(), history + 1);
        assert_eq!(pending_of(&grid, cx), 1, "its edit is kept");
        grid.read_with(cx, |grid, _| assert!(grid.delegate().row_has_pending(1)));
        assert!(relation_is_stale(&workspace, cx, id), "the read is owed");
        assert_eq!(resume_after_of(&workspace, cx), None);
        let notice = shown_notice(&workspace, cx).expect("the status line says so");
        assert!(
            notice.contains("The rows were not read again: this tab has unapplied edits"),
            "{notice}"
        );
        assert!(table_tab_is_open(&workspace, cx, id));

        // The next act of the user's that saves the row sends it as usual.
        save(&workspace, cx);
        settle_through(cx);
        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("jay")
        );
        assert_eq!(pending_of(&grid, cx), 0);
        quit(workspace, cx);
    });
}

/// A Row view of `birds` whose first row the server has refused: `name` is
/// `NOT NULL`, the staged `NULL` was sent when the ring left the row, and the
/// ring is back on it.
fn refused_row_view(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (u64, Grid) {
    use crate::result_grid::NewValue;

    let (id, grid) = open_in_row_mode(workspace, cx);
    focus_grid(&grid, cx);
    ring_on(&grid, cx, 0, 1);
    stage(&grid, cx, 0, 1, NewValue::Null);
    press(cx, "down");
    assert_eq!(refused_row(&grid, cx).map(|(row, _)| row), Some(0));
    assert_eq!(ring_of(&grid, cx), Some((0, 1)));
    (id, grid)
}

/// What a blocked action leaves behind while the prompt is up: the question
/// asked, nothing sent, the row still dirty and still marked.
fn assert_asks_about_the_refused_row(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    grid: &Grid,
    history: usize,
) {
    settle_through(cx);
    assert!(discard_holds(workspace, cx).is_some(), "the prompt is up");
    assert_eq!(front_dialog(workspace, cx), Some("discard"));
    assert_eq!(
        history_sql(workspace, cx).len(),
        history,
        "nothing was sent"
    );
    assert_eq!(pending_of(grid, cx), 1);
    assert_eq!(refused_row(grid, cx).map(|(row, _)| row), Some(0));
    assert_eq!(
        on_disk("select name from birds where id = 1").as_deref(),
        Some("wren")
    );
}

fn answer_discard(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, discard: bool) {
    workspace.update_in(cx, |workspace, window, cx| match discard {
        true => workspace.confirm_discard_close(window, cx),
        false => {
            workspace.cancel_discard_close(cx);
        }
    });
    settle_through(cx);
}

/// A refused row is gone, and with it the mark, and the server was never
/// asked to change it.
fn assert_refused_row_dropped(grid: &Grid, cx: &mut VisualTestContext) {
    assert_eq!(pending_of(grid, cx), 0);
    assert_eq!(refused_row(grid, cx), None);
    assert_eq!(
        on_disk("select name from birds where id = 1").as_deref(),
        Some("wren")
    );
}

#[gpui::test]
fn a_refused_rows_notice_says_how_the_row_goes_back(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let _ = refused_row_view(&workspace, cx);
        let notice = shown_notice(&workspace, cx).expect("the status line says why");
        assert!(notice.contains("NOT NULL"), "{notice}");
        assert!(
            notice.contains("Esc puts it back as it was read"),
            "{notice}"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn closing_a_tab_whose_row_the_server_refused_asks_to_discard_the_row(cx: &mut TestAppContext) {
    use crate::actions::CloseTab;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, grid) = refused_row_view(&workspace, cx);
        let history = history_sql(&workspace, cx).len();

        cx.dispatch_action(CloseTab);
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        assert!(table_tab_is_open(&workspace, cx, id));

        answer_discard(&workspace, cx, false);
        assert!(discard_holds(&workspace, cx).is_none());
        assert!(table_tab_is_open(&workspace, cx, id));
        assert_eq!(pending_of(&grid, cx), 1, "Cancel keeps everything");
        assert!(refused_row(&grid, cx).is_some());

        cx.dispatch_action(CloseTab);
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        answer_discard(&workspace, cx, true);
        assert!(!table_tab_is_open(&workspace, cx, id), "then it closes");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn switching_tabs_from_a_row_the_server_refused_asks_to_discard_the_row(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let query = workspace.read_with(cx, |workspace, _| {
            Tab::Query(workspace.profile().unwrap().session.queries[0].id)
        });
        let (id, grid) = refused_row_view(&workspace, cx);
        let history = history_sql(&workspace, cx).len();
        let front = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace.profile().unwrap().session.active
            })
        };

        bring_forward(&workspace, cx, query);
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        assert_eq!(front(&workspace, cx), Tab::Object(id));

        answer_discard(&workspace, cx, false);
        assert_eq!(front(&workspace, cx), Tab::Object(id));
        assert_eq!(pending_of(&grid, cx), 1, "Cancel keeps everything");

        bring_forward(&workspace, cx, query);
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        answer_discard(&workspace, cx, true);
        assert_eq!(front(&workspace, cx), query);
        assert_refused_row_dropped(&grid, cx);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_refresh_over_a_row_the_server_refused_asks_to_discard_the_row(cx: &mut TestAppContext) {
    use crate::actions::RefreshRelation;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, FIVE_BIRDS);
        let (_, grid) = refused_row_view(&workspace, cx);
        let history = history_sql(&workspace, cx).len();
        rusqlite::Connection::open(store::dbdelve_directory().unwrap().join("e2e.db"))
            .and_then(|db| db.execute("update birds set name = 'finch' where id = 3", []))
            .unwrap();

        cx.dispatch_action(RefreshRelation);
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        answer_discard(&workspace, cx, false);
        assert_eq!(pending_of(&grid, cx), 1, "Cancel keeps everything");
        assert_eq!(cell_of(&grid, cx, 2, 1).as_deref(), Some("kite"));

        cx.dispatch_action(RefreshRelation);
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        answer_discard(&workspace, cx, true);
        assert_refused_row_dropped(&grid, cx);
        assert_eq!(cell_of(&grid, cx, 2, 1).as_deref(), Some("finch"), "read");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_page_turn_over_a_row_the_server_refused_asks_to_discard_the_row(cx: &mut TestAppContext) {
    use crate::actions::NextPage;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, FIVE_BIRDS);
        let (id, grid) = refused_row_view(&workspace, cx);
        // A row limit the page turn pages by.
        workspace.update(cx, |workspace, _| {
            let ObjectBody::Relation { limit, .. } = workspace
                .object_body_mut(id)
                .expect("the relation tab is open")
            else {
                panic!("a relation tab");
            };
            *limit = Some(2);
        });
        let history = history_sql(&workspace, cx).len();

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.next_page(&NextPage, window, cx)
        });
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        answer_discard(&workspace, cx, false);
        assert_eq!(relation_view(&workspace, cx, id).3, 0, "the page stays");
        assert_eq!(pending_of(&grid, cx), 1, "Cancel keeps everything");

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.next_page(&NextPage, window, cx)
        });
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        answer_discard(&workspace, cx, true);
        assert_eq!(relation_view(&workspace, cx, id).3, 2, "then it turns");
        assert_eq!(first_column(&grid, cx), ["3", "4"]);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_sort_or_a_filter_over_a_row_the_server_refused_asks_to_discard_the_row(
    cx: &mut TestAppContext,
) {
    use crate::actions::SortColumn;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, FIVE_BIRDS);
        let (id, grid) = refused_row_view(&workspace, cx);
        let history = history_sql(&workspace, cx).len();
        let input = workspace.update_in(cx, |workspace, window, cx| {
            workspace.push_filter_row(id, window, cx);
            workspace.filter_rows_mut(id).unwrap()[0].value.clone()
        });
        let bars = bars_of(&workspace, cx, id);

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.sort_column(&SortColumn { column: 1 }, window, cx)
        });
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        answer_discard(&workspace, cx, false);
        assert!(relation_view(&workspace, cx, id).1.is_empty());
        assert_eq!(pending_of(&grid, cx), 1, "Cancel keeps everything");

        input.update_in(cx, |input, window, cx| {
            input.set_value("id > 2", window, cx)
        });
        workspace.update(cx, |workspace, cx| workspace.submit_filter(id, &input, cx));
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        answer_discard(&workspace, cx, false);
        assert_eq!(
            bars_of(&workspace, cx, id),
            bars,
            "Cancel puts the bar back"
        );
        assert_eq!(relation_view(&workspace, cx, id).0, "");

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.sort_column(&SortColumn { column: 1 }, window, cx)
        });
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        answer_discard(&workspace, cx, true);
        assert_refused_row_dropped(&grid, cx);
        assert_eq!(relation_view(&workspace, cx, id).1.len(), 1, "it sorted");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn switching_to_draft_from_a_row_the_server_refused_asks_to_discard_the_row(
    cx: &mut TestAppContext,
) {
    use crate::session::EditMode;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (_, grid) = refused_row_view(&workspace, cx);
        let history = history_sql(&workspace, cx).len();

        pick_edit_mode(&workspace, cx, EditMode::Draft);
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Row));
        answer_discard(&workspace, cx, false);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Row));
        assert_eq!(pending_of(&grid, cx), 1, "Cancel keeps everything");

        pick_edit_mode(&workspace, cx, EditMode::Draft);
        assert_asks_about_the_refused_row(&workspace, cx, &grid, history);
        answer_discard(&workspace, cx, true);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Draft));
        assert_refused_row_dropped(&grid, cx);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn discarding_a_refused_row_drops_only_that_row_and_the_others_are_saved_first(
    cx: &mut TestAppContext,
) {
    use crate::{actions::RefreshRelation, result_grid::NewValue};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, FIVE_BIRDS);
        let (_, grid) = refused_row_view(&workspace, cx);
        stage(&grid, cx, 2, 1, NewValue::Value("owl".into()));
        let history = history_sql(&workspace, cx).len();

        // The ring is on the refused row, so that is the one the action meets.
        cx.dispatch_action(RefreshRelation);
        settle_through(cx);
        assert!(discard_holds(&workspace, cx).is_some());
        assert_eq!(history_sql(&workspace, cx).len(), history);
        assert_eq!(pending_of(&grid, cx), 2);

        answer_discard(&workspace, cx, true);
        assert_eq!(refused_row(&grid, cx), None);
        assert_eq!(
            on_disk("select name from birds where id = 3").as_deref(),
            Some("owl"),
            "the other row was saved, not discarded"
        );
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        assert_eq!(pending_of(&grid, cx), 0);
        assert_no_discard_prompt(&workspace, cx);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_refused_row_puts_a_changed_filter_bar_back(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, grid) = open_in_row_mode(&workspace, cx);
        let input = workspace.update_in(cx, |workspace, window, cx| {
            workspace.push_filter_row(id, window, cx);
            workspace.filter_rows_mut(id).unwrap()[0].value.clone()
        });
        let bars = bars_of(&workspace, cx, id);
        let view = relation_view(&workspace, cx, id);

        stage(&grid, cx, 0, 1, NewValue::Null);
        input.update_in(cx, |input, window, cx| {
            input.set_value("id = 2", window, cx)
        });
        workspace.update(cx, |workspace, cx| workspace.submit_filter(id, &input, cx));
        settle_through(cx);

        assert_eq!(relation_view(&workspace, cx, id), view, "the filter stays");
        assert_eq!(bars_of(&workspace, cx, id), bars, "and so do the bars");
        assert_eq!(pending_of(&grid, cx), 1);
        assert!(refused_row(&grid, cx).is_some());
        assert_no_discard_prompt(&workspace, cx);
        quit(workspace, cx);
    });
}

/// A relation tab in Row mode with one filter bar and a valid edit on its first
/// row, the bar then changed to `id = 2` and submitted: the row is sent first
/// and the filter waits behind it. Returns the bars as they were before the
/// change.
fn filter_waiting_on_a_row_send(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    connection_open: bool,
) -> (
    u64,
    Vec<(crate::filter::Operator, crate::filter::Conjunction, String)>,
) {
    use crate::result_grid::NewValue;

    let (id, grid) = open_in_row_mode(workspace, cx);
    let input = workspace.update_in(cx, |workspace, window, cx| {
        workspace.push_filter_row(id, window, cx);
        workspace.filter_rows_mut(id).unwrap()[0].value.clone()
    });
    let bars = bars_of(workspace, cx, id);
    stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
    if !connection_open {
        workspace.update(cx, |workspace, _| {
            workspace.profile_mut().unwrap().state = ProfileState::Idle;
        });
    }
    input.update_in(cx, |input, window, cx| {
        input.set_value("id = 2", window, cx)
    });
    workspace.update(cx, |workspace, cx| workspace.submit_filter(id, &input, cx));
    (id, bars)
}

#[gpui::test]
fn a_filter_waiting_on_a_row_send_into_a_closed_connection_puts_the_bars_back(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, bars) = filter_waiting_on_a_row_send(&workspace, cx, false);
        settle_through(cx);

        assert_eq!(relation_view(&workspace, cx, id).0, "", "no filter ran");
        assert_eq!(bars_of(&workspace, cx, id), bars);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_filter_waiting_on_a_row_send_goes_back_when_another_action_already_waits(
    cx: &mut TestAppContext,
) {
    use crate::session::AfterEdits;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, bars) = filter_waiting_on_a_row_send(&workspace, cx, true);
        workspace.update(cx, |workspace, _| {
            workspace.profile_mut().unwrap().session.resume_after =
                Some((Tab::Object(id), AfterEdits::Refresh));
        });
        settle_through(cx);

        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin"),
            "the row was sent"
        );
        assert_eq!(
            relation_view(&workspace, cx, id).0,
            "",
            "the filter did not run"
        );
        assert_eq!(bars_of(&workspace, cx, id), bars);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_filter_waiting_on_a_row_send_goes_back_when_the_landing_finds_another_connection_in_front(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, bars) = filter_waiting_on_a_row_send(&workspace, cx, true);
        let front = workspace.update(cx, |workspace, _| workspace.active.take());
        settle_through(cx);
        workspace.update(cx, |workspace, _| workspace.active = front);
        settle(cx);

        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin"),
            "the row was sent"
        );
        assert_eq!(
            relation_view(&workspace, cx, id).0,
            "",
            "the filter did not run"
        );
        assert_eq!(bars_of(&workspace, cx, id), bars);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn dropping_the_prompts_puts_back_the_bars_of_an_action_waiting_to_resume(cx: &mut TestAppContext) {
    use crate::session::AfterEdits;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, _) = open_in_row_mode(&workspace, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.push_filter_row(id, window, cx)
        });
        let bars = bars_of(&workspace, cx, id);
        workspace.update(cx, |workspace, _| {
            let before = workspace.filter_rows_mut(id).unwrap().clone();
            workspace.filter_rows_mut(id).unwrap()[0].applied = "id = 2".into();
            let session = &mut workspace.profile_mut().unwrap().session;
            session.resume_after = Some((
                Tab::Object(id),
                AfterEdits::Filter {
                    before: Some(before),
                },
            ));
            session.clear_prompts();
            assert!(session.resume_after.is_none());
        });
        assert_eq!(bars_of(&workspace, cx, id), bars);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_row_send_read_back_that_is_not_a_select_is_not_run_and_the_tab_is_owed_a_read(
    cx: &mut TestAppContext,
) {
    use crate::session::Refresh;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, grid) = open_in_row_mode(&workspace, cx);
        let rows_id = grid.read_with(cx, |grid, _| grid.delegate().rows_id());

        // The write is a real one; only the read it carries is not a read.
        workspace.update(cx, |workspace, cx| {
            workspace.execute_and_then(
                "UPDATE \"birds\" SET \"name\" = 'robin' WHERE \"id\" = 1;".into(),
                Tab::Object(id),
                Some(Refresh::Row {
                    row: 0,
                    rows_id,
                    read: "DELETE FROM birds".into(),
                    sent: Vec::new(),
                    to: None,
                    then: None,
                }),
                false,
                None,
                cx,
            )
        });
        settle_through(cx);

        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin"),
            "the write ran"
        );
        assert_eq!(
            on_disk("select count(*) from birds").as_deref(),
            Some("2"),
            "the carried statement did not"
        );
        let notice = shown_notice(&workspace, cx).expect("the status line says what happened");
        assert!(
            notice.contains("refused to run a read it wrote itself"),
            "{notice}"
        );
        assert!(relation_is_stale(&workspace, cx, id));
        quit(workspace, cx);
    });
}

/// Whether relation tab `id` of the profile in front is owed a re-read.
fn relation_is_stale(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, id: u64) -> bool {
    workspace.read_with(cx, |workspace, _| {
        let tab = workspace
            .profile()
            .and_then(|profile| profile.session.objects.iter().find(|tab| tab.id == id))
            .expect("the relation tab is open");
        matches!(tab.body, ObjectBody::Relation { stale: true, .. })
    })
}

fn bring_forward(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, tab: Tab) {
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| workspace.activate_tab(tab, window, cx));
    });
    settle(cx);
}

/// A Snowflake relation tab, opened on the recorded mock, which no result of
/// is ever editable (`docs/agents/engines.md`): its keys are declared and not
/// enforced, so Row mode has no row to send and nothing to switch to.
#[gpui::test]
fn a_snowflake_table_offers_no_edit_mode(cx: &mut TestAppContext) {
    use crate::db::snowflake_mock::{Mock, Response};

    store::tests::with_home(|| {
        crate::IGNORE_ENVIRONMENT.set(true);
        let mock = Mock::start();
        let within = "IN SCHEMA \"DBDELVE_TEST\".\"SALES\"";
        for (statement, name) in [
            ("FROM INFORMATION_SCHEMA.TABLES", "relations"),
            ("FROM INFORMATION_SCHEMA.FUNCTIONS", "functions"),
            ("FROM INFORMATION_SCHEMA.PROCEDURES", "procedures"),
            ("TABLE_NAME = 'ORDER_LINES'", "columns_ORDER_LINES"),
            (&format!("SHOW PRIMARY KEYS {within}"), "show_primary"),
            (&format!("SHOW UNIQUE KEYS {within}"), "show_unique"),
            (&format!("SHOW IMPORTED KEYS {within}"), "show_imported"),
        ] {
            mock.on_statement(
                "",
                statement,
                [Response::fixture(200, &format!("{name}.json"))],
            );
        }
        // Rows of the recorded `customers` answer stand in for the preview's:
        // what the tab shows is not what is asserted.
        mock.answer("SELECT * FROM \"SALES\".\"ORDER_LINES\"", "customers");

        let config = mock.config();
        let (workspace, cx) = launch_with_profiles(
            cx,
            &format!(
                "{}[[profiles]]\nid = \"snow\"\nname = \"Snow\"\nhost = {:?}\ndatabase = {:?}\n\
                 user = {:?}\nengine = \"snowflake\"\naccount = {:?}\nprivate_key = {:?}\n\
                 warehouse = {:?}\nmode = \"full\"\n",
                sqlite_profile("orders", "Orders", ""),
                config.host.as_deref().expect("the mock names its address"),
                config.database,
                config.user,
                config.account,
                config.private_key,
                config.warehouse.as_deref().expect("a warehouse"),
            ),
        );
        cx.executor().allow_parking();
        activate_by_id(&workspace, cx, "snow");
        drive_until(cx, |cx| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profile()
                    .is_some_and(|profile| profile.id == "snow" && profile.connection().is_some())
            })
        });
        settle(cx);
        let id = workspace.update_in(cx, |workspace, window, cx| {
            let opened = crate::session::OpenedObject::Relation {
                schema: "SALES".into(),
                name: "ORDER_LINES".into(),
                kind: Default::default(),
                filter: String::new(),
                filters: Vec::new(),
            };
            let id = workspace
                .open_object(opened, window, cx)
                .expect("the relation opens");
            workspace.activate_tab(Tab::Object(id), window, cx);
            id
        });
        drive_until(cx, |cx| {
            settle(cx);
            relation_grid(&workspace, cx, id)
                .read_with(cx, |grid, _| !grid.delegate().result().rows.is_empty())
        });

        assert_eq!(front_edit_mode(&workspace, cx), None);
        assert_eq!(drawn(cx, &["edit-mode"]), [false]);
        let rows = palette_listing(&workspace, cx);
        for label in ["Edit in Row mode", "Edit in Draft mode"] {
            assert!(!rows.iter().any(|row| row == label), "{label}: {rows:?}");
        }
        quit(workspace, cx);
    });
}

/// A refusal to leave Draft is about the draft, so it goes once the draft
/// does, and once the view is in Row mode: the notice would otherwise stay
/// over a view it no longer describes.
#[gpui::test]
fn the_refusal_to_leave_draft_is_taken_down_with_the_draft_and_with_the_switch(
    cx: &mut TestAppContext,
) {
    use crate::{actions::DiscardEdits, result_grid::NewValue, session::EditMode};

    const REFUSAL: &str = "The draft has 1 edit, so the view was not switched to Row mode.";

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);

        stage(&grid, cx, 0, 1, NewValue::Value("jay".into()));
        pick_edit_mode(&workspace, cx, EditMode::Row);
        assert_eq!(shown_notice(&workspace, cx).as_deref(), Some(REFUSAL));

        // Discard is the condition the refusal reported going away.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.discard_edits(&DiscardEdits, window, cx)
            });
        });
        settle(cx);
        assert_eq!(shown_notice(&workspace, cx), None);

        // And the switch that follows says nothing of the refusal it replaces.
        stage(&grid, cx, 0, 1, NewValue::Value("jay".into()));
        pick_edit_mode(&workspace, cx, EditMode::Row);
        assert_eq!(shown_notice(&workspace, cx).as_deref(), Some(REFUSAL));
        grid.update(cx, |grid, _| grid.delegate_mut().discard_pending());
        pick_edit_mode(&workspace, cx, EditMode::Row);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Row));
        assert_eq!(shown_notice(&workspace, cx), None);
        quit(workspace, cx);
    });
}

/// `launch_with_profiles` with more `[settings]` keys, such as
/// `edit_mode = "draft"`.
fn launch_with_settings<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
    profiles: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    launch_connections(cx, "", settings, profiles)
}

/// `launch_with_settings` over the SQLite file `DB_PATH` names, `schema`
/// having been run against it first, as `launch_on_sqlite_holding` does for
/// its one profile: every profile that says `DB_PATH` lists the same tables.
fn launch_connections<'a>(
    cx: &'a mut TestAppContext,
    schema: &str,
    settings: &str,
    profiles: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let directory = store::dbdelve_directory().expect("the test home has a data directory");
    std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
    let database = directory.join("e2e.db");
    std::fs::write(&database, b"").expect("the database file must be creatable");
    rusqlite::Connection::open(&database)
        .and_then(|connection| connection.execute_batch(schema))
        .expect("the schema must apply");
    // The first profile is put in front, as a launch that left it there does;
    // a file with no `active` starts on the welcome surface instead.
    let active = profiles
        .lines()
        .find_map(|line| line.strip_prefix("id = "))
        .map(|id| format!("active = {id}\n"))
        .unwrap_or_default();
    std::fs::write(
        directory.join("profiles.toml"),
        format!("{active}[settings]\ncheck_for_updates = false\n{settings}\n{profiles}")
            .replace("DB_PATH", &format!("{:?}", database.display().to_string())),
    )
    .expect("profiles.toml must be writable");
    launch(cx)
}

/// One SQLite profile as `profiles.toml` spells it. `extra` is more scalar
/// keys, which have to come before any table.
fn sqlite_profile(id: &str, name: &str, extra: &str) -> String {
    format!(
        "[[profiles]]\nid = {id:?}\nname = {name:?}\nhost = \"\"\ndatabase = \"\"\n\
         user = \"\"\nengine = \"sqlite\"\npath = DB_PATH\n{extra}\n"
    )
}

/// Runs `sql` in the query tab in front, and waits for the answer.
fn run_in_front(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, sql: &str) {
    cx.update(|window, cx| {
        let editor = workspace
            .read(cx)
            .profile()
            .and_then(|profile| profile.session.active_query_tab())
            .expect("a connected profile opens on a query tab")
            .editor
            .clone();
        editor.update(cx, |input, cx| input.set_value(sql, window, cx));
    });
    cx.dispatch_action(crate::actions::RunQuery);
    settle(cx);
    query_tab(workspace, cx, |tab, _, _| match &tab.query {
        QueryState::Complete { .. } => {}
        QueryState::Failed(error) => panic!("{sql} failed: {error}"),
        _ => panic!("{sql} never completed"),
    });
}

/// Let the window run while threads outside it work, until `done` holds. The
/// executor never advances its clock here, so only real time passes.
fn drive_until(cx: &mut VisualTestContext, mut done: impl FnMut(&mut VisualTestContext) -> bool) {
    let started = std::time::Instant::now();
    while !done(cx) {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "nothing happened in five seconds"
        );
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// The notice where `Workspace::note` put it: on the profile in front, else on
/// the connection form, else on the welcome surface.
fn shown_notice(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    workspace.read_with(cx, |workspace, _| match workspace.profile() {
        Some(profile) => profile.session.shown_notice().map(str::to_string),
        None => match &workspace.form {
            Some(form) => form.error.clone(),
            None => workspace.welcome_notice.clone(),
        },
    })
}

fn activate_by_id(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, id: &str) {
    workspace.update(cx, |workspace, cx| {
        let index = workspace
            .profiles
            .iter()
            .position(|profile| profile.id == id)
            .expect("the profile was restored");
        workspace.activate(index, cx);
    });
    settle(cx);
}

/// Which of the transaction indicators and controls the status bar draws.
fn drawn(cx: &mut VisualTestContext, selectors: &[&'static str]) -> Vec<bool> {
    selectors
        .iter()
        .map(|selector| cx.debug_bounds(selector).is_some())
        .collect()
}

/// The app launched on `profiles`, the `[[profiles]]` tables of a hand-written
/// `profiles.toml`, with no profile chosen to be in front. `DB_PATH` in the text
/// stands for the path of an empty SQLite file written beside it, so a test can
/// say what a profile looks like on disk without knowing where the home is.
fn launch_with_profiles<'a>(
    cx: &'a mut TestAppContext,
    profiles: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    launch_with_settings(cx, "", profiles)
}

/// The window size and sidebar the status bar is drawn at.
fn resize_to(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    width: f32,
    sidebar_hidden: bool,
) {
    workspace.update(cx, |workspace, _| workspace.sidebar_hidden = sidebar_hidden);
    cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(900.)));
    settle(cx);
}

/// Whether the item is drawn whole: at the width it has in a wide window,
/// inside the bar, and clear of the bar's left-hand side, which clips every
/// item on it to its own bounds. An item pushed past an edge, or squeezed,
/// still has bounds.
fn drawn_whole(
    cx: &mut VisualTestContext,
    selector: &'static str,
    wide: gpui::Bounds<gpui::Pixels>,
) -> bool {
    let bounds = |cx: &mut VisualTestContext, selector| {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"))
    };
    let item = bounds(cx, selector);
    let inside = |clip: gpui::Bounds<gpui::Pixels>| item.intersect(&clip) == item;
    item.size == wide.size
        && inside(bounds(cx, "status-bar"))
        && !bounds(cx, "status-left").intersects(&item)
}

/// The pending edits' Discard and Apply edits are not what a narrow bar cuts:
/// they are drawn whole at 700 px with the sidebar open and closed, beside the
/// Edit menu and a notice too long for the bar.
#[gpui::test]
fn the_staged_edit_buttons_are_drawn_whole_in_a_narrow_window(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        workspace.update(cx, |workspace, cx| {
            workspace.note("A notice long enough to fill the bar. ".repeat(8), cx)
        });
        resize_to(&workspace, cx, 1536., true);
        let items = ["edit-mode", "discard-edits", "apply-edits"];
        let wide = items.map(|item| {
            cx.debug_bounds(item)
                .unwrap_or_else(|| panic!("{item} is drawn"))
        });
        for hidden in [false, true] {
            resize_to(&workspace, cx, 700., hidden);
            for (item, wide) in items.into_iter().zip(wide) {
                assert!(
                    drawn_whole(cx, item, wide),
                    "{item} is cut off at 700px, sidebar hidden {hidden}"
                );
            }
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_cell_put_back_while_its_row_is_saving_stays_staged_after_the_save(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, grid) = open_in_row_mode(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        // Not through Save, which runs the executor to the answer.
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.save_row(id, 0, None, None, cx))
        });
        assert_eq!(state_kind(&workspace, cx), "running");
        stage(&grid, cx, 0, 1, NewValue::Value("wren".into()));

        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        grid.read_with(cx, |grid, _| {
            let grid = grid.delegate();
            assert_eq!(
                grid.cell(0, 1),
                Some("robin"),
                "the row as the server holds it"
            );
            assert_eq!(
                grid.pending_cells(0),
                vec![(1, NewValue::Value("wren".into()))],
                "the value put back is left to send"
            );
        });

        save(&workspace, cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn escape_leaves_a_row_alone_while_it_is_saving(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.save_row(id, 0, None, None, cx))
        });
        assert_eq!(state_kind(&workspace, cx), "running");

        let reverted = workspace.update_in(cx, |workspace, window, cx| {
            workspace.revert_active_row(window, cx)
        });
        assert!(!reverted);
        assert_eq!(pending_of(&grid, cx), 1, "the edit being sent stays");

        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(pending_of(&grid, cx), 0);
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().cell(0, 1), Some("robin"))
        });

        // Once the row has landed, Esc puts an edit back as it did.
        stage(&grid, cx, 0, 1, NewValue::Value("jay".into()));
        let reverted = workspace.update_in(cx, |workspace, window, cx| {
            workspace.revert_active_row(window, cx)
        });
        assert!(reverted);
        assert_eq!(pending_of(&grid, cx), 0);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_filter_asked_for_while_a_tabs_edits_are_being_sent_puts_its_bars_back(
    cx: &mut TestAppContext,
) {
    use crate::{actions::ApplyEdits, result_grid::NewValue};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        let input = workspace.update_in(cx, |workspace, window, cx| {
            workspace.push_filter_row(id, window, cx);
            workspace.filter_rows_mut(id).unwrap()[0].value.clone()
        });
        let bars = bars_of(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        cx.dispatch_action(ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        assert_eq!(state_kind(&workspace, cx), "running");

        input.update_in(cx, |input, window, cx| {
            input.set_value("id = 2", window, cx)
        });
        workspace.update(cx, |workspace, cx| workspace.submit_filter(id, &input, cx));
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("This tab's edits are still being sent.")
        );
        assert_eq!(bars_of(&workspace, cx, id), bars);

        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(relation_view(&workspace, cx, id).0, "", "no filter ran");
        assert_eq!(bars_of(&workspace, cx, id), bars);
        quit(workspace, cx);
    });
}

/// A table read through a plugin that writes its rows, which a Row send is
/// never made on: the menu offers Classic and Draft, so a view in Draft can
/// go back to Classic.
#[gpui::test]
fn a_plugin_table_offers_classic_and_draft_but_not_row(cx: &mut TestAppContext) {
    use crate::session::EditMode;

    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            "active = \"echo\"\n\
             [settings]\ncheck_for_updates = false\nedit_mode = \"draft\"\n\
             [[profiles]]\nid = \"echo\"\nname = \"Echo\"\nhost = \"\"\ndatabase = \"\"\n\
             user = \"\"\nengine = \"plugin:echo\"\nspeaks = \"postgres\"\n\
             connection = \"DSN=echo\"\n",
        )
        .unwrap();

        let (workspace, cx) = launch_with(cx, vec![crate::db::echo::plugin()]);
        cx.executor().allow_parking();
        workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| matches!(profile.config, ConnectionConfig::External(_)))
                .unwrap();
            if let ConnectionConfig::External(config) = &mut workspace.profiles[index].config {
                config.password = "x".into();
            }
            workspace.activate(index, cx);
        });
        cx.run_until_parked();
        let id = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let opened = crate::session::OpenedObject::Relation {
                    schema: "s".into(),
                    name: "t".into(),
                    kind: Default::default(),
                    filter: String::new(),
                    filters: Vec::new(),
                };
                let id = workspace.open_object(opened, window, cx).unwrap();
                workspace.activate_tab(Tab::Object(id), window, cx);
                id
            })
        });
        settle(cx);

        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Draft);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Draft));
        let rows = palette_listing(&workspace, cx);
        assert!(
            rows.iter().any(|row| row == "Edit in Classic mode"),
            "{rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row == "Edit in Row mode"),
            "{rows:?}"
        );

        pick_edit_mode(&workspace, cx, EditMode::Classic);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Classic));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_profile_saved_onto_a_target_without_row_mode_takes_its_tables_out_of_it(
    cx: &mut TestAppContext,
) {
    use crate::session::EditMode;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, _) = open_in_row_mode(&workspace, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            let profile = workspace.profile_mut().unwrap();
            // Idle, so the edited config waits for the next connect rather
            // than opening one here.
            profile.state = ProfileState::Idle;
            let ConnectionConfig::Sqlite { path, .. } = &profile.config else {
                panic!("the profile is SQLite");
            };
            let config = ConnectionConfig::Turso {
                path: path.clone(),
                statement_timeout: 0,
            };
            let (profile_id, name) = (profile.id.clone(), profile.name.clone());
            workspace.save_profile(&profile_id, name, config, None, window, cx);
        });
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Draft);
        quit(workspace, cx);
    });
}

/// Picks an edit mode from the status bar's menu: the button must be drawn,
/// and the pick is the action its menu rows carry.
fn pick_edit_mode(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    mode: crate::session::EditMode,
) {
    assert!(
        cx.debug_bounds("edit-mode").is_some(),
        "the edit mode menu is not drawn"
    );
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.pick_edit_mode(&crate::actions::SetEditMode { mode }, window, cx)
    });
    settle(cx);
}

/// Makes Draft the mode a new table tab starts in, for a test written about
/// Draft: Classic is the default.
fn draft_by_default(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update(cx, |workspace, cx| {
        workspace.set_default_edit_mode(crate::session::EditMode::Draft, cx)
    });
}
