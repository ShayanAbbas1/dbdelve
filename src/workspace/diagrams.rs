//! Opening a schema's ER diagram and reading what it draws.

use std::sync::{Arc, OnceLock};

use super::*;

use crate::diagram::{index::Scene, layout};
use crate::diagram_view::{DiagramEvent, DiagramView, Load, Ready};

/// Above this many tables a diagram opens with key columns only (design §6.5).
const KEYS_ONLY_ABOVE: usize = 300;

impl Workspace {
    /// The schema a diagram opens on when nothing names one: the object tab
    /// in front's, else the one unqualified names resolve in, else the
    /// catalog's first.
    pub(crate) fn diagram_schema(profile: &Profile) -> Option<String> {
        if let Some(tab) = profile.session.active_object() {
            return Some(tab.schema.clone());
        }
        if let Some(schema) = profile.active_schema() {
            return Some(schema.to_string());
        }
        match &profile.catalog {
            CatalogState::Loaded(catalog, _) => {
                catalog.schemas.first().map(|schema| schema.name.clone())
            }
            _ => None,
        }
    }

    /// Bring a schema's diagram to the front, opening and reading it if it
    /// has no tab yet.
    pub(crate) fn open_diagram(
        &mut self,
        schema: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        if !profile.config.target().draws_diagrams() {
            return;
        }
        let existing = profile
            .session
            .objects
            .iter()
            .find(|tab| matches!(tab.body, ObjectBody::Diagram { .. }) && tab.schema == schema)
            .map(|tab| tab.id);
        let id = match existing {
            Some(id) => id,
            None => {
                let id = profile.session.next_object_id;
                profile.session.next_object_id += 1;
                let view = cx.new(DiagramView::new);
                cx.subscribe_in(
                    &view,
                    window,
                    |workspace, _, event, window, cx| match event {
                        DiagramEvent::Open {
                            schema,
                            table,
                            structure,
                        } => workspace.open_diagram_table(schema, table, *structure, window, cx),
                    },
                )
                .detach();
                profile.session.objects.push(ObjectTab {
                    id,
                    schema,
                    name: String::new(),
                    kind: None,
                    body: ObjectBody::Diagram {
                        view,
                        side: None,
                        stale: false,
                    },
                });
                profile.session.place_last(session::TabKey::Object(id));
                id
            }
        };
        self.activate_tab(Tab::Object(id), window, cx);
    }

    /// A table off a diagram, in the tab the explorer opens for it: its rows,
    /// or its structure.
    fn open_diagram_table(
        &mut self,
        schema: &str,
        table: &str,
        structure: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = match self.catalog() {
            Some(catalog) => relation_kind(catalog, schema, table).unwrap_or_default(),
            None => RelationKind::default(),
        };
        self.open_relation_view(
            schema.to_string(),
            table.to_string(),
            kind,
            structure,
            window,
            cx,
        );
    }

    /// Read a diagram's schema on a connection of its own and lay it out, both
    /// off the UI thread, unless it is read or being read already. A stale
    /// diagram is read again at the keys-only setting on screen, and stays on
    /// screen until the new one replaces it.
    pub(crate) fn load_diagram(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(connection) = profile.connection() else {
            return;
        };
        let (profile_id, generation) = (profile.id.clone(), profile.generation);
        let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        let ObjectBody::Diagram { view, side, stale } = &mut tab.body else {
            return;
        };
        let shown = view.read(cx).keys_only();
        if side.is_some() || (shown.is_some() && !*stale) {
            return;
        }
        *stale = false;
        let schema = tab.schema.clone();
        let reading = Arc::new(OnceLock::new());
        *side = Some(reading.clone());
        let issued = reading.clone();
        let task = spawn_blocking(cx.background_executor(), move || {
            let own = connection.apart()?;
            let graph = reading.get_or_init(|| own).schema_graph(&schema)?;
            let keys_only = shown.unwrap_or(graph.tables.len() > KEYS_ONLY_ABOVE);
            let scene = Scene::new(layout::schema(&graph, keys_only));
            Ok::<_, DbError>(Ready::new(graph, scene))
        });
        cx.spawn(async move |workspace, cx| {
            let result = task.await;
            workspace
                .update(cx, |workspace, cx| {
                    let current = workspace.issued_to(&profile_id, generation).is_some();
                    let Some(profile) =
                        (workspace.profiles.iter_mut()).find(|profile| profile.id == profile_id)
                    else {
                        return;
                    };
                    let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id)
                    else {
                        return;
                    };
                    let ObjectBody::Diagram { view, side, stale } = &mut tab.body else {
                        return;
                    };
                    // The slot is freed even when the answer is dropped, or a
                    // read that outlived its connection would block every
                    // later one.
                    if !side.as_ref().is_some_and(|side| Arc::ptr_eq(side, &issued)) {
                        return;
                    }
                    *side = None;
                    if !current {
                        *stale = true;
                        return;
                    }
                    let shown = view.read(cx).keys_only().is_some();
                    // A Refresh while this was out: it may predate the change.
                    let again = *stale;
                    let load = match result {
                        Ok(ready) => Load::Ready(Box::new(ready)),
                        Err(error) => {
                            tracing::debug!(error = %error.message, "schema graph read failed");
                            let message =
                                format!("Could not read {}: {}", tab.schema, error.message);
                            // A diagram already drawn stays, with the reason in
                            // the status bar, and is tried again when next shown.
                            if shown {
                                *stale = true;
                                profile.session.notice = Some(message);
                                cx.notify();
                                return;
                            }
                            Load::Failed(message.into())
                        }
                    };
                    view.update(cx, |view, cx| view.set_load(load, cx));
                    if again
                        && workspace
                            .profile()
                            .is_some_and(|front| front.id == profile_id)
                    {
                        workspace.load_diagram(id, cx);
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Stops a diagram's read that is still out, on its own connection, so a
    /// closed tab does not leave the server working for nobody.
    pub(crate) fn cancel_diagram(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(side) = self.profile_mut().and_then(|profile| {
            let tab = profile
                .session
                .objects
                .iter_mut()
                .find(|tab| tab.id == id)?;
            match &mut tab.body {
                ObjectBody::Diagram { side, .. } => side.take(),
                _ => None,
            }
        }) else {
            return;
        };
        spawn_cancel(cx.background_executor(), move || {
            if let Some(connection) = side.get() {
                let _ = connection.cancel(&CancelToken::default());
            }
        })
        .detach();
    }
}
