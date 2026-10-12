//! The app driven the way a person drives it: launched from a saved
//! `profiles.toml`, typed into, run from the keyboard. GPUI's test platform
//! has no screen, so these run headless in CI like any other test.

use std::borrow::Cow;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use gpui_component::Root;

use super::Workspace;
use crate::{
    db::Engine,
    keybindings,
    session::QueryState,
    store,
    theme::{Fonts, Theme},
};

/// What `main` sets up before the window opens, minus the menu bar and the
/// panic log, which the test platform has no use for.
fn launch(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
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
        let theme = Theme::default();
        theme.apply_to_components(cx);
        cx.set_global(theme);
        cx.bind_keys(keybindings::build_bindings(&Default::default()));
    });
    let mut workspace = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let entity = cx.new(|cx| Workspace::new(window, cx));
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

#[gpui::test]
fn a_query_typed_and_run_from_the_keyboard_lands_its_rows(cx: &mut TestAppContext) {
    store::tests::with_home(|| {
        let directory = store::dbdelve_directory().expect("the test home has a data directory");
        std::fs::create_dir_all(&directory).expect("the data directory must be creatable");
        // An empty file is an empty database. dbdelve opens a SQLite file
        // rather than creating one, so it has to be there first.
        let database = directory.join("e2e.db");
        std::fs::write(&database, b"").expect("the database file must be creatable");
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

        cx.simulate_input("select 1");
        cx.simulate_keystrokes("secondary-enter");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, _| {
            let tab = workspace
                .profile()
                .and_then(|profile| profile.session.active_query_tab())
                .expect("a fresh profile opens on a query tab");
            match &tab.query {
                QueryState::Complete { rows, .. } => assert_eq!(*rows, 1),
                QueryState::Failed(error) => panic!("the query failed: {error}"),
                _ => panic!("the query never completed"),
            }
        });

        quit(workspace, cx);
    });
}
