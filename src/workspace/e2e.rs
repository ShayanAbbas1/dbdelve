//! The app driven the way a person drives it: launched from a saved
//! `profiles.toml`, typed into, run from the keyboard. GPUI's test platform
//! has no screen, so these run headless in CI like any other test.

use std::{borrow::Cow, cell::RefCell, rc::Rc};

use gpui::{
    App, AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, Window,
};
use gpui_component::{Root, input::InputState, tree::TreeEvent};

use super::Workspace;
use crate::{
    actions::{CancelQuery, Disconnect, ImportRowsFromClipboard, SubmitConnection},
    connection_form::ConnectionForm,
    db::{CancelToken, ConnectionConfig, Engine, ExternalConfig, Fetch, RelationKind, SshTunnel},
    explorer::{self, ObjectKind},
    keybindings,
    palette::{Command, Mode as PaletteMode, Scope},
    session::{CatalogState, ObjectBody, ProfileState, QueryState, QueryTab, Tab},
    sidebar::{
        LOADING, ListingScope, NO_PROJECT_ROW, ViewMode, connection_row, database_row, project_row,
        scoped_row,
    },
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

/// The app launched on `profiles`, the `[[profiles]]` tables of a hand-written
/// `profiles.toml`, with `settings` among the `[settings]` keys, over the SQLite
/// file `DB_PATH` names, `schema` having been run against it first, as
/// `launch_on_sqlite_holding` does for its one profile: every profile that says
/// `DB_PATH` lists the same tables.
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
            .open_object(opened, None, window, cx)
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

fn assert_startup_theme(
    cx: &mut TestAppContext,
    settings: &str,
    matching: bool,
    expected_theme: impl FnOnce(theme::Appearance) -> &'static str,
) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            format!("[settings]\ncheck_for_updates = false\n{settings}"),
        )
        .unwrap();

        let (workspace, cx) = launch(cx);
        cx.update(|window, cx| {
            let system = theme::Appearance::of_system(window.appearance());
            assert_eq!(
                workspace.read(cx).settings.match_system_appearance,
                matching
            );
            assert_eq!(theme::theme(cx).name, expected_theme(system));
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn startup_follows_system_appearance_without_a_saved_choice(cx: &mut TestAppContext) {
    assert_startup_theme(cx, "", true, |system| theme::starting_theme(system).name);
}

#[gpui::test]
fn startup_preserves_a_legacy_fixed_theme(cx: &mut TestAppContext) {
    assert_startup_theme(cx, "theme = \"Dracula\"\n", false, |_| "Dracula");
}

#[gpui::test]
fn startup_preserves_a_saved_system_appearance_opt_out(cx: &mut TestAppContext) {
    assert_startup_theme(
        cx,
        "theme = \"Dracula\"\nmatch_system_appearance = false\n",
        false,
        |_| "Dracula",
    );
}

#[gpui::test]
fn startup_honors_a_saved_system_appearance_opt_in(cx: &mut TestAppContext) {
    assert_startup_theme(
        cx,
        "theme = \"GitHub Dark\"\nmatch_system_appearance = true\n\
         light_theme = \"GitHub Light\"\ndark_theme = \"Dracula\"\n",
        true,
        |system| match system {
            theme::Appearance::Light => "GitHub Light",
            theme::Appearance::Dark => "Dracula",
        },
    );
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
fn the_close_all_chord_empties_the_strip(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let strip = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profile()
                    .expect("a profile is open")
                    .session
                    .strip_tabs()
            })
        };
        cx.simulate_keystrokes("secondary-t secondary-t");
        settle(cx);
        assert!(strip(cx).len() >= 3);

        cx.simulate_keystrokes("secondary-k w");
        settle(cx);
        assert_eq!(strip(cx), Vec::new());

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_row_panel_draft_keeps_its_tab_open_for_the_discard_question(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table fruit (id integer primary key, name text); \
             insert into fruit values (1, 'apple');",
        );
        cx.simulate_input("select * from fruit");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        press(cx, "right");
        press(cx, "secondary-shift-e");
        let input = workspace.read_with(cx, |workspace, _| {
            let inputs = workspace.row_panel.inputs.as_ref().expect("Edit mode");
            inputs.fields[1]
                .as_ref()
                .expect("an input on the name")
                .input
                .clone()
        });
        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.set_value("pear", window, cx));
        });
        settle(cx);

        cx.simulate_keystrokes("secondary-k w");
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().expect("a connection in front").session;
            let discard = session
                .pending_discard
                .as_ref()
                .expect("the draft is asked about");
            assert_eq!(discard.tab, session.active);
            assert!(discard.also.is_empty());
            assert_eq!(session.strip_tabs(), vec![session.active]);
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn text_typed_in_an_open_cell_keeps_its_tab_open_for_the_discard_question(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table fruit (id integer primary key, name text); \
             insert into fruit values (1, 'apple');",
        );
        cx.simulate_input("select * from fruit");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        press(cx, "right");
        press(cx, "enter");
        cx.simulate_input("s");
        settle(cx);

        cx.simulate_keystrokes("secondary-k w");
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().expect("a connection in front").session;
            let discard = session
                .pending_discard
                .as_ref()
                .expect("the draft is asked about");
            assert_eq!(discard.tab, session.active);
            assert!(discard.also.is_empty());
        });
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

        let scratch = active(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            let crate::session::Tab::Query(scratch) = scratch else {
                panic!("the new query is in front");
            };
            workspace.close_buffer(scratch, window, cx);
            let id = workspace.profile().unwrap().id.clone();
            store::write_query(&id, "daily", "select 1").expect("the query saves");
            workspace.reload_saved_queries(cx);
            workspace.open_saved_query("daily".into(), window, cx);
        });
        settle(cx);
        assert_eq!(
            selected(cx),
            Some(explorer::query_row("daily")),
            "a saved query's tab has its row"
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

/// The connections tree's visible rows, as (depth, id).
fn connections_rows(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Vec<(usize, String)> {
    workspace.read_with(cx, |workspace, cx| {
        let tree = workspace.connections.tree.read(cx);
        (0..)
            .map_while(|ix| tree.entry(ix))
            .map(|entry| (entry.depth(), entry.item().id.to_string()))
            .collect()
    })
}

/// Where the connections tree lists the row `id`, if it is showing.
fn connections_row(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: &str,
) -> Option<usize> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .connections
            .tree
            .read(cx)
            .index_of(&id.to_string().into())
    })
}

/// The connections tree's row `id` made showing and selected, through every
/// folded row above it, those the tree left out as well as those it drew.
fn reveal_connections_row(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, id: &str) {
    workspace.update(cx, |workspace, cx| {
        workspace.reveal_connections_row(id, cx);
        workspace.connections.tree.update(cx, |tree, cx| {
            let row = id.to_string().into();
            tree.reveal_item(&row, gpui::ScrollStrategy::Top, cx);
            // A reveal alone repaints nothing; selecting does.
            tree.set_selected_index(tree.index_of(&row), cx);
        });
    });
    settle(cx);
}

/// A row of the connections tree: whether it is open, and its children's ids
/// and labels with whether each is disabled.
type ConnectionsItem = (bool, Vec<(String, String, bool)>);

fn connections_item(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: &str,
) -> Option<ConnectionsItem> {
    let index = connections_row(workspace, cx, id)?;
    workspace.read_with(cx, |workspace, cx| {
        let item = workspace
            .connections
            .tree
            .read(cx)
            .entry(index)?
            .item()
            .clone();
        Some((
            item.is_expanded(),
            item.children
                .iter()
                .map(|child| {
                    (
                        child.id.to_string(),
                        child.label.to_string(),
                        child.is_disabled(),
                    )
                })
                .collect(),
        ))
    })
}

/// A click on the connections tree's row `index`.
fn click_connections_row(cx: &mut VisualTestContext, index: usize) {
    // `debug_bounds` takes a `'static` selector; a test's few rows can leak.
    let selector: &'static str = format!("connections-row-{index}").leak();
    let bounds = cx.debug_bounds(selector).expect("the row is drawn");
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    settle(cx);
}

fn profile_state_of(workspace: &Workspace, id: &str) -> &'static str {
    match workspace
        .profiles
        .iter()
        .find(|profile| profile.id == id)
        .expect("the profile was restored")
        .state
    {
        ProfileState::Idle => "idle",
        ProfileState::Connecting { .. } => "connecting",
        ProfileState::Connected(_) => "connected",
        ProfileState::Failed(_) => "failed",
    }
}

fn profile_state(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: &str,
) -> &'static str {
    workspace.read_with(cx, |workspace, _| profile_state_of(workspace, id))
}

fn run_command(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, command: Command) {
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.run_command(command, window, cx)
    });
    settle(cx);
}

/// The id of the connection in front, for a test already holding the workspace.
fn front_id(workspace: &Workspace) -> String {
    workspace
        .profile()
        .expect("a connection is in front")
        .id
        .clone()
}

fn front_profile(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    workspace.read_with(cx, |workspace, _| workspace.profile().unwrap().id.clone())
}

/// Two SQLite profiles over one file holding `birds`, `b` in the Zoo project.
/// `a` is in front, so Zoo starts folded.
fn two_connections_folded(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    // Left alone, CI's live job puts a connection from `PG*` among these.
    crate::IGNORE_ENVIRONMENT.set(true);
    launch_connections(
        cx,
        "create table birds (name text);",
        "view_mode = \"complex\"",
        &format!(
            "[[projects]]\nname = \"Zoo\"\nconnections = [\"b\"]\n{}{}",
            sqlite_profile("a", "Birds", ""),
            sqlite_profile("b", "Flocks", ""),
        ),
    )
}

/// `two_connections_folded` with Zoo opened, as a person opens it.
fn two_connections(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let (workspace, cx) = two_connections_folded(cx);
    let zoo = connections_row(&workspace, cx, &project_row("Zoo")).expect("Zoo is listed");
    click_connections_row(cx, zoo);
    (workspace, cx)
}

#[gpui::test]
fn the_connections_tree_lists_every_connection_under_its_project_and_connects_one_on_expand(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections_folded(cx);
        let (a, b) = (connection_row("a"), connection_row("b"));

        // The project in front starts open and every other one folded.
        assert_eq!(
            connections_rows(&workspace, cx),
            [
                (0, NO_PROJECT_ROW.to_string()),
                (1, a.clone()),
                (0, project_row("Zoo")),
            ]
        );
        let zoo = connections_row(&workspace, cx, &project_row("Zoo")).unwrap();
        click_connections_row(cx, zoo);
        assert_eq!(
            connections_rows(&workspace, cx),
            [
                (0, NO_PROJECT_ROW.to_string()),
                (1, a),
                (0, project_row("Zoo")),
                (1, b.clone()),
            ]
        );
        assert_eq!(front_profile(&workspace, cx), "a");
        assert_eq!(profile_state(&workspace, cx, "b"), "idle");
        assert_eq!(
            connections_item(&workspace, cx, &b),
            Some((
                false,
                vec![(scoped_row("b", None, "loading"), LOADING.to_string(), true)]
            )),
            "an idle connection is folded over one row that is not a spinner"
        );

        let before = workspace.read_with(cx, |workspace, _| workspace.next_generation);
        let at = connections_row(&workspace, cx, &b).unwrap();
        click_connections_row(cx, at);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.next_generation, before + 1, "one connect");
        });
        assert_eq!(profile_state(&workspace, cx, "b"), "connected");

        assert!(
            connections_row(&workspace, cx, &scoped_row("b", None, "schema-main")).is_some(),
            "the opened connection lists its schema"
        );
        assert_eq!(front_profile(&workspace, cx), "a");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn expanding_a_connection_again_while_it_connects_starts_no_second_connect(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        let before = workspace.read_with(cx, |workspace, _| workspace.next_generation);

        workspace.update(cx, |workspace, cx| {
            workspace.connections.tree.update(cx, |_, cx| {
                for _ in 0..2 {
                    cx.emit(TreeEvent::Expanded(connection_row("b").into()));
                }
            });
        });

        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.next_generation, before + 1);
        });
        assert_eq!(profile_state(&workspace, cx, "b"), "connecting");

        // A reveal of a row of a connection that is already up lands here too.
        settle(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.connections.tree.update(cx, |_, cx| {
                cx.emit(TreeEvent::Expanded(connection_row("b").into()));
            });
        });
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.next_generation, before + 1);
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_object_row_of_a_connection_behind_opens_on_that_connection(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        // Tall enough to draw every row of both connections, which a click
        // needs: the list draws only what is in view.
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
        settle(cx);
        for id in ["a", "b"] {
            // Opened and let load, the way a person would, one after the other.
            let at = connections_row(&workspace, cx, &connection_row(id)).unwrap();
            click_connections_row(cx, at);
        }
        let reveal = |id: &str, cx: &mut VisualTestContext| {
            let row = scoped_row(id, None, &explorer::relation_row("main", "birds"));
            reveal_connections_row(&workspace, cx, &row);
            row
        };
        let a_birds = reveal("a", cx);
        let b_birds = reveal("b", cx);
        assert_ne!(a_birds, b_birds);
        workspace.read_with(cx, |workspace, _| {
            for row in [&a_birds, &b_birds] {
                assert!(
                    workspace.connections.nodes.contains_key(row.as_str()),
                    "{row} is a row of its own"
                );
            }
        });
        assert_eq!(front_profile(&workspace, cx), "a");

        let at = connections_row(&workspace, cx, &b_birds).expect("the table is showing");
        click_connections_row(cx, at);

        assert_eq!(front_profile(&workspace, cx), "b");
        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .and_then(|profile| profile.session.active_object())
                .expect("an object tab is in front");
            assert_eq!((tab.schema.as_str(), tab.name.as_str()), ("main", "birds"));
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_selection_whose_connection_was_left_mid_run_waits_and_runs_the_rest_when_it_comes_back(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        for id in ["b", "a"] {
            let at = connections_row(&workspace, cx, &connection_row(id)).unwrap();
            click_connections_row(cx, at);
        }
        assert_eq!(front_profile(&workspace, cx), "a");
        cx.update(|window, cx| {
            let editor = workspace
                .read(cx)
                .profile()
                .and_then(|profile| profile.session.active_query_tab())
                .expect("`a` is on a query tab")
                .editor
                .clone();
            editor.focus_handle(cx).focus(window, cx);
        });
        // Writes, so they go one after another rather than at once.
        cx.simulate_input("insert into birds values ('wren'); insert into birds values ('robin')");
        cx.simulate_keystrokes("secondary-a");
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.run_query(&crate::actions::RunQuery, window, cx);
                let b = workspace
                    .profiles
                    .iter()
                    .position(|profile| profile.id == "b")
                    .expect("`b` is listed");
                workspace.activate(b, cx);
            });
        });
        settle(cx);
        let waiting = |workspace: &Workspace| {
            let a = workspace
                .profiles
                .iter()
                .find(|profile| profile.id == "a")
                .expect("`a` is listed");
            a.session
                .active_query_tab()
                .and_then(|tab| tab.queue.as_ref())
                .map(|queue| queue.remaining.len())
        };
        let rows = || -> i64 {
            let database = store::dbdelve_directory()
                .expect("the test home has a data directory")
                .join("e2e.db");
            rusqlite::Connection::open(database)
                .and_then(|connection| {
                    connection.query_row("SELECT count(*) FROM birds", [], |row| row.get(0))
                })
                .expect("the database reads")
        };
        assert_eq!(
            workspace.read_with(cx, |workspace, _| waiting(workspace)),
            Some(1)
        );
        assert_eq!(
            rows(),
            1,
            "only the first statement ran, and nothing in front"
        );

        workspace.update(cx, |workspace, cx| {
            let a = workspace
                .profiles
                .iter()
                .position(|profile| profile.id == "a")
                .expect("`a` is listed");
            workspace.activate(a, cx);
        });
        settle(cx);
        assert_eq!(
            workspace.read_with(cx, |workspace, _| waiting(workspace)),
            Some(0)
        );
        assert_eq!(rows(), 2, "the rest ran once `a` was in front again");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_connect_that_fails_stays_on_its_own_row(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_connections(
            cx,
            "create table birds (name text);",
            "view_mode = \"complex\"",
            &format!(
                "{}{}{}",
                sqlite_profile("a", "Birds", ""),
                sqlite_profile("b", "Flocks", ""),
                "[[profiles]]\nid = \"c\"\nname = \"Gone\"\nhost = \"\"\ndatabase = \"\"\n\
                 user = \"\"\nengine = \"sqlite\"\npath = \"/dbdelve/no/such/file.db\"\n",
            ),
        );
        let (a, b, c) = (
            connection_row("a"),
            connection_row("b"),
            connection_row("c"),
        );
        let a_before = connections_item(&workspace, cx, &a);
        let b_before = connections_item(&workspace, cx, &b);

        let at = connections_row(&workspace, cx, &c).unwrap();
        click_connections_row(cx, at);

        let failure = workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profiles.iter().find(|profile| profile.id == "c");
            match profile.map(|profile| &profile.state) {
                Some(ProfileState::Failed(message)) => message.clone(),
                _ => panic!("the connect did not fail"),
            }
        });
        assert_eq!(
            connections_item(&workspace, cx, &c),
            Some((true, vec![(scoped_row("c", None, "notice"), failure, true)]))
        );
        assert_eq!(connections_item(&workspace, cx, &a), a_before);
        assert_eq!(connections_item(&workspace, cx, &b), b_before);

        quit(workspace, cx);
    });
}

/// One DuckDB profile, `e2e`, over a file holding `t (id INTEGER)`, with the
/// connections tree drawn.
fn launch_duckdb_connection(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    launch_duckdb(cx, "")
}

/// The same, with `extra` among the profile's scalar keys and a second file,
/// `other.duckdb` holding `u (id INTEGER)`, beside it for a statement to
/// attach. Both files are closed again before the app opens one.
fn launch_duckdb<'a>(
    cx: &'a mut TestAppContext,
    extra: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    launch_duckdb_with(cx, "", extra)
}

/// `launch_duckdb` with more `[settings]` keys.
fn launch_duckdb_with<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
    extra: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    launch_duckdb_seeded(
        cx,
        settings,
        extra,
        "CREATE TABLE t (id INTEGER);",
        "CREATE TABLE u (id INTEGER);",
    )
}

/// `launch_duckdb_with`, the two files starting from the statements given
/// rather than from `t` and `u`.
fn launch_duckdb_seeded<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
    extra: &str,
    own_seed: &str,
    other_seed: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    // Left alone, CI's live job puts a connection from `PG*` among these.
    crate::IGNORE_ENVIRONMENT.set(true);
    let directory = store::dbdelve_directory().expect("the test home has a data directory");
    std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
    let database = directory.join("e2e.duckdb");
    ::duckdb::Connection::open(&database)
        .and_then(|connection| connection.execute_batch(own_seed))
        .expect("the schema must apply");
    ::duckdb::Connection::open(directory.join("other.duckdb"))
        .and_then(|connection| connection.execute_batch(other_seed))
        .expect("the second schema must apply");
    std::fs::write(
        directory.join("profiles.toml"),
        format!(
            "active = \"e2e\"\n\
             [settings]\n\
             check_for_updates = false\n\
             view_mode = \"complex\"\n\
             {settings}\n\
             [[profiles]]\n\
             id = \"e2e\"\n\
             name = \"E2E\"\n\
             host = \"\"\n\
             database = \"\"\n\
             user = \"\"\n\
             engine = \"duckdb\"\n\
             path = {:?}\n\
             {extra}\n",
            database.display().to_string()
        ),
    )
    .expect("profiles.toml must be writable");
    let (workspace, cx) = launch(cx);
    cx.executor().allow_parking();
    // Tall enough to draw every row, which a click needs.
    cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
    settle(cx);
    (workspace, cx)
}

#[gpui::test]
fn a_duckdb_connection_shows_its_database_then_its_schemas(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_duckdb_connection(cx);
        let click = |id: &str, cx: &mut VisualTestContext| {
            let at = connections_row(&workspace, cx, id)
                .unwrap_or_else(|| panic!("{id} is not showing"));
            click_connections_row(cx, at);
        };

        click(&connection_row("e2e"), cx);
        let database = workspace.read_with(cx, |workspace, _| {
            workspace
                .profile()
                .and_then(|profile| profile.active_database())
                .expect("the connection names its database")
        });
        assert_eq!(database, "e2e", "DuckDB names a file's database after it");
        let db = database_row("e2e", &database);
        let schema = scoped_row("e2e", Some(&database), "schema-main");
        let tables = scoped_row("e2e", Some(&database), "category-Tables-main");
        let t = scoped_row("e2e", Some(&database), &explorer::relation_row("main", "t"));
        assert_eq!(
            connections_item(&workspace, cx, &connection_row("e2e"))
                .map(|(_, rows)| { rows.into_iter().map(|(id, ..)| id).collect::<Vec<_>>() }),
            Some(vec![db.clone()]),
            "the database is the connection's one row"
        );
        for id in [&db, &schema, &tables] {
            click(id, cx);
        }
        assert_eq!(
            connections_rows(&workspace, cx),
            [
                (0, connection_row("e2e")),
                (1, db.clone()),
                (2, schema),
                (3, tables),
                (4, t.clone()),
            ]
        );
        workspace.read_with(cx, |workspace, _| {
            assert!(matches!(
                workspace.connections.nodes.get(&db),
                Some(crate::sidebar::Node::Database { .. })
            ));
            // Each folder row counts what it holds, from what has loaded.
            let counts = &workspace.connections.counts;
            for id in [connection_row("e2e"), db.clone()] {
                assert_eq!(counts.get(&id), Some(&1), "{id}");
            }
            assert_eq!(
                counts.get(&scoped_row("e2e", Some(&database), "schema-main")),
                Some(&1)
            );
        });

        // The table opens on the profile, and the tree finds the row of the
        // tab in front by the id that names the database.
        click(&t, cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            let tab = profile
                .session
                .active_object()
                .expect("an object tab is in front");
            assert_eq!((tab.schema.as_str(), tab.name.as_str()), ("main", "t"));
            assert_eq!(super::sidebar::front_row_id(profile), Some(t.clone()));
        });
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        let selected = workspace.read_with(cx, |workspace, cx| {
            let tree = workspace.connections.tree.read(cx);
            tree.selected_item().map(|item| item.id.to_string())
        });
        assert_eq!(selected, Some(t));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_sequence_row_opens_nothing_and_shows_its_ddl_from_its_menu(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_duckdb_seeded(cx, "", "", "CREATE SEQUENCE ticket START 5;", "");
        let click = |id: &str, cx: &mut VisualTestContext| {
            let at = connections_row(&workspace, cx, id)
                .unwrap_or_else(|| panic!("{id} is not showing"));
            click_connections_row(cx, at);
        };
        click(&connection_row("e2e"), cx);
        let database = workspace.read_with(cx, |workspace, _| {
            workspace
                .profile()
                .and_then(|profile| profile.active_database())
                .expect("the connection names its database")
        });
        for id in [
            database_row("e2e", &database),
            scoped_row("e2e", Some(&database), "schema-main"),
            scoped_row("e2e", Some(&database), "category-Sequences-main"),
        ] {
            click(&id, cx);
        }
        let tabs = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().unwrap().session;
                (session.queries.len(), session.objects.len())
            })
        };
        let before = tabs(&workspace, cx);
        let row = scoped_row(
            "e2e",
            Some(&database),
            &explorer::sequence_row("main", "ticket"),
        );
        click(&row, cx);
        assert_eq!(tabs(&workspace, cx), before, "a click opens nothing");

        let node = workspace.read_with(cx, |workspace, cx| workspace.selected_node(cx));
        let node = node.expect("the clicked row is selected");
        assert_eq!(
            (node.kind, node.name.as_str(), node.schema.as_deref()),
            (crate::sidebar::NodeKind::Sequence, "ticket", Some("main"))
        );
        let actions = crate::sidebar::node_actions(
            node.kind,
            &node.name,
            crate::db::Target::built_in(Engine::DuckDb),
        );
        assert_eq!(actions.len(), 2, "its name and its definition");

        cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
        for action in actions {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.run_node_action(&node, action, window, cx)
                })
            });
            settle(cx);
        }
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        assert_eq!(copied, "ticket");
        assert_eq!(front_dialog(&workspace, cx), Some("show-ddl"));
        let text = ddl_text(&workspace, cx).expect("the dialog holds a definition");
        assert!(text.starts_with("CREATE SEQUENCE"), "{text}");
        assert!(text.contains("ticket"), "{text}");
        assert!(
            cx.debug_bounds("show-ddl-diagram-choice").is_none(),
            "a sequence has no columns to draw"
        );
        assert_eq!(tabs(&workspace, cx), before, "the dialog is no tab");
        press(cx, "escape");
        quit(workspace, cx);
    });
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

/// What `SELECT current_database()` answers on the connection of the profile
/// in front, which is the one its own tabs run on.
fn own_database(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let connection = workspace.read_with(cx, |workspace, _| {
        workspace
            .profile()
            .and_then(|profile| profile.connection())
            .expect("the profile is connected")
    });
    let result = connection
        .query(
            "SELECT current_database()",
            &CancelToken::default(),
            Fetch::default(),
        )
        .expect("the connection answers");
    result.rows[0]
        .iter()
        .next()
        .flatten()
        .map(str::to_string)
        .expect("a database is named")
}

/// The DuckDB profile of `launch_duckdb`, connected and holding `other`
/// attached, with the connection listed again to say so.
fn duckdb_with_other_attached(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    let (workspace, cx) = launch_duckdb(cx, "mode = \"full\"");
    let at = connections_row(&workspace, cx, &connection_row("e2e")).expect("the connection shows");
    click_connections_row(cx, at);
    attach_other(&workspace, cx);
    (workspace, cx)
}

/// Attaches `other.duckdb` to the instance the profile in front is on, and
/// lists the connection's databases again to say so.
fn attach_other(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    let other = store::dbdelve_directory()
        .expect("the test home has a data directory")
        .join("other.duckdb");
    run_in_front(
        workspace,
        cx,
        &format!("ATTACH '{}' AS other", other.display()),
    );
    workspace.update(cx, |workspace, cx| workspace.refresh_schema(cx));
    settle(cx);
}

#[gpui::test]
fn select_database_lists_a_new_database_in_the_connections_tree(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_duckdb(cx, "mode = \"full\"");
        let at =
            connections_row(&workspace, cx, &connection_row("e2e")).expect("the connection shows");
        click_connections_row(cx, at);
        let other = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("other.duckdb");
        // Attached without the schema refresh that lists databases again, as
        // a database created on the server is.
        run_in_front(
            &workspace,
            cx,
            &format!("ATTACH '{}' AS other", other.display()),
        );
        let shown = |cx: &mut VisualTestContext| {
            connections_item(&workspace, cx, &database_row("e2e", "other")).is_some()
        };
        assert!(!shown(cx), "the tree has not been told of it yet");

        cx.dispatch_action(crate::actions::SelectDatabase);
        settle(cx);
        assert!(
            shown(cx),
            "the fetch that fills the Select Database list shows the database too"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_attached_duckdb_database_expands_over_a_session_of_its_own(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        let (own, other) = (database_row("e2e", "e2e"), database_row("e2e", "other"));
        assert_eq!(
            connections_item(&workspace, cx, &connection_row("e2e"))
                .map(|(_, rows)| rows.into_iter().map(|(id, ..)| id).collect::<Vec<_>>()),
            Some(vec![own.clone(), other.clone()]),
            "the own database and the one attached"
        );
        assert_eq!(
            connections_item(&workspace, cx, &other),
            Some((
                false,
                vec![(
                    scoped_row("e2e", Some("other"), "loading"),
                    LOADING.to_string(),
                    true
                )]
            )),
            "folded, and nothing has opened a session"
        );
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert!(profile.bound.is_empty() && profile.database_catalogs.is_empty());
        });

        let click = |id: &str, cx: &mut VisualTestContext| {
            let at = connections_row(&workspace, cx, id)
                .unwrap_or_else(|| panic!("{id} is not showing"));
            click_connections_row(cx, at);
        };
        click(&other, cx);
        let schema = scoped_row("e2e", Some("other"), "schema-main");
        let tables = scoped_row("e2e", Some("other"), "category-Tables-main");
        let u = scoped_row("e2e", Some("other"), &explorer::relation_row("main", "u"));
        click(&schema, cx);
        click(&tables, cx);

        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert!(matches!(
                profile.bound.get("other"),
                Some(crate::session::BoundSession::Open(_))
            ));
            match profile.database_catalogs.get("other") {
                Some(CatalogState::Loaded(catalog, _)) => {
                    let names: Vec<_> = catalog
                        .schemas
                        .iter()
                        .flat_map(|schema| &schema.relations)
                        .map(|relation| relation.name.as_str())
                        .collect();
                    assert_eq!(names, ["u"]);
                }
                _ => panic!("the catalog of `other` has not loaded"),
            }
            assert_eq!(profile.active_database().as_deref(), Some("e2e"));
        });
        assert!(
            connections_row(&workspace, cx, &u).is_some(),
            "`u` is listed under the database it is in"
        );
        // The profile's own tabs are where they were.
        assert_eq!(own_database(&workspace, cx), "e2e");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_session_lost_after_its_replacement_registered_leaves_the_replacement_in_place(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        let other = database_row("e2e", "other");
        let at = connections_row(&workspace, cx, &other).expect("`other` shows");
        click_connections_row(cx, at);
        workspace.update(cx, |workspace, _| {
            let profile = workspace.profile_mut().expect("the connection is in front");
            let Some(crate::session::BoundSession::Open(first)) = profile.bound.get("other") else {
                panic!("expanding `other` opened a session for it");
            };
            let first = first.clone();
            // Lost, and another opened in its place before the first one's
            // failure was told.
            let second = crate::session::Bound::new(first.connection.clone());
            profile.bound.insert(
                "other".into(),
                crate::session::BoundSession::Open(second.clone()),
            );
            assert!(!profile.is_registered("other", &first));
            profile.lose_session("other", &first);
            assert!(
                profile.is_registered("other", &second),
                "the first session's loss says nothing of the second"
            );
            profile.lose_session("other", &second);
            assert!(!profile.bound.contains_key("other"));
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn every_mode_change_reaches_the_sessions_opened_for_other_databases(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        let other = database_row("e2e", "other");
        let at = connections_row(&workspace, cx, &other).expect("`other` shows");
        click_connections_row(cx, at);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert_eq!(
                profile.sessions_to_hold().len(),
                1,
                "`other`'s, beside its own"
            );
        });

        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::ReadOnly, cx)
        });
        settle(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::ReadWrite, cx)
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.held, [2, 2], "both were handed to the hold");
            let Some(crate::session::BoundSession::Open(session)) =
                workspace.profile().unwrap().bound.get("other")
            else {
                panic!("`other`'s session is open");
            };
            assert!(
                session.holds.is_latest(2),
                "each change was stamped on `other`'s own line, which orders them"
            );
        });

        // A mode picked while a session is still opening finds nothing of it
        // to hold, so its landing drops it and opens another under the mode.
        workspace.update(cx, |workspace, cx| {
            let profile = workspace.profile_mut().expect("the connection is in front");
            profile.bound.clear();
            profile.database_catalogs.clear();
            workspace.expand_node(&other.clone().into(), cx);
            workspace.set_mode(crate::sql::Mode::ReadOnly, cx);
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.opened, ["other", "other", "other"]);
            let profile = workspace.profile().expect("the connection is in front");
            assert!(matches!(
                profile.bound.get("other"),
                Some(crate::session::BoundSession::Open(_))
            ));
            assert_eq!(workspace.held.last(), Some(&1), "only its own was open");
        });

        quit(workspace, cx);
    });
}

/// A tree row's menu hands its action the row's database as it was listed,
/// which may be the connection's own by the time the action runs.
#[gpui::test]
fn a_relation_opened_naming_the_connections_own_database_opens_on_the_connection(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_duckdb(cx, "mode = \"full\"");
        run_in_front(&workspace, cx, "CREATE TABLE birds (name text)");
        let own = workspace.read_with(cx, |workspace, _| {
            workspace
                .profile()
                .and_then(|profile| profile.active_database())
                .expect("DuckDB names the database it is on")
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_relation_view(
                Some(own),
                "main".into(),
                "birds".into(),
                crate::db::RelationKind::Table,
                false,
                window,
                cx,
            );
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            let tab = profile
                .session
                .objects
                .iter()
                .find(|tab| tab.name == "birds")
                .expect("the birds have a tab");
            assert_eq!(tab.database, None);
            assert!(workspace.opened.is_empty(), "no session was opened");
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn expanding_a_database_twice_while_it_opens_opens_one_session(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        let other = database_row("e2e", "other");
        workspace.update(cx, |workspace, cx| {
            let tree = workspace.connections.tree.clone();
            tree.update(cx, |_, cx| {
                for _ in 0..2 {
                    cx.emit(TreeEvent::Expanded(other.clone().into()));
                }
            });
        });
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert!(matches!(
                profile.bound.get("other"),
                Some(crate::session::BoundSession::Opening)
            ));
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.opened, ["other"], "one session was opened");
            let profile = workspace.profile().expect("the connection is in front");
            assert!(matches!(
                profile.database_catalogs.get("other"),
                Some(CatalogState::Loaded(..))
            ));
        });

        quit(workspace, cx);
    });
}

/// What the profile in front holds of the databases it opened sessions for:
/// how many sessions and how many catalogs.
fn held_of_other_databases(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (usize, usize) {
    workspace.read_with(cx, |workspace, _| {
        let profile = workspace.profile().expect("a profile is in front");
        (profile.bound.len(), profile.database_catalogs.len())
    })
}

#[gpui::test]
fn disconnecting_or_reconnecting_drops_every_session_and_database_catalog(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_duckdb(
            cx,
            "mode = \"full\"\n\
             [[profiles]]\n\
             id = \"z\"\n\
             name = \"Z\"\n\
             host = \"\"\n\
             database = \"\"\n\
             user = \"\"\n\
             engine = \"sqlite\"\n\
             path = \"z.db\"\n",
        );
        let at = connections_row(&workspace, cx, &connection_row("e2e")).expect("it shows");
        click_connections_row(cx, at);
        attach_other(&workspace, cx);
        let other = database_row("e2e", "other");
        let expand = |cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, cx| {
                workspace.expand_node(&other.clone().into(), cx)
            });
            settle(cx);
        };
        expand(cx);
        assert_eq!(held_of_other_databases(&workspace, cx), (1, 1));

        // Another profile's removal leaves this one's alone.
        workspace.update(cx, |workspace, cx| {
            let z = workspace.profiles.iter().position(|p| p.id == "z");
            workspace.remove_profile(z.expect("z is listed"), cx);
        });
        settle(cx);
        assert_eq!(held_of_other_databases(&workspace, cx), (1, 1));

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.disconnect(&Disconnect, window, cx);
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = &workspace.profiles[0];
            assert!(profile.bound.is_empty() && profile.database_catalogs.is_empty());
        });

        // Connected again, with the database attached again (an attachment
        // went with the instance), and expanded again.
        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        attach_other(&workspace, cx);
        expand(cx);
        assert_eq!(held_of_other_databases(&workspace, cx), (1, 1));
        workspace.update(cx, |workspace, cx| workspace.begin_connect(0, cx));
        assert_eq!(
            held_of_other_databases(&workspace, cx),
            (0, 0),
            "a connect leaves nothing of the connection it replaces"
        );
        settle(cx);
        assert_eq!(held_of_other_databases(&workspace, cx), (0, 0));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_database_that_cannot_be_opened_says_so_on_its_own_row(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        let (own, other) = (database_row("e2e", "e2e"), database_row("e2e", "other"));
        let before = connections_item(&workspace, cx, &own);
        run_in_front(&workspace, cx, "DETACH other");

        workspace.update(cx, |workspace, cx| {
            workspace.expand_node(&other.clone().into(), cx)
        });
        settle(cx);
        let said = workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("a profile is in front");
            assert!(profile.bound.is_empty(), "nothing is left half open");
            match profile.database_catalogs.get("other") {
                Some(CatalogState::Failed(message)) => message.clone(),
                _ => panic!("the failure is not on its catalog"),
            }
        });
        assert!(!said.is_empty());
        assert_eq!(
            connections_item(&workspace, cx, &other).map(|(_, rows)| rows),
            Some(vec![(
                scoped_row("e2e", Some("other"), "notice"),
                said,
                true
            )])
        );
        assert_eq!(connections_item(&workspace, cx, &own), before);

        // Another expand asks again: a failure is not a verdict.
        workspace.update(cx, |workspace, cx| {
            workspace.expand_node(&other.clone().into(), cx)
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.opened, ["other", "other"]);
        });

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_filter_says_how_many_connections_it_did_not_search(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
        settle(cx);
        // `a` connected; `b` never asked to load.
        let a = connections_row(&workspace, cx, &connection_row("a")).expect("a is showing");
        click_connections_row(cx, a);
        assert_eq!(profile_state(&workspace, cx, "b"), "idle");
        let footer = |cx: &mut VisualTestContext| cx.debug_bounds("connections-unsearched");
        assert!(footer(cx).is_none(), "no text, no line");

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let filter = workspace.connections.filter.clone();
                filter.update(cx, |input, cx| input.focus(window, cx));
            });
        });
        cx.simulate_input("bir");
        settle(cx);
        assert!(
            footer(cx).is_some(),
            "b was not searched, and the tree says so"
        );
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.connections.unsearched),
            1
        );
        assert_eq!(profile_state(&workspace, cx, "b"), "idle", "and stays idle");

        cx.simulate_keystrokes("secondary-a backspace");
        settle(cx);
        assert!(footer(cx).is_none(), "cleared, the line goes");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn typing_in_the_connections_filter_never_connects_or_loads_anything(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
        settle(cx);
        // `a` connected, with its schema opened; `b` never asked to load.
        for id in [connection_row("a"), scoped_row("a", None, "schema-main")] {
            let at = connections_row(&workspace, cx, &id).expect("the row is showing");
            click_connections_row(cx, at);
        }
        assert_eq!(profile_state(&workspace, cx, "b"), "idle");
        // Typed as a person types, into the field with the keyboard: only a
        // key press tells the tree what it holds.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let filter = workspace.connections.filter.clone();
                filter.update(cx, |input, cx| input.focus(window, cx));
            });
        });
        let type_in = |text: &str, cx: &mut VisualTestContext| {
            cx.simulate_keystrokes("secondary-a");
            match text.is_empty() {
                true => cx.simulate_keystrokes("backspace"),
                false => cx.simulate_input(text),
            }
            settle(cx);
        };
        let shown = |cx: &mut VisualTestContext| connections_rows(&workspace, cx);
        let before = shown(cx);
        let state = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                (
                    workspace.next_generation,
                    store::history("a").len(),
                    store::history("b").len(),
                )
            })
        };
        let untouched = state(cx);
        assert!(
            before
                .iter()
                .any(|(_, id)| *id == scoped_row("a", None, "schema-main"))
        );

        // `birds` is a's; `b` is idle, named Flocks, and holds nothing loaded.
        type_in("bir", cx);
        let narrowed = shown(cx);
        let birds = scoped_row("a", None, &explorer::relation_row("main", "birds"));
        assert!(narrowed.iter().any(|(_, id)| *id == birds), "{narrowed:?}");
        assert!(!narrowed.iter().any(|(_, id)| *id == connection_row("b")));
        assert_eq!(profile_state(&workspace, cx, "b"), "idle");
        assert_eq!(state(cx), untouched);

        // An idle connection is kept by its own name, and still not loaded.
        type_in("flo", cx);
        assert!(
            shown(cx).iter().any(|(_, id)| *id == connection_row("b")),
            "{:?}",
            shown(cx)
        );
        assert_eq!(profile_state(&workspace, cx, "b"), "idle");
        assert_eq!(state(cx), untouched);

        // Cleared, the tree is the user's again: every fold as it was.
        type_in("", cx);
        assert_eq!(shown(cx), before);
        assert_eq!(profile_state(&workspace, cx, "b"), "idle");
        assert_eq!(state(cx), untouched);
        assert!(workspace.read_with(cx, |workspace, _| {
            workspace.connections.folds_before_filter.is_none()
        }));

        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_connections_tree_takes_the_keyboard_and_escape_hands_it_back(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections_folded(cx);
        assert!(!sidebar_has_keyboard(&workspace, cx));

        let selected = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                let tree = workspace.connections.tree.read(cx);
                tree.selected_item().map(|item| item.id.to_string())
            })
        };

        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        assert!(sidebar_has_keyboard(&workspace, cx));
        assert_eq!(
            selected(cx),
            Some(connection_row("a")),
            "with no object open, the connection in front is the row"
        );
        // An arrow only moves a tree that holds the keyboard.
        cx.simulate_keystrokes("down");
        settle(cx);
        assert_eq!(selected(cx), Some(project_row("Zoo")));

        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(!sidebar_has_keyboard(&workspace, cx));
        assert!(editor_has_keyboard(&workspace, cx));

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

/// What the platform's dialog is asking, if one is up.
fn platform_prompt(cx: &mut VisualTestContext) -> Option<String> {
    cx.pending_prompt().map(|(message, _)| message)
}

/// The platform's dialog answered with the button labelled `button`.
fn answer_platform_prompt(cx: &mut VisualTestContext, button: &str) {
    cx.simulate_prompt_answer(button);
    settle(cx);
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
            workspace.open_import_file(
                profile,
                None,
                "main".into(),
                "birds".into(),
                file,
                window,
                cx,
            );
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
            workspace.import_rows_into(None, "main".into(), "birds".into(), window, cx);
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
                    None,
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
fn a_selection_failing_inside_a_bound_tabs_transaction_says_nothing_was_committed(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        cx.executor().allow_parking();
        new_query_here(&workspace, cx, Some("other"), None);
        // Opened before the selection, on the session of `other`, which the
        // profile's own flags know nothing of.
        run_in_front(&workspace, cx, "BEGIN");
        cx.update(|window, cx| {
            let editor = workspace
                .read(cx)
                .profile()
                .and_then(|profile| profile.session.active_query_tab())
                .expect("the bound tab is in front")
                .editor
                .clone();
            editor.update(cx, |input, cx| {
                input.set_value(
                    "create table probe (a integer); insert into probe values (1); \
                     select * from nope; insert into probe values (2)",
                    window,
                    cx,
                )
            });
        });
        cx.simulate_keystrokes("secondary-a secondary-enter");
        settle(cx);
        let failed = workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("a profile is active");
            let tab = profile.session.active_query_tab().expect("a query tab");
            let last = tab.queue.as_ref().and_then(|queue| queue.done.last());
            match &last.expect("a queue with results").state {
                QueryState::Failed(error) => error.message.clone(),
                _ => panic!("the third statement did not fail"),
            }
        });
        assert!(!failed.contains("committed"), "{failed}");

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
            ("Copy Name", "birds"),
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
        // The menu's Copy Name, the sixth item, copies what the palette's does.
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("before menu".into()));
        pick_from_explorer_menu(&workspace, cx, &explorer::relation_row("main", "birds"), 6);
        let named = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        assert_eq!(named, "birds");
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

/// What the `Show DDL` dialog holds, if it is up.
fn ddl_text(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .ddl_view
            .as_ref()
            .map(|view| match &view.sections[0].text {
                super::DdlText::Loading => "loading".to_string(),
                super::DdlText::Ready(text) => text.clone(),
                super::DdlText::Failed(error) => format!("failed: {error}"),
            })
    })
}

#[gpui::test]
fn show_ddl_opens_a_read_only_view_of_a_tables_definition_and_hands_the_keyboard_back(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let before = workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            (session.queries.len(), session.history.len())
        });
        let typed = buffer(&workspace, cx);
        assert!(editor_has_keyboard(&workspace, cx));

        workspace.update(cx, |workspace, cx| {
            workspace.show_ddl(
                None,
                "main".into(),
                "birds".into(),
                ObjectKind::Relation(RelationKind::Table),
                cx,
            )
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("show-ddl"));
        assert!(dialog_has_keyboard(&workspace, cx));
        let text = ddl_text(&workspace, cx).expect("the dialog holds a definition");
        assert!(text.contains("CREATE TABLE birds"), "{text}");
        assert!(
            cx.debug_bounds("show-ddl-text").is_some(),
            "the definition is drawn"
        );
        // A dialog, not a buffer: nothing a tab holds changed.
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert_eq!((session.queries.len(), session.history.len()), before);
        });
        assert_eq!(buffer(&workspace, cx), typed);

        press(cx, "escape");
        assert_eq!(front_dialog(&workspace, cx), None);
        assert_eq!(ddl_text(&workspace, cx), None);
        assert!(editor_has_keyboard(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_relation_menu_offers_show_ddl_where_the_target_copies_ddl(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        // Open Data, Open Structure, Show in Diagram, Import Rows, Export, then
        // the copies: Copy Name, then Copy DDL as the seventh item and Show DDL
        // the one after it.
        for (downs, copied, dialog) in [(7, true, None), (8, false, Some("show-ddl"))] {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("before menu".into()));
            let row = explorer_row(&workspace, "relation-main\0birds", cx).center();
            cx.simulate_event(gpui::MouseDownEvent {
                position: row,
                modifiers: gpui::Modifiers::default(),
                button: gpui::MouseButton::Right,
                click_count: 1,
                first_mouse: false,
            });
            settle(cx);
            cx.update(|window, _| {
                assert!(
                    window
                        .context_stack()
                        .iter()
                        .any(|context| context.contains("PopupMenu")),
                    "the row's menu is up"
                )
            });
            for _ in 0..downs {
                cx.simulate_keystrokes("down");
            }
            press(cx, "enter");
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default();
            assert_eq!(
                text.contains("CREATE TABLE birds"),
                copied,
                "item {downs}: {text}"
            );
            assert_eq!(front_dialog(&workspace, cx), dialog, "item {downs}");
            if dialog.is_some() {
                press(cx, "escape");
            }
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_empty_definition_says_so_and_offers_nothing_to_copy(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let put_up = |text: super::DdlText, cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, cx| {
                let profile = workspace
                    .profile()
                    .expect("a connection is in front")
                    .id
                    .clone();
                workspace.ddl_view = Some(super::DdlView {
                    profile,
                    name: "birds".into(),
                    sections: vec![super::DdlSection {
                        title: "Table main.birds".into(),
                        text,
                    }],
                    request: 1,
                });
                cx.notify();
            });
            settle(cx);
        };
        let clipboard = |cx: &mut VisualTestContext| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default()
        };

        put_up(super::DdlText::Ready(String::new()), cx);
        assert_eq!(front_dialog(&workspace, cx), Some("show-ddl"));
        assert!(cx.debug_bounds("show-ddl-empty").is_some());
        assert!(cx.debug_bounds("show-ddl-text").is_none());
        // Neither the button nor the ring puts anything on the clipboard.
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
        let copy = cx
            .debug_bounds("show-ddl-1")
            .expect("Copy is drawn")
            .center();
        cx.simulate_click(copy, gpui::Modifiers::default());
        settle(cx);
        press(cx, "left");
        assert_eq!(ringed_button(&workspace, cx), 1, "the ring is on Copy");
        press(cx, "enter");
        assert_eq!(clipboard(cx), "before");
        assert_eq!(
            front_dialog(&workspace, cx),
            Some("show-ddl"),
            "Copy leaves the dialog up"
        );

        // The same click copies a definition that is there.
        put_up(
            super::DdlText::Ready("CREATE TABLE birds (name text);".into()),
            cx,
        );
        let copy = cx
            .debug_bounds("show-ddl-1")
            .expect("Copy is drawn")
            .center();
        cx.simulate_click(copy, gpui::Modifiers::default());
        settle(cx);
        assert_eq!(clipboard(cx), "CREATE TABLE birds (name text);");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn show_ddl_reads_another_databases_definition_over_its_session(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        run_in_front(
            &workspace,
            cx,
            "ALTER TABLE other.main.t ADD COLUMN extra TEXT",
        );
        let shown = |database: Option<&str>, cx: &mut VisualTestContext| {
            let database = database.map(str::to_string);
            workspace.update(cx, |workspace, cx| {
                workspace.show_ddl(
                    database,
                    "main".into(),
                    "t".into(),
                    ObjectKind::Relation(RelationKind::Table),
                    cx,
                )
            });
            settle(cx);
            let text = ddl_text(&workspace, cx).expect("the dialog holds a definition");
            press(cx, "escape");
            text
        };
        let other = shown(Some("other"), cx);
        assert!(
            other.contains("extra"),
            "`other`'s `t`, which has `extra`: {other}"
        );
        let own = shown(None, cx);
        assert!(
            own.contains("CREATE TABLE t") && !own.contains("extra"),
            "the connection's own `t`: {own}"
        );
        // Asking for the own database by name is the connection's own `t` too.
        let named = shown(Some("e2e"), cx);
        assert_eq!(named, own);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_dialogs_of_another_databases_table_name_the_database(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        let heading = |database: Option<&str>, cx: &mut VisualTestContext| {
            let database = database.map(str::to_string);
            workspace.update(cx, |workspace, cx| {
                workspace.show_ddl(
                    database,
                    "main".into(),
                    "t".into(),
                    ObjectKind::Relation(RelationKind::Table),
                    cx,
                )
            });
            settle(cx);
            let heading = workspace.read_with(cx, |workspace, _| {
                workspace.ddl_view.as_ref().map(|view| view.heading())
            });
            press(cx, "escape");
            heading
        };
        assert_eq!(
            heading(Some("other"), cx).as_deref(),
            Some("Table other.main.t")
        );
        assert_eq!(
            heading(None, cx).as_deref(),
            Some("Table main.t"),
            "a table of the connection's own database is named as it was"
        );

        let file = store::dbdelve_directory().unwrap().join("t.csv");
        std::fs::write(&file, "name,id\nwren,2\n").unwrap();
        let named = |database: Option<&str>, cx: &mut VisualTestContext| {
            let (database, file) = (database.map(str::to_string), file.clone());
            workspace.update_in(cx, |workspace, window, cx| {
                let profile = workspace.profile().unwrap().id.clone();
                workspace.open_import_file(
                    profile,
                    database,
                    "main".into(),
                    "t".into(),
                    file,
                    window,
                    cx,
                );
            });
            settle(cx);
            workspace.read_with(cx, |workspace, _| {
                let dialog = workspace.profile().unwrap().session.import_rows.as_ref();
                dialog.map(|dialog| dialog.target_name())
            })
        };
        assert_eq!(named(Some("other"), cx).as_deref(), Some("other.main.t"));
        assert_eq!(named(None, cx).as_deref(), Some("main.t"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn copy_ddl_reads_another_databases_definition_over_its_session(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        run_in_front(
            &workspace,
            cx,
            "ALTER TABLE other.main.t ADD COLUMN extra TEXT",
        );
        for (database, has_extra) in [(Some("other"), true), (None, false)] {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
            workspace.update(cx, |workspace, cx| {
                workspace.copy_ddl(
                    database.map(str::to_string),
                    "main".into(),
                    "t".into(),
                    RelationKind::Table,
                    cx,
                )
            });
            settle(cx);
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default();
            assert!(text.contains("CREATE TABLE t"), "{database:?}: {text}");
            assert_eq!(text.contains("extra"), has_extra, "{database:?}: {text}");
        }
        quit(workspace, cx);
    });
}

/// `other`'s `t` is the only one with `extra`, so a read that lands on the
/// connection's own database after a bound tab's `USE` shows it without.
#[gpui::test]
fn a_use_in_a_bound_tab_moves_no_read_of_its_databases_objects(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        run_in_front(
            &workspace,
            cx,
            "ALTER TABLE other.main.t ADD COLUMN extra TEXT",
        );
        new_query_here(&workspace, cx, Some("other"), None);
        let leave = |cx: &mut VisualTestContext| {
            run_in_front(&workspace, cx, "USE e2e");
            assert_eq!(
                answer_in_the_first_query_tab(&workspace, cx, "SELECT current_database()")
                    .as_deref(),
                Some("e2e"),
                "the profile's own session is where it was"
            );
        };

        leave(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.show_ddl(
                Some("other".into()),
                "main".into(),
                "t".into(),
                ObjectKind::Relation(RelationKind::Table),
                cx,
            )
        });
        settle(cx);
        let shown = ddl_text(&workspace, cx).expect("the dialog holds a definition");
        assert!(shown.contains("extra"), "Show DDL after a USE: {shown}");
        press(cx, "escape");

        leave(cx);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
        workspace.update(cx, |workspace, cx| {
            workspace.copy_ddl(
                Some("other".into()),
                "main".into(),
                "t".into(),
                RelationKind::Table,
                cx,
            )
        });
        settle(cx);
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        assert!(copied.contains("extra"), "Copy DDL after a USE: {copied}");

        leave(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.show_ddl(
                Some("other".into()),
                "main".into(),
                "t".into(),
                ObjectKind::Relation(RelationKind::Table),
                cx,
            )
        });
        settle(cx);
        let shown = workspace.read_with(cx, |workspace, _| {
            match workspace
                .ddl_view
                .as_ref()
                .and_then(|view| view.sections.first())
                .map(|section| &section.text)
            {
                Some(super::DdlText::Ready(text)) => text.clone(),
                _ => panic!("the definition has not loaded"),
            }
        });
        assert!(shown.contains("extra"), "Show DDL after a USE: {shown}");
        press(cx, "escape");

        leave(cx);
        click_t_under(&workspace, cx, "other");
        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            let ObjectBody::Relation { structure, .. } = &tab.body else {
                panic!("a relation tab");
            };
            let crate::StructureState::Loaded(structure) = structure else {
                panic!("the structure has not loaded");
            };
            let names: Vec<_> = structure.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(names, ["name", "id", "extra"], "the structure after a USE");
        });
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

/// The app launched on one saved DuckDB profile over a database `schema` has
/// been run against, with its query tab in front. Its reads and the exports
/// run on threads the scheduler does not own, so it may park for them.
fn launch_on_duckdb_holding<'a>(
    cx: &'a mut TestAppContext,
    schema: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let directory = store::dbdelve_directory().expect("the test home has a data directory");
    std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
    let database = directory.join("e2e.duckdb");
    ::duckdb::Connection::open(&database)
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
    (workspace, cx)
}

/// A session moved on behind the modeless save panel still gets its rows, under
/// every column they hold, rather than what its connection now says of a table
/// of that name.
#[gpui::test]
fn a_session_changed_behind_the_save_panel_is_not_asked_about_the_table(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_duckdb_holding(
            cx,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, \
             twice INTEGER GENERATED ALWAYS AS (id * 2) VIRTUAL); \
             INSERT INTO t (id) VALUES (1);",
        );
        let directory = store::dbdelve_directory().expect("the test home has a data directory");
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

#[gpui::test]
fn the_palette_opens_the_schema_export_through_the_schema_picker(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_on_duckdb_holding(cx, "CREATE TABLE customers (id INTEGER PRIMARY KEY);");
        let active = workspace.read_with(cx, |workspace, _| {
            workspace
                .profile()
                .unwrap()
                .active_schema()
                .map(str::to_owned)
        });
        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        cx.simulate_input("Export Schema");
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(
            front_dialog(&workspace, cx),
            None,
            "the picker is not a dialog"
        );
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("schema-export"));
        workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.schema_export.as_ref();
            assert_eq!(Some(dialog.unwrap().schema.as_str()), active.as_deref());
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_database_switch_closes_the_schema_export_and_waits_for_a_running_export(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.open_schema_export("main".into(), cx);
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("schema-export"));

        // Its tables are the database's being left: an Export after the switch
        // would read the other database's.
        let switch = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.set_database("other".into(), window, cx)
                })
            })
        };
        assert!(switch(&workspace, cx));
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().unwrap().session.schema_export.is_none());
        });

        // A running export reads table after table on the connection the
        // switch would move.
        workspace.update(cx, |workspace, _| {
            workspace.profile_mut().unwrap().session.exporting = Some(crate::session::Exporting {
                file: "schema.sql".into(),
                started: std::time::Instant::now(),
                progress: Default::default(),
            });
        });
        let database = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let profile = workspace.profile().unwrap();
                profile.config.database().map(str::to_string)
            })
        };
        let before = database(&workspace, cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                assert!(!workspace.set_database("e2e".into(), window, cx));
            })
        });
        assert_eq!(database(&workspace, cx), before);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.profile().unwrap().session.notice.as_deref(),
                Some("An export is running, so the database was not switched.")
            );
        });
        workspace.update(cx, |workspace, _| {
            workspace.profile_mut().unwrap().session.exporting = None;
        });

        // A disconnect drops the connection the dialog read its keys on.
        workspace.update(cx, |workspace, cx| {
            workspace.open_schema_export("main".into(), cx);
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("schema-export"));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.disconnect(&Disconnect, window, cx);
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profiles[0].session.schema_export.is_none());
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_schema_export_dialog_takes_the_keyboard_and_hands_it_back(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_duckdb_holding(
            cx,
            "CREATE TABLE customers (id INTEGER PRIMARY KEY); \
             CREATE TABLE orders (id INTEGER PRIMARY KEY, \
             customer_id INTEGER REFERENCES customers (id));",
        );
        let read = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let dialog = workspace.profile().unwrap().session.schema_export.as_ref();
                dialog.map(|dialog| !matches!(dialog.keys, super::exports::Loading::Reading))
            })
        };

        workspace.update(cx, |workspace, cx| {
            workspace.open_schema_export("main".into(), cx);
        });
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("schema-export"));
        assert!(dialog_has_keyboard(&workspace, cx));
        for _ in 0..500 {
            if read(&workspace, cx) == Some(true) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            settle(cx);
        }
        workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.schema_export.as_ref();
            let dialog = dialog.expect("the dialog is still up");
            assert!(
                matches!(&dialog.keys, super::exports::Loading::Read(keys) if keys["orders"].len() == 1),
                "the foreign keys were not read"
            );
            assert_eq!(
                dialog.tables,
                vec![("customers".to_string(), true), ("orders".to_string(), true)]
            );
        });

        let before = buffer(&workspace, cx);
        cx.simulate_keystrokes("escape");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), None, "escape left it up");
        assert!(read(&workspace, cx).is_none());
        assert!(editor_has_keyboard(&workspace, cx));
        cx.simulate_input("x");
        settle(cx);
        assert_eq!(buffer(&workspace, cx), format!("{before}x"));

        // Tab walks every control: Structure, Rows, Select all, Select none,
        // each table, Cancel and Export. `space` unticks a table and `enter`
        // on Export asks where, and writes what is ticked.
        workspace.update(cx, |workspace, cx| {
            workspace.open_schema_export("main".into(), cx);
        });
        settle(cx);
        for _ in 0..500 {
            if read(&workspace, cx) == Some(true) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            settle(cx);
        }
        for _ in 0..5 {
            cx.simulate_keystrokes("tab");
            settle(cx);
            assert!(in_dialog(&workspace, cx));
        }
        press(cx, "space");
        workspace.read_with(cx, |workspace, _| {
            let dialog = workspace.profile().unwrap().session.schema_export.as_ref();
            assert_eq!(
                dialog.unwrap().tables,
                vec![
                    ("customers".to_string(), false),
                    ("orders".to_string(), true)
                ]
            );
        });
        cx.simulate_keystrokes("tab tab tab");
        settle(cx);
        press(cx, "enter");
        assert_eq!(front_dialog(&workspace, cx), None, "enter left it up");
        let path = store::dbdelve_directory().unwrap().join("main.sql");
        let chosen = path.clone();
        cx.simulate_new_path_selection(|_| Some(chosen));
        for _ in 0..500 {
            settle(cx);
            if path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let written = std::fs::read_to_string(&path).expect("the file is written");
        assert!(
            written.starts_with(
                "-- Schema \"main\" exported by DBDelve: 1 table, structure and rows.\n"
            ),
            "{written}"
        );

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
                let member = workspace.member_in(&front_id(workspace), leaf.target);
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
                let birds = workspace
                    .member_in(&front_id(workspace), leaf.target)
                    .unwrap();
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
                    let member = workspace
                        .member_in(&front_id(workspace), leaf.target)
                        .unwrap();
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
                        workspace.open_object(opened, None, window, cx).unwrap()
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
                let id = workspace.open_object(opened, None, window, cx).unwrap();
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
                let birds = workspace
                    .member_in(&front_id(workspace), leaf.target)
                    .unwrap();
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

#[cfg(debug_assertions)]
#[gpui::test]
fn result_grid_scrollbars_stay_outside_the_viewport(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table nests (id integer primary key);
             with recursive rows(n) as (values (1) union all select n + 1 from rows where n < 100)
             insert into nests select n from rows;",
        );
        cx.executor().allow_parking();
        let nests = explorer_row(&workspace, "relation-main\0nests", cx).center();
        cx.simulate_click(nests, gpui::Modifiers::default());
        settle(cx);

        let viewport = cx
            .debug_bounds("table-viewport")
            .expect("the grid is drawn");
        let horizontal = cx
            .debug_bounds("table-scrollbar-horizontal")
            .expect("the horizontal track is laid out");
        let vertical = cx
            .debug_bounds("table-scrollbar-vertical")
            .expect("the vertical track is laid out");
        assert_eq!(horizontal.top(), viewport.bottom());
        assert_eq!(vertical.left(), viewport.right());
        assert_eq!(horizontal.right(), vertical.left());
        assert_eq!(vertical.bottom(), horizontal.top());

        let row_height = gpui::px(theme::layout::grid(
            gpui_component::Size::Medium.table_row_height().into(),
        ));
        // The gutter takes the body from eight whole rows down to seven.
        let window_size = cx.update(|window, _| window.viewport_size());
        let viewport_height = row_height * 9. - horizontal.size.height / 2.;
        cx.simulate_resize(gpui::size(
            window_size.width,
            window_size.height + viewport_height - viewport.size.height,
        ));
        settle(cx);

        let viewport = cx.debug_bounds("table-viewport").unwrap();
        let grid = workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            let ObjectBody::Relation { results, .. } = &tab.body else {
                panic!("expected the relation preview");
            };
            results.clone()
        });
        cx.update(|window, cx| {
            grid.update(cx, |grid, cx| {
                grid.focus_handle(cx).focus(window, cx);
                grid.set_selected_cell(0, 0, cx);
            });
        });
        settle(cx);
        cx.simulate_keystrokes("pagedown");
        settle(cx);
        assert_eq!(
            grid.read_with(cx, |grid, _| grid.selected_cell()),
            Some((7, 0))
        );

        // Keep the table mounted to exercise the shared empty-table layout.
        grid.update(cx, |grid, cx| {
            drop(grid.delegate_mut().take_rows());
            cx.notify();
        });
        settle(cx);
        assert_eq!(
            cx.debug_bounds("table-viewport").unwrap().right(),
            viewport.right()
        );
        assert!(cx.debug_bounds("table-scrollbar-vertical").is_none());

        quit(workspace, cx);
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
fn a_launch_copying_imported_passwords_connects_once_they_are_copied(cx: &mut TestAppContext) {
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n[settings]\ncheck_for_updates = false\n[[profiles]]\n\
                 id = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\nuser = \"\"\n\
                 engine = \"sqlite\"\npath = {:?}\n",
                database.display().to_string()
            ),
        )
        .unwrap();
        let copy = directory.with_file_name("dbdelve-e2e-import");
        std::fs::create_dir_all(&copy).unwrap();
        store::stable_import::hand_over_an_empty_copy(&copy);

        let (workspace, cx) = launch(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(
                matches!(workspace.profiles[0].state, ProfileState::Idle),
                "no connect before the passwords are copied"
            );
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(matches!(
                workspace.profiles[0].state,
                ProfileState::Connected(_)
            ));
        });

        quit(workspace, cx);
    });
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
                None,
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
                None,
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
fn a_tab_of_another_database_offers_no_diagram_of_the_connections_own(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        let at =
            connections_row(&workspace, cx, &database_row("e2e", "other")).expect("`other` shows");
        click_connections_row(cx, at);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_relation_view(
                Some("other".into()),
                "main".into(),
                "u".into(),
                crate::db::RelationKind::Table,
                true,
                window,
                cx,
            );
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .and_then(|profile| profile.session.active_object())
                .expect("the structure of `other`'s table is in front");
            assert_eq!(tab.database.as_deref(), Some("other"));
            assert!(matches!(tab.body, ObjectBody::Structure { .. }));
        });
        assert!(cx.debug_bounds("show-diagram").is_none());
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
                None,
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
fn ssm_cleanup_notice_copies_only_session_id_and_preserves_other_notice(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.welcome_notice = Some("Existing notice".into());
            workspace
                .cleanup_notices
                .push(crate::db::ssm::CleanupResult {
                    accepted: false,
                    session: Some("alice-123".into()),
                    profile: Some("development".into()),
                    region: None,
                });
            cx.notify();
        });
        settle(cx);
        let copy = cx.debug_bounds("cleanup-copy-0").expect("copy action");
        cx.simulate_click(copy.center(), gpui::Modifiers::default());
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("alice-123".into())
        );
        let dismiss = cx
            .debug_bounds("cleanup-dismiss-0")
            .expect("dismiss action");
        cx.simulate_click(dismiss.center(), gpui::Modifiers::default());
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.cleanup_notices.is_empty());
            assert_eq!(workspace.welcome_notice.as_deref(), Some("Existing notice"));
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_cleanup_notice_without_session_has_no_copy_action(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        workspace.update(cx, |workspace, cx| {
            workspace
                .cleanup_notices
                .push(crate::db::ssm::CleanupResult {
                    accepted: false,
                    session: None,
                    profile: None,
                    region: None,
                });
            cx.notify();
        });
        settle(cx);
        assert!(cx.debug_bounds("cleanup-copy-0").is_none());
        assert!(cx.debug_bounds("cleanup-dismiss-0").is_some());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_cleanup_notice_survives_form_and_connected_workspace(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        workspace.update(cx, |workspace, cx| {
            workspace
                .cleanup_notices
                .push(crate::db::ssm::CleanupResult {
                    accepted: false,
                    session: Some("alice-123".into()),
                    profile: None,
                    region: None,
                });
            cx.notify();
        });
        settle(cx);
        assert!(
            cx.debug_bounds("cleanup-copy-0").is_some(),
            "connected workspace"
        );
        assert!(
            cx.debug_bounds("cleanup-profile: Profile: AWS CLI configuration")
                .is_some(),
            "profile wording"
        );
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.new_connection_in(None, window, cx)
        });
        settle(cx);
        assert!(
            cx.debug_bounds("cleanup-copy-0").is_some(),
            "connection form"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_cleanup_notice_keyboard_dismiss_keeps_actions_available(cx: &mut TestAppContext) {
    // CI's PG* settings must not replace this welcome-screen fixture.
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        workspace.update(cx, |workspace, cx| {
            workspace
                .cleanup_notices
                .push(crate::db::ssm::CleanupResult {
                    accepted: false,
                    session: None,
                    profile: None,
                    region: None,
                });
            cx.notify();
        });
        settle(cx);
        cx.update(|window, cx| {
            window.blur(cx);
            window.focus_next(cx);
            window.draw(cx).clear(cx);
        });
        press(cx, "enter");
        // Focus restoration happens during rendering. Draw its dispatch path
        // before sending the next shortcut on the headless platform.
        cx.update(|window, cx| window.draw(cx).clear(cx));
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.cleanup_notices.is_empty());
            assert!(workspace.form.is_none());
        });
        cx.update(|window, cx| {
            assert!(
                window.focused(cx).is_some(),
                "dismiss must retain a keyboard dispatch path"
            )
        });
        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        workspace.read_with(cx, |workspace, _| assert!(workspace.palette.is_some()));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_cleanup_notice_mouse_dismiss_after_tab_keeps_actions_available(cx: &mut TestAppContext) {
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let (workspace, cx) = launch(cx);
        workspace.update(cx, |workspace, cx| {
            workspace
                .cleanup_notices
                .push(crate::db::ssm::CleanupResult {
                    accepted: false,
                    session: None,
                    profile: None,
                    region: None,
                });
            cx.notify();
        });
        settle(cx);
        cx.update(|window, cx| {
            window.blur(cx);
            window.focus_next(cx);
            window.draw(cx).clear(cx);
        });
        let dismiss = cx
            .debug_bounds("cleanup-dismiss-0")
            .expect("dismiss action");
        cx.simulate_click(dismiss.center(), gpui::Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.cleanup_notices.is_empty())
        });
        cx.update(|window, cx| {
            assert!(
                window.focused(cx).is_some(),
                "a mouse dismiss must retain a keyboard dispatch path"
            )
        });
        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        workspace.read_with(cx, |workspace, _| assert!(workspace.palette.is_some()));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_disconnect_delivers_failed_cleanup_notice(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let attempt = crate::db::ConnectAttempt::default();
        attempt.finish().unwrap();
        let profile_index = workspace.update(cx, |workspace, _| {
            let index = workspace.active.unwrap();
            let profile = &mut workspace.profiles[index];
            assert!(matches!(profile.state, ProfileState::Connected(_)));
            profile.cleanup_attempt = Some(attempt.clone());
            index
        });
        disconnect(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.active.is_none());
            assert!(matches!(
                workspace.profiles[profile_index].state,
                ProfileState::Idle
            ));
            assert!(workspace.profiles[profile_index].cleanup_attempt.is_none());
            assert!(workspace.cleanup_notices.is_empty());
        });
        // Deliver after disconnect returns, as the background AWS request does.
        let result = crate::db::ssm::CleanupResult {
            accepted: false,
            session: Some("alice-123".into()),
            profile: Some("development".into()),
            region: Some("test-region".into()),
        };
        attempt.report_cleanup(result.clone());
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.cleanup_notices, vec![result]);
        });
        assert!(cx.debug_bounds("cleanup-copy-0").is_some());
        assert!(cx.debug_bounds("cleanup-dismiss-0").is_some());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_disconnect_does_not_warn_after_successful_cleanup(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let attempt = crate::db::ConnectAttempt::default();
        attempt.finish().unwrap();
        let profile_index = workspace.update(cx, |workspace, _| {
            let index = workspace.active.unwrap();
            let profile = &mut workspace.profiles[index];
            assert!(matches!(profile.state, ProfileState::Connected(_)));
            profile.cleanup_attempt = Some(attempt.clone());
            index
        });
        disconnect(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.active.is_none());
            assert!(workspace.profiles[profile_index].cleanup_attempt.is_none());
        });
        attempt.report_cleanup(crate::db::ssm::CleanupResult {
            accepted: true,
            session: Some("alice-123".into()),
            profile: Some("development".into()),
            region: Some("test-region".into()),
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.cleanup_notices.is_empty())
        });
        assert!(cx.debug_bounds("cleanup-copy-0").is_none());
        assert!(cx.debug_bounds("cleanup-dismiss-0").is_none());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_disconnect_while_connecting_delivers_missing_session_notice(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let attempt = crate::db::ConnectAttempt::default();
        workspace.update(cx, |workspace, _| {
            let profile = workspace.profile_mut().unwrap();
            profile.cleanup_attempt = Some(attempt.clone());
            profile.state = ProfileState::Connecting {
                _attempt: attempt.guard(),
            };
        });
        disconnect(cx);
        assert!(attempt.is_cancelled());
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.active.is_none());
            assert!(workspace.cleanup_notices.is_empty());
        });
        let result = crate::db::ssm::CleanupResult {
            accepted: false,
            session: None,
            profile: Some("development".into()),
            region: Some("test-region".into()),
        };
        attempt.report_cleanup(result.clone());
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.cleanup_notices, vec![result]);
        });
        assert!(cx.debug_bounds("cleanup-copy-0").is_none());
        let dismiss = cx
            .debug_bounds("cleanup-dismiss-0")
            .expect("dismiss action");
        cx.simulate_click(dismiss.center(), gpui::Modifiers::default());
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.cleanup_notices.is_empty());
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn ssm_disconnect_delivers_cleanup_completed_before_subscription(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let attempt = crate::db::ConnectAttempt::default();
        workspace.update(cx, |workspace, _| {
            let profile = workspace.profile_mut().unwrap();
            profile.cleanup_attempt = Some(attempt.clone());
            profile.state = ProfileState::Connecting {
                _attempt: attempt.guard(),
            };
        });
        // The worker can finish cleanup before the UI processes its connect failure.
        let result = crate::db::ssm::CleanupResult {
            accepted: false,
            session: None,
            profile: Some("development".into()),
            region: Some("test-region".into()),
        };
        attempt.report_cleanup(result.clone());
        disconnect(cx);
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.active.is_none());
            assert_eq!(workspace.cleanup_notices, vec![result]);
        });
        assert!(cx.debug_bounds("cleanup-copy-0").is_none());
        assert!(cx.debug_bounds("cleanup-dismiss-0").is_some());
        quit(workspace, cx);
    });
}

/// Whether the sidebar draws the connections tree: only it draws rows named
/// `connections-row-{n}`.
fn connections_tree_drawn(cx: &mut VisualTestContext) -> bool {
    cx.debug_bounds("connections-row-0").is_some()
}

fn view_mode_in_effect(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> ViewMode {
    workspace.read_with(cx, |workspace, _| workspace.view_mode())
}

/// `b`, in the Zoo project, and `a`, in none, with `settings` as `[settings]`
/// keys; `b` in front and connected.
fn zoo_and_a<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    // Left alone, CI's live job puts a connection from `PG*` among these.
    crate::IGNORE_ENVIRONMENT.set(true);
    let (workspace, cx) = launch_connections(
        cx,
        "create table birds (name text);",
        settings,
        &format!(
            "[[projects]]\nname = \"Zoo\"\nconnections = [\"b\"]\n{}{}",
            sqlite_profile("b", "Flocks", ""),
            sqlite_profile("a", "Birds", ""),
        ),
    );
    workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
    settle(cx);
    (workspace, cx)
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

#[gpui::test]
fn a_project_can_show_every_connection_while_the_global_setting_stays_simple(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = zoo_and_a(cx, "");
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Simple);
        assert_eq!(front_profile(&workspace, cx), "b");
        assert!(
            !connections_tree_drawn(cx),
            "the profile's own tree is drawn"
        );

        run_command(
            &workspace,
            cx,
            Command::SetProjectViewMode(0, Some(ViewMode::Complex)),
        );
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Complex);
        assert!(connections_tree_drawn(cx), "the connections tree is drawn");

        let saved = store::peek_profiles(&[]).expect("the file reads");
        assert_eq!(saved.projects[0].view_mode.as_deref(), Some("complex"));
        let global = saved.settings.and_then(|settings| settings.view_mode);
        assert!(
            matches!(global.as_deref(), None | Some("simple")),
            "{global:?}"
        );

        // The project in front decides: a connection in no project follows
        // the global setting, and Zoo's own again follows Zoo's.
        activate_by_id(&workspace, cx, "a");
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Simple);
        assert!(!connections_tree_drawn(cx), "the simple tree is back");
        activate_by_id(&workspace, cx, "b");
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Complex);
        assert!(connections_tree_drawn(cx));

        run_command(&workspace, cx, Command::SetProjectViewMode(0, None));
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Simple);
        let file = std::fs::read_to_string(
            store::dbdelve_directory()
                .expect("the home has a data directory")
                .join("profiles.toml"),
        )
        .expect("the file reads");
        assert!(!file.contains("complex"), "{file}");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn switching_the_view_mode_moves_no_tab_and_no_connection(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = zoo_and_a(cx, "");
        assert_eq!(profile_state(&workspace, cx, "b"), "connected");
        workspace.update_in(cx, |workspace, window, cx| {
            let editor = workspace
                .profile()
                .and_then(|profile| profile.session.active_query_tab())
                .expect("a profile opens on a query tab")
                .editor
                .clone();
            editor.update(cx, |input, cx| input.set_value("select 1", window, cx));
        });
        settle(cx);
        let snapshot = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            let text = buffer(workspace, cx);
            workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().expect("b is in front").session;
                (
                    session.queries.iter().map(|tab| tab.id).collect::<Vec<_>>(),
                    text,
                    session.active,
                    workspace.active,
                    workspace.next_generation,
                    profile_state_of(workspace, "b"),
                    profile_state_of(workspace, "a"),
                )
            })
        };
        let before = snapshot(&workspace, cx);
        assert_eq!(before.6, "idle");

        for _ in 0..2 {
            run_command(
                &workspace,
                cx,
                Command::SetProjectViewMode(0, Some(ViewMode::Complex)),
            );
            assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Complex);
            run_command(&workspace, cx, Command::SetProjectViewMode(0, None));
            assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Simple);
        }
        assert_eq!(snapshot(&workspace, cx), before);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_every_connection_switch_flips_the_sidebar_and_persists(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = zoo_and_a(cx, "");
        // Tall enough to draw the General tab's rows, which are drawn only
        // while in view.
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(2000.)));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.show_settings(window, cx)
        });
        settle(cx);
        let click = |selector: &'static str, cx: &mut VisualTestContext| {
            let bounds = cx.debug_bounds(selector).expect("the switch is drawn");
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            settle(cx);
        };
        let saved = || {
            store::peek_profiles(&[])
                .and_then(|list| list.settings)
                .expect("the settings are saved")
        };

        click("sidebar-every-connection", cx);
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Complex);
        assert_eq!(saved().view_mode.as_deref(), Some("complex"));
        click("sidebar-every-connection", cx);
        assert_eq!(view_mode_in_effect(&workspace, cx), ViewMode::Simple);
        assert_eq!(saved().view_mode.as_deref(), Some("simple"));

        click("sidebar-current-project", cx);
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.listing_scope()),
            ListingScope::Current
        );
        assert_eq!(saved().listing_scope.as_deref(), Some("current"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_project_can_list_only_its_own_connections_while_the_setting_lists_all(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = zoo_and_a(cx, "view_mode = \"complex\"");
        assert_eq!(
            connections_rows(&workspace, cx)
                .into_iter()
                .filter(|(depth, _)| *depth == 0)
                .map(|(_, id)| id)
                .collect::<Vec<_>>(),
            [NO_PROJECT_ROW.to_string(), project_row("Zoo")],
            "every project is listed"
        );

        run_command(
            &workspace,
            cx,
            Command::SetProjectListing(0, Some(ListingScope::Current)),
        );
        assert_eq!(
            connections_rows(&workspace, cx),
            [(0, connection_row("b"))],
            "Zoo lists its own connections, with no group row"
        );
        let saved = store::peek_profiles(&[]).expect("the file reads");
        assert_eq!(saved.projects[0].listing_scope.as_deref(), Some("current"));

        // A connection in no project follows the setting, which lists all.
        activate_by_id(&workspace, cx, "a");
        assert_eq!(
            connections_rows(&workspace, cx)
                .into_iter()
                .filter(|(depth, _)| *depth == 0)
                .count(),
            2
        );
        quit(workspace, cx);
    });
}

fn connections_label(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: &str,
) -> Option<String> {
    let index = connections_row(workspace, cx, id)?;
    workspace.read_with(cx, |workspace, cx| {
        let tree = workspace.connections.tree.read(cx);
        Some(tree.entry(index)?.item().label.to_string())
    })
}

fn selected_connections_row(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<String> {
    workspace.read_with(cx, |workspace, cx| {
        let tree = workspace.connections.tree.read(cx);
        tree.selected_item().map(|item| item.id.to_string())
    })
}

#[gpui::test]
fn adding_renaming_and_removing_a_connection_redraws_the_connections_tree(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        let config = workspace.read_with(cx, |workspace, _| workspace.profiles[0].config.clone());
        let id_named = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext, name: &str| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profiles
                    .iter()
                    .find(|profile| profile.name == name)
                    .map(|profile| profile.id.clone())
            })
        };
        let listed = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            connections_rows(workspace, cx)
                .into_iter()
                .filter(|(depth, _)| *depth == 1)
                .count()
        };
        assert_eq!(listed(&workspace, cx), 2);

        // The form's save of a new connection.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.create_profile(
                "Extra".into(),
                config.clone(),
                None,
                crate::sql::Mode::ReadOnly,
                crate::connection_form::Origin::Form,
                window,
                cx,
            );
        });
        settle(cx);
        let id = id_named(&workspace, cx, "Extra").expect("the connection was added");
        let row = connection_row(&id);
        assert_eq!(listed(&workspace, cx), 3);
        assert_eq!(
            connections_label(&workspace, cx, &row).as_deref(),
            Some("Extra")
        );

        // An edit, its name included, keeps the row.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.save_profile(&id, "Renamed".into(), config.clone(), None, window, cx);
        });
        settle(cx);
        assert_eq!(
            connections_label(&workspace, cx, &row).as_deref(),
            Some("Renamed")
        );
        assert_eq!(listed(&workspace, cx), 3);

        // A duplicate is a form until it is saved, and the save adds the row.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.duplicate_profile(0, window, cx);
        });
        settle(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.submit_connection(&SubmitConnection, window, cx);
        });
        settle(cx);
        assert_eq!(listed(&workspace, cx), 4);

        workspace.update(cx, |workspace, cx| {
            let index = workspace
                .profiles
                .iter()
                .position(|profile| profile.id == id)
                .expect("the connection is still there");
            workspace.remove_profile(index, cx);
        });
        settle(cx);
        assert_eq!(connections_row(&workspace, cx, &row), None);
        assert_eq!(listed(&workspace, cx), 3);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn disconnecting_redraws_the_connection_in_front_as_not_open(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        let a = connection_row("a");
        let schema = scoped_row("a", None, "schema-main");
        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        assert_eq!(profile_state(&workspace, cx, "a"), "connected");
        // Folded until it is opened, so its rows are one inert row until then.
        open_connections_rows(&workspace, cx, std::slice::from_ref(&a));
        let (_, rows) = connections_item(&workspace, cx, &a).expect("a is listed");
        assert!(rows.iter().any(|(id, ..)| id == &schema), "{rows:?}");

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.disconnect(&Disconnect, window, cx);
        });
        settle(cx);
        assert_eq!(profile_state(&workspace, cx, "a"), "idle");
        assert_eq!(
            connections_item(&workspace, cx, &a),
            Some((
                false,
                vec![(scoped_row("a", None, "loading"), LOADING.to_string(), true)]
            ))
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn moving_a_connection_to_another_project_keeps_its_rows_and_folds(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_connections(cx);
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
        settle(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.create_project("Other".into(), cx)
        });
        // `b` in front and connected, its schema open and a relation selected.
        activate_by_id(&workspace, cx, "b");
        assert_eq!(profile_state(&workspace, cx, "b"), "connected");
        let b = connection_row("b");
        let schema = scoped_row("b", None, "schema-main");
        let birds = scoped_row("b", None, &explorer::relation_row("main", "birds"));
        for row in [&b, &schema] {
            let at = connections_row(&workspace, cx, row).expect("the row is listed");
            click_connections_row(cx, at);
        }
        // A relation is under its category, which a reveal opens.
        reveal_connections_row(&workspace, cx, &birds);
        assert!(
            connections_row(&workspace, cx, &birds).is_some(),
            "the schema's relations are listed"
        );

        // What the rows of `b` are, and the group row they are drawn under.
        let kept = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            let group = connections_rows(workspace, cx)
                .into_iter()
                .take_while(|(_, id)| id != &b)
                .filter(|(depth, _)| *depth == 0)
                .last()
                .map(|(_, id)| id);
            (
                connections_row(workspace, cx, &b).is_some(),
                connections_item(workspace, cx, &schema).map(|(open, _)| open),
                selected_connections_row(workspace, cx),
                group,
            )
        };
        let rows = (true, Some(true), Some(birds.clone()));
        let before = kept(&workspace, cx);
        assert_eq!(
            before,
            (rows.0, rows.1, rows.2.clone(), Some(project_row("Zoo")))
        );

        let index = workspace.read_with(cx, |workspace, _| {
            workspace.profiles.iter().position(|p| p.id == "b").unwrap()
        });
        workspace.update(cx, |workspace, cx| {
            workspace.move_to_project(index, Some("Other"), cx)
        });
        settle(cx);
        assert_eq!(
            kept(&workspace, cx),
            (rows.0, rows.1, rows.2.clone(), Some(project_row("Other"))),
            "moved"
        );

        workspace.update(cx, |workspace, cx| {
            workspace.rename_project("Other", "Renamed".into(), cx)
        });
        settle(cx);
        assert_eq!(
            kept(&workspace, cx),
            (rows.0, rows.1, rows.2.clone(), Some(project_row("Renamed"))),
            "its project renamed"
        );

        workspace.update(cx, |workspace, cx| workspace.remove_project("Renamed", cx));
        settle(cx);
        assert_eq!(
            kept(&workspace, cx),
            (rows.0, rows.1, rows.2, Some(NO_PROJECT_ROW.to_string())),
            "its project deleted"
        );
        quit(workspace, cx);
    });
}

/// Where the connections tree drew `kind` (`logo`, `name`, `host`,
/// `count`, `status`, `fill`) for the connection `id`, if it did.
fn connection_part(
    cx: &mut VisualTestContext,
    kind: &str,
    id: &str,
) -> Option<gpui::Bounds<gpui::Pixels>> {
    // `debug_bounds` takes a `'static` selector; a test's few rows can leak.
    let selector: &'static str = format!("connections-{kind}-{id}").leak();
    cx.debug_bounds(selector)
}

/// The status the connections tree's dot for `id` was drawn in.
fn connection_dot(cx: &mut VisualTestContext, id: &str) -> Option<&'static str> {
    ["idle", "connecting", "connected", "failed"]
        .into_iter()
        .find(|status| {
            let selector: &'static str = format!("connections-dot-{id}-{status}").leak();
            cx.debug_bounds(selector).is_some()
        })
}

/// Two SQLite connections in complex mode, `a` in front and connected, each
/// with its own extra keys, and `settings` among the `[settings]`.
fn coloured_connections<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
    a: &str,
    b: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    // Left alone, CI's live job puts a connection from `PG*` among these.
    crate::IGNORE_ENVIRONMENT.set(true);
    let (workspace, cx) = launch_connections(
        cx,
        "create table birds (name text);",
        &format!("view_mode = \"complex\"\n{settings}"),
        &format!(
            "{}{}",
            sqlite_profile("a", "Birds", a),
            sqlite_profile("b", "Flocks", b)
        ),
    );
    settle(cx);
    (workspace, cx)
}

/// One DuckDB profile as `profiles.toml` spells it, over the file at `path`,
/// which is created holding `t`. A DuckDB connection counts the schemas it
/// lists; a SQLite one is flat and counts nothing.
fn duckdb_profile(id: &str, name: &str, path: &std::path::Path, extra: &str) -> String {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the directory must be creatable");
    }
    ::duckdb::Connection::open(path)
        .and_then(|connection| connection.execute_batch("CREATE TABLE t (id INTEGER);"))
        .expect("the schema must apply");
    format!(
        "[[profiles]]\nid = {id:?}\nname = {name:?}\nhost = \"\"\ndatabase = \"\"\n\
         user = \"\"\nengine = \"duckdb\"\npath = {:?}\n{extra}\n",
        path.display().to_string()
    )
}

#[gpui::test]
fn a_connection_row_carries_its_logo_status_and_colour(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        // Left alone, CI's live job puts a connection from `PG*` among these.
        crate::IGNORE_ENVIRONMENT.set(true);
        let directory = store::dbdelve_directory().expect("the test home has a data directory");
        let (workspace, cx) = launch_connections(
            cx,
            "",
            "view_mode = \"complex\"",
            &format!(
                "{}{}",
                duckdb_profile(
                    "a",
                    "Birds",
                    &directory.join("birds.duckdb"),
                    "color = \"red\""
                ),
                sqlite_profile("b", "Flocks", "")
            ),
        );
        settle(cx);
        assert_eq!(profile_state(&workspace, cx, "a"), "connected");

        let [logo, name, host, count, status] =
            ["logo", "name", "host", "count", "status"].map(|kind| {
                connection_part(cx, kind, "a")
                    .unwrap_or_else(|| panic!("the row of `a` draws its {kind}"))
            });
        assert!(
            connection_part(cx, "fill", "a").is_some(),
            "red fills its row"
        );
        let lefts = [logo, name, host, count, status].map(|bounds| bounds.left());
        assert!(
            lefts.windows(2).all(|pair| pair[0] < pair[1]),
            "logo, name, host, count, status, left to right: {lefts:?}"
        );
        assert_eq!(connection_dot(cx, "a"), Some("connected"));

        for kind in ["logo", "host", "status"] {
            assert!(
                connection_part(cx, kind, "b").is_some(),
                "`b` draws its {kind}"
            );
        }
        assert!(
            connection_part(cx, "fill", "b").is_none(),
            "`b` is uncoloured and fills nothing"
        );
        assert_eq!(connection_dot(cx, "b"), Some("idle"), "never absent");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_long_host_gives_way_before_the_connection_name(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        crate::IGNORE_ENVIRONMENT.set(true);
        let directory = store::dbdelve_directory().expect("the test home has a data directory");
        let long = directory
            .join("a".repeat(48))
            .join("b".repeat(48))
            .join("c".repeat(48))
            .join("birds.duckdb");
        let (workspace, cx) = launch_connections(
            cx,
            "",
            "view_mode = \"complex\"",
            &duckdb_profile("a", "Birds", &long, ""),
        );
        settle(cx);
        let name_before = connection_part(cx, "name", "a").expect("the name is drawn");
        let host_before = connection_part(cx, "host", "a").expect("the host is drawn");

        workspace.update_in(cx, |workspace, window, cx| {
            let narrowest = gpui::px(crate::theme::layout::SIDEBAR_MIN_WIDTH);
            workspace.sidebar_width.set(narrowest);
            workspace
                .shell_split
                .update(cx, |state, cx| state.resize_panel(0, narrowest, window, cx));
        });
        settle(cx);

        let name = connection_part(cx, "name", "a").expect("the name is drawn");
        let host = connection_part(cx, "host", "a").expect("the host is drawn");
        let count = connection_part(cx, "count", "a").expect("the count is drawn");
        assert!(
            host.size.width < host_before.size.width,
            "the narrow sidebar took its room from the host"
        );
        assert_eq!(
            name.size.width, name_before.size.width,
            "the name keeps its text"
        );
        assert!(
            name.right() <= host.left(),
            "the host starts after the name"
        );
        assert!(
            host.right() <= count.left(),
            "the host ends before the count"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_status_dot_follows_the_connection(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        // `b` in front and connected; `a` never connected.
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_connections(
            cx,
            "create table t (id integer);",
            "view_mode = \"complex\"",
            &format!(
                "{}{}",
                sqlite_profile("b", "Flocks", ""),
                sqlite_profile("a", "Orders", "")
            ),
        );
        settle(cx);
        assert_eq!(connection_dot(cx, "a"), Some("idle"), "idle is a status");

        workspace.update(cx, |workspace, cx| {
            workspace.connections.tree.update(cx, |_, cx| {
                cx.emit(TreeEvent::Expanded(connection_row("a").into()));
            });
        });
        assert_eq!(profile_state(&workspace, cx, "a"), "connecting");
        cx.update(|window, cx| window.simulate_next_frame(cx));
        assert_eq!(connection_dot(cx, "a"), Some("connecting"));
        settle(cx);
        assert_eq!(connection_dot(cx, "a"), Some("connected"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn two_connections_of_one_colour_are_two_tinted_rows(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let tinted = "color = \"blue\"";
        let (workspace, cx) = coloured_connections(cx, "", tinted, tinted);
        for id in ["a", "b"] {
            for kind in ["fill", "status"] {
                assert!(
                    connection_part(cx, kind, id).is_some(),
                    "`{id}` draws its own {kind}"
                );
            }
        }
        let (a, b) = (
            connection_part(cx, "fill", "a").unwrap(),
            connection_part(cx, "fill", "b").unwrap(),
        );
        assert_ne!(a.origin.y, b.origin.y, "two rows, not one");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_density_setting_sets_the_connections_trees_row_height(cx: &mut TestAppContext) {
    use crate::{sidebar::Density, theme::layout};

    store::tests::with_home(|| {
        let (workspace, cx) = coloured_connections(cx, "sidebar_density = \"compact\"", "", "");
        let height = |cx: &mut VisualTestContext, selector: &'static str| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is drawn"))
                .size
                .height
        };
        assert_eq!(
            height(cx, "connections-row-0"),
            gpui::px(layout::chrome(22.))
        );

        workspace.update(cx, |workspace, cx| {
            workspace.set_density(Density::Comfortable, cx)
        });
        settle(cx);
        assert_eq!(
            height(cx, "connections-row-0"),
            gpui::px(layout::chrome(30.))
        );

        // The simple tree keeps its rows whatever the density.
        workspace.update(cx, |workspace, cx| {
            workspace.set_view_mode(ViewMode::Simple, cx)
        });
        settle(cx);
        let simple = height(cx, "explorer-row-0");
        for density in Density::ALL {
            workspace.update(cx, |workspace, cx| workspace.set_density(density, cx));
            settle(cx);
            assert_eq!(height(cx, "explorer-row-0"), simple, "{density:?}");
        }
        let saved = store::peek_profiles(&[]).expect("the file reads");
        let saved = saved.settings.and_then(|settings| settings.sidebar_density);
        assert_eq!(saved.as_deref(), Some(Density::ALL[2].slug()));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn hiding_the_host_drops_it_from_connection_rows(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = coloured_connections(cx, "", "", "");
        assert!(connection_part(cx, "host", "a").is_some());
        workspace.update(cx, |workspace, cx| workspace.set_show_host(false, cx));
        settle(cx);
        assert!(connection_part(cx, "host", "a").is_none());
        assert!(connection_part(cx, "host", "b").is_none());
        let saved = store::peek_profiles(&[]).expect("the file reads");
        assert_eq!(
            saved
                .settings
                .and_then(|settings| settings.sidebar_show_host),
            Some(false)
        );
        workspace.update(cx, |workspace, cx| workspace.set_show_host(true, cx));
        settle(cx);
        assert!(connection_part(cx, "host", "a").is_some());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn with_sizes_off_no_size_read_starts(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            launch_duckdb_with(cx, "sidebar_show_sizes = false", "mode = \"full\"");
        let reads = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| workspace.size_reads)
        };
        let at = connections_row(&workspace, cx, &connection_row("e2e")).expect("it shows");
        click_connections_row(cx, at);
        attach_other(&workspace, cx);
        let other = database_row("e2e", "other");
        let at = connections_row(&workspace, cx, &other).expect("the attached database shows");
        click_connections_row(cx, at);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert!(
                matches!(
                    profile.database_catalogs.get("other"),
                    Some(CatalogState::Loaded(..))
                ),
                "the other database loaded"
            );
        });
        assert_eq!(reads(&workspace, cx), 0, "no size was asked for");
        assert!(connection_part(cx, "count", "e2e").is_some(), "counts stay");

        workspace.update(cx, |workspace, cx| workspace.set_show_sizes(true, cx));
        settle(cx);
        assert_eq!(
            reads(&workspace, cx),
            2,
            "one for the connection and one for the loaded database"
        );
        quit(workspace, cx);
    });
}

/// Opens a tab with New Query Here on `database` and `schema` of the profile
/// in front, and waits for it to be the one in front.
fn new_query_here(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    database: Option<&str>,
    schema: Option<&str>,
) {
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.new_query_here(
                "e2e",
                database.map(str::to_string),
                schema.map(str::to_string),
                window,
                cx,
            );
        });
    });
    settle(cx);
}

/// The binding of the query tab in front.
fn front_binding(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<crate::session::Binding> {
    query_tab(workspace, cx, |tab, _, _| tab.binding.clone())
}

/// The first cell of the result in the query tab in front.
fn answer(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    query_tab(workspace, cx, |tab, _, cx| {
        let grid = tab.results.read(cx);
        let result = grid.delegate().result();
        result.rows.first()?.get(0).map(str::to_string)
    })
}

/// Whether the profile in front has left a transaction open or changed its
/// session, as its own tabs' statements are read to.
fn profile_flags(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (bool, bool) {
    workspace.read_with(cx, |workspace, _| {
        let profile = workspace.profile().expect("a profile is in front");
        (profile.in_transaction, profile.session_changed)
    })
}

#[gpui::test]
fn a_query_tab_opened_on_an_attached_database_runs_there_and_moves_nothing_else(
    cx: &mut TestAppContext,
) {
    use crate::session::Binding;
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("e2e"));
        let own = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });

        new_query_here(&workspace, cx, Some("other"), None);
        assert_eq!(
            front_binding(&workspace, cx),
            Some(Binding {
                database: "other".into(),
                schema: None
            })
        );
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("other"));
        assert_eq!(
            buffer(&workspace, cx),
            "SELECT current_database()",
            "the text is what was typed"
        );
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().unwrap();
            assert_eq!(profile.session.history[0].sql, "SELECT current_database()");
            assert_eq!(profile.active_database().as_deref(), Some("e2e"));
            assert!(matches!(
                profile.bound.get("other"),
                Some(crate::session::BoundSession::Open(_))
            ));
        });

        // A transaction of the user's on the shared session is that
        // session's, never the profile's.
        for statement in ["BEGIN", "SELECT current_database()", "ROLLBACK"] {
            run_in_front(&workspace, cx, statement);
            assert_eq!(profile_flags(&workspace, cx), (false, false), "{statement}");
        }
        assert_eq!(buffer(&workspace, cx), "ROLLBACK");

        // The profile's own tab is where it was.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.activate_tab(own, window, cx));
        });
        settle(cx);
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("e2e"));
        assert_eq!(own_database(&workspace, cx), "e2e");
        assert_eq!(profile_flags(&workspace, cx), (false, false));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn new_query_here_on_a_connection_row_opens_an_ordinary_tab(cx: &mut TestAppContext) {
    use crate::session::Binding;
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, None, None);
        assert_eq!(
            front_binding(&workspace, cx),
            None,
            "a connection row names no database"
        );
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("e2e"));
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace.profile().unwrap().bound.is_empty(),
                "the profile's own session ran it"
            );
        });

        new_query_here(&workspace, cx, Some(""), None);
        assert_eq!(
            front_binding(&workspace, cx),
            None,
            "a blank name is the login default"
        );

        new_query_here(&workspace, cx, Some("e2e"), None);
        assert_eq!(
            front_binding(&workspace, cx),
            None,
            "the connection's own database is its own session"
        );

        new_query_here(&workspace, cx, Some("other"), None);
        assert_eq!(
            front_binding(&workspace, cx),
            Some(Binding {
                database: "other".into(),
                schema: None
            })
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn new_query_in_complex_mode_opens_an_unbound_tab_on_the_profiles_own_session(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.new_query(&crate::actions::NewQuery, window, cx);
            });
        });
        settle(cx);
        assert_eq!(front_binding(&workspace, cx), None);
        assert_eq!(profile_flags(&workspace, cx), (false, false));
        run_in_front(&workspace, cx, "BEGIN");
        assert_eq!(
            profile_flags(&workspace, cx),
            (true, false),
            "the profile's own session, which keeps its transaction controls"
        );
        run_in_front(&workspace, cx, "ROLLBACK");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_session_a_bound_run_opened_is_kept_and_its_database_reads_over_it(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().unwrap().bound.is_empty());
        });
        new_query_here(&workspace, cx, Some("other"), None);
        run_in_front(&workspace, cx, "SELECT current_database()");
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert!(matches!(
                profile.bound.get("other"),
                Some(crate::session::BoundSession::Open(_))
            ));
            // Left at "Loading" the row would never read the database: a
            // registered session counts as already being read.
            assert!(
                matches!(
                    profile.database_catalogs.get("other"),
                    Some(CatalogState::Loaded(..))
                ),
                "the database's catalog was read over the session the run kept"
            );
        });

        // A second tab bound to the same database shares the session.
        new_query_here(&workspace, cx, Some("other"), None);
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("other"));
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.profile().unwrap().bound.len(), 1);
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_edit_applied_in_a_bound_tab_writes_to_its_database(cx: &mut TestAppContext) {
    use crate::session::Refresh;
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, Some("other"), None);
        // `u` is in `other` and in no other database: run on the profile's
        // own connection, this would fail.
        let tab = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });
        workspace.update(cx, |workspace, cx| {
            workspace.execute_unchecked(
                insert_into_u(),
                tab,
                Some(Refresh::Statement {
                    sql: "SELECT count(*) FROM u".into(),
                    verify: Some(verified_insert_into_u()),
                }),
                false,
                None,
                None,
                cx,
            );
        });
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| match &tab.query {
            QueryState::Complete { .. } => {}
            QueryState::Failed(error) => panic!("the edit failed: {error}"),
            _ => panic!("the edit never completed"),
        });
        assert_eq!(answer(&workspace, cx).as_deref(), Some("1"));
        quit(workspace, cx);
    });
}

/// The generated `INSERT` of 7 into `u`, as the grid writes it: an edit on a
/// target that verifies applies is sent through its verification, never as
/// the text alone.
fn insert_into_u() -> String {
    crate::sql::insert_row(Engine::DuckDb, "main", "u", &[("id", Some("7"))], &[])
        .expect("the INSERT is written")
}

/// [`insert_into_u`] as the one step of a verified apply.
fn verified_insert_into_u() -> crate::session::Verification {
    crate::session::Verification {
        steps: vec![crate::db::VerifiedStep {
            sql: insert_into_u().trim_end_matches(';').into(),
            kind: crate::db::StepKind::Insert,
            reread: None,
            before: Vec::new(),
            read_back: None,
            by_key: true,
        }],
        rows: vec!["added row 1".into()],
        identities: Vec::new(),
    }
}

/// Applies `INSERT INTO u VALUES (7)` from the tab in front as an edit does,
/// and reads back what the tab holds once it lands.
fn apply_insert_from_the_front_tab(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> QueryState {
    use crate::session::Refresh;
    let tab = workspace.read_with(cx, |workspace, _| {
        workspace.profile().unwrap().session.active
    });
    workspace.update(cx, |workspace, cx| {
        workspace.execute_unchecked(
            insert_into_u(),
            tab,
            Some(Refresh::Statement {
                sql: "SELECT count(*) FROM u".into(),
                verify: Some(verified_insert_into_u()),
            }),
            false,
            None,
            None,
            cx,
        );
    });
    settle(cx);
    query_tab(workspace, cx, |tab, _, _| tab.query.clone())
}

#[gpui::test]
fn an_edit_is_refused_while_a_transaction_is_open_on_its_databases_session(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, Some("other"), None);
        run_in_front(&workspace, cx, "BEGIN");

        // A second tab bound to `other` shares the session the BEGIN is on:
        // an edit applied there would join a transaction nothing shows.
        // Refused as an edit send is: the tab keeps what it showed and the
        // refusal is said beside the statement.
        new_query_here(&workspace, cx, Some("other"), None);
        assert!(
            !matches!(
                apply_insert_from_the_front_tab(&workspace, cx),
                QueryState::Failed(_)
            ),
            "the tab keeps what it showed"
        );
        let notice = shown_notice(&workspace, cx).unwrap_or_default();
        assert!(
            notice.contains("Not sent") && notice.contains("transaction"),
            "an edit must not join another tab's open transaction: {notice}"
        );

        // Ending it from either tab frees the session, and nothing was written.
        run_in_front(&workspace, cx, "ROLLBACK");
        run_in_front(&workspace, cx, "SELECT count(*) FROM u");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("0"));
        assert!(
            matches!(
                apply_insert_from_the_front_tab(&workspace, cx),
                QueryState::Complete { .. }
            ),
            "the edit runs once the transaction has ended"
        );
        assert_eq!(answer(&workspace, cx).as_deref(), Some("1"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_edit_issued_before_a_transaction_opened_on_its_session_is_refused_when_its_turn_comes(
    cx: &mut TestAppContext,
) {
    use crate::session::Refresh;
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, Some("other"), None);
        run_in_front(&workspace, cx, "SELECT count(*) FROM u");
        let tab = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });
        // Nothing is open when the edit is issued; another tab's BEGIN takes
        // the session before the edit's turn there comes.
        workspace.update(cx, |workspace, cx| {
            workspace.execute_unchecked(
                insert_into_u(),
                tab,
                Some(Refresh::Statement {
                    sql: "SELECT count(*) FROM u".into(),
                    verify: Some(verified_insert_into_u()),
                }),
                false,
                None,
                None,
                cx,
            );
            let Some(crate::session::BoundSession::Open(session)) =
                workspace.profile().unwrap().bound.get("other")
            else {
                panic!("the read opened a session for `other`");
            };
            _ = session.leave(Some(true), false, || Ok::<_, crate::db::DbError>(()));
        });
        settle(cx);
        // Refused as an edit send is: the tab keeps what it showed and the
        // refusal is said beside the statement.
        let notice = shown_notice(&workspace, cx).unwrap_or_default();
        assert!(
            notice.contains("Not sent") && notice.contains("transaction"),
            "an edit must not join a transaction opened while it waited: {notice}"
        );
        query_tab(&workspace, cx, |tab, _, _| {
            assert!(
                matches!(tab.query, QueryState::Complete { .. }),
                "the tab keeps the rows it showed"
            )
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_write_waiting_on_its_session_to_open_is_not_sent_once_the_mode_is_read_only(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, Some("other"), None);
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace.profile().unwrap().bound.is_empty(),
                "no session is open for `other`, so the run opens one"
            );
        });
        let tab = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });
        // Issued under Full, with the session still to open: the mode is
        // switched before the run's landing finds it open.
        workspace.update(cx, |workspace, cx| {
            workspace.execute_unchecked(
                "INSERT INTO u VALUES (7)".into(),
                tab,
                None,
                false,
                None,
                None,
                cx,
            );
            workspace.set_mode(crate::sql::Mode::ReadOnly, cx);
        });
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| match &tab.query {
            QueryState::Failed(error) => assert!(
                error.message.contains("Not sent") && error.message.contains("Read-only"),
                "{}",
                error.message
            ),
            _ => panic!("a write must not reach a session the mode no longer admits"),
        });
        run_in_front(&workspace, cx, "SELECT count(*) FROM u");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("0"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn every_row_of_a_bound_tabs_result_is_not_fetched_on_the_profiles_own_session(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        run_in_front(&workspace, cx, "SELECT 1");
        assert!(
            workspace.read_with(cx, |workspace, _| workspace.refetch().is_ok()),
            "an ordinary tab's statement is fetched again as it always was"
        );
        new_query_here(&workspace, cx, Some("other"), None);
        run_in_front(&workspace, cx, "SELECT 1");
        let refusal = workspace.read_with(cx, |workspace, _| workspace.refetch());
        assert_eq!(
            refusal.err(),
            Some(
                "this tab runs on the session of the database it is bound to, \
                 and every row would be fetched on the connection's own."
            )
        );
        quit(workspace, cx);
    });
}

/// A second window over the same home, as `launch` builds it, once the first
/// has gone: what a relaunch reads is what the first left in `profiles.toml`.
fn relaunch_closing(
    workspace: Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    close_window(workspace, cx);
    launch_again(cx)
}

/// The window gone, and with it the workspace, having written what it keeps.
fn close_window(workspace: Entity<Workspace>, cx: &mut VisualTestContext) {
    let released = workspace.downgrade();
    cx.update(|window, _| window.remove_window());
    drop(workspace);
    cx.cx.run_until_parked();
    // The window goes, and the workspace with it, writing its buffers on
    // release, only when effects are next flushed.
    cx.cx.update(|_| {});
    assert!(
        released.upgrade().is_none(),
        "the first workspace is released"
    );
}

/// A second window over the same home, once `close_window` has closed the
/// first.
fn launch_again(cx: &mut VisualTestContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let mut relaunched = None;
    let (_, cx) = cx.cx.add_window_view(|window, cx| {
        let entity = cx.new(|cx| Workspace::new(Vec::new(), window, cx));
        relaunched = Some(entity.clone());
        Root::new(entity, window, cx)
    });
    settle(cx);
    (relaunched.expect("the window builds the workspace"), cx)
}

/// Brings the query tab `bound` says is bound, or is not, to the front.
fn front_the_tab(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, bound: bool) {
    let tab = workspace.read_with(cx, |workspace, _| {
        let id = workspace
            .profile()
            .and_then(|profile| {
                profile
                    .session
                    .queries
                    .iter()
                    .find(|tab| tab.binding.is_some() == bound)
            })
            .expect("the tab is open")
            .id;
        Tab::Query(id)
    });
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| workspace.activate_tab(tab, window, cx));
    });
    settle(cx);
}

#[gpui::test]
fn a_bound_tab_keeps_its_binding_across_a_relaunch(cx: &mut TestAppContext) {
    use crate::session::Binding;
    let other = Binding {
        database: "other".into(),
        schema: None,
    };
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, Some("other"), None);
        let saved = store::peek_profiles(&[]).expect("the file reads");
        let tabs = &saved.profiles[0].open_queries;
        assert_eq!(
            tabs.iter()
                .map(|tab| tab.database.as_deref())
                .collect::<Vec<_>>(),
            [None, Some("other")],
            "the binding is written with its tab"
        );

        let (workspace, cx) = relaunch_closing(workspace, cx);
        assert_eq!(front_binding(&workspace, cx), Some(other.clone()));

        // `other` was attached to the first window's instance and is not on
        // the new one: the run says what the open said, and the tab is
        // still bound to `other`.
        cx.update(|window, cx| {
            let editor = workspace
                .read(cx)
                .profile()
                .and_then(|profile| profile.session.active_query_tab())
                .expect("the bound tab is in front")
                .editor
                .clone();
            editor.update(cx, |input, cx| {
                input.set_value("SELECT current_database()", window, cx)
            });
        });
        cx.dispatch_action(crate::actions::RunQuery);
        settle(cx);
        query_tab(&workspace, cx, |tab, _, _| match &tab.query {
            QueryState::Failed(error) => assert!(
                error.message.contains("other"),
                "the open's own words name the database: {}",
                error.message
            ),
            _ => panic!("a run on a database the server lacks must fail"),
        });
        assert_eq!(front_binding(&workspace, cx), Some(other.clone()));

        front_the_tab(&workspace, cx, false);
        attach_other(&workspace, cx);
        front_the_tab(&workspace, cx, true);
        assert_eq!(front_binding(&workspace, cx), Some(other));
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("other"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_use_in_a_bound_tab_is_undone_before_its_next_run(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, Some("other"), None);
        run_in_front(&workspace, cx, "USE e2e");
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert_eq!(
                session
                    .tab_notice
                    .as_ref()
                    .map(|(_, notice)| notice.as_str()),
                Some("The next run in this tab starts again on other.")
            );
        });
        run_in_front(&workspace, cx, "SELECT current_database()");
        assert_eq!(answer(&workspace, cx).as_deref(), Some("other"));
        assert!(
            !profile_flags(&workspace, cx).1,
            "the profile's own session is unmarked"
        );
        quit(workspace, cx);
    });
}

/// The DuckDB profile in complex mode with `other` attached, each file
/// holding a `t (name TEXT, id INTEGER PRIMARY KEY)` of its own: `own` in the
/// connection's, `other` in the attached one.
fn two_databases_each_holding_t(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    let (workspace, cx) = launch_duckdb_seeded(
        cx,
        "",
        "mode = \"full\"",
        "CREATE TABLE t (name TEXT, id INTEGER PRIMARY KEY); INSERT INTO t VALUES ('own', 1);",
        "CREATE TABLE t (name TEXT, id INTEGER PRIMARY KEY); INSERT INTO t VALUES ('other', 1);",
    );
    let at = connections_row(&workspace, cx, &connection_row("e2e")).expect("the connection shows");
    click_connections_row(cx, at);
    attach_other(&workspace, cx);
    (workspace, cx)
}

/// Clicks down the tree to `t` under `database` of the connection `e2e`,
/// opening each folder on the way, and the table last.
fn click_t_under(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, database: &str) {
    let rows = [
        database_row("e2e", database),
        scoped_row("e2e", Some(database), "schema-main"),
        scoped_row("e2e", Some(database), "category-Tables-main"),
        scoped_row("e2e", Some(database), &explorer::relation_row("main", "t")),
    ];
    for id in rows {
        // A folder already open stays open: only a row that is not showing
        // is one its parent has yet to reveal.
        let at =
            connections_row(workspace, cx, &id).unwrap_or_else(|| panic!("{id} is not showing"));
        click_connections_row(cx, at);
    }
}

/// The database and the first cell of the object tab in front.
fn front_object_database_and_cell(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<(Option<String>, Option<String>)> {
    workspace.read_with(cx, |workspace, cx| {
        let tab = workspace.profile()?.session.active_object()?;
        let ObjectBody::Relation { results, .. } = &tab.body else {
            return None;
        };
        let cell = results.read(cx).delegate().cell(0, 0).map(str::to_string);
        Some((tab.database.clone(), cell))
    })
}

#[gpui::test]
fn a_table_under_another_database_opens_that_databases_rows(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        click_t_under(&workspace, cx, "other");
        assert_eq!(
            front_object_database_and_cell(&workspace, cx),
            Some((Some("other".into()), Some("other".into()))),
            "the table under `other` reads `other`'s row"
        );
        let first = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });

        click_t_under(&workspace, cx, "e2e");
        assert_eq!(
            front_object_database_and_cell(&workspace, cx),
            Some((None, Some("own".into()))),
            "the same `main.t` under the connection's own database reads its own row"
        );
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert_eq!(session.objects.len(), 2, "two tabs, both listed");
            assert_ne!(session.active, first);
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_tab_of_another_database_counts_and_describes_it_there(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        run_in_front(
            &workspace,
            cx,
            "ALTER TABLE other.main.t ADD COLUMN extra TEXT",
        );
        run_in_front(
            &workspace,
            cx,
            "INSERT INTO other.main.t (name, id) VALUES ('second', 2)",
        );
        // `refresh_schema` reads `other`'s catalog again over its session.
        click_t_under(&workspace, cx, "other");
        let other = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });
        let Tab::Object(other_id) = other else {
            panic!("the table opened an object tab");
        };
        workspace.update(cx, |workspace, cx| workspace.count_rows(other_id, cx));
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            let ObjectBody::Relation {
                count, structure, ..
            } = &tab.body
            else {
                panic!("a relation tab");
            };
            assert!(
                matches!(count, crate::session::RowCount::Counted(_, 2)),
                "`other`'s two rows, not the connection's one"
            );
            let crate::StructureState::Loaded(structure) = structure else {
                panic!("the structure has not loaded");
            };
            let names: Vec<_> = structure.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(
                names,
                ["name", "id", "extra"],
                "`other`'s `t`, which has `extra`"
            );
        });

        click_t_under(&workspace, cx, "e2e");
        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            let ObjectBody::Relation { structure, .. } = &tab.body else {
                panic!("a relation tab");
            };
            let crate::StructureState::Loaded(structure) = structure else {
                panic!("the structure has not loaded");
            };
            let names: Vec<_> = structure.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(
                names,
                ["name", "id"],
                "the connection's own `t` has no `extra`"
            );
        });
        quit(workspace, cx);
    });
}

/// Runs `sql` in the profile's first query tab, which is its own and bound to
/// nothing, and reads the first cell of its answer. The tab in front is left
/// as it was.
fn answer_in_the_first_query_tab(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    sql: &str,
) -> Option<String> {
    let (front, first) = workspace.read_with(cx, |workspace, _| {
        let session = &workspace.profile().unwrap().session;
        (session.active, Tab::Query(session.queries[0].id))
    });
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.activate_tab(first, window, cx)
        });
    });
    settle(cx);
    run_in_front(workspace, cx, sql);
    let answered = answer(workspace, cx);
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.activate_tab(front, window, cx)
        });
    });
    settle(cx);
    answered
}

#[gpui::test]
fn an_edit_on_another_databases_table_is_written_there(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        click_t_under(&workspace, cx, "other");
        let grid = workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            match &tab.body {
                ObjectBody::Relation { results, .. } => results.clone(),
                _ => panic!("a relation tab"),
            }
        });
        grid.update(cx, |grid, _| {
            assert!(grid.delegate_mut().set_pending(
                0,
                0,
                crate::result_grid::NewValue::Value("edited".into())
            ));
        });
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            front_object_database_and_cell(&workspace, cx),
            Some((Some("other".into()), Some("edited".into()))),
            "the tab reads the edit back from the database it was written to"
        );
        assert_eq!(
            answer_in_the_first_query_tab(
                &workspace,
                cx,
                "SELECT name FROM other.main.t WHERE id = 1"
            )
            .as_deref(),
            Some("edited"),
            "`other.duckdb` changed"
        );
        assert_eq!(
            answer_in_the_first_query_tab(
                &workspace,
                cx,
                "SELECT name FROM e2e.main.t WHERE id = 1"
            )
            .as_deref(),
            Some("own"),
            "the connection's own file did not"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_export_of_another_databases_table_reads_it_there(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        click_t_under(&workspace, cx, "other");
        let path = store::dbdelve_directory()
            .expect("the test home has a data directory")
            .join("exported.csv");
        workspace.update(cx, |workspace, cx| {
            let refetch = workspace
                .refetch()
                .expect("the tab's rows can be fetched again");
            let (id, generation) = {
                let profile = workspace.profile().unwrap();
                (profile.id.clone(), profile.generation)
            };
            workspace.start_export(
                &id,
                generation,
                refetch,
                crate::export::InsertTarget::default(),
                path.clone(),
                cx,
            );
        });
        for _ in 0..20 {
            settle(cx);
            if workspace.read_with(cx, |workspace, _| {
                workspace.profile().unwrap().session.exporting.is_none()
            }) {
                break;
            }
        }
        let written = std::fs::read_to_string(&path).expect("the export wrote its file");
        assert!(written.contains("other"), "{written}");
        assert!(!written.contains("own"), "{written}");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_export_of_another_databases_table_minds_only_that_databases_transaction(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        click_t_under(&workspace, cx, "other");
        workspace.update(cx, |workspace, _| {
            workspace.profile_mut().unwrap().in_transaction = true;
        });
        assert!(
            workspace.read_with(cx, |workspace, _| workspace.refetch().is_ok()),
            "a transaction on the connection's own session is not where the rows came from"
        );
        workspace.update(cx, |workspace, _| {
            let profile = workspace.profile_mut().unwrap();
            profile.in_transaction = false;
            let Some(crate::session::BoundSession::Open(session)) = profile.bound.get("other")
            else {
                panic!("the rows were read over a session of `other`'s own");
            };
            _ = session.leave(Some(true), false, || Ok::<_, crate::db::DbError>(()));
        });
        assert!(
            workspace.read_with(cx, |workspace, _| workspace.refetch().is_err()),
            "a transaction on the session the rows came from refuses the export"
        );
        // A COMMIT that failed, one cancelled before it was sent among them,
        // may have left the transaction open; one that went through has not.
        workspace.update(cx, |workspace, _| {
            let Some(crate::session::BoundSession::Open(session)) =
                workspace.profile().unwrap().bound.get("other")
            else {
                panic!("the session of `other` is still open");
            };
            _ = session.leave(Some(false), false, || {
                Err::<(), _>(crate::db::DbError {
                    message: "Cancelled before it was sent.".into(),
                    position: None,
                })
            });
            assert!(session.in_transaction());
            _ = session.leave(Some(false), false, || Ok::<_, crate::db::DbError>(()));
            assert!(!session.in_transaction());
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_export_of_another_databases_table_is_refused_once_a_tab_changed_a_setting_on_its_session(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        // A setting the binding does not put back lasts on the session, and a
        // session of the export's own would not read under it.
        new_query_here(&workspace, cx, Some("other"), None);
        run_in_front(&workspace, cx, "SET threads = 2");
        click_t_under(&workspace, cx, "other");
        assert!(
            workspace.read_with(cx, |workspace, _| workspace.refetch().is_err()),
            "a setting changed on the session the rows came from refuses the export"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_tab_of_another_database_comes_back_after_a_relaunch(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        click_t_under(&workspace, cx, "other");
        let saved = store::peek_profiles(&[]).expect("the file reads");
        assert_eq!(
            saved.profiles[0]
                .open_objects
                .iter()
                .map(|object| object.database.as_deref())
                .collect::<Vec<_>>(),
            [Some("other")],
            "the database is written with its tab"
        );

        let (workspace, cx) = relaunch_closing(workspace, cx);
        workspace.read_with(cx, |workspace, _| {
            let objects = &workspace.profile().unwrap().session.objects;
            assert_eq!(
                objects
                    .iter()
                    .map(|tab| tab.database.as_deref())
                    .collect::<Vec<_>>(),
                [Some("other")],
                "the tab is restored on its database"
            );
        });

        // `other` was attached to the first window's instance and is not on
        // the new one. Attached again, the tab reads it.
        front_the_tab(&workspace, cx, false);
        attach_other(&workspace, cx);
        let id = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.objects[0].id
        });
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_tab(Tab::Object(id), window, cx);
                workspace.load_relation(id, cx);
            });
        });
        settle(cx);
        // Not the snapshot the tab was restored with: a run that completed.
        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            assert!(
                matches!(
                    &tab.body,
                    ObjectBody::Relation {
                        query: QueryState::Complete { .. },
                        stale: false,
                        ..
                    }
                ),
                "the reload ran on `other`"
            );
        });
        assert_eq!(
            front_object_database_and_cell(&workspace, cx),
            Some((Some("other".into()), Some("other".into())))
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn select_database_closes_only_the_own_databases_object_tabs(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        click_t_under(&workspace, cx, "e2e");
        click_t_under(&workspace, cx, "other");
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.profile().unwrap().session.objects.len(), 2);
        });

        // The picker is the simple mode's way to another database: the tabs
        // were opened in the connections tree, which only complex mode has.
        workspace.update(cx, |workspace, cx| {
            workspace.set_view_mode(crate::sidebar::ViewMode::Simple, cx)
        });
        settle(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_database("other".into(), window, cx)
            });
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let objects = &workspace.profile().unwrap().session.objects;
            assert_eq!(
                objects
                    .iter()
                    .map(|tab| tab.database.as_deref())
                    .collect::<Vec<_>>(),
                [Some("other")],
                "the connection's own tab went and the other database's stayed"
            );
        });

        // Its session was dropped with the switch's others and opens again
        // on the tab's next read.
        let id = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.objects[0].id
        });
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_tab(Tab::Object(id), window, cx);
                workspace.requery_relation(id, |_, _, _, _| true, cx);
            });
        });
        settle(cx);
        assert_eq!(
            front_object_database_and_cell(&workspace, cx),
            Some((Some("other".into()), Some("other".into())))
        );
        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .unwrap()
                .session
                .active_object()
                .unwrap();
            assert!(matches!(
                &tab.body,
                ObjectBody::Relation {
                    query: QueryState::Complete { .. },
                    ..
                }
            ));
        });
        quit(workspace, cx);
    });
}

/// `a` in front and `b` behind it, both connected and each on its one query
/// tab, over one SQLite file.
fn a_in_front_of_b<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    a_in_front_of_b_with(cx, settings, "")
}

/// `a_in_front_of_b`, `b` with `extra` among its scalar keys.
fn a_in_front_of_b_with<'a>(
    cx: &'a mut TestAppContext,
    settings: &str,
    extra: &str,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    // Left alone, CI's live job puts a connection from `PG*` among these.
    crate::IGNORE_ENVIRONMENT.set(true);
    let (workspace, cx) = launch_connections(
        cx,
        "create table birds (name text);",
        settings,
        &format!(
            "{}{}",
            sqlite_profile("a", "Birds", ""),
            sqlite_profile("b", "Flocks", extra)
        ),
    );
    cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
    activate_by_id(&workspace, cx, "b");
    activate_by_id(&workspace, cx, "a");
    (workspace, cx)
}

/// The strip's chips, left to right, by connection id.
fn strip_chips(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<(String, Tab)> {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .strip_chips()
            .into_iter()
            .map(|(index, tab)| (workspace.profiles[index].id.clone(), tab))
            .collect()
    })
}

fn chip_part(
    cx: &mut VisualTestContext,
    part: &str,
    profile: &str,
    tab: Tab,
) -> Option<gpui::Bounds<gpui::Pixels>> {
    // `debug_bounds` takes a `'static` selector; a test's few chips can leak.
    cx.debug_bounds(crate::views::chip_selector(part, profile, tab).leak())
}

fn new_query_tab(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.new_query(&crate::actions::NewQuery, window, cx);
        });
    });
    settle(cx);
}

#[gpui::test]
fn the_strip_in_complex_mode_lists_every_connections_tabs_and_a_foreign_chip_brings_its_connection_forward(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = a_in_front_of_b(cx, "view_mode = \"complex\"");
        new_query_tab(&workspace, cx);
        let chips = strip_chips(&workspace, cx);
        assert_eq!(
            chips.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["a", "a", "b"],
            "a's two tabs, then b's one"
        );
        let lefts: Vec<_> = chips
            .iter()
            .map(|(id, tab)| {
                chip_part(cx, "", id, *tab)
                    .unwrap_or_else(|| panic!("the chip of {id} {tab:?} is not drawn"))
                    .left()
            })
            .collect();
        assert!(lefts.windows(2).all(|pair| pair[0] < pair[1]), "{lefts:?}");
        for (id, tab) in &chips {
            assert!(
                chip_part(cx, "context", id, *tab).is_some(),
                "the chip of {id} {tab:?} names its connection"
            );
        }
        workspace.read_with(cx, |workspace, _| {
            let context = |index: usize, tab: Tab| {
                crate::views::chip_context(&workspace.profiles[index], tab, true)
            };
            assert_eq!(context(1, chips[2].1).as_deref(), Some("Flocks"));
            assert_eq!(context(0, chips[0].1).as_deref(), Some("Birds"));
        });

        let (id, tab) = chips[2].clone();
        let at = chip_part(cx, "", &id, tab).unwrap().center();
        cx.simulate_click(at, gpui::Modifiers::default());
        settle(cx);
        assert_eq!(front_profile(&workspace, cx), "b");
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.profile().unwrap().session.active, tab);
        });
        assert_eq!(
            strip_chips(&workspace, cx),
            chips,
            "a's tabs are still listed"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_bound_tabs_chip_names_its_connection_and_binding(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        new_query_here(&workspace, cx, Some("other"), None);
        let chips = strip_chips(&workspace, cx);
        let (bound, plain) = (chips[1].clone(), chips[0].clone());
        let context = |cx: &mut VisualTestContext, complex: bool| {
            workspace.read_with(cx, |workspace, _| {
                let profile = &workspace.profiles[0];
                (
                    crate::views::chip_context(profile, bound.1, complex),
                    crate::views::chip_context(profile, plain.1, complex),
                )
            })
        };
        assert_eq!(
            context(cx, true),
            (Some("E2E · other".into()), Some("E2E".into()))
        );
        assert!(chip_part(cx, "context", &bound.0, bound.1).is_some());
        assert!(chip_part(cx, "context", &plain.0, plain.1).is_some());

        workspace.update(cx, |workspace, cx| {
            workspace.set_view_mode(ViewMode::Simple, cx)
        });
        settle(cx);
        assert_eq!(context(cx, false), (Some("other".into()), None));
        assert!(
            chip_part(cx, "context", &bound.0, bound.1).is_some(),
            "a bound chip says where it runs in simple mode"
        );
        assert!(
            chip_part(cx, "context", &plain.0, plain.1).is_none(),
            "an unbound chip says nothing in simple mode"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_strip_in_simple_mode_is_the_front_connections_alone(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = a_in_front_of_b(cx, "");
        new_query_tab(&workspace, cx);
        let chips = strip_chips(&workspace, cx);
        assert_eq!(
            chips.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["a", "a"]
        );
        for (id, tab) in &chips {
            assert!(chip_part(cx, "", id, *tab).is_some());
            assert!(chip_part(cx, "context", id, *tab).is_none());
        }
        let behind = workspace.read_with(cx, |workspace, _| {
            workspace.profiles[1].session.strip_tabs()
        });
        assert!(!behind.is_empty(), "b has a tab to leave out");
        for tab in behind {
            assert!(chip_part(cx, "", "b", tab).is_none());
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn only_the_front_connections_chips_can_be_dragged(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = a_in_front_of_b(cx, "view_mode = \"complex\"");
        let dragging = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| workspace.tab_strip.drag.is_some())
        };
        // Pressed and moved, not yet let go, which is when a drag is in hand.
        let pick_up = |cx: &mut VisualTestContext, id: &str, tab: Tab| {
            let from = chip_part(cx, "", id, tab)
                .expect("the chip is drawn")
                .center();
            let none = gpui::Modifiers::default();
            cx.simulate_mouse_down(from, gpui::MouseButton::Left, none);
            for at in [
                from + gpui::point(gpui::px(0.), gpui::px(8.)),
                from + gpui::point(gpui::px(12.), gpui::px(8.)),
            ] {
                cx.simulate_mouse_move(at, gpui::MouseButton::Left, none);
                cx.update(|window, cx| window.draw(cx).clear(cx));
            }
            let held = dragging(cx);
            // Let go well below the chip: over it, the press would be a click
            // and bring its connection forward.
            let off = from + gpui::point(gpui::px(12.), gpui::px(300.));
            cx.simulate_mouse_move(off, gpui::MouseButton::Left, none);
            cx.simulate_mouse_up(off, gpui::MouseButton::Left, none);
            settle(cx);
            held
        };
        let chips = strip_chips(&workspace, cx);
        let (a, b) = (chips[0].clone(), chips[1].clone());
        assert_eq!((a.0.as_str(), b.0.as_str()), ("a", "b"));

        assert!(pick_up(cx, &a.0, a.1), "the front connection's chip drags");
        assert!(
            !pick_up(cx, &b.0, b.1),
            "another connection's chip is not dragged"
        );
        assert_eq!(front_profile(&workspace, cx), "a");

        let at = chip_part(cx, "", &b.0, b.1).unwrap().center();
        cx.simulate_click(at, gpui::Modifiers::default());
        settle(cx);
        assert_eq!(front_profile(&workspace, cx), "b");
        assert!(pick_up(cx, &b.0, b.1), "once in front, its chip drags");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_coloured_connections_chips_follow_the_titlebar_setting(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) =
            a_in_front_of_b_with(cx, "view_mode = \"complex\"", "color = \"red\"");
        new_query_tab(&workspace, cx);
        let chips = strip_chips(&workspace, cx);
        assert_eq!(chips.len(), 3);
        let tinted = |cx: &mut VisualTestContext| -> Vec<bool> {
            chips
                .iter()
                .map(|(id, tab)| chip_part(cx, "tint", id, *tab).is_some())
                .collect()
        };
        assert_eq!(tinted(cx), [false, false, true], "b is coloured, a is not");

        workspace.update(cx, |workspace, cx| workspace.set_color_titlebar(false, cx));
        settle(cx);
        assert_eq!(tinted(cx), [false, false, false]);

        workspace.update(cx, |workspace, cx| workspace.set_color_titlebar(true, cx));
        settle(cx);
        assert_eq!(tinted(cx), [false, false, true]);

        // The colour is complex mode's: simple mode draws b's chips as before.
        activate_by_id(&workspace, cx, "b");
        workspace.update(cx, |workspace, cx| {
            workspace.set_view_mode(ViewMode::Simple, cx)
        });
        settle(cx);
        let (id, tab) = chips[2].clone();
        assert!(chip_part(cx, "", &id, tab).is_some());
        assert!(chip_part(cx, "tint", &id, tab).is_none());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_tab_chords_walk_every_connections_tabs_in_complex_mode(cx: &mut TestAppContext) {
    use crate::actions::{NextTab, PreviousTab};
    store::tests::with_home(|| {
        let (workspace, cx) = a_in_front_of_b(cx, "view_mode = \"complex\"");
        new_query_tab(&workspace, cx);
        let chips = strip_chips(&workspace, cx);
        assert_eq!(chips.len(), 3);
        let at = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                (
                    workspace.profile().unwrap().id.clone(),
                    workspace.profile().unwrap().session.active,
                )
            })
        };
        let next = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| workspace.next_tab(&NextTab, window, cx));
            });
            settle(cx);
        };
        let previous = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.previous_tab(&PreviousTab, window, cx)
                });
            });
            settle(cx);
        };
        // a is in front on its last tab.
        assert_eq!(at(cx), chips[1]);
        next(cx);
        assert_eq!(at(cx), chips[2], "b's first tab, and b in front");
        // The end of the strip wraps to its start.
        next(cx);
        assert_eq!(at(cx), chips[0]);
        previous(cx);
        assert_eq!(at(cx), chips[2]);
        previous(cx);
        assert_eq!(at(cx), chips[1], "back on a");
        quit(workspace, cx);
    });
}

/// Right-click the connections tree's row `index`, step `downs` items down its
/// menu and take that one. The menu's labels are not readable from outside
/// the widget, so a menu is compared by what each of its items does.
fn pick_from_connections_menu(cx: &mut VisualTestContext, index: usize, downs: usize) {
    // `debug_bounds` takes a `'static` selector; a test's few rows can leak.
    let selector: &'static str = format!("connections-row-{index}").leak();
    let at = cx
        .debug_bounds(selector)
        .expect("the row is drawn")
        .center();
    cx.simulate_event(gpui::MouseDownEvent {
        position: at,
        modifiers: gpui::Modifiers::default(),
        button: gpui::MouseButton::Right,
        click_count: 1,
        first_mouse: false,
    });
    settle(cx);
    cx.update(|window, _| {
        assert!(
            window
                .context_stack()
                .iter()
                .any(|context| context.contains("PopupMenu")),
            "the row's menu is up"
        )
    });
    for _ in 0..downs {
        cx.simulate_keystrokes("down");
    }
    press(cx, "enter");
}

#[gpui::test]
fn the_connections_trees_object_menu_is_simple_modes_menu(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_connections(
            cx,
            "create table birds (name text);",
            "view_mode = \"complex\"",
            &format!(
                "{}{}",
                sqlite_profile("a", "Bïrds \"prod\"", ""),
                sqlite_profile("b", "Flocks", ""),
            ),
        );
        let clipboard = |cx: &mut VisualTestContext| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default()
        };
        // Connection `b` is not in front. Its table's menu is the one simple
        // mode offers: the same item does the same thing at the same place.
        let b = connection_row("b");
        let at = connections_row(&workspace, cx, &b).expect("b is listed");
        click_connections_row(cx, at);
        for id in [
            scoped_row("b", None, "schema-main"),
            scoped_row("b", None, "category-Tables-main"),
        ] {
            let at = connections_row(&workspace, cx, &id).expect("the row is showing");
            click_connections_row(cx, at);
        }
        let birds = scoped_row("b", None, &explorer::relation_row("main", "birds"));
        // Open Data, Open Structure, Show in Diagram, Import Rows, Export,
        // then the copies: Copy Name, Copy DDL, and Show DDL right after it.
        for (downs, copied, dialog) in [
            (6, "birds", None),
            (7, "CREATE TABLE birds (name text);", None),
            (8, "before menu", Some("show-ddl")),
        ] {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("before menu".into()));
            let at = connections_row(&workspace, cx, &birds).expect("birds is showing");
            pick_from_connections_menu(cx, at, downs);
            assert_eq!(clipboard(cx), copied, "item {downs}");
            assert_eq!(front_dialog(&workspace, cx), dialog, "item {downs}");
            // Copying a name acts on nothing of the connection's.
            if downs > 6 {
                assert_eq!(
                    front_profile(&workspace, cx),
                    "b",
                    "the row's connection is in front once an item acts on it"
                );
            }
            if dialog.is_some() {
                press(cx, "escape");
            }
            // Back to `a`, so each item has to bring `b` forward itself.
            workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
            settle(cx);
        }

        // A connection's Copy Name is its name, whatever characters it holds.
        // New Query Here, Copy Name, Refresh.
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("before menu".into()));
        let at = connections_row(&workspace, cx, &connection_row("a")).expect("a is listed");
        pick_from_connections_menu(cx, at, 2);
        assert_eq!(clipboard(cx), "Bïrds \"prod\"");

        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_saved_querys_row_in_the_connections_tree_has_its_menu(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        crate::IGNORE_ENVIRONMENT.set(true);
        store::write_query("b", "nightly", "select 1").expect("the query saves");
        let (workspace, cx) = launch_connections(
            cx,
            "create table birds (name text);",
            "view_mode = \"complex\"",
            &format!(
                "{}{}",
                sqlite_profile("a", "Birds", ""),
                sqlite_profile("b", "Flocks", ""),
            ),
        );
        let at = connections_row(&workspace, cx, &connection_row("b")).expect("b is listed");
        click_connections_row(cx, at);
        let query = scoped_row("b", None, &explorer::query_row("nightly"));
        if connections_row(&workspace, cx, &query).is_none() {
            let group = scoped_row("b", None, explorer::FILES_GROUP);
            let at = connections_row(&workspace, cx, &group).expect("b's queries are listed");
            click_connections_row(cx, at);
        }
        workspace.update(cx, |workspace, cx| workspace.activate(0, cx));
        settle(cx);
        assert_eq!(front_profile(&workspace, cx), "a");

        // Open, the first item, on `b`, which comes forward for it.
        let at = connections_row(&workspace, cx, &query).expect("the saved query is showing");
        pick_from_connections_menu(cx, at, 1);
        assert_eq!(front_profile(&workspace, cx), "b");
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert_eq!(
                session.saved_query_of(session.active_query_tab().unwrap()),
                Some("nightly")
            );
        });
        quit(workspace, cx);
    });
}

/// The tabs of the connection in front, as the number of query tabs.
fn query_tab_count(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    workspace.read_with(cx, |workspace, _| {
        workspace.profile().unwrap().session.queries.len()
    })
}

/// A right click on the simple tree's row `row`, `downs` items down its menu.
fn pick_from_explorer_menu(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    row: &str,
    downs: usize,
) {
    let at = explorer_row(workspace, row, cx).center();
    cx.simulate_event(gpui::MouseDownEvent {
        position: at,
        modifiers: gpui::Modifiers::default(),
        button: gpui::MouseButton::Right,
        click_count: 1,
        first_mouse: false,
    });
    settle(cx);
    for _ in 0..downs {
        cx.simulate_keystrokes("down");
    }
    press(cx, "enter");
}

#[gpui::test]
fn new_query_here_in_simple_mode_moves_the_schema_then_opens_a_tab(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_duckdb_seeded(
            cx,
            "",
            "mode = \"full\"",
            "CREATE TABLE t (id INTEGER); CREATE SCHEMA s2; CREATE TABLE s2.u (id INTEGER);",
            "CREATE TABLE u (id INTEGER);",
        );
        workspace.update(cx, |workspace, cx| {
            workspace.set_view_mode(crate::sidebar::ViewMode::Simple, cx)
        });
        settle(cx);
        let (before, schema) = workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().unwrap();
            (
                profile.session.queries.len(),
                profile.active_schema().map(str::to_string),
            )
        });
        assert_eq!(schema.as_deref(), Some("main"));

        // Show Diagram, Set as Active Schema, then New Query Here.
        pick_from_explorer_menu(&workspace, cx, "schema-s2", 3);
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().unwrap();
            assert_eq!(profile.active_schema(), Some("s2"));
            assert_eq!(
                profile.session.queries.len(),
                before + 1,
                "a new tab opened"
            );
            let front = profile
                .session
                .active_query_tab()
                .expect("the new tab is in front");
            assert!(
                front.binding.is_none(),
                "unbound: the profile moved, not the tab"
            );
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn new_query_here_in_simple_mode_stops_when_the_database_cannot_move(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_duckdb_seeded(
            cx,
            "",
            "mode = \"full\"",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO t VALUES (1, 'wren');",
            "CREATE TABLE u (id INTEGER);",
        );
        workspace.update(cx, |workspace, cx| {
            workspace.set_view_mode(crate::sidebar::ViewMode::Simple, cx)
        });
        settle(cx);
        cx.simulate_input("select * from t");
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
        let before = query_tab_count(&workspace, cx);

        // No local engine draws its schema rows as databases, so the row's
        // meaning is what the caller says it is.
        let moved = cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.move_to_schema_row("other".into(), true, window, cx)
            })
        });
        assert!(!moved, "the database was not switched");
        assert_eq!(query_tab_count(&workspace, cx), before);
        workspace.read_with(cx, |workspace, _| {
            let notice = workspace.profile().unwrap().session.notice.clone();
            assert!(
                notice
                    .as_deref()
                    .is_some_and(|notice| notice.ends_with("so the database was not switched.")),
                "{notice:?}"
            );
        });

        grid.update(cx, |grid, _| grid.delegate_mut().discard_pending());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn refresh_on_another_database_rereads_it_over_its_session(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        let other = database_row("e2e", "other");
        let at = connections_row(&workspace, cx, &other).expect("other is listed");
        click_connections_row(cx, at);
        let tables = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                match workspace.profile().unwrap().database_catalogs.get("other") {
                    Some(CatalogState::Loaded(catalog, _)) => catalog
                        .schemas
                        .iter()
                        .flat_map(|schema| schema.relations.iter().map(|r| r.name.clone()))
                        .collect::<Vec<_>>(),
                    _ => panic!("the catalog of `other` has not loaded"),
                }
            })
        };
        assert_eq!(tables(cx), ["t"]);
        run_in_front(&workspace, cx, "CREATE TABLE other.main.fresh (id INTEGER)");
        run_in_front(&workspace, cx, "CREATE TABLE fresh_own (id INTEGER)");
        assert_eq!(tables(cx), ["t"], "nothing has read it again");

        // New Query Here, Copy Name, Refresh.
        let at = connections_row(&workspace, cx, &other).expect("other is listed");
        pick_from_connections_menu(cx, at, 3);
        settle(cx);
        let mut listed = tables(cx);
        listed.sort();
        assert_eq!(listed, ["fresh", "t"]);
        // Only that database was read: the connection's own is as it was.
        workspace.read_with(cx, |workspace, _| {
            let own = workspace.catalog().expect("the connection's own catalog");
            let names: Vec<_> = own
                .schemas
                .iter()
                .flat_map(|schema| schema.relations.iter().map(|r| r.name.as_str()))
                .collect();
            assert_eq!(names, ["t"]);
        });
        quit(workspace, cx);
    });
}

/// Makes the simple tree's row `row` the selected one, as arrowing to it does,
/// without opening it.
fn select_explorer_row(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, row: &str) {
    workspace.update(cx, |workspace, cx| {
        let tree = workspace.profile().unwrap().session.explorer_tree.clone();
        tree.update(cx, |tree, cx| {
            let at = tree.index_of(&row.to_string().into());
            assert!(at.is_some(), "{row} is listed");
            tree.set_selected_index(at, cx);
        });
    });
    settle(cx);
}

#[gpui::test]
fn the_palette_offers_the_selected_nodes_actions_as_its_menu_does(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let offered = |cx: &mut VisualTestContext| {
            cx.simulate_keystrokes("secondary-shift-p");
            settle(cx);
            let labels = palette_labels(&workspace, cx);
            cx.simulate_keystrokes("escape");
            settle(cx);
            ["New Query Here", "Copy Name", "Show DDL", "Refresh"]
                .map(|label| labels.iter().any(|shown| shown == label))
        };

        assert_eq!(offered(cx), [false; 4], "nothing is selected");
        select_explorer_row(&workspace, cx, &explorer::relation_row("main", "birds"));
        assert_eq!(
            offered(cx),
            [false, true, true, false],
            "a table offers what its menu does: its name and its definition"
        );
        select_explorer_row(&workspace, cx, "schema-main");
        assert_eq!(
            offered(cx),
            [true, true, false, true],
            "a schema offers a query here, its name and a refresh"
        );

        select_explorer_row(&workspace, cx, &explorer::relation_row("main", "birds"));
        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        cx.simulate_input("Show DDL");
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("show-ddl"));
        let text = ddl_text(&workspace, cx).expect("the dialog holds a definition");
        assert!(text.contains("CREATE TABLE birds"), "{text}");
        press(cx, "escape");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn no_node_action_sends_a_generated_statement(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        let counts = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().unwrap().session;
                (store::history("e2e").len(), session.history.len())
            })
        };
        let before = counts(&workspace, cx);
        let nodes = workspace.read_with(cx, |workspace, _| {
            let id = front_id(workspace);
            let leaf = workspace.profile().unwrap().session.explorer_leaves
                [explorer::relation_row("main", "birds").as_str()];
            [
                workspace.row_node(
                    &id,
                    None,
                    &explorer::relation_row("main", "birds"),
                    Some(leaf),
                ),
                workspace.row_node(&id, None, "schema-main", None),
            ]
        });
        for node in nodes
            .into_iter()
            .map(|node| node.expect("the row is a node"))
        {
            for action in crate::sidebar::node_actions(
                node.kind,
                &node.name,
                crate::db::Target::built_in(Engine::Sqlite),
            ) {
                cx.update(|window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.run_node_action(&node, action, window, cx)
                    })
                });
                settle(cx);
                if front_dialog(&workspace, cx).is_some() {
                    press(cx, "escape");
                }
            }
        }
        assert_eq!(counts(&workspace, cx), before, "no statement was recorded");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_palette_row_acts_on_the_connection_of_the_node_it_was_built_for(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_connections(
            cx,
            "create table birds (name text);",
            "view_mode = \"complex\"",
            &format!(
                "{}{}",
                sqlite_profile("a", "Birds", ""),
                sqlite_profile("b", "Flocks", ""),
            ),
        );
        for id in [
            connection_row("b"),
            scoped_row("b", None, "schema-main"),
            scoped_row("b", None, "category-Tables-main"),
        ] {
            let at = connections_row(&workspace, cx, &id).expect("the row is showing");
            click_connections_row(cx, at);
        }
        // Selected by arrowing to it, which opens nothing and moves nothing.
        let birds = scoped_row("b", None, &explorer::relation_row("main", "birds"));
        workspace.update(cx, |workspace, cx| {
            let tree = workspace.connections.tree.clone();
            tree.update(cx, |tree, cx| {
                let at = tree.index_of(&birds.clone().into());
                tree.set_selected_index(at, cx);
            });
        });
        settle(cx);
        assert_eq!(front_profile(&workspace, cx), "a");

        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        cx.simulate_input("Show DDL");
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("show-ddl"));
        assert_eq!(
            front_profile(&workspace, cx),
            "b",
            "the node's connection is in front once the row acts"
        );
        press(cx, "escape");
        quit(workspace, cx);
    });
}

/// Connections `a` and `b` over one file holding `birds` and `flocks`, both
/// open and with their tables showing in the connections tree.
fn selection_fixture(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    crate::IGNORE_ENVIRONMENT.set(true);
    let (workspace, cx) = launch_connections(
        cx,
        "create table birds (name text); create table flocks (name text);",
        "view_mode = \"complex\"",
        &format!(
            "{}{}",
            sqlite_profile("a", "Birds", ""),
            sqlite_profile("b", "Flocks", ""),
        ),
    );
    // Tall enough to draw every row of both connections, which a click needs.
    cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
    settle(cx);
    for id in ["a", "b"] {
        let at = connections_row(&workspace, cx, &connection_row(id)).unwrap();
        click_connections_row(cx, at);
    }
    // `a`'s `birds` is revealed last, so it is the row the tree has selected.
    for id in ["b", "a"] {
        for name in ["flocks", "birds"] {
            reveal_connections_row(&workspace, cx, &table_row(id, name));
        }
    }
    (workspace, cx)
}

/// The connections tree's row of `name`, a table of connection `id`.
fn table_row(id: &str, name: &str) -> String {
    scoped_row(id, None, &explorer::relation_row("main", name))
}

/// A click on the connections tree's row `id`, with `modifiers` held.
fn click_connections_row_with(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: &str,
    modifiers: gpui::Modifiers,
) {
    let index = connections_row(workspace, cx, id).expect("the row is showing");
    let selector: &'static str = format!("connections-row-{index}").leak();
    let bounds = cx.debug_bounds(selector).expect("the row is drawn");
    cx.simulate_click(bounds.center(), modifiers);
    settle(cx);
}

/// The names of the object tabs of connection `id`, in tab order.
fn object_tab_names(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: &str,
) -> Vec<String> {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .expect("the profile was restored")
            .session
            .objects
            .iter()
            .map(|tab| tab.name.clone())
            .collect()
    })
}

/// The ids of the rows the connections tree has selected, and the row the
/// tree itself has.
fn selected_rows(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (Vec<String>, Option<String>) {
    workspace.read_with(cx, |workspace, cx| {
        (
            workspace.connections.selection.rows.clone(),
            workspace
                .connections
                .tree
                .read(cx)
                .selected_item()
                .map(|item| item.id.to_string()),
        )
    })
}

#[gpui::test]
fn arrowing_off_a_gathered_selection_and_back_does_not_gather_it_again(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = selection_fixture(cx);
        let (birds, flocks) = (table_row("a", "birds"), table_row("a", "flocks"));
        click_connections_row_with(&workspace, cx, &birds, gpui::Modifiers::default());
        click_connections_row_with(&workspace, cx, &flocks, gpui::Modifiers::secondary_key());
        assert_eq!(
            selected_rows(&workspace, cx).0,
            [birds.clone(), flocks.clone()]
        );

        cx.dispatch_action(gpui_base::actions::SelectUp);
        settle(cx);
        let (_, moved_to) = selected_rows(&workspace, cx);
        assert_eq!(
            moved_to.as_deref(),
            Some(birds.as_str()),
            "the arrow moved the row"
        );
        cx.dispatch_action(gpui_base::actions::SelectDown);
        settle(cx);
        let rows = workspace.read_with(cx, |workspace, cx| workspace.selected_rows(cx));
        assert_eq!(rows, [flocks], "the arrow let the gathered rows go");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn secondary_clicking_two_tables_selects_both_and_open_all_opens_each(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = selection_fixture(cx);
        let (birds, flocks) = (table_row("a", "birds"), table_row("a", "flocks"));
        click_connections_row_with(&workspace, cx, &birds, gpui::Modifiers::default());
        click_connections_row_with(&workspace, cx, &flocks, gpui::Modifiers::secondary_key());

        let (rows, anchor) = selected_rows(&workspace, cx);
        assert_eq!(rows, [birds.clone(), flocks.clone()]);
        assert_eq!(
            anchor.as_deref(),
            Some(flocks.as_str()),
            "the last row clicked"
        );
        workspace.read_with(cx, |workspace, cx| {
            let kinds: Vec<_> = workspace
                .selected_nodes(cx)
                .iter()
                .map(|node| node.kind)
                .collect();
            assert_eq!(
                crate::sidebar::selection_header(&kinds),
                "2 tables selected"
            );
        });
        assert_eq!(
            object_tab_names(&workspace, cx, "a"),
            ["birds"],
            "the modifier click opened nothing; the plain one opened its table"
        );

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.run_selection_action(crate::sidebar::SelectionAction::OpenAll, window, cx)
        });
        settle(cx);
        assert_eq!(object_tab_names(&workspace, cx, "a"), ["birds", "flocks"]);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn open_all_opens_each_row_on_its_own_connection(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = selection_fixture(cx);
        let (a_birds, b_flocks) = (table_row("a", "birds"), table_row("b", "flocks"));
        click_connections_row_with(&workspace, cx, &a_birds, gpui::Modifiers::default());
        click_connections_row_with(&workspace, cx, &b_flocks, gpui::Modifiers::secondary_key());
        assert_eq!(object_tab_names(&workspace, cx, "b"), Vec::<String>::new());

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.run_selection_action(crate::sidebar::SelectionAction::OpenAll, window, cx)
        });
        settle(cx);
        assert_eq!(object_tab_names(&workspace, cx, "a"), ["birds"]);
        assert_eq!(object_tab_names(&workspace, cx, "b"), ["flocks"]);
        assert_eq!(
            front_profile(&workspace, cx),
            "b",
            "the connection acted on last is the one in front"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn shift_clicking_selects_every_row_between_the_anchor_and_the_click(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = selection_fixture(cx);
        let (birds, flocks) = (table_row("a", "birds"), table_row("a", "flocks"));
        let tables = scoped_row("a", None, "category-Tables-main");
        click_connections_row_with(&workspace, cx, &birds, gpui::Modifiers::default());
        click_connections_row_with(&workspace, cx, &flocks, gpui::Modifiers::shift());
        assert_eq!(
            selected_rows(&workspace, cx).0,
            [birds.clone(), flocks.clone()]
        );
        // A modifier click on a folder selects it and leaves it as it was.
        let schema = scoped_row("a", None, "schema-main");
        assert_eq!(
            connections_item(&workspace, cx, &schema).map(|item| item.0),
            Some(true)
        );
        click_connections_row_with(&workspace, cx, &schema, gpui::Modifiers::secondary_key());
        assert_eq!(
            connections_item(&workspace, cx, &schema).map(|item| item.0),
            Some(true)
        );
        assert!(selected_rows(&workspace, cx).0.contains(&schema));
        // The category's label between them is no node an action applies to,
        // and a plain click on it is the whole selection again.
        click_connections_row_with(&workspace, cx, &tables, gpui::Modifiers::default());
        assert_eq!(selected_rows(&workspace, cx).0, [tables]);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn simple_mode_keeps_one_selected_row(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, "create table birds (name text);");
        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        let row = explorer_row(&workspace, "relation-main\0birds", cx).center();
        cx.simulate_click(row, gpui::Modifiers::secondary_key());
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.profile().unwrap().session.objects.len(),
                1,
                "a secondary-modifier click opens the table as a plain click does"
            );
            assert_eq!(
                workspace.connections.selection,
                crate::sidebar::Selection::default(),
                "no selection set exists in simple mode"
            );
        });
        quit(workspace, cx);
    });
}

/// `birds` and `flocks` of `a` selected, with nothing opened: the tree's own
/// row is `birds` already, and a modifier click adds `flocks` to it.
fn two_tables_selected(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    click_connections_row_with(
        workspace,
        cx,
        &table_row("a", "flocks"),
        gpui::Modifiers::secondary_key(),
    );
    assert_eq!(
        selected_rows(workspace, cx).0,
        [table_row("a", "birds"), table_row("a", "flocks")]
    );
}

/// The clipboard's text.
fn clipboard_text(cx: &mut VisualTestContext) -> String {
    cx.read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default()
}

#[gpui::test]
fn copy_names_and_copy_ddl_cover_every_selected_table(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = selection_fixture(cx);
        two_tables_selected(&workspace, cx);
        let at = connections_row(&workspace, cx, &table_row("a", "birds")).unwrap();

        // The header, then Open All, Copy Names, Copy DDL, Show DDL, Pin and
        // Refresh: the header is a row the keyboard stops at.
        pick_from_connections_menu(cx, at, 3);
        assert_eq!(clipboard_text(cx), "birds\nflocks");

        pick_from_connections_menu(cx, at, 4);
        settle(cx);
        assert_eq!(
            clipboard_text(cx),
            "CREATE TABLE birds (name text);\n\nCREATE TABLE flocks (name text);"
        );
        assert_eq!(
            object_tab_names(&workspace, cx, "a"),
            Vec::<String>::new(),
            "copying opens nothing"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn show_ddl_on_a_selection_lists_each_definition_in_one_dialog(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = selection_fixture(cx);
        two_tables_selected(&workspace, cx);
        let at = connections_row(&workspace, cx, &table_row("a", "birds")).unwrap();

        pick_from_connections_menu(cx, at, 5);
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("show-ddl"));
        workspace.read_with(cx, |workspace, _| {
            let view = workspace.ddl_view.as_ref().expect("the dialog is up");
            let sections: Vec<_> = view
                .sections
                .iter()
                .map(|section| match &section.text {
                    super::DdlText::Ready(text) => (section.title.clone(), text.clone()),
                    _ => (section.title.clone(), "not read".to_string()),
                })
                .collect();
            assert_eq!(
                sections,
                [
                    (
                        "Table main.birds".to_string(),
                        "CREATE TABLE birds (name text);".to_string()
                    ),
                    (
                        "Table main.flocks".to_string(),
                        "CREATE TABLE flocks (name text);".to_string()
                    ),
                ]
            );
        });
        assert!(cx.debug_bounds("show-ddl-text-0").is_some());
        assert!(cx.debug_bounds("show-ddl-text-1").is_some());
        assert!(
            cx.debug_bounds("show-ddl-diagram-choice").is_none(),
            "a diagram is of one relation"
        );

        // Each definition has a Copy of its own.
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
        let copy = cx.debug_bounds("show-ddl-copy-1").expect("drawn").center();
        cx.simulate_click(copy, gpui::Modifiers::default());
        settle(cx);
        assert_eq!(clipboard_text(cx), "CREATE TABLE flocks (name text);");

        press(cx, "escape");
        assert_eq!(front_dialog(&workspace, cx), None);
        assert!(
            workspace.read_with(cx, |workspace, _| workspace.ddl_view.is_none()),
            "escape closes it"
        );
        assert!(editor_has_keyboard(&workspace, cx), "the keyboard is back");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn pin_and_refresh_act_on_every_selected_row(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = selection_fixture(cx);
        two_tables_selected(&workspace, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.run_selection_action(crate::sidebar::SelectionAction::Pin, window, cx);
            // Again: Pin pins what is not pinned and unpins nothing.
            workspace.run_selection_action(crate::sidebar::SelectionAction::Pin, window, cx)
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.profile().unwrap().shortcuts.pins.len(), 2);
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn refresh_reads_each_database_of_the_selection_again(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = two_databases_each_holding_t(cx);
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
        settle(cx);
        let other = database_row("e2e", "other");
        let at = connections_row(&workspace, cx, &other).expect("other is listed");
        click_connections_row(cx, at);
        run_in_front(&workspace, cx, "CREATE TABLE other.main.fresh (id INTEGER)");
        run_in_front(&workspace, cx, "CREATE TABLE fresh_own (id INTEGER)");

        let t_rows: Vec<String> = workspace.read_with(cx, |workspace, _| {
            let mut rows: Vec<_> = workspace
                .connections
                .nodes
                .keys()
                .filter(|id| id.ends_with("relation-main\0t"))
                .cloned()
                .collect();
            rows.sort();
            rows
        });
        assert_eq!(
            t_rows.len(),
            2,
            "`t` of each database is listed: {t_rows:?}"
        );
        for row in t_rows.iter().rev() {
            reveal_connections_row(&workspace, cx, row);
        }
        // The first is the tree's row, the second joins it.
        click_connections_row_with(&workspace, cx, &t_rows[1], gpui::Modifiers::secondary_key());
        assert_eq!(selected_rows(&workspace, cx).0.len(), 2);

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.run_selection_action(crate::sidebar::SelectionAction::Refresh, window, cx)
        });
        settle(cx);
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let names = |catalog: &crate::db::Catalog| {
                let mut names: Vec<_> = catalog
                    .schemas
                    .iter()
                    .flat_map(|schema| schema.relations.iter().map(|r| r.name.clone()))
                    .collect();
                names.sort();
                names
            };
            let profile = workspace.profile().unwrap();
            assert_eq!(
                names(workspace.catalog().expect("the own catalog")),
                ["fresh_own", "t"]
            );
            match profile.database_catalogs.get("other") {
                Some(CatalogState::Loaded(catalog, _)) => {
                    assert_eq!(names(catalog), ["fresh", "t"])
                }
                _ => panic!("the catalog of `other` has not loaded"),
            }
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_selection_survives_a_rebuild_and_drops_rows_that_are_gone(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = selection_fixture(cx);
        two_tables_selected(&workspace, cx);
        let (birds, flocks) = (table_row("a", "birds"), table_row("a", "flocks"));

        workspace.update(cx, |workspace, cx| workspace.rebuild_connections(cx));
        settle(cx);
        assert_eq!(selected_rows(&workspace, cx).0, [birds.clone(), flocks]);

        rusqlite::Connection::open(store::dbdelve_directory().unwrap().join("e2e.db"))
            .and_then(|db| db.execute_batch("drop table flocks"))
            .unwrap();
        workspace.update(cx, |workspace, cx| workspace.refresh_schema(cx));
        settle(cx);
        let (rows, tree_row) = selected_rows(&workspace, cx);
        assert_eq!(
            rows,
            std::slice::from_ref(&birds),
            "a row that is gone drops out"
        );
        assert_eq!(
            tree_row.as_deref(),
            Some(birds.as_str()),
            "and the tree's own row is one that is left"
        );
        quit(workspace, cx);
    });
}

/// What the palette's Commands scope lists, with each row's detail.
fn palette_rows(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Vec<(String, Option<String>)> {
    cx.simulate_keystrokes("secondary-shift-p");
    settle(cx);
    let rows = workspace.read_with(cx, |workspace, cx| {
        let palette = workspace.palette.as_ref().expect("the palette is up");
        palette
            .read(cx)
            .labels_with_details()
            .into_iter()
            .map(|(label, detail)| (label.to_string(), detail.map(str::to_string)))
            .collect()
    });
    cx.simulate_keystrokes("escape");
    settle(cx);
    rows
}

#[gpui::test]
fn a_long_host_shortens_before_its_connections_name_in_the_palette(cx: &mut TestAppContext) {
    // CI's live job sets `PG*`, whose profile would be put in front on launch.
    crate::IGNORE_ENVIRONMENT.set(true);
    store::tests::with_home(|| {
        let host = format!("{}.example.com", "a-very-long-host-name".repeat(8));
        let (workspace, cx) = launch_connections(
            cx,
            "",
            "",
            &format!(
                "{}[[profiles]]\nid = \"far\"\nname = \"Faraway\"\nhost = {host:?}\n\
                 database = \"d\"\nuser = \"u\"\nengine = \"postgres\"\n",
                sqlite_profile("e2e", "E2E", "")
            ),
        );
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(crate::actions::SwitchConnection), cx)
        });
        settle(cx);
        let name = cx
            .debug_bounds("palette-label-Faraway")
            .expect("the connection's row is drawn");
        assert!(
            name.size.width > gpui::px(20.),
            "the name keeps its width beside a host wider than the row: {name:?}"
        );
        cx.simulate_keystrokes("escape");
        settle(cx);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_palette_offers_the_selections_actions_as_its_menu_does(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = selection_fixture(cx);
        two_tables_selected(&workspace, cx);
        let selection = [
            "Open All",
            "Copy Names",
            "Copy DDL",
            "Show DDL",
            "Pin",
            "Refresh",
        ];

        let rows = palette_rows(&workspace, cx);
        for label in selection {
            assert!(
                rows.contains(&(label.to_string(), Some("2 tables selected".to_string()))),
                "{label} is offered, detailed with how many: {rows:?}"
            );
        }
        for label in ["Copy Name", "New Query Here"] {
            assert!(
                rows.iter().all(|(shown, _)| shown != label),
                "{label} is one row's, and several are selected"
            );
        }

        cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        cx.simulate_input("Copy Names");
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(clipboard_text(cx), "birds\nflocks");

        // One row selected: its own node's rows, as 07-15's palette lists them.
        click_connections_row_with(
            &workspace,
            cx,
            &table_row("a", "flocks"),
            gpui::Modifiers::default(),
        );
        let rows = palette_rows(&workspace, cx);
        for label in ["Open All", "Copy Names"] {
            assert!(
                rows.iter().all(|(shown, _)| shown != label),
                "{label} needs several rows"
            );
        }
        assert!(
            rows.contains(&("Copy Name".to_string(), Some("flocks".to_string()))),
            "the one selected row offers its own: {rows:?}"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn no_selection_action_sends_a_generated_statement(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = selection_fixture(cx);
        two_tables_selected(&workspace, cx);
        let counts = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let session = &workspace.profile().unwrap().session;
                (store::history("a").len(), session.history.len())
            })
        };
        let before = counts(&workspace, cx);
        for action in [
            crate::sidebar::SelectionAction::OpenAll,
            crate::sidebar::SelectionAction::CopyNames,
            crate::sidebar::SelectionAction::CopyDdl,
            crate::sidebar::SelectionAction::ShowDdl,
            crate::sidebar::SelectionAction::Pin,
            crate::sidebar::SelectionAction::Refresh,
        ] {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.run_selection_action(action, window, cx)
            });
            settle(cx);
            settle(cx);
            if front_dialog(&workspace, cx).is_some() {
                press(cx, "escape");
            }
        }
        assert_eq!(counts(&workspace, cx), before, "no statement was recorded");
        quit(workspace, cx);
    });
}

/// Connections `a` and `b` over one file holding `birds` and `flocks`, in
/// complex mode, `a` in front. The window is tall enough to draw every row.
fn remembering_fixture(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    crate::IGNORE_ENVIRONMENT.set(true);
    let (workspace, cx) = launch_connections(
        cx,
        "create table birds (name text); create table flocks (name text);",
        "view_mode = \"complex\"",
        &format!(
            "{}{}",
            sqlite_profile("a", "Birds", ""),
            sqlite_profile("b", "Flocks", ""),
        ),
    );
    cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
    settle(cx);
    (workspace, cx)
}

/// A click on each of `ids` in turn, each as showing when its turn comes.
fn open_connections_rows(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    ids: &[String],
) {
    for id in ids {
        let at = connections_row(workspace, cx, id)
            .unwrap_or_else(|| panic!("{id} is not showing to be clicked"));
        click_connections_row(cx, at);
    }
}

/// Whether the connections tree's row `id` is showing and open.
fn connections_row_open(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    id: &str,
) -> bool {
    connections_item(workspace, cx, id).is_some_and(|(open, _)| open)
}

#[gpui::test]
fn folds_and_the_selection_in_the_connections_tree_come_back_after_a_relaunch(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = remembering_fixture(cx);
        let (a_main, a_tables) = (
            scoped_row("a", None, "schema-main"),
            scoped_row("a", None, "category-Tables-main"),
        );
        let (b_main, b_tables) = (
            scoped_row("b", None, "schema-main"),
            scoped_row("b", None, "category-Tables-main"),
        );
        let (birds, flocks) = (table_row("a", "birds"), table_row("a", "flocks"));
        open_connections_rows(
            &workspace,
            cx,
            &[connection_row("a"), a_main.clone(), a_tables.clone()],
        );
        // Opening `b` connects it, and its rows are there once it has.
        open_connections_rows(&workspace, cx, &[connection_row("b"), b_main.clone()]);
        assert_eq!(profile_state(&workspace, cx, "b"), "connected");
        click_connections_row_with(&workspace, cx, &birds, gpui::Modifiers::default());
        click_connections_row_with(&workspace, cx, &flocks, gpui::Modifiers::secondary_key());
        assert_eq!(
            selected_rows(&workspace, cx).0,
            [birds.clone(), flocks.clone()]
        );

        let (workspace, cx) = relaunch_closing(workspace, cx);
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
        settle(cx);
        assert_eq!(
            profile_state(&workspace, cx, "b"),
            "idle",
            "nothing is connected on the strength of what was stored"
        );
        for row in [connection_row("a"), a_main, a_tables] {
            assert!(
                connections_row_open(&workspace, cx, &row),
                "{row} is open as it was left"
            );
        }
        assert_eq!(
            selected_rows(&workspace, cx),
            (vec![birds, flocks.clone()], Some(flocks)),
            "the rows that were selected are, and the last is the tree's own"
        );
        assert!(
            !connections_row_open(&workspace, cx, &connection_row("b")),
            "an idle connection stays folded whatever was stored"
        );
        assert_eq!(
            connections_item(&workspace, cx, &connection_row("b"))
                .map(|(_, children)| children.len()),
            Some(1),
            "and holds only its notice"
        );

        open_connections_rows(&workspace, cx, &[connection_row("b")]);
        assert_eq!(profile_state(&workspace, cx, "b"), "connected");
        assert!(
            connections_row_open(&workspace, cx, &b_main),
            "once it has loaded its rows open as they were left"
        );
        assert!(
            !connections_row_open(&workspace, cx, &b_tables),
            "and only those"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn removing_a_connection_takes_its_stored_rows_and_leaves_the_others(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = remembering_fixture(cx);
        open_connections_rows(
            &workspace,
            cx,
            &[
                connection_row("a"),
                scoped_row("a", None, "schema-main"),
                connection_row("b"),
                scoped_row("b", None, "schema-main"),
                scoped_row("b", None, "category-Tables-main"),
            ],
        );
        click_connections_row_with(
            &workspace,
            cx,
            &table_row("b", "birds"),
            gpui::Modifiers::default(),
        );
        let stored = |id: &str| {
            let profile = store::peek_profiles(&[])
                .expect("profiles.toml reads back")
                .profiles
                .into_iter()
                .find(|profile| profile.id == id)
                .unwrap_or_else(|| panic!("{id} was saved"));
            (profile.open_rows, profile.selected_rows)
        };
        let (a_before, b_before) = (stored("a"), stored("b"));
        assert!(
            a_before.0.contains(&connection_row("a")) && b_before.0.contains(&connection_row("b")),
            "each connection saved its open rows: {a_before:?} {b_before:?}"
        );
        assert_eq!(b_before.1, [table_row("b", "birds")]);

        workspace.update(cx, |workspace, cx| {
            let at = workspace
                .profiles
                .iter()
                .position(|profile| profile.id == "b")
                .expect("b is listed");
            workspace.remove_profile(at, cx);
        });
        settle(cx);
        let file = std::fs::read_to_string(
            store::dbdelve_directory()
                .expect("the test home has a data directory")
                .join("profiles.toml"),
        )
        .expect("profiles.toml reads");
        assert!(
            !file.contains("connection:1:b"),
            "no stored row names the removed connection:\n{file}"
        );
        assert_eq!(stored("a"), a_before, "another's rows do not change");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn expanding_a_folded_loaded_row_builds_its_rows(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = remembering_fixture(cx);
        let (a, main) = (connection_row("a"), scoped_row("a", None, "schema-main"));
        assert_eq!(profile_state(&workspace, cx, "a"), "connected");
        assert_eq!(
            connections_item(&workspace, cx, &a),
            Some((false, vec![(format!("folded:{a}"), String::new(), true)])),
            "a connected connection left folded is one row and one inert row"
        );
        open_connections_rows(&workspace, cx, std::slice::from_ref(&a));
        assert!(
            connections_row_open(&workspace, cx, &a),
            "the click opened it"
        );
        let (_, children) = connections_item(&workspace, cx, &a).expect("the row is showing");
        assert!(
            children.iter().any(|(id, ..)| *id == main),
            "and its rows were built for it: {children:?}"
        );
        assert_eq!(
            connections_item(&workspace, cx, &main).map(|(open, children)| (open, children.len())),
            Some((false, 1)),
            "each folded in its turn"
        );
        open_connections_rows(&workspace, cx, std::slice::from_ref(&main));
        assert_eq!(
            connections_item(
                &workspace,
                cx,
                &scoped_row("a", None, "category-Tables-main")
            )
            .map(|(open, children)| (open, children.len())),
            Some((false, 1)),
            "the schema's own rows are there to be clicked, each folded in its turn"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_rebuild_landing_while_moving_through_the_tree_keeps_folds_and_the_selection(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = remembering_fixture(cx);
        let rows = [
            connection_row("a"),
            scoped_row("a", None, "schema-main"),
            scoped_row("a", None, "category-Tables-main"),
        ];
        open_connections_rows(&workspace, cx, &rows);
        let (birds, flocks) = (table_row("a", "birds"), table_row("a", "flocks"));
        click_connections_row_with(&workspace, cx, &birds, gpui::Modifiers::default());
        click_connections_row_with(&workspace, cx, &flocks, gpui::Modifiers::secondary_key());
        let before = selected_rows(&workspace, cx);
        assert_eq!(before.0, [birds, flocks]);

        // A saved query changing, then table sizes landing: both rebuild the
        // tree while the user is part-way through it.
        workspace.update(cx, |workspace, cx| {
            workspace.refresh_query_library("a", cx);
            workspace.read_sizes_again(cx);
        });
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.refresh_query_library("a", cx));
        settle(cx);

        for row in &rows {
            assert!(connections_row_open(&workspace, cx, row), "{row} is open");
        }
        assert_eq!(selected_rows(&workspace, cx), before);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn focusing_the_sidebar_reveals_the_front_row_inside_a_folded_subtree(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = remembering_fixture(cx);
        let birds = table_row("a", "birds");
        open_connections_rows(
            &workspace,
            cx,
            &[
                connection_row("a"),
                scoped_row("a", None, "schema-main"),
                scoped_row("a", None, "category-Tables-main"),
                birds.clone(),
            ],
        );
        // Folded again and the tree rebuilt, so what was under it is gone from
        // the rows drawn.
        open_connections_rows(&workspace, cx, &[connection_row("a")]);
        workspace.update(cx, |workspace, cx| workspace.refresh_query_library("a", cx));
        settle(cx);
        assert!(connections_row(&workspace, cx, &birds).is_none());

        cx.simulate_keystrokes("secondary-k e");
        settle(cx);
        assert_eq!(
            selected_rows(&workspace, cx).1,
            Some(birds),
            "the tab in front is revealed where it is, through every folded row above it"
        );
        quit(workspace, cx);
    });
}

/// Sets the connections filter to `text`.
fn filter_connections(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, text: &str) {
    cx.update(|window, cx| {
        let filter = workspace.read(cx).connections.filter.clone();
        filter.update(cx, |input, cx| input.set_value(text, window, cx));
    });
    workspace.update(cx, |workspace, cx| workspace.rebuild_connections(cx));
    settle(cx);
}

#[gpui::test]
fn a_filter_naming_the_connection_keeps_its_other_databases_whole(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        let other = database_row("e2e", "other");
        open_connections_rows(
            &workspace,
            cx,
            &[
                other,
                scoped_row("e2e", Some("other"), "schema-main"),
                scoped_row("e2e", Some("other"), "category-Tables-main"),
            ],
        );
        let u = scoped_row("e2e", Some("other"), &explorer::relation_row("main", "u"));
        assert!(connections_row(&workspace, cx, &u).is_some());
        // A name neither database has, so only the connection can match it.
        workspace.update(cx, |workspace, _| {
            workspace
                .profile_mut()
                .expect("the connection is in front")
                .name = "Prod".into();
        });

        filter_connections(&workspace, cx, "Prod");
        assert!(
            connections_row(&workspace, cx, &u).is_some(),
            "a connection the filter names lists every database of it whole"
        );
        filter_connections(&workspace, cx, "e2e");
        assert!(
            connections_row(&workspace, cx, &u).is_none(),
            "its own database named by the filter keeps no other database whole"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_refresh_inside_a_failed_transaction_lists_the_databases_apart_from_it(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        run_in_front(&workspace, cx, "BEGIN");
        cx.update(|window, cx| {
            let editor = workspace
                .read(cx)
                .profile()
                .and_then(|profile| profile.session.active_query_tab())
                .expect("the tab is still there")
                .editor
                .clone();
            editor.update(cx, |input, cx| {
                input.set_value("SELECT error('the transaction fails')", window, cx)
            });
        });
        cx.dispatch_action(crate::actions::RunQuery);
        settle(cx);
        workspace.update(cx, |workspace, cx| {
            let profile = workspace.profile_mut().expect("the connection is in front");
            assert!(profile.in_transaction, "the transaction is still open");
            profile.session.notice = None;
            workspace.refresh_schema(cx);
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().expect("the connection is in front");
            assert_eq!(
                profile.session.notice.as_deref(),
                Some("Schema refreshed."),
                "nothing was read on the session inside the failed transaction"
            );
            assert!(profile.databases.names.iter().any(|name| name == "other"));
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_session_opened_for_a_connection_that_has_ended_is_not_kept(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = duckdb_with_other_attached(cx);
        workspace.update(cx, |workspace, cx| {
            let profile = workspace.profile_mut().expect("the connection is in front");
            let (id, generation) = (profile.id.clone(), profile.generation);
            let connection = profile.connection().expect("the connection is open");
            let held = profile.mode == crate::sql::Mode::ReadOnly;
            assert!(
                workspace
                    .keep_session(
                        &id,
                        generation + 1,
                        "other".into(),
                        held,
                        connection.clone(),
                        cx
                    )
                    .is_none(),
                "a connection replaced since keeps nothing and runs nothing"
            );
            let profile = workspace.profile_mut().expect("the connection is in front");
            profile.state = ProfileState::Failed("lost".into());
            assert!(
                workspace
                    .keep_session(&id, generation, "other".into(), held, connection, cx)
                    .is_none(),
                "nor does one that was lost"
            );
            let profile = workspace.profile().expect("the connection is in front");
            assert!(profile.bound.is_empty(), "and no session is registered");
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_stored_selection_waits_for_a_connection_that_loads_later(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = remembering_fixture(cx);
        let (a_birds, b_birds) = (table_row("a", "birds"), table_row("b", "birds"));
        open_connections_rows(
            &workspace,
            cx,
            &[
                connection_row("a"),
                scoped_row("a", None, "schema-main"),
                scoped_row("a", None, "category-Tables-main"),
                connection_row("b"),
                scoped_row("b", None, "schema-main"),
                scoped_row("b", None, "category-Tables-main"),
            ],
        );
        click_connections_row_with(&workspace, cx, &a_birds, gpui::Modifiers::default());
        click_connections_row_with(&workspace, cx, &b_birds, gpui::Modifiers::secondary_key());
        assert_eq!(
            selected_rows(&workspace, cx).0,
            [a_birds.clone(), b_birds.clone()]
        );

        let (workspace, cx) = relaunch_closing(workspace, cx);
        cx.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(900.)));
        settle(cx);
        assert_eq!(profile_state(&workspace, cx, "b"), "idle");
        assert_eq!(
            selected_rows(&workspace, cx).0,
            std::slice::from_ref(&a_birds)
        );
        let stored = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profiles
                    .iter()
                    .find(|profile| profile.id == "b")
                    .expect("b is listed")
                    .selected_rows
                    .clone()
            })
        };
        assert_eq!(
            stored(&workspace, cx),
            std::slice::from_ref(&b_birds),
            "b's rows wait for it rather than being lost to a's"
        );

        // Connected without a click, which would be a selection of its own.
        workspace.update(cx, |workspace, cx| {
            let at = workspace
                .profiles
                .iter()
                .position(|profile| profile.id == "b")
                .expect("b is listed");
            workspace.begin_connect(at, cx);
        });
        settle(cx);
        assert_eq!(profile_state(&workspace, cx, "b"), "connected");
        assert_eq!(
            selected_rows(&workspace, cx).0,
            [a_birds, b_birds],
            "and join the selection once b has loaded"
        );
        quit(workspace, cx);
    });
}

/// A `.sql` file in the test home, holding `text`, as the user's own file.
fn user_sql_file(name: &str, text: &str) -> std::path::PathBuf {
    let path = crate::sql_file::canonical(&store::home().expect("the test home").join(name));
    std::fs::write(&path, text).expect("the file must be writable");
    path
}

/// `path` opened as a dropped file is, waited on until its tab is in front:
/// the file is read on the blocking pool, which the test scheduler does not
/// own.
fn open_sql_file(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    path: &std::path::Path,
) {
    cx.executor().allow_parking();
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_files(vec![path.to_path_buf()], window, cx)
        });
    });
    for _ in 0..500 {
        settle(cx);
        if front_file(workspace, cx).as_deref() == Some(path) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("{} never opened", path.display());
}

fn front_file(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<std::path::PathBuf> {
    workspace.read_with(cx, |workspace, _| {
        let tab = workspace.profile()?.session.active_query_tab()?;
        Some(tab.file.as_ref()?.path.clone())
    })
}

fn front_dirty(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    query_tab(workspace, cx, |tab, _, _| tab.dirty)
}

#[gpui::test]
fn the_save_chord_in_the_editor_writes_the_bound_file_as_the_buffer_holds_it(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = user_sql_file("orders.sql", "SELECT 1;\r\n");
        open_sql_file(&workspace, cx, &path);
        assert!(editor_has_keyboard(&workspace, cx));
        assert_eq!(
            buffer(&workspace, cx),
            "SELECT 1;\r\n",
            "line endings as read"
        );
        assert!(!front_dirty(&workspace, cx));

        cx.simulate_input("x");
        assert!(front_dirty(&workspace, cx));
        cx.simulate_keystrokes("secondary-z");
        settle(cx);
        assert!(!front_dirty(&workspace, cx), "undone back to the file");

        cx.simulate_input("-- ");
        cx.simulate_keystrokes("secondary-s");
        settle(cx);
        assert_eq!(
            std::fs::read(&path).expect("the file is there"),
            b"-- SELECT 1;\r\n"
        );
        assert!(!front_dirty(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_open_chord_in_the_editor_binds_the_picked_file_to_a_tab(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.executor().allow_parking();
        let path = user_sql_file("picked.sql", "SELECT 1;");
        assert!(editor_has_keyboard(&workspace, cx));
        cx.simulate_keystrokes("secondary-o");
        settle(cx);
        assert!(cx.did_prompt_for_paths());
        let picked = path.clone();
        cx.simulate_path_prompt_response(|_| Some(vec![picked]));
        for _ in 0..500 {
            settle(cx);
            if front_file(&workspace, cx).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(front_file(&workspace, cx), Some(path));
        assert_eq!(buffer(&workspace, cx), "SELECT 1;");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn save_as_binds_an_untitled_tab_to_the_file_it_writes(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("SELECT 2;");
        // Untitled, so Save asks where.
        cx.simulate_keystrokes("secondary-s");
        settle(cx);
        let chosen = store::home().expect("the test home").join("report");
        cx.simulate_new_path_selection(|_| Some(chosen));
        settle(cx);

        let path = crate::sql_file::canonical(&store::home().unwrap().join("report.sql"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SELECT 2;");
        assert_eq!(front_file(&workspace, cx), Some(path));
        assert!(!front_dirty(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_save_chord_in_the_grid_applies_edits(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("select 1");
        cx.simulate_keystrokes("secondary-enter");
        settle(cx);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        assert!(grid_has_keyboard(&workspace, cx));

        let resolved: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let _observer = cx.update({
            let resolved = resolved.clone();
            |_, cx| {
                cx.observe_keystrokes(move |event, _, _| {
                    let action = event.action.as_ref().map(|action| action.as_any());
                    resolved.borrow_mut().push(match action {
                        Some(action) if action.is::<crate::actions::ApplyEdits>() => "apply",
                        Some(action) if action.is::<crate::actions::SaveQuery>() => "save",
                        _ => "other",
                    });
                })
            }
        });
        cx.simulate_keystrokes("secondary-s");
        assert_eq!(*resolved.borrow(), ["apply"]);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_relaunch_brings_a_file_tab_back_dirty_without_writing_its_file(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        // CI's `PG*` would put their connection, or the form saying what they
        // lack, in front on the relaunch.
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = user_sql_file("draft.sql", "SELECT 1;");
        open_sql_file(&workspace, cx, &path);
        cx.simulate_input("-- ");
        assert!(front_dirty(&workspace, cx));

        let (workspace, cx) = relaunch_closing(workspace, cx);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SELECT 1;");
        let tab = workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().expect("a connection in front").session;
            session.tab_on(&path).expect("the file tab came back")
        });
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_tab(Tab::Query(tab), window, cx)
            });
        });
        settle(cx);
        assert_eq!(buffer(&workspace, cx), "-- SELECT 1;");
        assert!(front_dirty(&workspace, cx));
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn a_relaunch_with_two_tabs_on_one_file_keeps_both_buffers(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        // CI's `PG*` would put their connection, or the form saying what they
        // lack, in front on the relaunch.
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = user_sql_file("shared.sql", "SELECT 1;");
        let link = path.with_file_name("link.sql");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        open_sql_file(&workspace, cx, &path);
        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        cx.simulate_input("SELECT 2;");
        // As a build that stored paths as they were picked would have left it.
        workspace.update(cx, |workspace, _| {
            let session = &mut workspace.profile_mut().expect("a connection").session;
            let Tab::Query(id) = session.active else {
                panic!("a query tab is in front");
            };
            session.query_tab_mut(id).expect("the tab").file = Some(crate::session::FileBinding {
                path: link.clone(),
                saved: Some("SELECT 1;".into()),
                bom: false,
                stamp: None,
                conflict: None,
            });
        });

        let (workspace, cx) = relaunch_closing(workspace, cx);
        let buffers = workspace.read_with(cx, |workspace, cx| {
            let session = &workspace.profile().expect("a connection in front").session;
            let mut buffers: Vec<_> = session
                .queries
                .iter()
                .map(|tab| {
                    let path = tab.file.as_ref().map(|file| file.path.clone());
                    (path, tab.editor.read(cx).value().to_string())
                })
                .collect();
            buffers.sort();
            buffers
        });
        assert_eq!(
            buffers,
            [
                (None, String::new()),
                (None, "SELECT 2;".to_string()),
                (Some(path.clone()), "SELECT 1;".to_string()),
            ]
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn renaming_or_deleting_a_saved_query_reaches_its_tab_on_another_connection(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        crate::IGNORE_ENVIRONMENT.set(true);
        store::write_query("b", "nightly", "select 1").expect("the query saves");
        let (workspace, cx) = launch_connections(
            cx,
            "",
            "",
            &format!(
                "{}{}",
                sqlite_profile("a", "A", ""),
                sqlite_profile("b", "B", "")
            ),
        );
        let queries = crate::sql_file::canonical(&store::query_directory("b").unwrap());
        open_sql_file(&workspace, cx, &queries.join("nightly.sql"));
        let on_a = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace.profiles[0]
                    .session
                    .queries
                    .iter()
                    .filter_map(|tab| Some(tab.file.as_ref()?.path.clone()))
                    .collect::<Vec<_>>()
            })
        };
        // A lists the file by its tab alone.
        workspace.update(cx, |workspace, cx| {
            workspace.forget_recent_file(&queries.join("nightly.sql"), cx)
        });
        workspace.update(cx, |workspace, cx| workspace.activate(1, cx));
        settle(cx);

        rename_query(&workspace, cx, "nightly", "weekly");
        assert_eq!(on_a(&workspace, cx), [queries.join("weekly.sql")]);
        let listed_on_a = workspace.read_with(cx, |workspace, cx| {
            let tree = workspace.profiles[0].session.explorer_tree.clone();
            let row = explorer::file_row(&queries.join("weekly.sql"));
            tree.read(cx).index_of(&row.into()).is_some()
        });
        assert!(listed_on_a, "A's list follows its tab");

        workspace.update(cx, |workspace, cx| {
            workspace.ask_delete_saved_query("weekly".into(), cx)
        });
        settle(cx);
        assert!(
            cx.debug_bounds(
                "delete-question: Delete weekly.sql? Its tab is closed too. This cannot be undone."
            )
            .is_some(),
            "the tab on A closes too"
        );
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.delete_saved_query("weekly".into(), window, cx)
            });
        });
        settle(cx);
        assert_eq!(on_a(&workspace, cx), Vec::<std::path::PathBuf>::new());
        assert!(!queries.join("weekly.sql").exists());
        quit(workspace, cx);
    });
}

/// The sidebar's rename of the front connection's saved query `from`.
fn rename_query(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, from: &str, to: &str) {
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.start_query_rename(from.into(), from.into(), window, cx);
            let input = workspace
                .profile()
                .unwrap()
                .session
                .query_rename
                .as_ref()
                .unwrap()
                .input
                .clone();
            input.update(cx, |input, cx| input.set_value(to, window, cx));
            workspace.commit_query_rename(cx);
        });
    });
    settle(cx);
}

#[gpui::test]
fn a_rename_onto_a_deleted_querys_open_tab_is_refused(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        store::write_query("e2e", "daily", "select 1").expect("the query saves");
        store::write_query("e2e", "weekly", "select 2").expect("the query saves");
        let (workspace, cx) = launch_on_sqlite(cx);
        let queries = crate::sql_file::canonical(&store::query_directory("e2e").unwrap());
        open_sql_file(&workspace, cx, &queries.join("daily.sql"));
        open_sql_file(&workspace, cx, &queries.join("weekly.sql"));
        std::fs::remove_file(queries.join("daily.sql")).unwrap();
        workspace.update(cx, |workspace, cx| workspace.reload_saved_queries(cx));

        rename_query(&workspace, cx, "weekly", "daily");
        assert!(queries.join("weekly.sql").exists(), "nothing moved");
        assert!(is_open(&workspace, cx, &queries.join("weekly.sql")));
        let error = workspace.read_with(cx, |workspace, _| {
            let rename = workspace.profile().unwrap().session.query_rename.as_ref();
            rename.and_then(|rename| rename.error.clone())
        });
        assert_eq!(error.as_deref(), Some("daily is open in another tab."));
        quit(workspace, cx);
    });
}

/// Deleting a saved query that is a link takes the link alone, so the tab
/// on the file it leads to keeps its unsaved text.
#[cfg(unix)]
#[gpui::test]
fn deleting_a_link_query_leaves_the_tab_on_its_file(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        store::write_query("e2e", "real", "select 1").expect("the query saves");
        let queries = crate::sql_file::canonical(&store::query_directory("e2e").unwrap());
        std::os::unix::fs::symlink(queries.join("real.sql"), queries.join("alias.sql")).unwrap();
        let (workspace, cx) = launch_on_sqlite(cx);
        open_sql_file(&workspace, cx, &queries.join("real.sql"));
        cx.simulate_input("-- ");

        workspace.update(cx, |workspace, cx| {
            workspace.ask_delete_saved_query("alias".into(), cx)
        });
        settle(cx);
        assert!(
            cx.debug_bounds("delete-question: Delete alias.sql? This cannot be undone.")
                .is_some()
        );
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.delete_saved_query("alias".into(), window, cx)
            });
        });
        settle(cx);
        assert!(!queries.join("alias.sql").exists());
        assert!(is_open(&workspace, cx, &queries.join("real.sql")));
        assert!(front_dirty(&workspace, cx));
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn a_link_dropped_with_its_file_opens_one_tab(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = user_sql_file("both.sql", "SELECT 1;");
        let link = path.with_file_name("link.sql");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let before = query_tab_count(&workspace, cx);
        cx.executor().allow_parking();
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_files(vec![link.clone(), path.clone()], window, cx)
            });
        });
        for _ in 0..500 {
            settle(cx);
            if front_file(&workspace, cx).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(front_file(&workspace, cx), Some(path));
        assert_eq!(query_tab_count(&workspace, cx), before + 1);
        quit(workspace, cx);
    });
}

/// A dirty file tab on `name` whose scratch file holds bytes that are not
/// UTF-8 when the app comes back up, and the paths of the file and the
/// scratch file. `before_launch` runs with the app closed.
fn relaunch_over_an_unreadable_scratch<'a>(
    workspace: Entity<Workspace>,
    cx: &'a mut VisualTestContext,
    name: &str,
    before_launch: impl FnOnce(&std::path::Path),
) -> (
    Entity<Workspace>,
    &'a mut VisualTestContext,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let path = open_dirty_file(&workspace, cx, name);
    let (_, scratch) = front_scratch(&workspace, cx);
    close_window(workspace, cx);
    std::fs::write(&scratch, b"-- \xff SELECT 1;").unwrap();
    before_launch(scratch.parent().unwrap());
    let (workspace, cx) = launch_again(cx);
    bring_back(&workspace, cx, &path);
    (workspace, cx, path, scratch)
}

#[gpui::test]
fn a_scratch_file_that_will_not_read_back_is_set_aside_and_the_tab_carries_on(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        // CI's `PG*` would put their connection, or the form saying what they
        // lack, in front on the relaunch.
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_on_sqlite(cx);
        let (workspace, cx, path, scratch) =
            relaunch_over_an_unreadable_scratch(workspace, cx, "draft.sql", |_| {});
        let aside = scratch.with_file_name(
            scratch
                .file_name()
                .unwrap()
                .to_string_lossy()
                .replace(".sql", ".unread-1.sql"),
        );
        assert_eq!(std::fs::read(&aside).unwrap(), b"-- \xff SELECT 1;");
        workspace.read_with(cx, |workspace, _| {
            let notice = workspace.profile().unwrap().session.notice.clone();
            assert!(notice.is_some_and(|notice| notice.contains(".unread-1.sql")));
        });

        assert!(editor_has_keyboard(&workspace, cx));
        cx.simulate_input("x");
        assert!(front_dirty(&workspace, cx));
        cx.simulate_keystrokes("secondary-w");
        settle(cx);
        assert_eq!(
            platform_prompt(cx).as_deref(),
            Some("Save changes to draft.sql?")
        );
        answer_platform_prompt(cx, "Cancel");

        let (workspace, cx) = relaunch_closing(workspace, cx);
        bring_back(&workspace, cx, &path);
        assert_eq!(buffer(&workspace, cx), "xSELECT 1;", "hot exit kept it");
        assert_eq!(std::fs::read(&aside).unwrap(), b"-- \xff SELECT 1;");
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn a_scratch_file_that_cannot_be_moved_aside_is_never_deleted(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;

    store::tests::with_home(|| {
        // CI's `PG*` would put their connection, or the form saying what they
        // lack, in front on the relaunch.
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_on_sqlite(cx);
        let lock = |directory: &std::path::Path, mode| {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        let (workspace, cx, path, scratch) =
            relaunch_over_an_unreadable_scratch(workspace, cx, "stuck.sql", |directory| {
                lock(directory, 0o555)
            });
        // Root renames regardless, and so the tab carries on as above.
        if scratch.exists() {
            cx.simulate_input("x");
            cx.simulate_keystrokes("secondary-w");
            settle(cx);
            assert!(platform_prompt(cx).is_some());
            answer_platform_prompt(cx, "Don't Save");
            assert!(!is_open(&workspace, cx, &path));
            assert_eq!(std::fs::read(&scratch).unwrap(), b"-- \xff SELECT 1;");
        }
        lock(scratch.parent().unwrap(), 0o755);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn opening_a_file_already_open_brings_its_tab_up(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = user_sql_file("once.sql", "SELECT 1;");
        open_sql_file(&workspace, cx, &path);
        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        assert_eq!(front_file(&workspace, cx), None);
        let tabs = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profile()
                    .expect("a connection in front")
                    .session
                    .queries
                    .len()
            })
        };
        let before = tabs(cx);

        open_sql_file(&workspace, cx, &path);
        assert_eq!(tabs(cx), before);
        quit(workspace, cx);
    });
}

/// The front tab's id and its scratch file's path.
fn front_scratch(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (u64, std::path::PathBuf) {
    workspace.read_with(cx, |workspace, _| {
        let profile = workspace.profile().expect("a connection in front");
        let Tab::Query(id) = profile.session.active else {
            panic!("a query tab is in front");
        };
        let scratch = store::query_directory(&profile.id)
            .unwrap()
            .join(format!(".scratch-{id}.sql"));
        (id, scratch)
    })
}

fn is_open(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    path: &std::path::Path,
) -> bool {
    workspace.read_with(cx, |workspace, _| workspace.tab_on_file(path).is_some())
}

/// A file opened and typed into, so its tab holds text the file does not.
fn open_dirty_file(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    name: &str,
) -> std::path::PathBuf {
    let path = user_sql_file(name, "SELECT 1;");
    open_sql_file(workspace, cx, &path);
    cx.simulate_input("-- ");
    assert!(front_dirty(workspace, cx));
    path
}

/// Linux's prompt, which DBDelve draws: Escape cancels and hands the
/// keyboard back, a click behind it does not take the keyboard from it, the
/// buttons stack in the order asked, and Return saves.
#[gpui::test]
fn the_drawn_prompt_answers_the_keyboard_and_holds_the_mouse(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.update(|_, cx| cx.set_prompt_builder(crate::prompt::build));
        let path = open_dirty_file(&workspace, cx, "drawn.sql");
        let asked = |cx: &mut VisualTestContext| cx.update(|window, _| window.has_active_prompt());

        cx.simulate_keystrokes("secondary-w");
        settle(cx);
        assert!(asked(cx));
        assert!(!editor_has_keyboard(&workspace, cx), "the prompt has it");
        press(cx, "escape");
        assert!(!asked(cx));
        assert!(is_open(&workspace, cx, &path));
        assert!(editor_has_keyboard(&workspace, cx), "focus is handed back");

        cx.simulate_keystrokes("secondary-w");
        settle(cx);
        // Over the buffer, which takes the keyboard from GPUI's own prompt.
        let window = cx.update(|window, _| window.viewport_size());
        let buffer = gpui::point(window.width * 0.75, window.height * 0.2);
        cx.simulate_click(buffer, gpui::Modifiers::default());
        settle(cx);
        assert!(asked(cx));
        assert!(
            !editor_has_keyboard(&workspace, cx),
            "nothing behind took it"
        );
        let top =
            |cx: &mut VisualTestContext, button| cx.debug_bounds(button).expect("drawn").top();
        let stacked = [
            top(cx, "prompt: Save"),
            top(cx, "prompt: Don't Save"),
            top(cx, "prompt: Cancel"),
        ];
        assert!(stacked.is_sorted(), "{stacked:?}");
        press(cx, "enter");
        assert!(!asked(cx));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "-- SELECT 1;");
        assert!(!is_open(&workspace, cx, &path));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn closing_a_dirty_file_tab_asks_and_cancel_keeps_it(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = open_dirty_file(&workspace, cx, "kept.sql");

        cx.simulate_keystrokes("secondary-w");
        settle(cx);
        assert_eq!(
            platform_prompt(cx).as_deref(),
            Some("Save changes to kept.sql?")
        );
        answer_platform_prompt(cx, "Cancel");

        assert_eq!(platform_prompt(cx), None);
        assert_eq!(front_file(&workspace, cx), Some(path.clone()));
        assert!(front_dirty(&workspace, cx));
        assert!(editor_has_keyboard(&workspace, cx), "focus is handed back");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SELECT 1;");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn dont_save_closes_a_dirty_file_tab_and_removes_its_scratch(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = open_dirty_file(&workspace, cx, "dropped.sql");
        let (_, scratch) = front_scratch(&workspace, cx);
        workspace.read_with(cx, |workspace, cx| workspace.persist_buffers(cx));
        assert!(scratch.exists(), "the unsaved text is in its scratch file");

        cx.simulate_keystrokes("secondary-w");
        settle(cx);
        assert!(platform_prompt(cx).is_some());
        answer_platform_prompt(cx, "Don't Save");

        assert!(!is_open(&workspace, cx, &path));
        assert!(!scratch.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SELECT 1;");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn save_in_the_close_question_writes_the_file_and_closes_its_tab(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = open_dirty_file(&workspace, cx, "written.sql");

        cx.simulate_keystrokes("secondary-w");
        settle(cx);
        assert!(platform_prompt(cx).is_some());
        answer_platform_prompt(cx, "Save");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "-- SELECT 1;");
        assert!(!is_open(&workspace, cx, &path));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn close_all_tabs_asks_once_about_every_dirty_file_tab(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("SELECT 'untitled';");
        let first = open_dirty_file(&workspace, cx, "first.sql");
        let second = open_dirty_file(&workspace, cx, "second.sql");

        cx.simulate_keystrokes("secondary-k w");
        settle(cx);
        assert_eq!(
            platform_prompt(cx).as_deref(),
            Some("Save changes to 2 files?")
        );
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().expect("a connection in front").session;
            let asked = session.pending_discard.as_ref().expect("one question");
            let asked = asked.tabs().collect::<Vec<_>>();
            assert_eq!(asked.len(), 2);
            assert_eq!(session.strip_tabs(), asked, "the rest closed unasked");
        });
        answer_platform_prompt(cx, "Save");

        assert_eq!(platform_prompt(cx), None);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "-- SELECT 1;");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "-- SELECT 1;");
        assert_eq!(query_tab_count(&workspace, cx), 0);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn close_all_says_why_a_file_it_could_not_save_stayed_open(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let folder = store::home().unwrap().join("removed");
        std::fs::create_dir(&folder).unwrap();
        let stranded = open_dirty_file(&workspace, cx, "removed/stranded.sql");
        let saved = open_dirty_file(&workspace, cx, "saved.sql");
        std::fs::remove_dir_all(&folder).unwrap();

        cx.simulate_keystrokes("secondary-k w");
        settle(cx);
        assert!(platform_prompt(cx).is_some());
        answer_platform_prompt(cx, "Save");

        assert_eq!(std::fs::read_to_string(&saved).unwrap(), "-- SELECT 1;");
        assert!(!is_open(&workspace, cx, &saved));
        assert!(is_open(&workspace, cx, &stranded));
        let notice = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.notice.clone()
        });
        assert!(
            notice
                .as_deref()
                .is_some_and(|notice| notice.starts_with("Could not save stranded.sql")),
            "{notice:?}"
        );
        quit(workspace, cx);
    });
}

/// Close All Tabs over a file tab holding cell edits asks about the edits
/// first, holding the other dirty file tab with them, and Discard leaves one
/// Save question over both files.
#[gpui::test]
fn close_all_asks_about_edits_then_once_about_every_dirty_file(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let edited = user_sql_file("edited.sql", "select * from birds");
        open_sql_file(&workspace, cx, &edited);
        cx.dispatch_action(crate::actions::RunQuery);
        settle(cx);
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        cx.simulate_input("-- ");
        assert!(front_dirty(&workspace, cx));
        let other = open_dirty_file(&workspace, cx, "other.sql");

        cx.simulate_keystrokes("secondary-k w");
        settle(cx);
        assert_eq!(front_dialog(&workspace, cx), Some("discard"));
        confirm_discard(&workspace, cx);
        assert_eq!(front_dialog(&workspace, cx), None);
        assert_eq!(
            platform_prompt(cx).as_deref(),
            Some("Save changes to 2 files?"),
            "both files, in one question"
        );
        answer_platform_prompt(cx, "Save");

        assert_eq!(platform_prompt(cx), None);
        assert_eq!(
            std::fs::read_to_string(&edited).unwrap(),
            "-- select * from birds"
        );
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "-- SELECT 1;");
        assert_eq!(query_tab_count(&workspace, cx), 0);
        quit(workspace, cx);
    });
}

/// The tab on `path` brought back to the front.
fn bring_back(workspace: &Entity<Workspace>, cx: &mut VisualTestContext, path: &std::path::Path) {
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.bring_up_file(path, window, cx));
        });
    });
    settle(cx);
}

fn front_conflict(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Option<crate::session::Conflict> {
    query_tab(workspace, cx, |tab, _, _| tab.file.as_ref()?.conflict)
}

#[gpui::test]
fn an_outside_write_reloads_a_clean_tab_when_it_comes_forward(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = user_sql_file("outside.sql", "SELECT 1;");
        open_sql_file(&workspace, cx, &path);
        cx.simulate_keystrokes("secondary-t");
        settle(cx);

        std::fs::write(&path, "SELECT 22;").unwrap();
        bring_back(&workspace, cx, &path);
        assert_eq!(buffer(&workspace, cx), "SELECT 22;");
        assert!(!front_dirty(&workspace, cx));

        // And when the window comes back with the tab already in front.
        cx.deactivate_window();
        std::fs::write(&path, "SELECT 333;").unwrap();
        cx.update(|window, _| window.activate_window());
        settle(cx);
        assert_eq!(buffer(&workspace, cx), "SELECT 333;");
        assert!(!front_dirty(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_outside_write_under_unsaved_text_raises_the_bar(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = open_dirty_file(&workspace, cx, "contested.sql");
        cx.simulate_keystrokes("secondary-t");
        settle(cx);

        std::fs::write(&path, "SELECT 22;").unwrap();
        bring_back(&workspace, cx, &path);
        assert_eq!(buffer(&workspace, cx), "-- SELECT 1;", "nothing is lost");
        assert_eq!(
            front_conflict(&workspace, cx),
            Some(crate::session::Conflict::Asking)
        );
        assert!(cx.debug_bounds("file-banner: Changed on disk").is_some());

        let (id, _) = front_scratch(&workspace, cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.take_disk(id, false, window, cx)
            });
        });
        settle(cx);
        assert!(
            cx.debug_bounds("file-banner: Changed on disk").is_none(),
            "kept"
        );
        assert!(front_dirty(&workspace, cx));
        cx.simulate_keystrokes("secondary-s");
        settle(cx);
        assert_eq!(
            platform_prompt(cx).as_deref(),
            Some("contested.sql changed on disk. Overwrite it?")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SELECT 22;");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn save_after_an_outside_write_asks_before_overwriting_it(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = open_dirty_file(&workspace, cx, "raced.sql");
        std::fs::write(&path, "SELECT 22;").unwrap();

        cx.simulate_keystrokes("secondary-s");
        settle(cx);
        assert!(platform_prompt(cx).is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SELECT 22;");
        answer_platform_prompt(cx, "Overwrite");

        assert_eq!(platform_prompt(cx), None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "-- SELECT 1;");
        assert!(!front_dirty(&workspace, cx));
        assert!(editor_has_keyboard(&workspace, cx), "focus is handed back");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_file_deleted_on_disk_says_so_and_save_writes_it_again(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = user_sql_file("deleted.sql", "SELECT 1;");
        open_sql_file(&workspace, cx, &path);
        cx.simulate_keystrokes("secondary-t");
        settle(cx);

        std::fs::remove_file(&path).unwrap();
        bring_back(&workspace, cx, &path);
        assert_eq!(
            front_file(&workspace, cx),
            Some(path.clone()),
            "still bound"
        );
        assert!(front_dirty(&workspace, cx));
        assert!(cx.debug_bounds("file-banner: Deleted on disk").is_some());

        cx.simulate_keystrokes("secondary-s");
        settle(cx);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SELECT 1;");
        assert!(!front_dirty(&workspace, cx));
        assert!(cx.debug_bounds("file-banner: Deleted on disk").is_none());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_deleted_file_written_again_says_it_changed_and_offers_reload(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = open_dirty_file(&workspace, cx, "returned.sql");
        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        std::fs::remove_file(&path).unwrap();
        bring_back(&workspace, cx, &path);
        assert!(cx.debug_bounds("file-banner: Deleted on disk").is_some());

        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        std::fs::write(&path, "SELECT 22;").unwrap();
        bring_back(&workspace, cx, &path);
        assert!(cx.debug_bounds("file-banner: Changed on disk").is_some());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_file_gone_at_relaunch_comes_back_saying_it_was_deleted(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        // CI's `PG*` would put their connection, or the form saying what they
        // lack, in front on the relaunch.
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = open_dirty_file(&workspace, cx, "vanished.sql");
        let (id, _) = front_scratch(&workspace, cx);

        close_window(workspace, cx);
        std::fs::remove_file(&path).unwrap();
        let (workspace, cx) = launch_again(cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_tab(Tab::Query(id), window, cx)
            });
        });
        settle(cx);
        assert_eq!(front_file(&workspace, cx), Some(path));
        assert_eq!(buffer(&workspace, cx), "-- SELECT 1;");
        assert!(front_dirty(&workspace, cx));
        assert!(cx.debug_bounds("file-banner: Deleted on disk").is_some());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn save_as_into_the_saved_queries_folder_lists_the_query(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.simulate_input("SELECT 2;");
        cx.simulate_keystrokes("secondary-s");
        settle(cx);
        let queries = workspace.read_with(cx, |workspace, _| {
            let id = &workspace.profile().expect("a connection in front").id;
            store::query_directory(id).unwrap()
        });
        std::fs::create_dir_all(&queries).unwrap();
        cx.simulate_new_path_selection(|_| Some(queries.join("nightly")));
        settle(cx);

        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().expect("a connection in front").session;
            assert!(session.saved_queries.iter().any(|name| name == "nightly"));
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn save_after_keep_mine_recreates_a_file_deleted_since_without_asking(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = open_dirty_file(&workspace, cx, "kept-then-gone.sql");
        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        std::fs::write(&path, "SELECT 22;").unwrap();
        bring_back(&workspace, cx, &path);
        let (id, _) = front_scratch(&workspace, cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.take_disk(id, false, window, cx)
            });
        });
        settle(cx);

        std::fs::remove_file(&path).unwrap();
        cx.simulate_keystrokes("secondary-s");
        settle(cx);
        assert_eq!(platform_prompt(cx), None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "-- SELECT 1;");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn save_asks_about_an_outside_edit_that_kept_the_files_stamp(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = open_dirty_file(&workspace, cx, "same-stamp.sql");
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "SELECT 2;").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(modified))
            .unwrap();

        cx.simulate_keystrokes("secondary-s");
        settle(cx);
        assert!(platform_prompt(cx).is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SELECT 2;");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn closing_a_clean_tab_whose_file_was_deleted_behind_it_asks(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = user_sql_file("only-copy.sql", "SELECT 1;");
        open_sql_file(&workspace, cx, &path);
        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        std::fs::remove_file(&path).unwrap();

        cx.simulate_keystrokes("secondary-k w");
        settle(cx);
        assert!(platform_prompt(cx).is_some());
        assert!(is_open(&workspace, cx, &path));
        answer_platform_prompt(cx, "Save");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "SELECT 1;");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_bar_button_pressed_from_the_keyboard_hands_it_back_to_the_buffer(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = open_dirty_file(&workspace, cx, "keyboard.sql");
        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        std::fs::write(&path, "SELECT 22;").unwrap();
        bring_back(&workspace, cx, &path);
        assert!(editor_has_keyboard(&workspace, cx));

        // Tab order runs from the bar's buttons into the buffer below them.
        cx.update(|window, cx| window.focus_prev(cx));
        settle(cx);
        assert!(!editor_has_keyboard(&workspace, cx), "a bar button has it");
        press(cx, "enter");
        settle(cx);

        assert_eq!(
            front_conflict(&workspace, cx),
            Some(crate::session::Conflict::KeptMine)
        );
        assert!(editor_has_keyboard(&workspace, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_relaunch_asks_about_a_change_made_while_the_app_was_closed(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = open_dirty_file(&workspace, cx, "moved.sql");

        close_window(workspace, cx);
        std::fs::write(&path, "SELECT 333;").unwrap();
        let (workspace, cx) = launch_again(cx);
        bring_back(&workspace, cx, &path);
        assert_eq!(buffer(&workspace, cx), "-- SELECT 1;");
        assert_eq!(
            front_conflict(&workspace, cx),
            Some(crate::session::Conflict::Asking)
        );

        close_window(workspace, cx);
        let (workspace, cx) = launch_again(cx);
        bring_back(&workspace, cx, &path);
        assert_eq!(
            front_conflict(&workspace, cx),
            Some(crate::session::Conflict::Asking),
            "asking until it is answered"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_relaunch_keeps_keep_mine_so_save_still_asks(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        // CI's `PG*` would put their connection, or the form saying what they
        // lack, in front on the relaunch.
        crate::IGNORE_ENVIRONMENT.set(true);
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = open_dirty_file(&workspace, cx, "kept.sql");
        std::fs::write(&path, "SELECT 22;").unwrap();
        let (id, _) = front_scratch(&workspace, cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.take_disk(id, false, window, cx)
            });
        });
        settle(cx);

        let (workspace, cx) = relaunch_closing(workspace, cx);
        bring_back(&workspace, cx, &path);
        assert_eq!(
            front_conflict(&workspace, cx),
            Some(crate::session::Conflict::KeptMine)
        );
        assert!(editor_has_keyboard(&workspace, cx));
        cx.simulate_keystrokes("secondary-s");
        settle(cx);
        assert!(platform_prompt(cx).is_some());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_offer_to_move_saved_queries_shows_once_and_not_after_not_now(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        store::write_query("e2e", "daily", "select 1").expect("the query saves");
        let (workspace, cx) = launch_on_sqlite(cx);
        settle(cx);
        assert!(cx.debug_bounds("queries-notice").is_some());
        let not_now = cx
            .debug_bounds("queries-notice-dismiss")
            .expect("Not Now is drawn");
        cx.simulate_click(not_now.center(), gpui::Modifiers::default());
        settle(cx);
        assert!(cx.debug_bounds("queries-notice").is_none());
        assert!(editor_has_keyboard(&workspace, cx));

        let (workspace, cx) = relaunch_closing(workspace, cx);
        assert!(
            cx.debug_bounds("queries-notice").is_none(),
            "Not Now is remembered"
        );
        assert!(workspace.read_with(cx, |workspace, _| workspace.has_saved_queries()));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_move_files_each_connections_queries_and_repoints_a_dirty_tab_on_another(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        crate::IGNORE_ENVIRONMENT.set(true);
        store::write_query("b", "nightly", "select 1").expect("the query saves");
        let (workspace, cx) = launch_connections(
            cx,
            "",
            "",
            &format!(
                "{}{}",
                sqlite_profile("a", "A", ""),
                sqlite_profile("b", "B/Reports", "")
            ),
        );
        let queries = crate::sql_file::canonical(&store::query_directory("b").unwrap());
        let legacy = queries.join("nightly.sql");
        open_sql_file(&workspace, cx, &legacy);
        cx.simulate_input("-- ");
        assert!(front_dirty(&workspace, cx));

        let folder = store::home().unwrap().join("Moved");
        std::fs::create_dir_all(&folder).unwrap();
        workspace.update(cx, |workspace, cx| {
            workspace.move_saved_queries(&folder, cx)
        });
        settle(cx);

        let moved = crate::sql_file::canonical(&folder.join("B-Reports").join("nightly.sql"));
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "select 1");
        assert!(!legacy.exists());
        assert_eq!(front_file(&workspace, cx), Some(moved.clone()));
        assert!(
            front_dirty(&workspace, cx),
            "the unsaved text stays unsaved"
        );
        assert_eq!(buffer(&workspace, cx), "-- select 1");
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.check_front_file(window, cx))
        });
        assert_eq!(
            front_conflict(&workspace, cx),
            None,
            "the copy is no change"
        );
        workspace.read_with(cx, |workspace, cx| {
            let b = &workspace.profiles[1];
            assert!(b.session.saved_queries.is_empty());
            assert_eq!(b.recent_files, std::slice::from_ref(&moved));
            let a = &workspace.profiles[0];
            assert_eq!(a.recent_files, std::slice::from_ref(&moved));
            let tree = a.session.explorer_tree.read(cx);
            assert!(tree.index_of(&explorer::file_row(&moved).into()).is_some());
            assert!(tree.index_of(&explorer::file_row(&legacy).into()).is_none());
        });
        assert!(cx.debug_bounds("queries-notice").is_some(), "the report");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_move_skips_a_name_a_tab_on_a_deleted_file_holds(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        crate::IGNORE_ENVIRONMENT.set(true);
        store::write_query("a", "nightly", "select 1").expect("the query saves");
        let (workspace, cx) = launch_connections(cx, "", "", &sqlite_profile("a", "A", ""));
        let folder = store::home().unwrap().join("Moved");
        std::fs::create_dir_all(folder.join("A")).unwrap();
        let taken = user_sql_file("Moved/A/nightly.sql", "select 2");
        open_sql_file(&workspace, cx, &taken);
        std::fs::remove_file(&taken).unwrap();
        let queries = crate::sql_file::canonical(&store::query_directory("a").unwrap());
        let legacy = queries.join("nightly.sql");
        open_sql_file(&workspace, cx, &legacy);

        workspace.update(cx, |workspace, cx| {
            workspace.move_saved_queries(&folder, cx)
        });
        settle(cx);

        assert!(legacy.exists(), "left where it was");
        assert!(!taken.exists());
        assert!(is_open(&workspace, cx, &legacy));
        assert!(is_open(&workspace, cx, &taken));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_opened_file_is_listed_under_files_and_remove_from_list_leaves_it(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = user_sql_file("orders.sql", "SELECT 1;");
        open_sql_file(&workspace, cx, &path);
        let row = explorer::file_row(&path);
        explorer_row(&workspace, &row, cx);

        let (id, _) = front_scratch(&workspace, cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.close_now(Tab::Query(id), window, cx)
            })
        });
        settle(cx);
        explorer_row(&workspace, &row, cx);
        pick_from_explorer_menu(&workspace, cx, &row, 3);
        settle(cx);
        let listed = workspace.read_with(cx, |workspace, cx| {
            let tree = workspace.profile().unwrap().session.explorer_tree.clone();
            tree.read(cx).index_of(&row.clone().into()).is_some()
        });
        assert!(!listed, "the row is gone");
        assert!(path.exists(), "and the file is not");
        quit(workspace, cx);
    });
}

#[gpui::test]
fn clicking_a_listed_files_open_tab_selects_that_files_row(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let query = user_sql_file("query.sql", "SELECT 1;");
        let reports = user_sql_file("Reports.sql", "SELECT 2;");
        open_sql_file(&workspace, cx, &query);
        open_sql_file(&workspace, cx, &reports);

        let row = explorer_row(&workspace, &explorer::file_row(&query), cx).center();
        cx.simulate_click(row, gpui::Modifiers::default());
        settle(cx);
        assert_eq!(front_file(&workspace, cx), Some(query.clone()));
        let selected = workspace.read_with(cx, |workspace, cx| {
            let tree = workspace.profile().unwrap().session.explorer_tree.clone();
            tree.read(cx)
                .selected_item()
                .map(|item| item.id.to_string())
        });
        assert_eq!(selected, Some(explorer::file_row(&query)));
        quit(workspace, cx);
    });
}

/// The Files group's row menu orders every Files group, in either tree, and
/// the order is kept in the settings. Bringing a listed file up moves no row.
#[gpui::test]
fn the_files_groups_menu_sorts_its_rows_by_name_or_date_added(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let [zeta, alpha, mid] =
            ["zeta.sql", "alpha.sql", "Mid.sql"].map(|name| user_sql_file(name, "SELECT 1;"));
        for path in [&zeta, &alpha, &mid] {
            open_sql_file(&workspace, cx, path);
        }
        let rows = |paths: [&std::path::PathBuf; 3]| paths.map(|path| explorer::file_row(path));
        let listed = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                let tree = workspace.profile().unwrap().session.explorer_tree.read(cx);
                (0..)
                    .map_while(|ix| tree.entry(ix))
                    .map(|entry| entry.item().id.to_string())
                    .filter(|id| explorer::is_file_row(id))
                    .collect::<Vec<_>>()
            })
        };
        let saved = || {
            store::peek_profiles(&[])
                .unwrap()
                .settings
                .unwrap()
                .files_order
        };
        assert_eq!(listed(&workspace, cx), rows([&alpha, &mid, &zeta]));

        pick_from_explorer_menu(&workspace, cx, explorer::FILES_GROUP, 2);
        settle(cx);
        assert_eq!(listed(&workspace, cx), rows([&zeta, &alpha, &mid]));
        assert_eq!(saved().as_deref(), Some("added"));
        let row = explorer_row(&workspace, &explorer::file_row(&alpha), cx).center();
        cx.simulate_click(row, gpui::Modifiers::default());
        settle(cx);
        assert_eq!(front_file(&workspace, cx), Some(alpha.clone()));
        assert_eq!(listed(&workspace, cx), rows([&zeta, &alpha, &mid]));

        workspace.update(cx, |workspace, cx| {
            workspace.set_view_mode(crate::sidebar::ViewMode::Complex, cx)
        });
        settle(cx);
        let group = scoped_row("e2e", None, explorer::FILES_GROUP);
        reveal_connections_row(&workspace, cx, &group);
        let index = connections_row(&workspace, cx, &group).expect("the group is listed");
        pick_from_connections_menu(cx, index, 1);
        settle(cx);
        open_connections_rows(&workspace, cx, std::slice::from_ref(&group));
        let files: Vec<String> = connections_rows(&workspace, cx)
            .into_iter()
            .filter_map(|(_, id)| Some(crate::sidebar::unscoped(&id)?.inner.to_string()))
            .filter(|inner| explorer::is_file_row(inner))
            .collect();
        assert_eq!(files, rows([&alpha, &mid, &zeta]));
        assert_eq!(saved(), None, "by name is the default, not written");
        quit(workspace, cx);
    });
}

/// A listed file opens in a file tab whatever its name, Save As having
/// taken the one typed, and one gone since asks to leave the list.
#[gpui::test]
fn a_listed_file_opens_as_sql_whatever_its_extension(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        cx.executor().allow_parking();
        let listed = |name: &str, cx: &mut VisualTestContext| {
            let path = user_sql_file(name, "SELECT 1;");
            open_sql_file(&workspace, cx, &path);
            let (id, _) = front_scratch(&workspace, cx);
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.close_now(Tab::Query(id), window, cx)
                })
            });
            settle(cx);
            path
        };
        let open_row = |path: &std::path::Path, cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.open_listed_file(path.to_path_buf(), window, cx)
                })
            });
        };
        let notes = listed("notes.sql", cx);
        let csv = notes.with_file_name("notes.csv");
        std::fs::rename(&notes, &csv).unwrap();
        workspace.update(cx, |workspace, cx| {
            let profile_id = workspace.profile().unwrap().id.clone();
            workspace.remember_file(&profile_id, &csv, cx);
        });
        open_row(&csv, cx);
        for _ in 0..500 {
            settle(cx);
            if front_file(&workspace, cx).as_deref() == Some(csv.as_path()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(front_file(&workspace, cx), Some(csv));

        let gone = listed("gone.sql", cx);
        std::fs::remove_file(&gone).unwrap();
        open_row(&gone, cx);
        for _ in 0..500 {
            settle(cx);
            if front_dialog(&workspace, cx).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(front_dialog(&workspace, cx), Some("forget-file"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_palette_offers_the_saved_queries_move_with_no_connection_in_front(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        store::write_query("e2e", "a", "select 1").expect("the query saves");
        let (workspace, cx) = launch_on_sqlite(cx);
        disconnect(cx);
        workspace.read_with(cx, |workspace, _| assert!(workspace.profile().is_none()));
        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        assert!(
            palette_labels(&workspace, cx)
                .iter()
                .any(|label| label == "Move saved queries to a folder…")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_palette_names_files_of_one_name_by_their_folders_too(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        for folder in ["reports", "billing"] {
            std::fs::create_dir_all(store::home().unwrap().join(folder)).unwrap();
            let path = user_sql_file(&format!("{folder}/orders.sql"), "SELECT 1;");
            open_sql_file(&workspace, cx, &path);
        }
        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        let labels = palette_labels(&workspace, cx);
        for label in ["orders.sql — reports", "orders.sql — billing"] {
            assert!(labels.iter().any(|held| held == label), "{label}");
        }
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_file_removed_from_the_list_while_open_leaves_it_as_its_tab_closes(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite(cx);
        let path = user_sql_file("orders.sql", "SELECT 1;");
        open_sql_file(&workspace, cx, &path);
        let row = explorer::file_row(&path);
        explorer_row(&workspace, &row, cx);
        workspace.update(cx, |workspace, cx| workspace.forget_recent_file(&path, cx));
        settle(cx);
        explorer_row(&workspace, &row, cx);

        let (id, _) = front_scratch(&workspace, cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.close_now(Tab::Query(id), window, cx)
            })
        });
        settle(cx);
        let listed = workspace.read_with(cx, |workspace, cx| {
            let tree = workspace.profile().unwrap().session.explorer_tree.clone();
            tree.read(cx).index_of(&row.clone().into()).is_some()
        });
        assert!(!listed, "the row went with the tab");
        quit(workspace, cx);
    });
}

#[cfg(unix)]
#[gpui::test]
fn a_saved_query_that_links_to_another_moves_as_its_text_too(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        store::write_query("e2e", "a", "select 1").expect("the query saves");
        let queries = store::query_directory("e2e").unwrap();
        std::os::unix::fs::symlink(queries.join("a.sql"), queries.join("b.sql")).unwrap();
        let (workspace, cx) = launch_on_sqlite(cx);
        let folder = store::home().unwrap().join("Moved");
        workspace.update(cx, |workspace, cx| {
            workspace.move_saved_queries(&folder, cx)
        });
        settle(cx);

        let moved = folder.join("E2E");
        assert_eq!(
            std::fs::read_to_string(moved.join("a.sql")).unwrap(),
            "select 1"
        );
        assert_eq!(
            std::fs::read_to_string(moved.join("b.sql")).unwrap(),
            "select 1"
        );
        assert!(std::fs::symlink_metadata(queries.join("b.sql")).is_err());
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
        // Not through the action, which would run the executor to the answer
        // before the test can stage anything.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.apply_edits(&crate::actions::ApplyEdits, window, cx)
            })
        });
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
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

        // The rows were not read again, but the sent cell holds what was
        // sent: the grid shows it, and the second apply's re-read of row 1
        // matches it, so the edits kept are applied rather than refused as
        // drift the first apply caused.
        assert_eq!(cell_of(&grid, cx, 0, 1).as_deref(), Some("robin"));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("finch")
        );
        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("jay")
        );
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().pending_count(), 0);
        });
        quit(workspace, cx);
    });
}

/// A query tab's kept apply holds what the server stored, not what was
/// typed: SQLite stores the integer `07` as 7, and the next apply's re-read
/// of that row is compared with 7, so it is not refused as drift.
#[gpui::test]
fn a_kept_apply_holds_the_value_the_server_stored(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text not null, n integer); \
             insert into birds values (1, 'wren', 1), (2, 'tit', 2);",
        );
        run_in_front(&workspace, cx, "select * from birds order by id");
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        let stage = |cx: &mut VisualTestContext, row: usize, col: usize, value: NewValue| {
            grid.update(cx, |grid, _| {
                assert!(grid.delegate_mut().set_pending(row, col, value))
            });
        };
        stage(cx, 0, 2, NewValue::Value("07".into()));
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.apply_edits(&crate::actions::ApplyEdits, window, cx)
            })
        });
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        // Staged while the batch is out, so the rows are not read again.
        stage(cx, 1, 1, NewValue::Value("jay".into()));
        settle(cx);
        assert_eq!(
            on_disk("select n from birds where id = 1").as_deref(),
            Some("7")
        );
        assert_eq!(cell_of(&grid, cx, 0, 2).as_deref(), Some("7"));

        stage(cx, 0, 1, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None, "applied, not refused");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("finch")
        );
        quit(workspace, cx);
    });
}

/// A cell a query tab's kept apply set to DEFAULT holds the default the
/// server wrote, so putting it back to what it held before is an edit. On
/// DuckDB: SQLite has no DEFAULT in an UPDATE.
#[gpui::test]
fn a_kept_apply_holds_the_default_it_wrote(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().expect("the test home has a data directory");
        std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
        let database = directory.join("e2e.duckdb");
        ::duckdb::Connection::open(&database)
            .and_then(|connection| {
                connection.execute_batch(
                    "CREATE TABLE birds (id INTEGER PRIMARY KEY, name TEXT NOT NULL, \
                     note TEXT DEFAULT 'none'); \
                     INSERT INTO birds VALUES (1, 'wren', NULL), (2, 'tit', NULL);",
                )
            })
            .expect("the schema must apply");
        std::fs::write(
            directory.join("profiles.toml"),
            format!(
                "active = \"e2e\"\n[settings]\ncheck_for_updates = false\n[[profiles]]\n\
                 id = \"e2e\"\nname = \"E2E\"\nhost = \"\"\ndatabase = \"\"\nuser = \"\"\n\
                 engine = \"duckdb\"\npath = {:?}\n",
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
        run_in_front(&workspace, cx, "SELECT * FROM birds ORDER BY id");
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        let stage = |cx: &mut VisualTestContext, row: usize, col: usize, value: NewValue| {
            grid.update(cx, |grid, _| {
                grid.delegate_mut().set_pending(row, col, value)
            })
        };
        assert!(stage(cx, 0, 2, NewValue::Default));
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.apply_edits(&crate::actions::ApplyEdits, window, cx)
            })
        });
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        // Staged while the batch is out, so the rows are not read again.
        assert!(stage(cx, 1, 1, NewValue::Value("jay".into())));
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None, "the apply ran");
        assert_eq!(cell_of(&grid, cx, 0, 2).as_deref(), Some("none"));

        // NULL is what the cell held before the apply, not what it holds now.
        assert!(stage(cx, 0, 2, NewValue::Null), "setting NULL is an edit");
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None);
        // Nothing is left staged, so the statement ran again.
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(cell_of(&grid, cx, 0, 2), None, "the server holds NULL");
        assert_eq!(cell_of(&grid, cx, 1, 1).as_deref(), Some("jay"));
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
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        grid.update(cx, |grid, _| {
            // A NULL into a NOT NULL column, which the table refuses.
            assert!(grid.delegate_mut().set_pending(0, 1, NewValue::Null));
        });
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply edits", true, None)),
            "the statement is read in the review before it runs"
        );
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 2);
            assert_eq!(grid.delegate().pending_count(), 1);
        });
        assert!(
            matches!(front_state(&workspace, cx), QueryState::Complete { .. }),
            "the tab is as it was, not an error pane over the rows"
        );
        let (title, _, error) = front_review(&workspace, cx).expect("the review stays");
        assert_eq!(title, "Apply edits");
        let error = error.expect("the failure is said in the review");
        assert!(error.contains("NOT NULL"), "{error}");
        assert_eq!(
            buffer(&workspace, cx),
            "select * from birds order by id",
            "nothing was appended to the buffer"
        );
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
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

/// A trigger that changes an unedited binary column during a Row send: the
/// row panel shows the bytes read back, not the ones fetched before.
#[gpui::test]
fn the_row_panel_shows_binary_bytes_a_row_send_read_back(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text not null, b blob); \
             insert into birds values (1, 'wren', x'4142'), (2, 'tit', x'4142'); \
             create trigger rewrites after update of name on birds \
             begin update birds set b = x'4344' where id = new.id; end;",
        );
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        press(cx, "secondary-shift-e");
        let shown = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                let inputs = workspace.row_panel.inputs.as_ref().expect("Edit mode");
                let field = inputs.fields[2].as_ref().expect("an input on the blob");
                field.input.read(cx).value().to_string()
            })
        };
        assert_eq!(shown(&workspace, cx), "x'4142'");

        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        save(&workspace, cx);
        assert_eq!(cell_of(&grid, cx, 0, 1).as_deref(), Some("robin"));
        assert_eq!(shown(&workspace, cx), "x'4344'");
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

/// Sorting in memory reorders the rows the send's landing names its row in,
/// so the switch waits for the send, which then lands and clears its edit.
#[gpui::test]
fn entering_client_sorting_is_refused_while_a_row_send_is_out(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (id, grid) = open_in_row_mode(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        // Not through the action, which runs the executor to the answer.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.apply_edits(&crate::actions::ApplyEdits, window, cx);
                workspace.set_view_sorting(true, window, cx);
            })
        });
        settle(cx);

        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(pending_of(&grid, cx), 0, "the landing cleared the edit");
        workspace.read_with(cx, |workspace, _| {
            let sorting = workspace
                .profile()
                .unwrap()
                .session
                .sorting(Tab::Object(id))
                .cloned();
            assert!(
                sorting.is_some_and(|sorting| sorting.client_keys().is_none()),
                "still sorted by the server"
            );
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn escape_reverts_nothing_in_a_draft_view(cx: &mut TestAppContext) {
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

/// The review stays while its batch runs: closed, it would let an edit be
/// staged that the batch's success then drops unsaid.
#[gpui::test]
fn the_apply_review_does_not_close_while_its_batch_runs(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert!(front_review(&workspace, cx).is_some());

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        assert_eq!(state_kind(&workspace, cx), "running");
        // The review's Cancel and Esc stop the batch first.
        workspace.update(cx, |workspace, cx| workspace.close_apply_review(cx));
        assert!(front_review(&workspace, cx).is_some(), "the review stays");
        assert!(matches!(
            front_state(&workspace, cx),
            QueryState::Running {
                cancelling: Some(_),
                ..
            }
        ));
        let may_stage = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profile()
                    .unwrap()
                    .session
                    .may_stage_into(crate::session::Tab::Object(id))
            })
        };
        assert!(!may_stage(&workspace, cx));

        // Asked again once the cancel is out, they close it. The grid still
        // takes no edit until the batch lands: a success would drop it unsaid.
        workspace.update(cx, |workspace, cx| workspace.close_apply_review(cx));
        assert_eq!(front_review(&workspace, cx), None, "the review closed");
        assert!(!may_stage(&workspace, cx));

        // Here the batch is under way before the cancel reaches it, so it
        // lands as written, the draft clears and the grid takes edits again.
        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("finch")
        );
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(front_review(&workspace, cx), None);
        assert!(may_stage(&workspace, cx));
        quit(workspace, cx);
    });
}

/// Two tabs whose reviews were closed while their batches were out each keep
/// their own grid from taking edits until their own run lands: the second
/// does not free the first.
#[gpui::test]
fn reviews_closed_on_two_tabs_while_out_each_hold_their_own_grid(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;
    use crate::session::Tab;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text not null); \
             insert into birds values (1, 'wren'), (2, 'tit'); \
             create table nests (id integer primary key, name text not null); \
             insert into nests values (1, 'oak');",
        );
        draft_by_default(&workspace, cx);
        let birds = open_own_table(&workspace, cx, "birds");
        stage(
            &relation_grid(&workspace, cx, birds),
            cx,
            0,
            1,
            NewValue::Value("finch".into()),
        );
        let nests = open_own_table(&workspace, cx, "nests");
        stage(
            &relation_grid(&workspace, cx, nests),
            cx,
            0,
            1,
            NewValue::Value("elm".into()),
        );
        let may_stage = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext, id| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profile()
                    .unwrap()
                    .session
                    .may_stage_into(Tab::Object(id))
            })
        };
        let run_and_close = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext, id| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.activate_tab(Tab::Object(id), window, cx);
                workspace.apply_edits(&crate::actions::ApplyEdits, window, cx);
            });
            assert!(
                front_review(workspace, cx).is_some(),
                "{id}: a review is open"
            );
            workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
            // The first asks the batch to stop, the second closes the review.
            workspace.update(cx, |workspace, cx| workspace.close_apply_review(cx));
            workspace.update(cx, |workspace, cx| workspace.close_apply_review(cx));
            assert_eq!(front_review(workspace, cx), None, "{id}: the review closed");
        };

        run_and_close(&workspace, cx, birds);
        run_and_close(&workspace, cx, nests);
        assert!(
            !may_stage(&workspace, cx, birds),
            "birds' batch is still out"
        );
        assert!(
            !may_stage(&workspace, cx, nests),
            "nests' batch is still out"
        );

        settle(cx);
        assert!(may_stage(&workspace, cx, birds));
        assert!(may_stage(&workspace, cx, nests));
        quit(workspace, cx);
    });
}

/// A review whose run a reconnect has moved the profile on from closes at
/// once: that run's landing is dropped, and nothing else would end it.
#[gpui::test]
fn the_apply_review_closes_once_a_reconnect_has_moved_on_from_its_run(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        workspace.update(cx, |workspace, cx| {
            let index = workspace.active.expect("a profile is in front");
            workspace.reconnect(index, cx)
        });
        assert_eq!(state_kind(&workspace, cx), "running");
        workspace.update(cx, |workspace, cx| workspace.close_apply_review(cx));
        assert_eq!(front_review(&workspace, cx), None, "the review closed");
        quit(workspace, cx);
    });
}

/// A Row send whose landing a reconnect drops still ends: its row is no longer
/// out, so Esc reverts it again.
#[gpui::test]
fn a_row_send_dropped_by_a_reconnect_is_no_longer_out(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (id, grid) = open_in_row_mode(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        // Not through Save, which runs the executor to the answer.
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.save_row(id, 0, None, None, cx))
        });
        assert_eq!(state_kind(&workspace, cx), "running");
        assert_eq!(
            grid.read_with(cx, |table, _| table.delegate().sending_row()),
            Some(0)
        );
        workspace.update(cx, |workspace, cx| {
            let index = workspace.active.expect("a profile is in front");
            workspace.reconnect(index, cx)
        });
        settle(cx);
        assert_eq!(
            grid.read_with(cx, |table, _| table.delegate().sending_row()),
            None,
            "the dropped send ended"
        );
        // It wrote, and nothing on screen says so: the row is marked and the
        // grid sends nothing more until its rows are read again.
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        grid.read_with(cx, |table, _| {
            let grid = table.delegate();
            assert!(grid.outcome_unknown());
            assert_eq!(
                grid.send_failure(),
                Some((
                    0,
                    "The connection was refreshed before this edit's answer came back. Read the \
                     rows again to see what the server holds."
                ))
            );
        });
        quit(workspace, cx);
    });
}

/// A reconnect drops the landing of a review's run that wrote its draft. The
/// review goes and the grid is marked, so a second Run cannot send the
/// `INSERT` again.
#[gpui::test]
fn a_reviews_run_dropped_by_a_reconnect_is_not_sent_again(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        // Only the name is typed: the id is the server's, so a second INSERT
        // would add a second row.
        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        stage(
            &grid,
            cx,
            2,
            1,
            crate::result_grid::NewValue::Value("robin".into()),
        );
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply draft", true, None))
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        assert_eq!(state_kind(&workspace, cx), "running");
        workspace.update(cx, |workspace, cx| {
            let index = workspace.active.expect("a profile is in front");
            workspace.reconnect(index, cx)
        });
        settle(cx);
        assert_eq!(
            on_disk("select count(*) from birds where name = 'robin'").as_deref(),
            Some("1")
        );
        assert_eq!(front_review(&workspace, cx), None, "the review is gone");

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            on_disk("select count(*) from birds where name = 'robin'").as_deref(),
            Some("1"),
            "inserted once"
        );
        quit(workspace, cx);
    });
}

/// A Row send the mode prompt held is marked out once it is approved and runs,
/// as one sent straight away is: Esc leaves its row alone until it lands.
#[gpui::test]
fn a_row_send_approved_at_the_mode_prompt_is_out_until_it_lands(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        workspace.update(cx, |workspace, cx| {
            workspace.set_mode(crate::sql::Mode::ReadOnly, cx)
        });
        settle(cx);
        press(cx, "down");
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().unwrap().session.pending_run.is_some());
        });

        workspace.update(cx, |workspace, cx| workspace.approve_pending_run(cx));
        assert_eq!(state_kind(&workspace, cx), "running");
        assert_eq!(
            grid.read_with(cx, |table, _| table.delegate().sending_row()),
            Some(0),
            "the approved send is marked out"
        );

        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(
            grid.read_with(cx, |table, _| table.delegate().sending_row()),
            None
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

/// An input left untouched stages nothing when it closes, even when the cell
/// under it changed since it opened, as a Row send landing changes it: its
/// text is the old value, which was never typed.
#[gpui::test]
fn an_untouched_input_whose_cell_changed_under_it_stages_nothing_when_clicked_away(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);

        // What a landing whose trigger rewrote the cell leaves under the input.
        let landed = crate::db::Row::new([Some("1"), Some("WREN"), Some("1")]).unwrap();
        grid.update(cx, |table, cx| {
            assert!(table.delegate_mut().replace_row(0, landed));
            table.refresh(cx);
        });

        click_cell_of_row(cx, 1);
        assert_eq!(pending_of(&grid, cx), 0, "nothing was typed");
        assert_eq!(cell_of(&grid, cx, 0, 1).as_deref(), Some("WREN"));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "nothing was sent"
        );
        quit(workspace, cx);
    });
}

/// Save with a cell's input open sends what it holds with the row's other
/// staged cells, as Enter then Save would.
#[gpui::test]
fn save_in_row_mode_sends_the_open_inputs_text_with_the_rows_edits(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        stage(&grid, cx, 0, 2, NewValue::Value("7".into()));
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("robin");
        settle(cx);

        cx.simulate_keystrokes("secondary-s");
        settle(cx);
        assert_eq!(
            on_disk("select name || ':' || n from birds where id = 1").as_deref(),
            Some("robin:7")
        );
        assert_eq!(pending_of(&grid, cx), 0);
        assert!(cx.update(|window, cx| grid.focus_handle(cx).is_focused(window)));
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
                .open_object(opened, None, window, cx)
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

/// The row panel's input of the row in front, `col`'s.
fn row_panel_input(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
    col: usize,
) -> Entity<gpui_component::input::TextareaState> {
    workspace.read_with(cx, |workspace, _| {
        let inputs = workspace.row_panel.inputs.as_ref().expect("Edit mode");
        inputs.fields[col].as_ref().expect("an input").input.clone()
    })
}

/// Text typed into the row panel and not yet staged is part of the row a
/// click on another row leaves, and goes with its grid edits.
#[gpui::test]
fn row_panel_text_goes_with_the_row_a_click_leaves(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        press(cx, "secondary-shift-e");
        let input = row_panel_input(&workspace, cx, 1);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.set_value("robin", window, cx));
        });
        settle(cx);
        assert_eq!(pending_of(&grid, cx), 0, "typed, not staged");

        ring_on(&grid, cx, 1, 1);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(pending_of(&grid, cx), 0);

        // Beside an edit already staged on the row, both go in one send.
        stage(&grid, cx, 1, 2, NewValue::Value("9".into()));
        let input = row_panel_input(&workspace, cx, 1);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.set_value("jay", window, cx));
        });
        settle(cx);
        ring_on(&grid, cx, 2, 1);
        assert_eq!(
            on_disk("select name || ':' || n from birds where id = 2").as_deref(),
            Some("jay:9")
        );
        assert_eq!(pending_of(&grid, cx), 0);
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
                .open_object(opened, None, window, cx)
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
/// Edit menu and a notice too long for the bar. The notice is on a line of its
/// own above the bar and wraps, so it is never cut either.
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
        let wide_notice = cx
            .debug_bounds("status-notice")
            .expect("the notice is drawn");
        for hidden in [false, true] {
            resize_to(&workspace, cx, 700., hidden);
            let (notice, bar) = (
                cx.debug_bounds("status-notice")
                    .expect("the notice is drawn"),
                cx.debug_bounds("status-bar").expect("the bar is drawn"),
            );
            assert!(
                notice.bottom() <= bar.top(),
                "the notice is above the bar at 700px, sidebar hidden {hidden}"
            );
            assert!(
                notice.left() >= bar.left() && notice.right() <= bar.right(),
                "the notice is inside the bar's width at 700px, sidebar hidden {hidden}"
            );
            assert!(
                notice.size.height > wide_notice.size.height,
                "the notice wraps rather than being cut at 700px, sidebar hidden {hidden}"
            );
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

/// Text a binary column cannot read, closed by a click on another row: the
/// input opens again on it, saying why, and the row is not sent without it.
#[gpui::test]
fn a_binary_input_holding_no_bytes_closed_by_a_click_stays_open_and_sends_nothing(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text not null, b blob); \
             insert into birds values (1, 'wren', x'6869'), (2, 'tit', x'6869');",
        );
        let (_, grid) = open_in_row_mode(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 2);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("zz");
        settle(cx);

        click_cell_of_row(cx, 1);
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("This is not hex: pairs of digits 0-9 and A-F, so it was not staged.")
        );
        assert_eq!(ring_of(&grid, cx), Some((0, 2)), "back on the refused cell");
        grid.read_with(cx, |grid, cx| {
            assert!(
                grid.delegate().commit_refusal(cx).is_some(),
                "its input is open again on the text"
            )
        });
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "nothing is sent"
        );
        assert_eq!(pending_of(&grid, cx), 1);
        assert!(history_sql(&workspace, cx).is_empty());
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
                let id = workspace.open_object(opened, None, window, cx).unwrap();
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

/// A profile edited to reach another database drops the edits its table tabs
/// staged against the old one: kept, a Row view's next leave would send one,
/// unread, to rows the edit was never made against.
#[gpui::test]
fn a_profile_saved_onto_another_target_discards_its_tables_edits(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        workspace.update_in(cx, |workspace, window, cx| {
            let profile = workspace.profile_mut().unwrap();
            let mut config = profile.config.clone();
            let ConnectionConfig::Sqlite { path, .. } = &mut config else {
                panic!("the profile is SQLite");
            };
            path.push_str(".other");
            let (profile_id, name) = (profile.id.clone(), profile.name.clone());
            workspace.save_profile(&profile_id, name, config, None, window, cx);
        });
        settle(cx);
        assert_eq!(
            grid.read_with(cx, |table, _| table.delegate().pending_count()),
            0
        );
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        quit(workspace, cx);
    });
}

/// A profile saved with only its statement timeout changed reaches the same
/// file: its tables keep their edits and nothing is said of a new target.
#[gpui::test]
fn a_profile_saved_with_a_new_timeout_keeps_its_tables_edits(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        workspace.update_in(cx, |workspace, window, cx| {
            let profile = workspace.profile_mut().unwrap();
            let mut config = profile.config.clone();
            let ConnectionConfig::Sqlite {
                statement_timeout, ..
            } = &mut config
            else {
                panic!("the profile is SQLite");
            };
            *statement_timeout = 30;
            let (profile_id, name) = (profile.id.clone(), profile.name.clone());
            workspace.save_profile(&profile_id, name, config, None, window, cx);
        });
        settle(cx);
        assert_eq!(
            grid.read_with(cx, |table, _| table.delegate().pending_count()),
            1
        );
        assert_eq!(shown_notice(&workspace, cx), None);
        quit(workspace, cx);
    });
}

/// A profile edited to reach another file takes with it what was read from
/// the old one beside its tables' edits: the review open on them, which would
/// run its batch on the new target, and a query tab's rows and their draft.
#[gpui::test]
fn a_profile_saved_onto_another_target_drops_its_review_and_query_results(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        run_in_front(&workspace, cx, "select * from birds");
        let query_grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        stage(&query_grid, cx, 0, 1, NewValue::Value("robin".into()));
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert!(front_review(&workspace, cx).is_some());

        workspace.update_in(cx, |workspace, window, cx| {
            let profile = workspace.profile_mut().unwrap();
            let mut config = profile.config.clone();
            let ConnectionConfig::Sqlite { path, .. } = &mut config else {
                panic!("the profile is SQLite");
            };
            path.push_str(".other");
            let (profile_id, name) = (profile.id.clone(), profile.name.clone());
            workspace.save_profile(&profile_id, name, config, None, window, cx);
        });
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None, "the review is gone");
        workspace.read_with(cx, |workspace, cx| {
            let profile = workspace.profile().unwrap();
            assert!(profile.session.insert_form.is_none());
            let query = profile.session.queries.first().expect("a query tab");
            let grid = query.results.read(cx).delegate();
            assert_eq!(grid.pending_count(), 0, "the query tab's draft is gone");
            assert_eq!(grid.fetched_rows(), 0, "and the rows it was made on");
        });
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

/// `BIRDS` with no rows: a table whose grid has nothing to draw until a row is added.
const EMPTY_BIRDS: &str = "create table birds (id integer primary key, name text not null);";

/// In a Draft view Delete row is a mark on the selected rows: nothing is
/// asked and nothing is sent, and pressing it again takes the mark back.
#[gpui::test]
fn delete_row_in_a_draft_view_toggles_a_mark_and_sends_nothing(cx: &mut TestAppContext) {
    use crate::{result_grid::NewValue, session::EditMode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        grid.update(cx, |grid, _| grid.delegate_mut().select_row(1));

        cx.dispatch_action(crate::actions::DeleteRow);
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None, "no review opens");
        grid.read_with(cx, |grid, _| {
            assert!(grid.delegate().is_marked(1));
            assert_eq!(
                grid.delegate().pending_value(0, 1),
                Some(&NewValue::Value("robin".into())),
                "the other row's edit is not the mark's to drop"
            );
            assert_eq!(grid.delegate().pending_count(), 2);
        });
        assert_eq!(
            on_disk("select count(*) from birds").as_deref(),
            Some("2"),
            "nothing was sent"
        );
        grid.update(cx, |_, cx| cx.notify());
        settle(cx);
        assert!(
            status_bar_lines(&workspace, cx)
                .iter()
                .any(|line| line == "2 pending changes"),
            "{:?}",
            status_bar_lines(&workspace, cx)
        );
        workspace.update(cx, |workspace, cx| {
            workspace.set_edit_mode(EditMode::Row, cx)
        });
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Draft);
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("The draft has 2 changes, so the view was not switched to Row mode.")
        );

        cx.dispatch_action(crate::actions::DeleteRow);
        settle(cx);
        grid.read_with(cx, |grid, _| {
            assert!(!grid.delegate().is_marked(1));
            assert_eq!(grid.delegate().pending_count(), 1);
        });
        grid.update(cx, |_, cx| cx.notify());
        settle(cx);
        assert!(
            status_bar_lines(&workspace, cx)
                .iter()
                .any(|line| line == "1 pending edit"),
            "{:?}",
            status_bar_lines(&workspace, cx)
        );
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("2"));
        quit(workspace, cx);
    });
}

/// The tracer: a draft of a mark, an edited cell and an added row is read in
/// one `Apply draft`, deletes first, and runs as one transaction.
#[gpui::test]
fn applying_a_draft_runs_its_deletes_updates_and_inserts_in_one_transaction_and_clears_the_draft(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| grid.delegate_mut().select_row(2));
        cx.dispatch_action(crate::actions::DeleteRow);
        settle(cx);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        add_bird(&grid, cx, "4", "jay");

        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply draft", true, None))
        );
        let sql = front_review_sql(&workspace, cx).expect("a review is open");
        assert!(sql.starts_with("BEGIN;"), "{sql}");
        assert!(sql.ends_with("COMMIT;"), "{sql}");
        let (delete, update, insert) = (
            sql.find("DELETE").expect("a DELETE"),
            sql.find("UPDATE").expect("an UPDATE"),
            sql.find("INSERT").expect("an INSERT"),
        );
        assert!(delete < update && update < insert, "{sql}");
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("3"));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "nothing is sent before Run"
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            on_disk("select count(*) from birds where id = 3").as_deref(),
            Some("0")
        );
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(
            on_disk("select name from birds where id = 4").as_deref(),
            Some("jay")
        );
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("3"));
        grid.read_with(cx, |table, _| {
            let grid = table.delegate();
            assert_eq!(grid.pending_count(), 0);
            assert_eq!(grid.marked_count(), 0);
            assert_eq!(grid.fetched_rows(), 3, "the table was read again");
        });
        assert_eq!(front_review(&workspace, cx), None, "the review is gone");
        quit(workspace, cx);
    });
}

/// A marked row is on its way out, so it takes no edit, and says so rather than
/// asking for a higher mode or calling the column the problem.
#[gpui::test]
fn editing_a_marked_row_says_it_is_marked_for_deletion(cx: &mut TestAppContext) {
    use crate::sql::Mode;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| {
            assert!(grid.delegate_mut().toggle_marks(&[1]));
        });
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 1, 1);

        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);

        grid.read_with(cx, |grid, _| {
            assert!(grid.delegate().is_marked(1));
            assert!(!grid.delegate().editable(1, 1), "no input opens on the row");
            assert_eq!(grid.delegate().pending_count(), 1, "only the mark");
        });
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("This row is marked for deletion.")
        );
        workspace.read_with(cx, |workspace, _| {
            let profile = workspace.profile().unwrap();
            assert_eq!(profile.mode, Mode::ReadWrite);
            assert!(profile.session.pending_run.is_none(), "no mode prompt");
        });
        quit(workspace, cx);
    });
}

/// A review shows the batch the draft made; a mark changed behind it would
/// leave the statement on screen describing something other than the draft.
#[gpui::test]
fn delete_row_changes_no_mark_while_a_review_holds_the_draft(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply edits", true, None))
        );
        let reviewed = front_review_sql(&workspace, cx);

        // Called rather than dispatched: with the review's dialog up the action
        // would have no focused element to reach.
        grid.update(cx, |grid, _| grid.delegate_mut().select_row(1));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.delete_row(&crate::actions::DeleteRow, window, cx)
        });
        settle(cx);
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().marked_count(), 0);
            assert_eq!(grid.delegate().pending_count(), 1);
        });
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply edits", true, None))
        );
        assert_eq!(front_review_sql(&workspace, cx), reviewed);

        workspace.update(cx, |workspace, cx| {
            workspace.close_apply_review(cx);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.delete_row(&crate::actions::DeleteRow, window, cx)
        });
        settle(cx);
        grid.read_with(cx, |grid, _| assert!(grid.delegate().is_marked(1)));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_cell_being_typed_in_is_kept_as_a_staged_edit_when_its_row_is_clicked_away_from_in_draft_mode(
    cx: &mut TestAppContext,
) {
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
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("robin");
        settle(cx);
        // Tab commits the name and opens the next cell, the path UAT walked.
        press(cx, "tab");
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("7");
        settle(cx);

        click_cell_of_row(cx, 1);
        assert_eq!(ring_of(&grid, cx).map(|(row, _)| row), Some(1));
        assert_eq!(pending_of(&grid, cx), 2, "both typed cells are kept");
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(0, 1),
                Some(&NewValue::Value("robin".into()))
            );
            assert_eq!(
                table.delegate().pending_value(0, 2),
                Some(&NewValue::Value("7".into()))
            );
        });
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "kept, and nothing is sent"
        );
        assert_eq!(
            on_disk("select n from birds where id = 1").as_deref(),
            Some("1")
        );
        assert!(history_sql(&workspace, cx).is_empty());
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_cell_being_typed_in_is_kept_when_another_cell_of_its_row_is_clicked_in_draft_mode(
    cx: &mut TestAppContext,
) {
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
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("robin");
        settle(cx);

        click_cell_of_row(cx, 0);
        assert_eq!(pending_of(&grid, cx), 1, "a same-row click keeps the text");
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(0, 1),
                Some(&NewValue::Value("robin".into()))
            );
        });
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );

        // A column move, as a header click makes it, closes an input too.
        ring_on(&grid, cx, 0, 2);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("9");
        settle(cx);
        grid.update(cx, |table, cx| {
            let col = table.delegate().table_col(1).expect("the name column");
            table.set_selected_col(col, cx);
        });
        settle(cx);
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(0, 2),
                Some(&NewValue::Value("9".into()))
            );
        });
        assert_eq!(
            on_disk("select n from birds where id = 1").as_deref(),
            Some("1")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_cell_being_typed_in_is_kept_when_another_cell_of_its_row_is_clicked_in_row_mode_and_sent_when_the_row_is_left(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        draft_by_default(&workspace, cx);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("robin");
        settle(cx);

        click_cell_of_row(cx, 0);
        assert_eq!(pending_of(&grid, cx), 1);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "the row was not left"
        );
        assert!(history_sql(&workspace, cx).is_empty());

        click_cell_of_row(cx, 1);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(pending_of(&grid, cx), 0);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_cell_being_typed_in_a_query_tabs_grid_is_kept_when_its_row_is_clicked_away_from(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        run_in_front(&workspace, cx, "select * from birds order by id");
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("robin");
        settle(cx);

        click_cell_of_row(cx, 1);
        assert_eq!(pending_of(&grid, cx), 1);
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(0, 1),
                Some(&NewValue::Value("robin".into()))
            );
        });
        assert!(!buffer(&workspace, cx).contains("UPDATE"));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_added_rows_cell_closed_by_a_click_is_staged_only_when_typed_in(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        focus_grid(&grid, cx);
        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        stage(&grid, cx, 2, 0, NewValue::Value("3".into()));

        ring_on(&grid, cx, 2, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        click_cell_of_row(cx, 0);
        grid.read_with(cx, |table, _| {
            assert_eq!(table.delegate().pending_value(2, 1), None);
        });
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        let sql = front_review_sql(&workspace, cx).expect("the draft is in review");
        assert!(
            !sql.contains("''"),
            "an untouched cell is not written: {sql}"
        );
        workspace.update(cx, |workspace, cx| workspace.close_apply_review(cx));
        settle(cx);

        ring_on(&grid, cx, 2, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("robin");
        settle(cx);
        click_cell_of_row(cx, 0);
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(2, 1),
                Some(&NewValue::Value("robin".into()))
            );
        });
        quit(workspace, cx);
    });
}

/// A wren with no count, beside a tit with one: a fetched NULL to open.
const NULL_BIRDS: &str = "create table birds (id integer primary key, name text not null, n integer); \
     insert into birds values (1, 'wren', null), (2, 'tit', 2);";

#[gpui::test]
fn an_untouched_input_closed_by_a_click_stages_nothing_over_a_null(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, NULL_BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        focus_grid(&grid, cx);

        // A fetched NULL, opened and left with nothing typed.
        ring_on(&grid, cx, 0, 2);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        click_cell_of_row(cx, 1);
        assert_eq!(pending_of(&grid, cx), 0, "the NULL is not turned into ''");

        // A staged NULL, the same.
        stage(&grid, cx, 1, 2, NewValue::Null);
        ring_on(&grid, cx, 1, 2);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        click_cell_of_row(cx, 0);
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(1, 2),
                Some(&NewValue::Null),
                "the staged NULL stays"
            );
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_untouched_input_on_a_null_closed_by_a_click_sends_nothing_in_row_mode(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, NULL_BIRDS);
        draft_by_default(&workspace, cx);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 2);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        click_cell_of_row(cx, 1);
        assert_eq!(pending_of(&grid, cx), 0);
        assert!(
            !history_sql(&workspace, cx)
                .iter()
                .any(|sql| sql.contains("UPDATE")),
            "leaving the row sends nothing"
        );
        assert_eq!(
            on_disk("select n is null from birds where id = 1").as_deref(),
            Some("1")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn go_to_column_stages_the_input_it_closes_so_a_later_edit_of_the_cell_stands(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("x");
        settle(cx);

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.reveal_column(2, window, cx)
        });
        settle(cx);
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(0, 1),
                Some(&NewValue::Value("x".into())),
                "staged by the move itself"
            );
        });

        ring_on(&grid, cx, 0, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("y");
        settle(cx);
        press(cx, "enter");
        click_cell_of_row(cx, 1);
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(0, 1),
                Some(&NewValue::Value("y".into())),
                "the newer value is the one staged"
            );
        });
        quit(workspace, cx);
    });
}

/// A wren and a kite around a bird with no name and no count: a NULL in a
/// nullable text column that has a column on each side of it, and a NULL in the
/// last column, so a forward Tab, a Shift-Tab, Up, Down and an edge key are all
/// reachable from one.
const NULL_NAMES: &str = "create table birds (id integer primary key, name text, n integer); \
     insert into birds values (1, 'wren', 1), (2, null, null), (3, 'kite', 3);";

/// Opens the input on `ring`, presses `key` with nothing typed and, when the
/// key opened the next cell's input, closes that one too.
fn open_and_close_untouched(
    grid: &Grid,
    cx: &mut VisualTestContext,
    ring: (usize, usize),
    key: &str,
) {
    ring_on(grid, cx, ring.0, ring.1);
    cx.dispatch_action(crate::actions::EditCell);
    settle(cx);
    press(cx, key);
    if key != "enter" {
        press(cx, "escape");
    }
}

#[gpui::test]
fn an_untouched_input_over_a_null_stages_nothing_however_it_is_closed_in_draft_mode(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, NULL_NAMES);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        focus_grid(&grid, cx);

        for (ring, key) in [
            ((1, 1), "enter"),
            ((1, 1), "tab"),
            ((1, 2), "shift-tab"),
            ((1, 1), "up"),
            ((1, 1), "down"),
        ] {
            open_and_close_untouched(&grid, cx, ring, key);
            assert_eq!(
                pending_of(&grid, cx),
                0,
                "{key} on {ring:?} staged what nobody typed"
            );
            grid.read_with(cx, |table, _| {
                for col in [1, 2] {
                    assert_eq!(
                        table.delegate().pending_value(1, col),
                        None,
                        "{key} on {ring:?} left a value on column {col}"
                    );
                }
            });
        }

        // The control: typed text still stages.
        ring_on(&grid, cx, 1, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("owl");
        settle(cx);
        press(cx, "enter");
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(1, 1),
                Some(&NewValue::Value("owl".into())),
                "typed text is staged"
            );
        });

        assert_eq!(
            on_disk("select name is null and n is null from birds where id = 2").as_deref(),
            Some("1")
        );
        assert!(
            !history_sql(&workspace, cx)
                .iter()
                .any(|sql| sql.contains("UPDATE")),
            "a Draft view sends nothing"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_untouched_input_over_a_staged_null_or_default_keeps_the_keyword_when_enter_or_tab_closes_it(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, NULL_NAMES);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        focus_grid(&grid, cx);

        stage(&grid, cx, 0, 1, NewValue::Null);
        open_and_close_untouched(&grid, cx, (0, 1), "enter");
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(0, 1),
                Some(&NewValue::Null),
                "Enter left the staged NULL"
            );
        });

        stage(&grid, cx, 2, 1, NewValue::Default);
        open_and_close_untouched(&grid, cx, (2, 1), "tab");
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(2, 1),
                Some(&NewValue::Default),
                "Tab left the staged DEFAULT"
            );
        });
        assert_eq!(pending_of(&grid, cx), 2);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn an_untouched_input_over_a_null_sends_nothing_in_row_mode_when_enter_or_an_edge_key_closes_it(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, NULL_NAMES);
        draft_by_default(&workspace, cx);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);

        // Enter, then the row is left by a click.
        ring_on(&grid, cx, 1, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        press(cx, "enter");
        click_cell_of_row(cx, 0);
        assert_eq!(pending_of(&grid, cx), 0);
        assert!(
            !history_sql(&workspace, cx)
                .iter()
                .any(|sql| sql.contains("UPDATE")),
            "Enter then leaving the row sends nothing"
        );
        assert_eq!(
            on_disk("select name is null from birds where id = 2").as_deref(),
            Some("1")
        );

        // The last column, where the step key has no cell to carry the edit to.
        ring_on(&grid, cx, 1, 2);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        press(cx, "tab");
        assert_eq!(pending_of(&grid, cx), 0);
        assert!(grid_holds_keyboard(&grid, cx));
        assert!(
            !history_sql(&workspace, cx)
                .iter()
                .any(|sql| sql.contains("UPDATE")),
            "the edge key sends nothing"
        );
        assert_eq!(
            on_disk("select n is null from birds where id = 2").as_deref(),
            Some("1")
        );

        // The control, so the test can see a send.
        ring_on(&grid, cx, 1, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("owl");
        settle(cx);
        click_cell_of_row(cx, 0);
        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("owl")
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn set_value_to_empty_still_stages_the_empty_string_over_a_null(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, NULL_NAMES);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        focus_grid(&grid, cx);

        ring_on(&grid, cx, 1, 1);
        grid.read_with(cx, |table, _| {
            assert!(table.delegate().offers_empty(1), "the cell menu offers it");
        });
        cx.dispatch_action(crate::actions::SetEmpty);
        settle(cx);
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(1, 1),
                Some(&NewValue::Value("".into()))
            );
        });

        // Reopened and closed untouched, it is still there.
        ring_on(&grid, cx, 1, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        press(cx, "enter");
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(1, 1),
                Some(&NewValue::Value("".into())),
                "an untouched reopen keeps the empty string"
            );
        });

        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        let sql = front_review_sql(&workspace, cx).expect("the draft is in review");
        assert!(sql.contains("\"name\" = ''"), "the review shows it: {sql}");
        workspace.update(cx, |workspace, cx| workspace.close_apply_review(cx));
        settle(cx);
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_step_key_past_the_grids_edge_commits_the_input_and_sends_the_row_in_row_mode(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        draft_by_default(&workspace, cx);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 2);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("7");
        settle(cx);

        // The last column: there is no cell to carry the edit to.
        press(cx, "tab");
        assert_eq!(
            on_disk("select n from birds where id = 1").as_deref(),
            Some("7"),
            "the row was sent"
        );
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(ring_of(&grid, cx), Some((0, 2)));
        assert!(grid_holds_keyboard(&grid, cx));
        assert!(
            history_sql(&workspace, cx)
                .iter()
                .any(|sql| sql.contains("UPDATE")),
            "the send is in the history"
        );

        // The last row, from a cell of another column.
        ring_on(&grid, cx, 2, 1);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("owl");
        settle(cx);
        press(cx, "down");
        assert_eq!(
            on_disk("select name from birds where id = 3").as_deref(),
            Some("owl")
        );
        assert_eq!(ring_of(&grid, cx).map(|(row, _)| row), Some(2));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_step_key_past_the_grids_edge_leaves_the_input_open_in_draft_mode(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 2);
        cx.dispatch_action(crate::actions::EditCell);
        settle(cx);
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("7");
        settle(cx);

        press(cx, "tab");
        assert_eq!(pending_of(&grid, cx), 0, "nothing is staged by the key");
        assert!(
            !grid_holds_keyboard(&grid, cx),
            "the input keeps the keyboard"
        );
        press(cx, "up");
        assert_eq!(pending_of(&grid, cx), 0);
        assert!(!grid_holds_keyboard(&grid, cx));
        assert_eq!(
            on_disk("select n from birds where id = 1").as_deref(),
            Some("1")
        );

        // The click is what keeps the text.
        click_cell_of_row(cx, 1);
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().pending_value(0, 2),
                Some(&NewValue::Value("7".into()))
            );
        });
        assert_eq!(
            on_disk("select n from birds where id = 1").as_deref(),
            Some("1")
        );
        assert!(history_sql(&workspace, cx).is_empty());
        quit(workspace, cx);
    });
}

/// The row's draft state is on its gutter as well as in its wash, since a
/// wash alone is colour and colour alone is not read the same by everyone.
#[gpui::test]
fn an_updated_row_is_marked_on_the_gutter_and_unmarked_when_discarded(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        settle(cx);
        assert!(cx.debug_bounds("row-updated-0").is_none());

        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        // `stage` writes the delegate directly, as a commit does before it
        // asks for the repaint.
        grid.update(cx, |_, cx| cx.notify());
        settle(cx);
        assert!(cx.debug_bounds("row-updated-0").is_some());
        assert!(cx.debug_bounds("row-updated-1").is_none());

        cx.dispatch_action(crate::actions::DiscardEdits);
        settle(cx);
        assert!(cx.debug_bounds("row-updated-0").is_none());
        quit(workspace, cx);
    });
}

/// A table with no rows shows its message and no grid; adding a row replaces the
/// message with the grid, the ring on the new row, and the keyboard reaches it.
#[gpui::test]
fn an_empty_table_draws_its_grid_once_a_row_is_added(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, EMPTY_BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        assert!(cx.debug_bounds("row-number-0").is_none());

        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        assert!(cx.debug_bounds("row-number-0").is_some());
        assert_eq!(ring_of(&grid, cx), Some((0, 0)));

        // The grid takes a column move: an empty grid refuses it.
        grid.update(cx, |table, _| table.delegate_mut().select_col(1));
        assert_eq!(ring_of(&grid, cx), Some((0, 1)));
        grid.update_in(cx, |table, window, cx| {
            crate::result_grid::take_keyboard(table, window, cx);
        });
        assert!(grid_holds_keyboard(&grid, cx));
        assert!(
            grid.read_with(cx, |table, _| table.selected_row())
                .is_some(),
            "the keyboard puts the library's selection on the added row"
        );

        stage(&grid, cx, 0, 0, NewValue::Value("1".into()));
        stage(&grid, cx, 0, 1, NewValue::Value("wren".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply draft", true, None))
        );
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("1"));
        quit(workspace, cx);
    });
}

/// An added row's draft state is on its gutter too, as an updated row's is.
#[gpui::test]
fn an_added_row_is_marked_on_the_gutter(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let _ = open_own_table(&workspace, cx, "birds");
        assert!(cx.debug_bounds("row-added-2").is_none());

        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        assert!(cx.debug_bounds("row-added-2").is_some());
        assert!(cx.debug_bounds("row-added-1").is_none());

        cx.dispatch_action(crate::actions::DiscardEdits);
        settle(cx);
        assert!(cx.debug_bounds("row-added-2").is_none());
        quit(workspace, cx);
    });
}

/// A row marked for deletion reads as deleted on its gutter, and the palette
/// names the gesture for what it does in a draft, an added row included.
#[gpui::test]
fn an_added_row_and_a_deleted_row_are_marked_on_the_gutter(cx: &mut TestAppContext) {
    use crate::session::EditMode;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        let has = |rows: &[String], label: &str| rows.iter().any(|row| row == label);

        ring_on(&grid, cx, 1, 1);
        let rows = palette_listing(&workspace, cx);
        assert!(has(&rows, "Mark row for deletion"), "{rows:?}");
        assert!(!has(&rows, "Delete row…"), "{rows:?}");

        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        cx.simulate_input("Mark row for deletion");
        cx.simulate_keystrokes("enter");
        settle(cx);
        grid.read_with(cx, |grid, _| assert!(grid.delegate().is_marked(1)));

        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        assert!(cx.debug_bounds("row-deleted-1").is_some());
        assert!(cx.debug_bounds("row-added-2").is_some());
        assert!(cx.debug_bounds("row-deleted-0").is_none());

        ring_on(&grid, cx, 2, 0);
        let rows = palette_listing(&workspace, cx);
        assert!(
            has(&rows, "Mark row for deletion"),
            "an added row can be removed from the palette: {rows:?}"
        );

        cx.dispatch_action(crate::actions::DiscardEdits);
        settle(cx);
        assert!(cx.debug_bounds("row-deleted-1").is_none());
        assert!(cx.debug_bounds("row-added-2").is_none());

        workspace.update(cx, |workspace, cx| {
            workspace.set_edit_mode(EditMode::Row, cx)
        });
        ring_on(&grid, cx, 1, 1);
        let rows = palette_listing(&workspace, cx);
        assert!(has(&rows, "Delete row…"), "{rows:?}");
        assert!(!has(&rows, "Mark row for deletion"), "{rows:?}");
        quit(workspace, cx);
    });
}

/// A mixed draft that the server refuses applies nothing: the batch's own
/// bracket rolls the delete and the update back with the insert that failed, and
/// the draft keeps all three parts for the person to correct.
#[gpui::test]
fn a_failed_draft_apply_keeps_all_three_parts_and_applies_nothing(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, THREE_BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| {
            assert!(grid.delegate_mut().toggle_marks(&[2]))
        });
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        // A duplicate key, which the table refuses.
        add_bird(&grid, cx, "1", "jay");

        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply draft", true, None))
        );
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        let (_, _, error) = front_review(&workspace, cx).expect("the review stays");
        let error = error.expect("the review says why");
        assert!(error.contains("UNIQUE"), "{error}");
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("3"));
        assert_eq!(
            on_disk("select count(*) from birds where id = 3").as_deref(),
            Some("1"),
            "the delete was rolled back"
        );
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "the update was rolled back"
        );
        grid.read_with(cx, |grid, _| {
            let grid = grid.delegate();
            assert_eq!(grid.marked_count(), 1);
            assert_eq!(grid.added_count(), 1);
            assert_eq!(
                grid.pending_value(0, 1),
                Some(&NewValue::Value("robin".into()))
            );
        });
        assert!(
            !matches!(front_state(&workspace, cx), QueryState::Failed(_)),
            "an error pane would cover the rows the draft is staged on"
        );
        quit(workspace, cx);
    });
}

/// The review says what it holds: `Apply edits` for staged cells, and
/// `Apply draft` once the draft holds a row.
#[gpui::test]
fn the_review_is_titled_by_what_the_draft_holds(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx).map(|(title, _, _)| title),
            Some("Apply edits")
        );

        workspace.update(cx, |workspace, cx| workspace.close_apply_review(cx));
        grid.update(cx, |grid, _| {
            grid.delegate_mut().discard_pending();
            assert!(grid.delegate_mut().toggle_marks(&[1]));
        });
        focus_grid(&grid, cx);
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx).map(|(title, _, _)| title),
            Some("Apply draft")
        );

        workspace.update(cx, |workspace, cx| workspace.close_apply_review(cx));
        grid.update(cx, |grid, _| grid.delegate_mut().discard_pending());
        focus_grid(&grid, cx);
        add_bird(&grid, cx, "3", "jay");
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx).map(|(title, _, _)| title),
            Some("Apply draft")
        );
        quit(workspace, cx);
    });
}

/// A row deleted and added back with its own key is the person's way to
/// replace it, and applies: the `DELETE` runs before the `INSERT`.
#[gpui::test]
fn a_row_deleted_and_added_back_with_its_key_applies_delete_first(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| {
            assert!(grid.delegate_mut().toggle_marks(&[1]))
        });
        add_bird(&grid, cx, "2", "jay");

        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply draft", true, None))
        );
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            on_disk("select name from birds where id = 2").as_deref(),
            Some("jay")
        );
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("2"));
        grid.read_with(cx, |grid, _| assert_eq!(grid.delegate().pending_count(), 0));
        quit(workspace, cx);
    });
}

/// A draft holding only an added row is a draft: the Apply and Discard
/// controls, the palette and the status bar count it, and it can neither be
/// switched away into Row mode nor read over by a requery unasked.
#[gpui::test]
fn a_draft_holding_only_an_added_row_shows_apply_and_cannot_switch_to_row_mode(
    cx: &mut TestAppContext,
) {
    use crate::{
        result_grid::NewValue,
        session::{AfterEdits, EditMode},
    };

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        stage(&grid, cx, 2, 1, NewValue::Value("robin".into()));
        grid.update(cx, |_, cx| cx.notify());
        settle(cx);

        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.pending_edit_count(cx), 1);
            assert!(workspace.has_pending_edits(cx));
        });
        assert_eq!(drawn(cx, &["apply-edits", "discard-edits"]), [true, true]);
        assert!(
            status_bar_lines(&workspace, cx)
                .iter()
                .any(|line| line == "1 pending change"),
            "{:?}",
            status_bar_lines(&workspace, cx)
        );

        workspace.update(cx, |workspace, cx| {
            workspace.set_edit_mode(EditMode::Row, cx)
        });
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Draft);
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("The draft has 1 change, so the view was not switched to Row mode.")
        );

        workspace.update(cx, |workspace, cx| workspace.refresh_relation(id, cx));
        settle(cx);
        assert!(matches!(
            discard_holds(&workspace, cx),
            Some(AfterEdits::Refresh)
        ));
        assert_eq!(front_review(&workspace, cx), None);
        workspace.update(cx, |workspace, cx| {
            workspace.cancel_discard_close(cx);
        });
        settle(cx);
        grid.read_with(cx, |table, _| {
            assert_eq!(table.delegate().added_count(), 1, "Cancel keeps the row");
            assert_eq!(table.delegate().fetched_rows(), 2);
        });
        quit(workspace, cx);
    });
}

/// `NewRow` as the workspace answers it, without the focus the action needs
/// to be dispatched.
fn new_row_directly(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.new_row(&crate::actions::NewRow, window, cx)
    });
}

/// `NewRow` waits for the rows to arrive, and for an open review to close:
/// a row added under either would not be the row the user is shown.
#[gpui::test]
fn new_row_is_refused_while_the_rows_load_or_a_review_holds_the_draft(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx).map(|(title, ..)| title),
            Some("Apply edits")
        );

        // Called rather than dispatched: with the review's dialog up, the
        // action would have no focused element to reach, and a refusal this
        // test means to pin would be a dispatch that never happened.
        new_row_directly(&workspace, cx);
        settle(cx);
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().added_count(),
                0,
                "a review holds the draft"
            );
        });
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().unwrap().session.insert_form.is_none());
        });

        // Closed and discarded, so the re-read asks nothing.
        workspace.update(cx, |workspace, cx| {
            workspace.close_apply_review(cx);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.discard_edits(&crate::actions::DiscardEdits, window, cx)
        });
        settle(cx);
        assert_eq!(pending_of(&grid, cx), 0);
        workspace.update(cx, |workspace, cx| workspace.refresh_relation(id, cx));
        assert_eq!(state_kind(&workspace, cx), "running");

        new_row_directly(&workspace, cx);
        grid.read_with(cx, |table, _| {
            assert_eq!(table.delegate().added_count(), 0, "the rows are loading");
        });
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("The rows are still being read, so no row was added.")
        );
        settle(cx);
        quit(workspace, cx);
    });
}

/// The statements the review in the session shows.
fn front_review_sql(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<String> {
    workspace.read_with(cx, |workspace, _| {
        Some(
            workspace
                .profile()?
                .session
                .apply_review
                .as_ref()?
                .sql
                .clone(),
        )
    })
}

/// In a Draft view, Insert is a row of the grid: nothing is asked, nothing is
/// sent, and the ring is on its first cell.
#[gpui::test]
fn new_row_in_a_draft_view_adds_a_grid_row(cx: &mut TestAppContext) {
    use gpui_component::table::TableDelegate;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        assert_eq!(
            edit_mode_of(&workspace, cx, id),
            crate::session::EditMode::Draft
        );

        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);

        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().unwrap().session.insert_form.is_none());
        });
        grid.read_with(cx, |table, cx| {
            let grid = table.delegate();
            assert_eq!(grid.fetched_rows(), 2);
            assert_eq!(grid.added_count(), 1);
            assert_eq!(grid.rows_count(cx), 3);
            assert_eq!(grid.active(), Some((2, 0)));
            assert_eq!(table.selected_row(), Some(2));
        });
        assert_eq!(front_review(&workspace, cx), None);
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("2"));
        quit(workspace, cx);
    });
}

/// Copy Row on an added row copies nothing: the row is not in the result, so
/// there is no read value to copy, and slicing for one panicked.
#[gpui::test]
fn copy_row_on_an_added_row_copies_nothing(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        assert_eq!(
            grid.read_with(cx, |table, _| table.delegate().active()),
            Some((2, 0))
        );

        cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.copy_row(&crate::actions::CopyRow, window, cx)
        });
        settle(cx);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("before")
        );
        quit(workspace, cx);
    });
}

/// The tracer: a row added in the grid is typed, read in `Apply draft`, inserted
/// by Run, and the draft is empty and the table read again.
#[gpui::test]
fn an_added_row_is_inserted_by_apply_draft_and_the_draft_clears(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        stage(&grid, cx, 2, 0, NewValue::Value("3".into()));
        stage(&grid, cx, 2, 1, NewValue::Value("robin".into()));

        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply draft", true, None))
        );
        let sql = front_review_sql(&workspace, cx).expect("a review is open");
        assert!(sql.starts_with("INSERT INTO"), "{sql}");
        assert!(
            !sql.contains("BEGIN"),
            "one statement is not bracketed: {sql}"
        );
        assert_eq!(
            on_disk("select count(*) from birds").as_deref(),
            Some("2"),
            "nothing is sent before Run"
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 3").as_deref(),
            Some("robin")
        );
        grid.read_with(cx, |table, _| {
            let grid = table.delegate();
            assert_eq!(grid.pending_count(), 0);
            assert_eq!(grid.added_count(), 0);
            assert_eq!(grid.fetched_rows(), 3, "the table was read again");
        });
        assert_eq!(cell_of(&grid, cx, 2, 1).as_deref(), Some("robin"));
        assert_eq!(front_review(&workspace, cx), None, "the review is gone");

        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            None,
            "an empty draft opens none"
        );
        assert_eq!(
            on_disk("select count(*) from birds").as_deref(),
            Some("3"),
            "inserted once"
        );
        quit(workspace, cx);
    });
}

/// Adds a row to the table in front and types its id and name.
fn add_bird(grid: &Grid, cx: &mut VisualTestContext, id: &str, name: &str) {
    use crate::result_grid::NewValue;

    cx.dispatch_action(crate::actions::NewRow);
    settle(cx);
    let row = grid.read_with(cx, |table, _| {
        table.delegate().fetched_rows() + table.delegate().added_count() - 1
    });
    stage(grid, cx, row, 0, NewValue::Value(id.to_string().into()));
    stage(grid, cx, row, 1, NewValue::Value(name.to_string().into()));
}

/// A sort in memory moves the fetched rows while the apply is out. The draft
/// it sent is cleared whole, so no `INSERT` is left staged to be sent twice,
/// and the table is read again.
#[gpui::test]
fn a_draft_sorted_in_memory_while_its_apply_is_out_is_cleared_whole_and_read_again(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        add_bird(&grid, cx, "3", "robin");
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply draft", true, None))
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        assert!(matches!(
            front_state(&workspace, cx),
            QueryState::Running { .. }
        ));
        let before = grid.read_with(cx, |table, _| table.delegate().rows_id());
        grid.update(cx, |table, _| {
            table.delegate_mut().sort_in_memory(vec![(1, false)])
        });
        assert_ne!(
            grid.read_with(cx, |table, _| table.delegate().rows_id()),
            before
        );
        settle(cx);

        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("finch")
        );
        assert_eq!(
            on_disk("select name from birds where id = 3").as_deref(),
            Some("robin")
        );
        grid.read_with(cx, |table, _| {
            assert_eq!(table.delegate().pending_count(), 0);
            assert_eq!(table.delegate().fetched_rows(), 3, "read again");
        });
        assert_ne!(
            shown_notice(&workspace, cx).as_deref(),
            Some(
                "The change was applied. The rows were not read again: this tab has unapplied edits."
            )
        );
        quit(workspace, cx);
    });
}

/// A second Run while the first is out is the same `INSERT` twice, which the
/// tab's one-run-at-a-time guard refuses.
#[gpui::test]
fn a_second_run_while_an_added_rows_insert_is_out_sends_nothing_more(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        // Only the name is typed: the id is the server's, so a second INSERT
        // would succeed and add a second row, which is what the count sees.
        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        stage(
            &grid,
            cx,
            2,
            1,
            crate::result_grid::NewValue::Value("robin".into()),
        );
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply draft", true, None))
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        assert!(matches!(
            front_state(&workspace, cx),
            QueryState::Running { .. }
        ));
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            on_disk("select count(*) from birds").as_deref(),
            Some("3"),
            "one row added, not two"
        );
        assert_eq!(
            on_disk("select count(*) from birds where name = 'robin'").as_deref(),
            Some("1"),
            "inserted once"
        );
        grid.read_with(cx, |table, _| {
            assert_eq!(table.delegate().pending_count(), 0);
        });
        assert_eq!(front_review(&workspace, cx), None);
        quit(workspace, cx);
    });
}

/// What the status bar's pending count says: the count and its noun, which is
/// `change` once the draft holds an added or marked row.
fn status_bar_lines(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
    workspace.read_with(cx, |workspace, cx| {
        let count = workspace.pending_edit_count(cx);
        let noun = match (workspace.draft_holds_rows(cx), count) {
            (false, 1) => "edit",
            (false, _) => "edits",
            (true, 1) => "change",
            (true, _) => "changes",
        };
        vec![format!("{count} pending {noun}")]
    })
}

/// Puts the table in front into Classic.
fn go_classic(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update(cx, |workspace, cx| {
        workspace.set_edit_mode(crate::session::EditMode::Classic, cx)
    });
}

/// `birds` opened in Classic, as `BIRDS` or a variation of it holds it.
fn open_in_classic_mode(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (u64, Grid) {
    let id = open_own_table(workspace, cx, "birds");
    go_classic(workspace, cx);
    assert_eq!(
        edit_mode_of(workspace, cx, id),
        crate::session::EditMode::Classic
    );
    let grid = relation_grid(workspace, cx, id);
    (id, grid)
}

#[gpui::test]
fn new_row_in_a_classic_view_opens_the_insert_form(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, grid) = open_in_classic_mode(&workspace, cx);

        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let form = workspace.profile().unwrap().session.insert_form.as_ref();
            assert_eq!(form.map(|form| form.tab), Some(Tab::Object(id)));
        });
        grid.read_with(cx, |grid, _| {
            let grid = grid.delegate();
            assert_eq!(grid.result().rows.len(), 2);
            assert!(!grid.holds_rows());
            assert_eq!(grid.added_count(), 0);
        });
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("2"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn delete_row_in_a_classic_view_reviews_one_delete(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, grid) = open_in_classic_mode(&workspace, cx);
        grid.update(cx, |grid, _| grid.delegate_mut().select_row(1));

        cx.dispatch_action(crate::actions::DeleteRow);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Delete row", false, None))
        );
        workspace.read_with(cx, |workspace, _| {
            let review = workspace
                .profile()
                .unwrap()
                .session
                .apply_review
                .as_ref()
                .expect("a review is open");
            assert!(review.sql.starts_with("DELETE"), "{}", review.sql);
            assert_eq!(review.sql.matches("DELETE").count(), 1, "{}", review.sql);
            assert!(!review.sql.contains("UPDATE") && !review.sql.contains("INSERT"));
            assert!(review.verify.is_some(), "the delete is verified");
        });
        grid.read_with(cx, |grid, _| assert_eq!(grid.delegate().marked_count(), 0));
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("2"));

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("1"));
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 1)
        });
        assert_eq!(
            edit_mode_of(&workspace, cx, id),
            crate::session::EditMode::Classic
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn save_in_a_classic_view_reviews_the_updates_before_anything_is_sent(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (_, grid) = open_in_classic_mode(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        save(&workspace, cx);

        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply edits", true, None))
        );
        workspace.read_with(cx, |workspace, _| {
            let review = workspace.profile().unwrap().session.apply_review.as_ref();
            assert!(review.is_some_and(|review| review.verify.is_some()));
        });
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        grid.read_with(cx, |grid, _| assert_eq!(grid.delegate().pending_count(), 0));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn delete_row_over_staged_edits_in_a_classic_view_keeps_them_and_says_so(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (id, grid) = open_in_classic_mode(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        grid.update(cx, |grid, _| grid.delegate_mut().select_row(1));

        cx.dispatch_action(crate::actions::DeleteRow);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("1"));
        grid.read_with(cx, |grid, _| {
            let grid = grid.delegate();
            assert_eq!(grid.pending_count(), 1);
            assert_eq!(grid.result().rows.len(), 2, "not read again");
        });
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert_eq!(
                session.tab_notice,
                Some((Tab::Object(id), Workspace::EDITS_KEPT.into()))
            );
        });

        save(&workspace, cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 1)
        });
        quit(workspace, cx);
    });
}

#[gpui::test]
fn switching_into_and_out_of_classic_keeps_reviewed_edits_and_sends_a_row_first(
    cx: &mut TestAppContext,
) {
    use crate::{result_grid::NewValue, session::EditMode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        let set = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext, mode| {
            workspace.update(cx, |workspace, cx| workspace.set_edit_mode(mode, cx))
        };
        let pending_discard = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .profile()
                    .unwrap()
                    .session
                    .pending_discard
                    .is_some()
            })
        };

        // Cell edits carry between Draft and Classic with no prompt.
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        set(&workspace, cx, EditMode::Classic);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Classic);
        grid.read_with(cx, |grid, _| assert_eq!(grid.delegate().pending_count(), 1));
        assert!(!pending_discard(&workspace, cx));
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );

        // Staged cells are not turned into per-row sends.
        set(&workspace, cx, EditMode::Row);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Classic);
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("The draft has 1 edit, so the view was not switched to Row mode.")
        );

        set(&workspace, cx, EditMode::Draft);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Draft);
        assert!(!pending_discard(&workspace, cx));
        grid.read_with(cx, |grid, _| assert_eq!(grid.delegate().pending_count(), 1));

        // A Draft holding an added row cannot become Classic.
        grid.update(cx, |grid, _| grid.delegate_mut().discard_pending());
        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        grid.read_with(cx, |grid, _| assert_eq!(grid.delegate().added_count(), 1));
        set(&workspace, cx, EditMode::Classic);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Draft);
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some("The draft has 1 change, so the view was not switched to Classic mode.")
        );

        grid.update(cx, |grid, _| grid.delegate_mut().discard_pending());
        set(&workspace, cx, EditMode::Classic);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Classic);
        assert_eq!(
            shown_notice(&workspace, cx),
            None,
            "the refusal is taken down"
        );

        // Leaving Row sends the edited row first.
        set(&workspace, cx, EditMode::Row);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Row);
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        set(&workspace, cx, EditMode::Classic);
        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Classic);
        quit(workspace, cx);
    });
}

/// Whether the grid of `tab` washes whole rows.
fn row_washes_of(grid: &Grid, cx: &mut VisualTestContext) -> bool {
    grid.read_with(cx, |grid, _| grid.delegate().row_washes())
}

#[gpui::test]
fn a_classic_view_paints_no_row_washes_and_keeps_none_across_a_reread(cx: &mut TestAppContext) {
    use crate::{result_grid::NewValue, session::EditMode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        workspace.update(cx, |workspace, cx| {
            workspace.set_default_edit_mode(EditMode::Classic, cx)
        });
        let id = open_own_table(&workspace, cx, "birds");
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Classic);
        let grid = relation_grid(&workspace, cx, id);
        assert!(!row_washes_of(&grid, cx), "set by the first load");

        workspace.update(cx, |workspace, cx| {
            workspace.set_edit_mode(EditMode::Draft, cx)
        });
        assert!(row_washes_of(&grid, cx));
        go_classic(&workspace, cx);
        assert!(!row_washes_of(&grid, cx));

        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        save(&workspace, cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("robin")
        );
        let grid = relation_grid(&workspace, cx, id);
        assert!(!row_washes_of(&grid, cx), "kept on the re-read grid");

        // A query tab answers Draft: the grid its run lands in keeps them.
        let query = workspace.read_with(cx, |workspace, _| {
            Tab::Query(workspace.profile().unwrap().session.queries[0].id)
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_tab(query, window, cx)
        });
        settle(cx);
        run_in_front(&workspace, cx, "select * from birds");
        let query_grid = workspace.read_with(cx, |workspace, _| {
            workspace
                .profile()
                .unwrap()
                .session
                .results(query)
                .expect("the query tab has a grid")
                .clone()
        });
        assert!(
            row_washes_of(&query_grid, cx),
            "a query tab keeps its washes once it runs"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_classic_table_restored_from_its_snapshot_paints_no_row_washes(cx: &mut TestAppContext) {
    use crate::session::EditMode;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        workspace.update(cx, |workspace, cx| {
            workspace.set_default_edit_mode(EditMode::Classic, cx)
        });
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);

        let stored = grid.read_with(cx, |grid, _| grid.delegate().stored());
        store::write_grid("e2e", &store::object_grid_key("main", "birds", ""), &stored)
            .expect("the snapshot is written");
        // The tab as a relaunch leaves it before its snapshot is looked for.
        workspace.update(cx, |workspace, _| {
            let profile = workspace.profile_mut().unwrap();
            let object = profile
                .session
                .objects
                .iter_mut()
                .find(|tab| tab.id == id)
                .unwrap();
            let ObjectBody::Relation {
                query, hydrated, ..
            } = &mut object.body
            else {
                panic!("the tab is a relation");
            };
            *hydrated = false;
            *query = QueryState::Idle;
        });
        grid.update(cx, |grid, _| {
            *grid.delegate_mut() = crate::result_grid::ResultGrid::empty();
        });
        assert!(row_washes_of(&grid, cx));

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.hydrate_tab(Tab::Object(id), window, cx)
        });
        // Not settled: the restored grid is what is under test, and a re-read
        // would build the grid again through the completion handler.
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 2)
        });
        assert!(!row_washes_of(&grid, cx));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_new_table_tab_starts_in_classic_when_it_is_the_default(cx: &mut TestAppContext) {
    use crate::session::EditMode;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text); \
             create table nests (id integer primary key, owner text);",
        );
        let birds = open_own_table(&workspace, cx, "birds");
        assert_eq!(edit_mode_of(&workspace, cx, birds), EditMode::Classic);

        workspace.update(cx, |workspace, cx| {
            workspace.set_default_edit_mode(EditMode::Draft, cx)
        });
        assert!(saved_profiles_text().contains("edit_mode = \"draft\""));
        assert_eq!(
            edit_mode_of(&workspace, cx, birds),
            EditMode::Classic,
            "each open tab holds its own"
        );
        let nests = open_own_table(&workspace, cx, "nests");
        assert_eq!(edit_mode_of(&workspace, cx, nests), EditMode::Draft);

        workspace.update(cx, |workspace, cx| {
            workspace.set_default_edit_mode(EditMode::Classic, cx)
        });
        assert!(!saved_profiles_text().contains("edit_mode"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn the_status_bar_and_the_palette_put_a_table_in_classic_mode(cx: &mut TestAppContext) {
    use crate::session::EditMode;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let query_tab = workspace.read_with(cx, |workspace, _| {
            Tab::Query(workspace.profile().unwrap().session.queries[0].id)
        });
        let has = |rows: &[String], label: &str| rows.iter().any(|row| row == label);
        open_own_table(&workspace, cx, "birds");
        assert_eq!(drawn(cx, &["edit-mode"]), [true]);

        pick_edit_mode(&workspace, cx, crate::session::EditMode::Classic);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Classic));
        let rows = palette_listing(&workspace, cx);
        assert!(has(&rows, "Edit in Draft mode"), "{rows:?}");
        assert!(has(&rows, "Edit in Row mode"), "{rows:?}");
        assert!(!has(&rows, "Edit in Classic mode"), "{rows:?}");

        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        cx.simulate_input("Edit in Draft mode");
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Draft));
        let rows = palette_listing(&workspace, cx);
        assert!(has(&rows, "Edit in Classic mode"), "{rows:?}");
        assert!(!has(&rows, "Edit in Draft mode"), "{rows:?}");

        cx.simulate_keystrokes("secondary-shift-p");
        settle(cx);
        cx.simulate_input("Edit in Classic mode");
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(front_edit_mode(&workspace, cx), Some(EditMode::Classic));

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_tab(query_tab, window, cx)
            });
        });
        settle(cx);
        assert_eq!(drawn(cx, &["edit-mode"]), [false]);
        quit(workspace, cx);
    });
}

/// Runs `sql` against `e2e.db` on a connection of its own, as another session
/// would, so a test changes a row behind a grid that has already read it.
fn behind_the_grid(sql: &str) {
    let path = store::dbdelve_directory()
        .expect("the test home has a data directory")
        .join("e2e.db");
    rusqlite::Connection::open(path)
        .and_then(|connection| connection.execute_batch(sql))
        .expect("the other session's statement applies");
}

/// Whether the profile in front holds the user's open transaction.
fn in_transaction(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace.profile().unwrap().in_transaction
    })
}

/// `birds` read as a target that names its rows by every column, which no
/// engine that verifies an apply produces yet (ClickHouse does, unverified).
fn birds_named_by_every_column() -> crate::result_grid::ResultGrid {
    use crate::db::{Column, EditTarget, QueryResult, RowMatch, UpdateSpelling, rows};
    let column = |name: &str| Column {
        name: name.into(),
        data_type: None,
    };
    crate::result_grid::ResultGrid::new(
        QueryResult {
            columns: vec![column("id"), column("name")],
            rows: rows(vec![
                vec![Some("1".into()), Some("wren".into())],
                vec![Some("2".into()), Some("tit".into())],
            ]),
            edit: Some(EditTarget {
                schema: "main".into(),
                table: "birds".into(),
                columns: vec![Some("id".into()), Some("name".into())],
                keys: vec![0, 1],
                row_match: RowMatch::EveryColumn { fixed: Vec::new() },
                update: UpdateSpelling::Update,
            }),
            ..QueryResult::default()
        },
        crate::sql::Mode::ReadWrite,
    )
}

#[gpui::test]
fn a_grid_naming_its_rows_by_every_column_is_refused_inside_the_users_open_transaction(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        run_in_front(&workspace, cx, "BEGIN");
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |table, cx| {
            *table.delegate_mut() = birds_named_by_every_column();
            table.refresh(cx);
        });

        // One write, but on a row that could count two: Apply edits.
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None, "no review opens");
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some(crate::session::APPLY_IN_TRANSACTION)
        );
        assert_eq!(pending_of(&grid, cx), 1, "the draft is kept");

        // The same write as a mark in a draft that holds rows.
        cx.dispatch_action(crate::actions::DiscardEdits);
        settle(cx);
        grid.update(cx, |table, _| {
            assert!(table.delegate_mut().toggle_marks(&[0]))
        });
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None, "no review opens");
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some(crate::session::APPLY_IN_TRANSACTION)
        );
        assert_eq!(pending_of(&grid, cx), 1, "the mark is kept");
        assert!(in_transaction(&workspace, cx));
        quit(workspace, cx);
    });
}

/// Inside the user's open transaction, New row and Delete row in a Classic
/// view are refused as Apply is, before a form or a review opens: the write's
/// own `BEGIN` and `COMMIT` would end the transaction at Run.
#[gpui::test]
fn new_row_and_delete_row_are_refused_before_their_form_and_review_inside_the_users_transaction(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        run_in_front(&workspace, cx, "BEGIN");
        let id = open_own_table(&workspace, cx, "birds");
        assert_eq!(
            edit_mode_of(&workspace, cx, id),
            crate::session::EditMode::Classic
        );
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| grid.delegate_mut().select_row(1));

        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().unwrap().session.insert_form.is_none());
        });
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some(crate::session::APPLY_IN_TRANSACTION)
        );

        cx.dispatch_action(crate::actions::DeleteRow);
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None, "no review opens");
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some(crate::session::APPLY_IN_TRANSACTION)
        );
        assert!(in_transaction(&workspace, cx));
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("2"));
        quit(workspace, cx);
    });
}

/// A view already in Row when the user opens a transaction on the profile's
/// own session sends nothing on Save: the send's `BEGIN` and `COMMIT` would
/// end the user's transaction, and its `ROLLBACK` would undo the send.
#[gpui::test]
fn a_row_send_is_refused_inside_a_transaction_opened_after_the_view_went_row(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        workspace.update(cx, |workspace, _| {
            workspace.profile_mut().unwrap().in_transaction = true;
        });
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        save(&workspace, cx);
        assert_eq!(
            grid.read_with(cx, |table, _| table
                .delegate()
                .send_failure()
                .map(|(row, why)| (row, why.to_string()))),
            Some((
                0,
                "A transaction is open on this connection; the row was not sent.".into()
            ))
        );
        assert_eq!(pending_of(&grid, cx), 1, "the edit is kept");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        quit(workspace, cx);
    });
}

/// A Row view sends nothing inside the user's open transaction, so a view is
/// not switched into Row while one is open; Draft is still offered.
#[gpui::test]
fn a_view_is_not_switched_into_row_inside_the_users_transaction(cx: &mut TestAppContext) {
    use crate::session::EditMode;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        run_in_front(&workspace, cx, "BEGIN");
        let id = open_own_table(&workspace, cx, "birds");

        pick_edit_mode(&workspace, cx, EditMode::Row);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Classic);
        assert_eq!(
            shown_notice(&workspace, cx).as_deref(),
            Some(
                "A transaction is open on this connection, so the view was not switched to \
                 Row mode."
            )
        );

        pick_edit_mode(&workspace, cx, EditMode::Draft);
        assert_eq!(edit_mode_of(&workspace, cx, id), EditMode::Draft);
        assert!(in_transaction(&workspace, cx));
        quit(workspace, cx);
    });
}

/// A query tab's Apply on a verifying engine is read in the review, applies
/// through the verified primitive, and leaves neither the buffer nor the
/// history holding a copy that would run without its verification.
#[gpui::test]
fn a_query_tabs_apply_is_reviewed_verified_and_leaves_the_buffer_and_history_alone(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        run_in_front(&workspace, cx, "select * from birds");
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply edits", true, None))
        );
        assert_eq!(buffer(&workspace, cx), "select * from birds");

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(front_review(&workspace, cx), None, "the review is gone");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("finch")
        );
        assert_eq!(buffer(&workspace, cx), "select * from birds");
        grid.read_with(cx, |grid, _| {
            let grid = grid.delegate();
            assert_eq!(grid.pending_count(), 0);
            assert_eq!(grid.cell(0, 1), Some("finch"), "the query ran again");
        });
        let held = history_sql(&workspace, cx);
        assert!(
            held.iter().any(|sql| sql == "select * from birds"),
            "{held:?}"
        );
        assert!(
            held.iter()
                .all(|sql| !sql.to_uppercase().contains("UPDATE")),
            "{held:?}"
        );
        quit(workspace, cx);
    });
}

/// A row another session changed after the query read it is not overwritten:
/// the verified apply finds it differs, writes nothing and names the row.
#[gpui::test]
fn a_query_tabs_apply_over_a_row_changed_on_the_server_applies_nothing_and_names_it(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        run_in_front(&workspace, cx, "select * from birds");
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply edits", true, None))
        );

        behind_the_grid("update birds set name = 'kite' where id = 1");
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            front_review(&workspace, cx),
            Some((
                "Apply edits",
                true,
                Some(
                    "Nothing was applied. Row id = 1 does not match the row as it was read."
                        .to_string()
                )
            )),
            "the review stays open and names the row"
        );
        grid.read_with(cx, |table, _| {
            assert_eq!(table.delegate().pending_count(), 1, "the draft is kept");
        });
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("kite")
        );

        // A second Run reads the rows again and does not replay the refusal:
        // with the row as it was read, the same review applies.
        behind_the_grid("update birds set name = 'wren' where id = 1");
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None, "the review is gone");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("finch")
        );
        quit(workspace, cx);
    });
}

/// The New row form of a query tab is counted like a table's: an `INSERT` a
/// trigger skipped changed no row and is refused; without the trigger it
/// applies.
#[gpui::test]
fn a_new_row_from_a_query_tab_is_counted_through_the_verified_apply(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let schema = format!(
            "{BIRDS} create trigger skips before insert on birds \
             begin select raise(ignore); end;"
        );
        let (workspace, cx) = launch_on_sqlite_holding(cx, &schema);
        draft_by_default(&workspace, cx);
        run_in_front(&workspace, cx, "select * from birds");

        new_row_directly(&workspace, cx);
        let fields: Vec<_> = workspace.read_with(cx, |workspace, _| {
            let form = workspace.profile().unwrap().session.insert_form.as_ref();
            form.expect("the form is open")
                .fields
                .iter()
                .map(|field| field.input.clone())
                .collect()
        });
        cx.update(|window, cx| {
            for (input, text) in fields.iter().zip(["3", "robin"]) {
                input.update(cx, |input, cx| input.set_value(text, window, cx));
            }
        });
        workspace.update_in(cx, |workspace, window, cx| {
            (0..fields.len()).for_each(|index| workspace.touch_insert_field(index, window, cx))
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.confirm_new_row(window, cx)
        });
        settle(cx);
        assert_eq!(front_review(&workspace, cx), Some(("Insert", false, None)));

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some((
                "Insert",
                false,
                Some("Nothing was applied. INSERT 1 (new row) affected 0 rows.".to_string())
            )),
            "the review stays and says what happened"
        );
        assert_eq!(
            on_disk("select count(*) from birds where id = 3").as_deref(),
            Some("0")
        );

        behind_the_grid("drop trigger skips");
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None, "the review is gone");
        assert_eq!(
            on_disk("select name from birds where id = 3").as_deref(),
            Some("robin")
        );
        quit(workspace, cx);
    });
}

/// A row another session changed after the grid read it is not overwritten:
/// the apply re-reads it inside its transaction, finds it differs, writes
/// nothing and says which row.
#[gpui::test]
fn a_row_changed_on_the_server_since_it_was_read_applies_nothing_and_is_named_in_the_review(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply edits", true, None))
        );

        behind_the_grid("update birds set name = 'kite' where id = 1");
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            front_review(&workspace, cx),
            Some((
                "Apply edits",
                true,
                Some(
                    "Nothing was applied. Row id = 1 does not match the row as it was read."
                        .to_string()
                )
            )),
            "the review stays open and names the row"
        );
        grid.read_with(cx, |table, _| {
            assert_eq!(table.delegate().pending_count(), 1, "the draft is kept");
        });
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("kite"),
            "the other session's value stands"
        );

        // A second Run reads the rows again, so it refuses again while they
        // still differ, and the draft is still there for it to refuse.
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx)
                .and_then(|(_, _, error)| error)
                .as_deref(),
            Some("Nothing was applied. Row id = 1 does not match the row as it was read.")
        );
        assert_eq!(pending_of(&grid, cx), 1);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("kite")
        );
        quit(workspace, cx);
    });
}

/// A row another session deleted after the grid read it is not written over:
/// it is named as gone.
#[gpui::test]
fn a_row_deleted_on_the_server_since_it_was_read_is_named_as_gone(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 1, 1, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);

        behind_the_grid("delete from birds where id = 2");
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            front_review(&workspace, cx)
                .and_then(|(_, _, error)| error)
                .as_deref(),
            Some("Nothing was applied. Row id = 2 is no longer on the server.")
        );
        assert_eq!(pending_of(&grid, cx), 1, "the draft is kept");
        assert_eq!(
            on_disk("select count(*) from birds where id = 2").as_deref(),
            Some("0")
        );
        quit(workspace, cx);
    });
}

const BIRDS_WITH_A_TRIGGER_THAT_KEEPS_TIT: &str = "create table birds (id integer primary key, \
     name text not null); insert into birds values (1, 'wren'), (2, 'tit'); \
     create trigger keeps before update on birds when old.id = 2 \
     begin select raise(ignore); end;";

/// An `UPDATE` a trigger skipped changed no row, which the server answers as
/// a success. The count refuses it, with the draft's `DELETE` undone.
#[gpui::test]
fn an_update_a_trigger_skipped_rolls_the_whole_draft_back_and_is_named(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS_WITH_A_TRIGGER_THAT_KEEPS_TIT);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| {
            assert!(grid.delegate_mut().toggle_marks(&[0]))
        });
        stage(&grid, cx, 1, 1, NewValue::Value("robin".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply draft", true, None))
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            front_review(&workspace, cx)
                .and_then(|(_, _, error)| error)
                .as_deref(),
            Some("Nothing was applied. UPDATE 2 (row id = 2) affected 0 rows.")
        );
        assert_eq!(
            on_disk("select count(*) from birds where id = 1").as_deref(),
            Some("1"),
            "the DELETE before it is undone"
        );
        grid.read_with(cx, |table, _| {
            assert_eq!(table.delegate().marked_count(), 1);
            assert_eq!(table.delegate().pending_count(), 2, "the mark and the edit");
        });

        // The rollback left the server's rows as they were, so a second Run
        // counts them again and refuses the same statement.
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx)
                .and_then(|(_, _, error)| error)
                .as_deref(),
            Some("Nothing was applied. UPDATE 2 (row id = 2) affected 0 rows.")
        );
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("2"));
        quit(workspace, cx);
    });
}

const BIRDS_WHERE_A_DELETE_TAKES_ITS_CHILD: &str = "create table birds (id integer primary key, \
     name text not null, parent integer); \
     insert into birds values (1, 'wren', null), (2, 'tit', 1), (3, 'jay', null); \
     create trigger cascades after delete on birds \
     begin delete from birds where parent = old.id; end;";

/// A draft that marks a row and the child a trigger deletes with it: the
/// child's own `DELETE` counts none, because the row was taken already, and
/// the second read of it finds it gone.
#[gpui::test]
fn a_delete_that_counts_zero_applies_only_when_its_row_is_gone(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS_WHERE_A_DELETE_TAKES_ITS_CHILD);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| {
            assert!(grid.delegate_mut().toggle_marks(&[0, 1]))
        });
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(front_review(&workspace, cx), None, "it applied");
        assert_eq!(
            on_disk("select group_concat(id) from birds").as_deref(),
            Some("3")
        );
        quit(workspace, cx);
    });
}

/// The same draft where a trigger keeps the child: its `DELETE` counts none
/// and the row is still there, which is a refusal.
#[gpui::test]
fn a_delete_that_counts_zero_with_its_row_still_there_is_refused_and_named(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            &format!(
                "{BIRDS_WHERE_A_DELETE_TAKES_ITS_CHILD} create trigger keeps before delete \
                 on birds when old.id = 2 begin select raise(ignore); end;"
            ),
        );
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| {
            assert!(grid.delegate_mut().toggle_marks(&[0, 1]))
        });
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            front_review(&workspace, cx)
                .and_then(|(_, _, error)| error)
                .as_deref(),
            Some("Nothing was applied. DELETE 2 (row id = 2) affected 0 rows.")
        );
        assert_eq!(
            on_disk("select count(*) from birds").as_deref(),
            Some("3"),
            "the first DELETE is undone"
        );
        quit(workspace, cx);
    });
}

/// A run the mode prompt held is still a verified one when it is let go.
#[gpui::test]
fn a_verified_apply_held_by_the_mode_prompt_still_reads_its_rows_when_resumed(
    cx: &mut TestAppContext,
) {
    use crate::{result_grid::NewValue, sql::Mode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        workspace.update(cx, |workspace, cx| workspace.set_mode(Mode::ReadOnly, cx));
        settle(cx);
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.profile().unwrap().session.pending_run.is_some());
        });

        behind_the_grid("update birds set name = 'kite' where id = 1");
        workspace.update(cx, |workspace, cx| workspace.approve_pending_run(cx));
        settle(cx);

        assert_eq!(
            front_review(&workspace, cx)
                .and_then(|(_, _, error)| error)
                .as_deref(),
            Some("Nothing was applied. Row id = 1 does not match the row as it was read.")
        );
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("kite")
        );
        quit(workspace, cx);
    });
}

/// A row another session changed after the grid read it is not written over by
/// a Row send: it is read again inside the send's transaction, found to differ,
/// and the row keeps its edits and its ring.
#[gpui::test]
fn a_row_send_over_a_row_changed_on_the_server_keeps_the_edits_and_the_ring(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));

        behind_the_grid("update birds set name = 'kite' where id = 1");
        press(cx, "down");

        let (row, message) = refused_row(&grid, cx).expect("the row says why it was refused");
        assert_eq!(row, 0);
        assert!(
            message.starts_with(
                "Nothing was applied. Row id = 1 does not match the row as it was read."
            ),
            "{message}"
        );
        assert!(
            message.ends_with("The row keeps its edits; Esc puts it back as it was read."),
            "{message}"
        );
        assert_eq!(ring_of(&grid, cx), Some((0, 1)), "the ring is back");
        grid.read_with(cx, |grid, _| {
            assert!(matches!(
                grid.delegate().pending_cells(0).as_slice(),
                [(1, NewValue::Value(name))] if name == "finch"
            ));
        });
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("kite"),
            "the other session's value stands"
        );

        // The row is read again by the next Save, not judged by the first one.
        behind_the_grid("update birds set name = 'wren' where id = 1");
        save(&workspace, cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("finch")
        );
        assert_eq!(refused_row(&grid, cx), None);
        assert_eq!(pending_of(&grid, cx), 0);
        quit(workspace, cx);
    });
}

/// An `UPDATE` a trigger skipped changed no row, which the server answers as a
/// success; the count refuses it and the row keeps its edit.
#[gpui::test]
fn a_row_send_a_trigger_skipped_is_refused_and_keeps_the_edits(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let schema = format!(
            "{BIRDS} create trigger keeps before update on birds when old.id = 1 \
             begin select raise(ignore); end;"
        );
        let (workspace, cx) = launch_on_sqlite_holding(cx, &schema);
        draft_by_default(&workspace, cx);
        let (_, grid) = open_in_row_mode(&workspace, cx);

        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        save(&workspace, cx);

        let (row, message) = refused_row(&grid, cx).expect("the row says why it was refused");
        assert_eq!(row, 0);
        assert!(
            message.starts_with("Nothing was applied. UPDATE 1 (row id = 1) affected 0 rows."),
            "{message}"
        );
        assert_eq!(pending_of(&grid, cx), 1, "the edit is kept");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren")
        );
        quit(workspace, cx);
    });
}

/// What the grid shows after a Row send is what the server holds once its
/// triggers ran, read by the send itself and not what was typed, and without
/// the relation being read again.
#[gpui::test]
fn a_verified_row_send_reads_its_row_back_from_inside_the_transaction(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        // SQLite cannot change `new` in a trigger, so the row is written again
        // after its update, as a `BEFORE` trigger would have it written.
        let schema = format!(
            "{BIRDS} create trigger shouts after update of name on birds \
             begin update birds set name = upper(new.name) where id = new.id; end;"
        );
        let (workspace, cx) = launch_on_sqlite_holding(cx, &schema);
        draft_by_default(&workspace, cx);
        let (_, grid) = open_in_row_mode(&workspace, cx);

        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        // A read of the whole relation would show this row; the send's own
        // read-back replaces its row in place and reads nothing else.
        behind_the_grid("insert into birds values (3, 'jay')");
        save(&workspace, cx);

        assert_eq!(cell_of(&grid, cx, 0, 1).as_deref(), Some("FINCH"));
        grid.read_with(cx, |grid, _| {
            assert_eq!(
                grid.delegate().result().rows.len(),
                2,
                "the relation was not read again"
            )
        });
        assert_eq!(pending_of(&grid, cx), 0);
        assert_eq!(refused_row(&grid, cx), None);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("FINCH")
        );
        quit(workspace, cx);
    });
}

/// The New row form's `INSERT` is counted like any write of a verifying
/// target: one a trigger skipped changed no row, and the review stays.
#[gpui::test]
fn a_new_row_whose_insert_a_trigger_skipped_is_refused_and_the_form_review_stays(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let schema = format!(
            "{BIRDS} create trigger skips before insert on birds \
             begin select raise(ignore); end;"
        );
        let (workspace, cx) = launch_on_sqlite_holding(cx, &schema);
        draft_by_default(&workspace, cx);
        let (_, grid) = open_in_row_mode(&workspace, cx);

        new_row_directly(&workspace, cx);
        let fields: Vec<_> = workspace.read_with(cx, |workspace, _| {
            let form = workspace.profile().unwrap().session.insert_form.as_ref();
            form.expect("the form is open")
                .fields
                .iter()
                .map(|field| field.input.clone())
                .collect()
        });
        cx.update(|window, cx| {
            for (input, text) in fields.iter().zip(["3", "robin"]) {
                input.update(cx, |input, cx| input.set_value(text, window, cx));
            }
        });
        // What the first keystroke does to a field, which `set_value` does
        // not raise an event for.
        workspace.update_in(cx, |workspace, window, cx| {
            (0..fields.len()).for_each(|index| workspace.touch_insert_field(index, window, cx))
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.confirm_new_row(window, cx)
        });
        settle(cx);
        assert_eq!(front_review(&workspace, cx), Some(("Insert", false, None)));

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some((
                "Insert",
                false,
                Some("Nothing was applied. INSERT 1 (new row) affected 0 rows.".to_string())
            )),
            "the review stays and says what happened"
        );
        assert_eq!(
            on_disk("select count(*) from birds where id = 3").as_deref(),
            Some("0")
        );

        // Without the trigger the same Run applies and the grid shows the row.
        behind_the_grid("drop trigger skips");
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None, "the review is gone");
        assert_eq!(
            on_disk("select name from birds where id = 3").as_deref(),
            Some("robin")
        );
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().result().rows.len(), 3)
        });
        quit(workspace, cx);
    });
}

/// A lone Delete row reads its row again before the `DELETE`: one another
/// session already deleted is found gone and nothing runs.
#[gpui::test]
fn delete_row_over_a_row_deleted_on_the_server_applies_nothing_and_says_it_is_gone(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        grid.update(cx, |grid, _| grid.delegate_mut().select_row(1));
        cx.dispatch_action(crate::actions::DeleteRow);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Delete row", false, None))
        );

        behind_the_grid("delete from birds where id = 2");
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            front_review(&workspace, cx),
            Some((
                "Delete row",
                false,
                Some("Nothing was applied. Row id = 2 is no longer on the server.".to_string())
            )),
            "the review stays and says the row is gone"
        );
        assert_eq!(on_disk("select count(*) from birds").as_deref(), Some("1"));
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_table_whose_column_cannot_be_matched_stays_read_only_in_row_mode(cx: &mut TestAppContext) {
    use crate::{result_grid::NewValue, session::EditMode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table logs (note text, at numeric); \
             insert into logs values ('a', 1);",
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
        assert_eq!(
            in_draft,
            (
                vec![false, false],
                Some(
                    "A table without a primary key is edited by matching every column, \
                     and at (numeric) cannot be matched by its value."
                        .to_string()
                ),
                0
            )
        );

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

/// An edit that reaches the send with no verification, for a target that
/// verifies applies, is not sent: every path that writes a grid's rows builds
/// one, and this is what stops a future one that forgets.
#[gpui::test]
fn an_edit_of_a_verifying_target_without_its_verification_is_not_sent(cx: &mut TestAppContext) {
    use crate::session::Refresh;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        workspace.update(cx, |workspace, cx| {
            workspace.execute_and_then(
                "UPDATE \"birds\" SET \"name\" = 'finch' WHERE \"id\" = 1;".into(),
                Tab::Object(id),
                Some(Refresh::Relation { id, verify: None }),
                false,
                None,
                cx,
            );
        });
        settle(cx);

        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "nothing reached the file"
        );
        let said = match (front_review(&workspace, cx), tab_notice(&workspace, cx)) {
            (Some((_, _, Some(error))), _) => error,
            (_, Some((_, notice))) => notice,
            other => panic!("the refusal was not said: {other:?}"),
        };
        assert_eq!(said, "dbdelve refused to send an edit it could not verify.");
        quit(workspace, cx);
    });
}

/// A second Apply while the first is out is the same `UPDATE` twice, and a
/// re-run of the tab's query while it is out would read rows the apply is
/// still changing; the tab's one-run-at-a-time guard refuses both.
#[gpui::test]
fn a_second_apply_from_a_query_tab_while_the_first_is_out_sends_nothing_more(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        // Every UPDATE that reaches the table leaves a line, so a second one
        // shows even though it would write the same name.
        let schema = format!(
            "{BIRDS} create table log (n integer); create trigger logs after update on birds \
             begin insert into log values (1); end;"
        );
        let (workspace, cx) = launch_on_sqlite_holding(cx, &schema);
        draft_by_default(&workspace, cx);
        run_in_front(&workspace, cx, "select * from birds");
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.apply_edits(&crate::actions::ApplyEdits, window, cx)
            })
        });
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply edits", true, None))
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        assert!(matches!(
            front_state(&workspace, cx),
            QueryState::Running { .. }
        ));
        let reviewing = front_review_sql(&workspace, cx);
        // Neither a second Run, nor a second Apply, nor a re-run of the query
        // sends anything while the first is out.
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.apply_edits(&crate::actions::ApplyEdits, window, cx);
                workspace.run_query(&crate::actions::RunQuery, window, cx);
            })
        });
        assert_eq!(
            front_review_sql(&workspace, cx),
            reviewing,
            "no other statement was put up for review"
        );
        settle(cx);

        assert_eq!(
            on_disk("select count(*) from log").as_deref(),
            Some("1"),
            "one UPDATE reached the table"
        );
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("finch")
        );
        assert_eq!(front_review(&workspace, cx), None);
        grid.read_with(cx, |grid, _| {
            assert_eq!(grid.delegate().pending_count(), 0);
        });
        quit(workspace, cx);
    });
}

/// With the unverified Row path gone, a Row send that reaches the send with no
/// verification is the one thing left to refuse.
#[gpui::test]
fn a_row_send_without_its_verification_is_not_sent(cx: &mut TestAppContext) {
    use crate::session::Refresh;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let (id, grid) = open_in_row_mode(&workspace, cx);
        let rows_id = grid.read_with(cx, |grid, _| grid.delegate().rows_id());
        workspace.update(cx, |workspace, cx| {
            workspace.execute_and_then(
                "UPDATE \"birds\" SET \"name\" = 'finch' WHERE \"id\" = 1;".into(),
                Tab::Object(id),
                Some(Refresh::Row {
                    row: 0,
                    rows_id,
                    sent: Vec::new(),
                    to: None,
                    then: None,
                    verify: None,
                }),
                false,
                None,
                cx,
            );
        });
        settle(cx);

        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "nothing reached the file"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_row_send_whose_read_back_is_not_a_select_writes_nothing(cx: &mut TestAppContext) {
    use crate::session::Refresh;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let (id, grid) = open_in_row_mode(&workspace, cx);
        let rows_id = grid.read_with(cx, |grid, _| grid.delegate().rows_id());

        // The write is a real one, verified as a Row send's is; only the
        // read-back its step carries is not a read.
        stage(
            &grid,
            cx,
            0,
            1,
            crate::result_grid::NewValue::Value("robin".into()),
        );
        let (write, before) = grid.read_with(cx, |grid, _| {
            let grid = grid.delegate();
            (
                crate::sql::update_batch(Engine::Sqlite, &grid.pending_updates())
                    .expect("the row's UPDATE is written"),
                grid.fetched_cells(0),
            )
        });
        let write = write.as_str();
        // The step is the statement as the gate cuts it, without its `;`.
        let step = write.trim_end_matches(';');
        let verify = Some(crate::session::Verification {
            steps: vec![crate::db::VerifiedStep {
                sql: step.into(),
                kind: crate::db::StepKind::Update,
                reread: crate::sql::row_reread(Engine::Sqlite, step),
                before,
                read_back: Some("DELETE FROM birds".into()),
                by_key: true,
            }],
            rows: vec!["id = 1".into()],
            identities: Vec::new(),
        });
        workspace.update(cx, |workspace, cx| {
            workspace.execute_and_then(
                write.into(),
                Tab::Object(id),
                Some(Refresh::Row {
                    row: 0,
                    rows_id,
                    sent: Vec::new(),
                    to: None,
                    then: None,
                    verify,
                }),
                false,
                None,
                cx,
            )
        });
        settle_through(cx);

        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "the write was not sent"
        );
        assert_eq!(
            on_disk("select count(*) from birds").as_deref(),
            Some("2"),
            "the carried statement did not run either"
        );
        quit(workspace, cx);
    });
}

const SIGHTINGS: &str = "create table sightings (bird text, place text); \
     insert into sightings values ('wren', NULL), ('tit', ''), ('jay', 'oak');";

/// A table with no primary key is edited by every value its row was read
/// with: a NULL is matched with `IS NULL` and an empty string with `= ''`, so
/// the row named is the one shown and no other.
#[gpui::test]
fn a_table_without_a_primary_key_is_edited_by_every_value_and_a_null_is_matched_with_is_null(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, SIGHTINGS);
        let id = open_own_table(&workspace, cx, "sightings");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 0, NewValue::Value("finch".into()));
        grid.update(cx, |grid, _| {
            assert!(grid.delegate_mut().toggle_marks(&[2]));
        });

        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply draft", true, None))
        );
        let sql = front_review_sql(&workspace, cx).expect("a review is open");
        assert!(
            sql.contains(r#"WHERE "bird" = 'wren' AND "place" IS NULL;"#),
            "{sql}"
        );
        assert!(
            sql.contains(
                r#"DELETE FROM "main"."sightings" WHERE "bird" = 'jay' AND "place" = 'oak';"#
            ),
            "{sql}"
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            on_disk("select bird from sightings where place is null").as_deref(),
            Some("finch")
        );
        assert_eq!(
            on_disk("select bird from sightings where place = ''").as_deref(),
            Some("tit")
        );
        assert_eq!(
            on_disk("select count(*) from sightings where bird = 'jay'").as_deref(),
            Some("0")
        );
        assert_eq!(
            on_disk("select count(*) from sightings").as_deref(),
            Some("2")
        );
        quit(workspace, cx);
    });
}

const TWO_IDENTICAL_SIGHTINGS: &str = "create table sightings (bird text, place text); \
     insert into sightings values ('wren', 'oak'), ('wren', 'oak');";

const DUPLICATE_SENTENCE: &str =
    "Nothing was applied. Row bird = wren, place = oak matches more than one row on the server.";

/// Two rows alike in every column cannot be told apart by their values, so an
/// edit of one is refused before any write and says which row, and neither
/// changes.
#[gpui::test]
fn a_duplicate_row_without_a_primary_key_is_refused_and_named(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, TWO_IDENTICAL_SIGHTINGS);
        let id = open_own_table(&workspace, cx, "sightings");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 0, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            front_review(&workspace, cx)
                .and_then(|(_, _, error)| error)
                .as_deref(),
            Some(DUPLICATE_SENTENCE)
        );
        assert_eq!(pending_of(&grid, cx), 1, "the draft is kept");
        assert_eq!(
            on_disk("select count(*) from sightings where bird = 'wren'").as_deref(),
            Some("2"),
            "neither row changed"
        );
        quit(workspace, cx);
    });
}

/// A duplicate another session inserted after the grid read its row makes the
/// re-read return two rows, so the apply is refused with nothing written.
#[gpui::test]
fn a_duplicate_inserted_behind_the_grid_is_caught_before_the_write(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table sightings (bird text, place text); \
             insert into sightings values ('wren', 'oak');",
        );
        let id = open_own_table(&workspace, cx, "sightings");
        let grid = relation_grid(&workspace, cx, id);
        stage(&grid, cx, 0, 0, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);

        behind_the_grid("insert into sightings values ('wren', 'oak')");
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            front_review(&workspace, cx)
                .and_then(|(_, _, error)| error)
                .as_deref(),
            Some(DUPLICATE_SENTENCE)
        );
        assert_eq!(pending_of(&grid, cx), 1, "the draft is kept");
        assert_eq!(
            on_disk("select count(*) from sightings where bird = 'wren'").as_deref(),
            Some("2"),
            "nothing was written"
        );
        quit(workspace, cx);
    });
}

/// Delete row in a Row view of a table with no primary key names its row by
/// every value and removes that row alone; over two rows alike it is refused
/// by the re-read.
#[gpui::test]
fn delete_row_on_a_table_without_a_primary_key_deletes_exactly_its_row(cx: &mut TestAppContext) {
    use crate::session::EditMode;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, SIGHTINGS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "sightings");
        workspace.update(cx, |workspace, cx| {
            workspace.set_edit_mode(EditMode::Row, cx)
        });
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| grid.delegate_mut().select_row(1));
        cx.dispatch_action(crate::actions::DeleteRow);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Delete row", false, None))
        );
        let sql = front_review_sql(&workspace, cx).expect("a review is open");
        assert!(sql.contains(r#""bird" = 'tit' AND "place" = ''"#), "{sql}");

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            on_disk("select count(*) from sightings").as_deref(),
            Some("2")
        );
        assert_eq!(
            on_disk("select count(*) from sightings where bird = 'tit'").as_deref(),
            Some("0")
        );
        assert_eq!(
            on_disk("select count(*) from sightings where place is null").as_deref(),
            Some("1"),
            "the NULL row is still there"
        );
        quit(workspace, cx);
    });

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, TWO_IDENTICAL_SIGHTINGS);
        let id = open_own_table(&workspace, cx, "sightings");
        workspace.update(cx, |workspace, cx| {
            workspace.set_edit_mode(EditMode::Row, cx)
        });
        let grid = relation_grid(&workspace, cx, id);
        grid.update(cx, |grid, _| grid.delegate_mut().select_row(0));
        cx.dispatch_action(crate::actions::DeleteRow);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);

        assert_eq!(
            front_review(&workspace, cx)
                .and_then(|(_, _, error)| error)
                .as_deref(),
            Some(DUPLICATE_SENTENCE)
        );
        assert_eq!(
            on_disk("select count(*) from sightings").as_deref(),
            Some("2"),
            "neither row was deleted"
        );
        quit(workspace, cx);
    });
}

/// A Row send on a table with no primary key reads its row back by the values
/// it now holds, since the values it was named by are gone; a value set to
/// DEFAULT leaves no value to name it by, so that send is refused and kept.
#[gpui::test]
fn a_row_send_on_a_table_without_a_primary_key_reads_its_row_back_by_its_new_values(
    cx: &mut TestAppContext,
) {
    use crate::{result_grid::NewValue, session::EditMode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, SIGHTINGS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "sightings");
        workspace.update(cx, |workspace, cx| {
            workspace.set_edit_mode(EditMode::Row, cx)
        });
        let grid = relation_grid(&workspace, cx, id);

        stage(&grid, cx, 0, 0, NewValue::Value("finch".into()));
        save(&workspace, cx);
        // Not "The row was not found after the update.", which a read by the
        // values the row had says before the grid reads the table again.
        assert!(
            shown_notice(&workspace, cx).is_some_and(|notice| notice.starts_with("1 row updated")),
            "{:?}",
            shown_notice(&workspace, cx)
        );
        assert_eq!(cell_of(&grid, cx, 0, 0).as_deref(), Some("finch"));
        assert_eq!(cell_of(&grid, cx, 0, 1), None, "the NULL is still NULL");
        assert_eq!(cell_of(&grid, cx, 1, 1).as_deref(), Some(""));
        grid.read_with(cx, |table, _| {
            assert_eq!(table.delegate().send_failure(), None);
            assert_eq!(table.delegate().pending_count(), 0);
        });
        assert_eq!(
            on_disk("select bird from sightings where place is null").as_deref(),
            Some("finch")
        );

        stage(&grid, cx, 1, 1, NewValue::Default);
        save(&workspace, cx);
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().send_failure(),
                Some((
                    1,
                    "The row was not sent: a value set to DEFAULT cannot be read back on a row \
                     named by every value."
                ))
            );
        });
        assert_eq!(pending_of(&grid, cx), 1, "the row's edit is kept");
        assert_eq!(
            on_disk("select count(*) from sightings where place = ''").as_deref(),
            Some("1"),
            "nothing was sent"
        );
        quit(workspace, cx);
    });
}

/// What Apply edits and Save say of a grid whose last apply may have landed.
const OUTCOME_UNKNOWN_REFUSAL: &str = "Not applied: whether the last apply reached the server is not known until the rows are read again.";

#[gpui::test]
fn an_apply_whose_outcome_is_unknown_is_refused_until_the_rows_are_read_again(
    cx: &mut TestAppContext,
) {
    use crate::{result_grid::NewValue, session::AfterEdits};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);

        // Entered where the landing leaves it: the grid is marked, with the
        // draft staged on it (the landing itself is driven by
        // `an_apply_whose_commit_goes_unanswered_marks_the_grid_and_is_not_sent_again`).
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        grid.update(cx, |table, _| table.delegate_mut().mark_outcome_unknown());

        for _ in 0..2 {
            cx.dispatch_action(crate::actions::ApplyEdits);
            settle(cx);
            assert_eq!(front_review(&workspace, cx), None, "no review opens");
            assert_eq!(
                shown_notice(&workspace, cx).as_deref(),
                Some(OUTCOME_UNKNOWN_REFUSAL)
            );
            assert_eq!(pending_of(&grid, cx), 1, "the draft is kept");
        }

        // A refresh asks about the draft first, and keeping it reads nothing.
        workspace.update(cx, |workspace, cx| workspace.refresh_relation(id, cx));
        settle(cx);
        assert!(matches!(
            discard_holds(&workspace, cx),
            Some(AfterEdits::Refresh)
        ));
        workspace.update(cx, |workspace, cx| {
            workspace.cancel_discard_close(cx);
        });
        settle(cx);
        assert!(grid.read_with(cx, |table, _| table.delegate().outcome_unknown()));

        // Discarding it lets the refresh read the rows again, and the grid
        // that holds them has no mark.
        workspace.update(cx, |workspace, cx| workspace.refresh_relation(id, cx));
        settle(cx);
        confirm_discard(&workspace, cx);
        let grid = relation_grid(&workspace, cx, id);
        assert!(!grid.read_with(cx, |table, _| table.delegate().outcome_unknown()));
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply edits", true, None))
        );
        quit(workspace, cx);
    });
}

/// An apply whose `COMMIT` lands and then goes unanswered, driven through the
/// real landing: the grid is marked, the review says nothing is known, and
/// neither its Run nor a new Apply sends the draft a second time.
#[gpui::test]
fn an_apply_whose_commit_goes_unanswered_marks_the_grid_and_is_not_sent_again(
    cx: &mut TestAppContext,
) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);
        // Only the name is typed: the id is the server's, so a second INSERT
        // would add a second row.
        cx.dispatch_action(crate::actions::NewRow);
        settle(cx);
        stage(
            &grid,
            cx,
            2,
            1,
            crate::result_grid::NewValue::Value("robin".into()),
        );
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);

        crate::db::SQLITE_COMMIT_ANSWER_LOST.set(true);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert!(
            !crate::db::SQLITE_COMMIT_ANSWER_LOST.get(),
            "the COMMIT was sent"
        );
        assert_eq!(
            on_disk("select count(*) from birds where name = 'robin'").as_deref(),
            Some("1"),
            "the COMMIT landed"
        );
        assert!(grid.read_with(cx, |table, _| table.delegate().outcome_unknown()));
        assert_eq!(
            front_review(&workspace, cx),
            Some((
                "Apply draft",
                true,
                Some(
                    "The connection was lost during COMMIT; whether the changes were applied \
                     is not known."
                        .to_string()
                )
            ))
        );

        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx)
                .and_then(|(_, _, error)| error)
                .as_deref(),
            Some(OUTCOME_UNKNOWN_REFUSAL)
        );
        workspace.update(cx, |workspace, cx| workspace.close_apply_review(cx));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(front_review(&workspace, cx), None, "no review opens");
        assert_eq!(
            on_disk("select count(*) from birds where name = 'robin'").as_deref(),
            Some("1"),
            "inserted once"
        );
        quit(workspace, cx);
    });
}

#[gpui::test]
fn a_review_that_was_open_and_a_row_save_are_refused_while_the_outcome_is_unknown(
    cx: &mut TestAppContext,
) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        let id = open_own_table(&workspace, cx, "birds");
        let grid = relation_grid(&workspace, cx, id);

        // A review opened before the answer to a COMMIT was lost still holds
        // its draft, and its Run would write it a second time.
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some(("Apply edits", true, None))
        );
        grid.update(cx, |table, _| table.delegate_mut().mark_outcome_unknown());
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        settle(cx);
        assert_eq!(
            front_review(&workspace, cx),
            Some((
                "Apply edits",
                true,
                Some(OUTCOME_UNKNOWN_REFUSAL.to_string())
            )),
            "the review stays, with the sentence"
        );
        assert_eq!(pending_of(&grid, cx), 1, "the draft is kept");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "nothing was sent"
        );
        quit(workspace, cx);
    });

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        let (_, grid) = open_in_row_mode(&workspace, cx);
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        grid.update(cx, |table, _| table.delegate_mut().mark_outcome_unknown());
        save(&workspace, cx);
        grid.read_with(cx, |table, _| {
            assert_eq!(
                table.delegate().send_failure(),
                Some((0, OUTCOME_UNKNOWN_REFUSAL))
            );
        });
        assert_eq!(pending_of(&grid, cx), 1, "the row's edit is kept");
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "nothing was sent"
        );
        quit(workspace, cx);
    });
}

/// A query tab's kept apply lands on the rows as they were sent: sorted in
/// memory under it, the sent cells would stay staged and be refused as drift
/// on the next apply. So the switch waits for the apply, as it does for a
/// Row send.
#[gpui::test]
fn entering_client_sorting_is_refused_while_a_query_tabs_apply_runs(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        run_in_front(&workspace, cx, "select * from birds");
        let grid = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        let tab = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });
        stage(&grid, cx, 0, 1, NewValue::Value("finch".into()));
        cx.dispatch_action(crate::actions::ApplyEdits);
        settle(cx);
        workspace.update(cx, |workspace, cx| workspace.run_apply_review(cx));
        assert_eq!(state_kind(&workspace, cx), "running");

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_view_sorting(true, window, cx)
        });
        workspace.read_with(cx, |workspace, _| {
            let sorting = workspace.profile().unwrap().session.sorting(tab).cloned();
            assert!(
                sorting.is_some_and(|sorting| sorting.client_keys().is_none()),
                "still sorted by the server"
            );
        });

        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("finch")
        );
        assert_eq!(pending_of(&grid, cx), 0, "the landing cleared the edit");
        quit(workspace, cx);
    });
}

/// Text in the row panel the grid refuses (bytes it cannot read) keeps the
/// row from going: sent without it, the row would be written half edited.
#[gpui::test]
fn a_row_whose_panel_text_is_refused_is_not_sent_without_it(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text not null, b blob); \
             insert into birds values (1, 'wren', x'4142'), (2, 'tit', x'4142');",
        );
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        press(cx, "secondary-shift-e");
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let input = row_panel_input(&workspace, cx, 2);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.set_value("0xZZ", window, cx));
        });
        settle(cx);

        ring_on(&grid, cx, 1, 1);
        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "the row was not sent without its panel value"
        );
        assert_eq!(pending_of(&grid, cx), 1, "the grid's edit is kept");
        assert_eq!(
            grid.read_with(cx, |table, _| table.delegate().active().map(|(row, _)| row)),
            Some(0),
            "the ring is back on the row"
        );
        let kept = row_panel_input(&workspace, cx, 2);
        assert_eq!(
            cx.update(|_, cx| kept.read(cx).value()).as_ref(),
            "0xZZ",
            "the refused text is still there to fix"
        );
        quit(workspace, cx);
    });
}

/// A Row send of a row named by every value puts the row the server holds
/// where it stood, rather than reading the whole relation again: the grid
/// stays as it was, and the row is named by its new values on its next send,
/// whatever other row has edits staged.
#[gpui::test]
fn a_row_send_by_every_value_shows_what_it_wrote_and_sends_again(cx: &mut TestAppContext) {
    use crate::{result_grid::NewValue, session::EditMode};

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, SIGHTINGS);
        let id = open_own_table(&workspace, cx, "sightings");
        workspace.update(cx, |workspace, cx| {
            workspace.set_edit_mode(EditMode::Row, cx)
        });
        let grid = relation_grid(&workspace, cx, id);
        let rows_id = grid.read_with(cx, |table, _| table.delegate().rows_id());

        stage(&grid, cx, 0, 0, NewValue::Value("finch".into()));
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.save_row(id, 0, None, None, cx))
        });
        settle(cx);
        assert_eq!(cell_of(&grid, cx, 0, 0).as_deref(), Some("finch"));
        assert_eq!(
            grid.read_with(cx, |table, _| table.delegate().rows_id()),
            rows_id,
            "the relation was not read again"
        );

        // Beside another row's edit, which stays staged.
        stage(&grid, cx, 2, 1, NewValue::Value("elm".into()));
        stage(&grid, cx, 0, 0, NewValue::Value("robin".into()));
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.save_row(id, 0, None, None, cx))
        });
        settle(cx);
        assert_eq!(cell_of(&grid, cx, 0, 0).as_deref(), Some("robin"));
        assert_eq!(pending_of(&grid, cx), 1, "the other row's edit stays");

        stage(&grid, cx, 0, 0, NewValue::Value("jay".into()));
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.save_row(id, 0, None, None, cx))
        });
        settle(cx);
        assert_eq!(
            on_disk("select bird from sightings where place is null").as_deref(),
            Some("jay")
        );
        assert_eq!(refused_row(&grid, cx), None);
        quit(workspace, cx);
    });
}

/// Closing a Row view whose panel text the grid refuses neither sends the row
/// without it nor closes the tab over it.
#[gpui::test]
fn a_close_over_refused_panel_text_sends_nothing_and_keeps_the_tab(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text not null, b blob); \
             insert into birds values (1, 'wren', x'4142'), (2, 'tit', x'4142');",
        );
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        press(cx, "secondary-shift-e");
        stage(&grid, cx, 0, 1, NewValue::Value("robin".into()));
        let input = row_panel_input(&workspace, cx, 2);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.set_value("0xZZ", window, cx));
        });
        settle(cx);
        let tab = workspace.read_with(cx, |workspace, _| {
            workspace.profile().unwrap().session.active
        });

        cx.simulate_keystrokes("secondary-w");
        settle(cx);
        assert_eq!(
            on_disk("select name from birds where id = 1").as_deref(),
            Some("wren"),
            "the row was not sent without its panel value"
        );
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert!(session.strip_tabs().contains(&tab), "the tab stays open");
        });
        assert_eq!(pending_of(&grid, cx), 1, "the grid's edit is kept");
        quit(workspace, cx);
    });
}

/// Close All Tabs asks once over every tab holding edits: Cancel keeps them
/// all, and Discard closes them all.
#[gpui::test]
fn close_all_asks_once_over_every_tab_with_edits(cx: &mut TestAppContext) {
    use crate::result_grid::NewValue;

    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(cx, BIRDS);
        draft_by_default(&workspace, cx);
        run_in_front(&workspace, cx, "select * from birds");
        let first = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        stage(&first, cx, 0, 1, NewValue::Value("finch".into()));
        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        run_in_front(&workspace, cx, "select * from birds");
        let second = query_tab(&workspace, cx, |tab, _, _| tab.results.clone());
        stage(&second, cx, 0, 1, NewValue::Value("robin".into()));
        cx.simulate_keystrokes("secondary-t");
        settle(cx);
        let strip = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace.profile().unwrap().session.strip_tabs()
            })
        };

        cx.simulate_keystrokes("secondary-k w");
        settle(cx);
        let held = workspace.read_with(cx, |workspace, _| {
            let discard = workspace
                .profile()
                .unwrap()
                .session
                .pending_discard
                .as_ref()
                .expect("the edits are asked about");
            assert_eq!(discard.also.len(), 1, "one question over both tabs");
            let mut held = discard.also.clone();
            held.push(discard.tab);
            held
        });
        assert_eq!(strip(cx).len(), 2, "the clean tab has gone");

        workspace.update(cx, |workspace, cx| workspace.cancel_discard_close(cx));
        settle(cx);
        assert_eq!(strip(cx).len(), 2, "Cancel keeps both");
        assert_eq!(pending_of(&first, cx), 1);
        assert_eq!(pending_of(&second, cx), 1);

        cx.simulate_keystrokes("secondary-k w");
        settle(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.confirm_discard_close(window, cx)
        });
        settle(cx);
        let left = strip(cx);
        assert!(
            held.iter().all(|tab| !left.contains(tab)),
            "Discard closes every tab it was asked over"
        );
        quit(workspace, cx);
    });
}

/// Text typed into a Row view's panel goes with a tab switch, as it did
/// before Close All Tabs counted it: only a close asks to discard it.
#[gpui::test]
fn a_tab_switch_over_typed_panel_text_is_not_asked_about(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let (workspace, cx) = launch_on_sqlite_holding(
            cx,
            "create table birds (id integer primary key, name text not null); \
             insert into birds values (1, 'wren'), (2, 'tit');",
        );
        let (_, grid) = open_in_row_mode(&workspace, cx);
        focus_grid(&grid, cx);
        ring_on(&grid, cx, 0, 1);
        cx.simulate_keystrokes("secondary-k r");
        settle(cx);
        press(cx, "secondary-shift-e");
        let input = row_panel_input(&workspace, cx, 1);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.set_value("robin", window, cx));
        });
        settle(cx);
        let other = workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            session
                .strip_tabs()
                .into_iter()
                .find(|tab| *tab != session.active)
                .expect("another tab")
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_tab(other, window, cx)
        });
        settle(cx);
        workspace.read_with(cx, |workspace, _| {
            let session = &workspace.profile().unwrap().session;
            assert!(session.pending_discard.is_none(), "nothing is asked");
            assert_eq!(session.active, other, "the switch went ahead");
        });
        quit(workspace, cx);
    });
}
