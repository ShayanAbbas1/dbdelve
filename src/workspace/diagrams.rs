//! Opening a schema's ER diagram and reading what it draws.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use super::*;

use crate::diagram::geometry::{MAX_CARD_WIDTH, MIN_CARD_WIDTH, Point, WORLD_LIMIT};
use crate::diagram::index::{Placement, Scene};
use crate::diagram::{layout, png};
use crate::diagram_view::{DiagramEvent, DiagramFile, DiagramView, Drawing, Load, Ready, TableKey};

use super::exports::{Ended, choose_path, could_not_write, write_replacing};

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

    /// The schemas the palette offers a diagram of, each with its database
    /// (`None` for the connection's own): the one a diagram opens on when
    /// nothing names one, then every other schema of the catalog with a
    /// table to draw. The first is the object tab in front's when there is
    /// one, so it keeps that tab's database: `public` of another database is
    /// not the connection's `public`.
    pub(crate) fn diagram_schemas(profile: &Profile) -> Vec<(Option<String>, String)> {
        let catalog = match &profile.catalog {
            CatalogState::Loaded(catalog, _) => Some(catalog),
            _ => None,
        };
        let own = profile.active_database();
        let database = (profile.session.active_object())
            .and_then(|tab| tab.database.clone())
            .filter(|database| own.as_ref() != Some(database));
        offered_schemas(database, Self::diagram_schema(profile), catalog)
    }

    /// Bring a schema's diagram to the front, opening and reading it if it
    /// has no tab yet, and with a `centre`, showing that table's
    /// neighbourhood. The schema is `database`'s, the connection's own where
    /// none is named.
    pub(crate) fn open_diagram(
        &mut self,
        database: Option<String>,
        schema: String,
        centre: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let database = self.elsewhere(database);
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let existing = (profile.session.objects.iter()).find_map(|tab| match &tab.body {
            ObjectBody::Diagram { view, .. }
                if tab.schema == schema && tab.database == database =>
            {
                Some((tab.id, view.clone()))
            }
            _ => None,
        });
        let id = match existing {
            Some((id, view)) => {
                if let Some(table) = &centre {
                    view.update(cx, |view, cx| view.centre_on(table, cx));
                }
                id
            }
            None => {
                let restore = centre.map(|centre| store::StoredDiagram {
                    centre: Some(centre),
                    ..store::StoredDiagram::default()
                });
                let Some(id) = self.diagram_tab(database, schema, restore, window, cx) else {
                    return;
                };
                id
            }
        };
        self.activate_tab(Tab::Object(id), window, cx);
    }

    /// Give a schema's diagram a tab, unread and not shown, looked at as
    /// `restore` says once read when it comes back from disk. None where the
    /// profile draws no diagrams. `database` is as a tab holds it: named only
    /// when it is not the connection's own.
    pub(crate) fn diagram_tab(
        &mut self,
        database: Option<String>,
        schema: String,
        restore: Option<store::StoredDiagram>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let profile = self.profile_mut()?;
        if !profile.draws_diagrams() {
            return None;
        }
        let id = profile.session.next_object_id;
        profile.session.next_object_id += 1;
        let view = cx.new(|cx| match restore {
            Some(restore) => DiagramView::restoring(restore, cx),
            None => DiagramView::new(cx),
        });
        let (profile_id, drawn, bound) = (profile.id.clone(), schema.clone(), database.clone());
        cx.subscribe_in(
            &view,
            window,
            move |workspace, view, event, window, cx| match event {
                DiagramEvent::Open {
                    schema,
                    table,
                    structure,
                } => workspace.open_diagram_table(
                    bound.clone(),
                    schema,
                    table,
                    *structure,
                    window,
                    cx,
                ),
                DiagramEvent::Arranged => {
                    let Some(database) = (workspace.profiles.iter())
                        .find(|profile| profile.id == profile_id)
                        .and_then(|profile| arrangement_database(profile, bound.as_deref()))
                    else {
                        return;
                    };
                    let cards = stored_cards(view.read(cx).arranged());
                    if let Err(message) =
                        store::write_diagram(&profile_id, database.as_deref(), &drawn, &cards)
                    {
                        workspace.note(message, cx);
                    }
                }
                DiagramEvent::Viewed => workspace.remember_profiles(cx),
                DiagramEvent::Moved => workspace.remember_profiles_soon(cx),
                DiagramEvent::Export(file) => workspace.export_diagram(view, *file, cx),
                DiagramEvent::CopyMermaid => workspace.copy_diagram_mermaid(view, cx),
            },
        )
        .detach();
        let profile = self.profile_mut()?;
        profile.session.objects.push(ObjectTab {
            id,
            database,
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
        Some(id)
    }

    fn front_diagram(&self) -> Option<Entity<DiagramView>> {
        (self.profile())
            .and_then(|profile| profile.session.active_object())
            .and_then(|tab| match &tab.body {
                ObjectBody::Diagram { view, .. } => Some(view.clone()),
                _ => None,
            })
    }

    /// The diagram tab in front, written as its Export menu would.
    pub(crate) fn export_front_diagram(&mut self, file: DiagramFile, cx: &mut Context<Self>) {
        if let Some(view) = self.front_diagram() {
            self.export_diagram(&view, file, cx);
        }
    }

    /// The diagram tab in front, copied as Mermaid as its Export menu would.
    pub(crate) fn copy_front_diagram_mermaid(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.front_diagram() {
            self.copy_diagram_mermaid(&view, cx);
        }
    }

    /// A diagram as Mermaid text on the clipboard, which pastes into a
    /// GitHub issue and draws there. Cheap enough for the UI thread: text,
    /// a line per column and per key.
    pub(crate) fn copy_diagram_mermaid(
        &mut self,
        view: &Entity<DiagramView>,
        cx: &mut Context<Self>,
    ) {
        let Some(drawing) = view.read(cx).drawing(cx) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(drawing.mermaid()));
        self.confirm("Copied the diagram as Mermaid.".to_string(), cx);
    }

    /// Write a diagram to a file the user picks, as it is drawn now. The
    /// drawing is taken before the panel opens, which is modeless: what was
    /// on screen when they asked is what they asked for.
    pub(crate) fn export_diagram(
        &mut self,
        view: &Entity<DiagramView>,
        file: DiagramFile,
        cx: &mut Context<Self>,
    ) {
        let Some(drawing) = view.read(cx).drawing(cx) else {
            return;
        };
        let Some(profile) = self.profile() else {
            return;
        };
        let (id, generation) = (profile.id.clone(), profile.generation);
        let chosen = choose_path(&drawing.stem, file.extension(), cx);
        cx.spawn(async move |workspace, cx| {
            let Some(path) = chosen.await else {
                return;
            };
            let file = DiagramFile::for_path(&path, file);
            // A large PNG takes seconds to draw, with nothing else to show
            // for it until it is written.
            if file == DiagramFile::Png {
                let (id, notice) = (id.clone(), "Exporting the diagram as PNG…".to_string());
                _ = workspace.update(cx, |workspace, cx| {
                    if let Some(profile) = workspace.issued_to(&id, generation) {
                        profile.session.notice = Some(notice);
                        cx.notify();
                    }
                });
            }
            let written = spawn_blocking(cx.background_executor(), move || {
                let written = write_drawing(&drawing, file, &path);
                match &written {
                    Ok(_) => tracing::info!(format = file.extension(), "diagram export finished"),
                    Err(_) => tracing::warn!(format = file.extension(), "diagram export failed"),
                }
                written
                    .map(|shrunk| exported_drawing(file, &path, shrunk))
                    .map_err(|ended| ended.notice(&path))
            })
            .await;
            _ = workspace.update(cx, |workspace, cx| {
                let Some(profile) = workspace.issued_to(&id, generation) else {
                    return;
                };
                match written {
                    Ok(message) => {
                        profile.session.notice = Some(message.clone());
                        workspace.clear_notice_later(id, generation, message, cx);
                    }
                    Err(message) => profile.session.notice = Some(message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A table off a diagram, in the tab the explorer opens for it: its rows,
    /// or its structure.
    fn open_diagram_table(
        &mut self,
        database: Option<String>,
        schema: &str,
        table: &str,
        structure: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let catalog = self
            .profile()
            .map(|profile| profile.catalog_state_of(database.as_deref()));
        let kind = match catalog {
            Some(CatalogState::Loaded(catalog, _)) => {
                relation_kind(catalog, schema, table).unwrap_or_default()
            }
            _ => RelationKind::default(),
        };
        self.open_relation_view(
            database,
            schema.to_string(),
            table.to_string(),
            kind,
            structure,
            window,
            cx,
        );
    }

    /// Read a diagram's schema on a connection of its own and lay it out, both
    /// off the UI thread, unless it is read or being read already. A diagram
    /// of another database is read on a session opened there for it. A stale
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
        let Some((bound, schema)) = (profile.session.objects.iter())
            .find(|tab| tab.id == id)
            .map(|tab| (tab.database.clone(), tab.schema.clone()))
        else {
            return;
        };
        let database = arrangement_database(profile, bound.as_deref());
        // The catalog a bound tab's diagram reads is narrowed by that
        // database's table lists, the tree's catalog by the own one's.
        let listed = match bound.as_deref() {
            Some(database) => profile.shown().in_another_database(database),
            None => profile.shown(),
        };
        // Counted from the narrowed catalog, as the graph is narrowed below.
        let known_tables = match profile.catalog_state_of(bound.as_deref()) {
            CatalogState::Loaded(catalog, _) => drawn_table_count(catalog, &schema),
            _ => None,
        };
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
        let wanted = view.read(cx).wanted_keys_only();
        *stale = false;
        let reading = Arc::new(SideRead::new());
        *side = Some(reading.clone());
        let issued = reading.clone();
        let saved_under = profile_id.clone();
        let task = spawn_blocking(cx.background_executor(), move || {
            if reading.cancelled() {
                return Ok(None);
            }
            let session = match &bound {
                Some(database) => connection.on_database(database)?,
                None => connection.apart()?,
            };
            let Some(own) = reading.publish(session) else {
                return Ok(None);
            };
            let graph = without_hidden(own.schema_graph(&schema)?, |name| {
                listed.shows_relation(&schema, name)
            });
            let keys_only =
                wanted.unwrap_or(known_tables.unwrap_or(graph.tables.len()) > KEYS_ONLY_ABOVE);
            let scene = Scene::new(layout::schema(&graph, keys_only));
            let mut ready = Ready::new(graph, scene);
            // A diagram read again keeps the arrangement on screen instead.
            if shown.is_none()
                && let Some(database) = database
            {
                let cards = store::read_diagram(&saved_under, database.as_deref(), &schema);
                ready.adopt(placements(cards));
            }
            Ok::<_, DbError>(Some(ready))
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
                        // A reconnect's own read was refused while this one
                        // held the slot, so a tab in front would wait on
                        // nothing.
                        let front = (workspace.profile()).is_some_and(|front| {
                            front.id == profile_id && front.session.active == Tab::Object(id)
                        });
                        if front {
                            workspace.load_diagram(id, cx);
                        }
                        return;
                    }
                    let shown = view.read(cx).keys_only().is_some();
                    // A Refresh while this was out: it may predate the change.
                    let again = *stale;
                    let load = match result {
                        Ok(Some(ready)) => Load::Ready(Box::new(ready)),
                        // Only a read its tab let go stops short, and that tab
                        // no longer holds it.
                        Ok(None) => return,
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

    /// Tells the diagram that was in front at the last render, if another
    /// tab or profile is now, to let its render caches go (design §6.6).
    /// Here rather than at each way a tab leaves the front, which are many.
    pub(crate) fn track_shown_diagram(&mut self, cx: &mut Context<Self>) {
        let front = (self.profile())
            .and_then(|profile| profile.session.active_object())
            .and_then(|tab| match &tab.body {
                ObjectBody::Diagram { view, .. } => Some(view.clone()),
                _ => None,
            });
        let was = std::mem::replace(
            &mut self.shown_diagram,
            front.as_ref().map(Entity::downgrade),
        );
        if let Some(was) = was.and_then(|was| was.upgrade())
            && front.as_ref() != Some(&was)
        {
            was.update(cx, |view, _| view.hide());
        }
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
        stop_side_read(side, cx);
    }

    /// Stops every diagram read still out from the connection a profile is
    /// letting go, as a closed tab's is: one stalled there would otherwise
    /// hold its tab's slot, and keep the next connection's read from
    /// starting, until it answered.
    pub(crate) fn cancel_diagram_reads(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.get_mut(index) else {
            return;
        };
        let sides = (profile.session.objects.iter_mut()).filter_map(|tab| match &mut tab.body {
            ObjectBody::Diagram { side, .. } => side.take(),
            _ => None,
        });
        for side in sides {
            stop_side_read(side, cx);
        }
    }
}

fn stop_side_read(side: Arc<SideRead<Connection>>, cx: &mut Context<Workspace>) {
    spawn_cancel(cx.background_executor(), move || {
        if let Some(connection) = side.cancel() {
            let _ = connection.cancel(&CancelToken::default());
        }
    })
    .detach();
}

/// The connection a diagram's read runs on, shared by the tab and the read so
/// that closing the tab stops the read wherever it is: before its connection
/// is opened, while it opens (a Postgres or MySQL handshake), or once its
/// statements are out.
pub(crate) struct SideRead<T> {
    state: Mutex<Side<T>>,
}

enum Side<T> {
    Opening,
    Open(Arc<T>),
    Cancelled,
}

impl<T> SideRead<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(Side::Opening),
        }
    }

    fn cancelled(&self) -> bool {
        matches!(
            *self.state.lock().unwrap_or_else(PoisonError::into_inner),
            Side::Cancelled
        )
    }

    /// Hands over the connection the read opened, to run on. None once the
    /// tab has let the read go: the connection is dropped, which closes it.
    fn publish(&self, own: T) -> Option<Arc<T>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match *state {
            Side::Opening => {
                let own = Arc::new(own);
                *state = Side::Open(own.clone());
                Some(own)
            }
            Side::Open(_) | Side::Cancelled => None,
        }
    }

    /// Lets the read go, with its connection if it has one, so what runs on
    /// it can be cancelled.
    fn cancel(&self) -> Option<Arc<T>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match std::mem::replace(&mut *state, Side::Cancelled) {
            Side::Open(connection) => Some(connection),
            Side::Opening | Side::Cancelled => None,
        }
    }
}

/// How many cards the loaded catalog says a schema's diagram holds, so the
/// first layout is made in the right mode without waiting on the read.
fn drawn_table_count(catalog: &Catalog, schema: &str) -> Option<usize> {
    let schema = catalog.schemas.iter().find(|found| found.name == schema)?;
    Some(drawn_tables(schema))
}

fn drawn_tables(schema: &crate::db::Schema) -> usize {
    (schema.relations.iter())
        .filter(|relation| drawn_in_diagrams(relation.kind) && relation.partition_of.is_none())
        .count()
}

/// `graph` without the tables `shows` leaves out, and without the keys to or
/// from them, so a diagram draws what the tree does. A column keeps its
/// foreign mark: the key is real, only the table it names is hidden.
///
/// ponytail: the hidden tables' columns are still read and dropped here; the
/// upgrade is a name list in each engine's graph SQL.
fn without_hidden(
    graph: crate::db::SchemaGraph,
    shows: impl Fn(&str) -> bool,
) -> crate::db::SchemaGraph {
    use crate::db::KeyTarget;
    let mut kept_at: Vec<Option<u32>> = Vec::with_capacity(graph.tables.len());
    let mut tables = Vec::new();
    for table in graph.tables {
        if shows(&table.name) {
            kept_at.push(Some(tables.len() as u32));
            tables.push(table);
        } else {
            kept_at.push(None);
        }
    }
    let at = |table: u32| kept_at.get(table as usize).copied().flatten();
    let keys = (graph.keys.into_iter())
        .filter_map(|mut key| {
            key.table = at(key.table)?;
            if let KeyTarget::Table { table, .. } = &mut key.referenced {
                *table = at(*table)?;
            }
            Some(key)
        })
        .collect();
    crate::db::SchemaGraph {
        tables,
        keys,
        ..graph
    }
}

/// `first`, in `database` (`None` for the connection's own), then the
/// catalog's other schemas that have a table to draw, all in the
/// connection's own database. A schema of the catalog is left out only where
/// `first` is the same schema of the same database.
fn offered_schemas(
    database: Option<String>,
    first: Option<String>,
    catalog: Option<&Catalog>,
) -> Vec<(Option<String>, String)> {
    let skipped = first.clone().filter(|_| database.is_none());
    let rest = (catalog.into_iter())
        .flat_map(|catalog| &catalog.schemas)
        .filter(|schema| skipped.as_ref() != Some(&schema.name) && drawn_tables(schema) > 0)
        .map(|schema| (None, schema.name.clone()));
    (first.map(|first| (database, first)).into_iter())
        .chain(rest)
        .collect()
}

/// Whether a relation of this kind is drawn as a card of its schema's
/// diagram, so has a neighbourhood to show.
pub(crate) fn drawn_in_diagrams(kind: RelationKind) -> bool {
    matches!(kind, RelationKind::Table | RelationKind::PartitionedTable)
}

/// The database a diagram's arrangement is filed under: `bound`, the one its
/// tab names, else the connection's own, named as a pin names its own: none
/// where the catalog lists every database as a schema or the profile has one.
/// Unknown, nothing is read or written: an arrangement filed without its
/// database would come back for the same schema in whatever database came
/// next.
fn arrangement_database(profile: &Profile, bound: Option<&str>) -> Option<Option<String>> {
    if let Some(bound) = bound {
        return Some(Some(bound.to_string()));
    }
    match profile.shortcut_database() {
        crate::explorer::ShortcutDatabase::Unnamed => Some(None),
        crate::explorer::ShortcutDatabase::On(database) => Some(Some(database)),
        crate::explorer::ShortcutDatabase::Unknown => None,
    }
}

/// The cards a diagram's file holds, by table. A value no card can take,
/// which only a hand edit writes, is dropped with its card's other parts kept.
fn placements(cards: Vec<store::StoredCard>) -> HashMap<TableKey, Placement> {
    // Checked as `f32`, which is what is kept: `1e100` is finite as read.
    let within = |value: Option<f64>, low: f32, high: f32| {
        value
            .map(|value| value as f32)
            .filter(|value| (low..=high).contains(value))
    };
    let at = |value| within(value, -WORLD_LIMIT, WORLD_LIMIT);
    cards
        .into_iter()
        .map(|card| {
            let placement = Placement {
                origin: at(card.x).zip(at(card.y)).map(|(x, y)| Point::new(x, y)),
                width: within(card.width, MIN_CARD_WIDTH, MAX_CARD_WIDTH),
                height: within(card.height, f32::MIN_POSITIVE, WORLD_LIMIT),
            };
            ((card.schema.into(), card.name.into()), placement)
        })
        .filter(|(_, placement)| !placement.is_empty())
        .collect()
}

/// What a diagram's file holds for its arrangement: a card per table, in
/// name order so the file reads the same however it was arrived at.
fn stored_cards<'a>(
    arranged: impl Iterator<Item = (&'a TableKey, &'a Placement)>,
) -> Vec<store::StoredCard> {
    let rounded = |value: f32| (f64::from(value) * 100.0).round() / 100.0;
    let mut cards: Vec<store::StoredCard> = arranged
        .map(|((schema, name), placement)| store::StoredCard {
            schema: schema.to_string(),
            name: name.to_string(),
            x: placement.origin.map(|origin| rounded(origin.x)),
            y: placement.origin.map(|origin| rounded(origin.y)),
            width: placement.width.map(rounded),
            height: placement.height.map(rounded),
        })
        .collect();
    cards.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
    cards
}

/// A drawing written to `path` as `file`, whole or not at all. A PNG too
/// large to draw at full size is shrunk, and the scale it was drawn at comes
/// back.
fn write_drawing(drawing: &Drawing, file: DiagramFile, path: &Path) -> Result<Option<f32>, Ended> {
    let (bytes, shrunk) = match file {
        DiagramFile::Svg => (drawing.svg().into_bytes(), None),
        DiagramFile::Mermaid => (drawing.mermaid().into_bytes(), None),
        DiagramFile::Png => {
            let png = png::render(&drawing.svg()).map_err(Ended::Failed)?;
            tracing::info!(png.width, png.height, png.scale, "diagram drawn as a PNG");
            let shrunk = png.shrunk().then_some(png.scale);
            (png.bytes, shrunk)
        }
    };
    write_replacing(path, |out| {
        std::io::Write::write_all(out, &bytes).map_err(could_not_write(path))
    })?;
    Ok(shrunk)
}

fn exported_drawing(file: DiagramFile, path: &Path, shrunk: Option<f32>) -> String {
    match shrunk {
        None => format!(
            "Exported the diagram as {} to {}.",
            file.label(),
            path.display()
        ),
        Some(scale) => format!(
            "Exported the diagram as {} to {}, drawn at {:.0}% because it is too large \
             to draw whole. SVG keeps every detail.",
            file.label(),
            path.display(),
            (scale * 100.0).floor().max(1.0),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Relation, Schema};

    /// Counts its drops, standing in for a connection, which closes on drop.
    struct Counted(Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn a_read_let_go_while_its_connection_opens_never_runs_on_it() {
        let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let closed = || dropped.load(std::sync::atomic::Ordering::SeqCst);

        let side = SideRead::new();
        assert!(!side.cancelled());
        assert!(side.cancel().is_none(), "nothing to cancel yet");
        assert!(side.cancelled(), "so the read need not open one");
        assert!(side.publish(Counted(dropped.clone())).is_none());
        assert_eq!(closed(), 1, "and one it opened anyway is closed");

        let side = SideRead::new();
        let running = side.publish(Counted(dropped.clone())).expect("not let go");
        let cancelled = side.cancel().expect("the close reaches the connection");
        assert!(Arc::ptr_eq(&running, &cancelled));
        assert!(side.cancelled());
        drop((running, cancelled));
        assert_eq!(closed(), 2);
    }

    #[test]
    fn an_arrangement_goes_to_its_file_and_back_as_it_was() {
        let mut arranged: HashMap<TableKey, Placement> = HashMap::new();
        arranged.insert(
            ("public".into(), "orders".into()),
            Placement {
                origin: Some(Point::new(12.5, -40.0)),
                width: None,
                height: Some(82.0),
            },
        );
        arranged.insert(
            ("elsewhere".into(), "a \"b\".c".into()),
            Placement {
                origin: None,
                width: Some(301.25),
                height: None,
            },
        );
        let cards = stored_cards(arranged.iter());
        assert_eq!(cards[0].schema, "elsewhere", "in name order");
        assert_eq!(placements(cards), arranged);
    }

    #[test]
    fn a_card_value_no_card_can_take_is_dropped_alone() {
        let card = |x: f64, width: Option<f64>| store::StoredCard {
            schema: "public".into(),
            name: "orders".into(),
            x: Some(x),
            y: Some(1.0),
            width,
            height: None,
        };
        let kept = placements(vec![card(f64::NAN, Some(300.0))]);
        assert_eq!(
            kept[&("public".into(), "orders".into())],
            Placement {
                origin: None,
                width: Some(300.0),
                height: None,
            }
        );
        assert!(placements(vec![card(f64::INFINITY, None)]).is_empty());
        assert!(
            placements(vec![card(1e100, None)]).is_empty(),
            "finite as read, infinite as kept"
        );
        assert!(placements(vec![card(2.0e6, Some(5_000.0))]).is_empty());
        assert!(placements(vec![card(f64::NAN, Some(-1.0))]).is_empty());
        let short = store::StoredCard {
            height: Some(-82.0),
            ..card(10.0, Some(f64::from(MIN_CARD_WIDTH)))
        };
        assert_eq!(
            placements(vec![short])[&("public".into(), "orders".into())],
            Placement {
                origin: Some(Point::new(10.0, 1.0)),
                width: Some(MIN_CARD_WIDTH),
                height: None,
            }
        );
    }

    #[test]
    fn the_catalog_counts_the_cards_a_schema_will_draw() {
        let relation = |name: &str, kind, partition_of: Option<&str>| Relation {
            name: name.into(),
            kind,
            partition_of: partition_of.map(Into::into),
            size: None,
            rows: None,
        };
        let catalog = Catalog {
            schemas: vec![Schema {
                name: "public".into(),
                relations: vec![
                    relation("orders", RelationKind::PartitionedTable, None),
                    relation("orders_2024", RelationKind::Table, Some("orders")),
                    relation("users", RelationKind::Table, None),
                    relation("active_users", RelationKind::View, None),
                ],
                routines: Vec::new(),
                sequences: Vec::new(),
            }],
            database: None,
            ..Default::default()
        };
        assert_eq!(drawn_table_count(&catalog, "public"), Some(2));
        assert_eq!(drawn_table_count(&catalog, "other"), None);
    }

    #[test]
    fn a_diagram_draws_only_the_tables_the_table_list_shows() {
        use crate::db::{
            GraphColumn, GraphKey, GraphTable, Include, KeyTarget, Levels, SchemaGraph, TableFilter,
        };
        let column = |name: &str, primary: bool, foreign: bool| GraphColumn {
            name: name.to_string().into(),
            type_label: 0,
            nullable: false,
            primary,
            foreign,
            unique: primary,
        };
        let table = |name: &str, columns: Vec<GraphColumn>| GraphTable {
            name: name.to_string().into(),
            columns,
        };
        let key = |name: &str, table: u32, referenced: KeyTarget| GraphKey {
            name: name.to_string().into(),
            table,
            columns: vec![1],
            referenced,
        };
        let to = |table: u32| KeyTarget::Table {
            table,
            columns: vec![0],
        };
        let graph = SchemaGraph {
            schema: "public".into(),
            tables: vec![
                table(
                    "a",
                    vec![column("id", true, false), column("b_id", true, true)],
                ),
                table(
                    "b",
                    vec![column("id", true, false), column("c_id", false, true)],
                ),
                table(
                    "c",
                    vec![column("id", true, false), column("a_id", false, true)],
                ),
            ],
            types: vec!["int".into()],
            keys: vec![
                key("a_b", 0, to(1)),
                key("b_c", 1, to(2)),
                key("c_a", 2, to(0)),
                key(
                    "a_far",
                    0,
                    KeyTarget::Elsewhere {
                        schema: "audit".into(),
                        table: "log".into(),
                    },
                ),
            ],
        };
        let shown = Include::default()
            .with_tables(vec![TableFilter {
                database: None,
                schema: "public".into(),
                tables: vec!["a".into(), "c".into()],
            }])
            .resolve(Levels::Flat, None, None);

        let drawn = without_hidden(graph, |name| shown.shows_relation("public", name));

        let names: Vec<&str> = drawn
            .tables
            .iter()
            .map(|table| table.name.as_ref())
            .collect();
        assert_eq!(names, ["a", "c"]);
        let keys: Vec<(&str, u32, &KeyTarget)> = (drawn.keys.iter())
            .map(|key| (key.name.as_ref(), key.table, &key.referenced))
            .collect();
        assert_eq!(
            keys,
            [
                ("c_a", 1, &to(0)),
                (
                    "a_far",
                    0,
                    &KeyTarget::Elsewhere {
                        schema: "audit".into(),
                        table: "log".into(),
                    }
                ),
            ],
            "a key to or from a hidden table goes, the rest point at the kept tables"
        );
        assert!(
            drawn.tables[0].columns[1].foreign,
            "b_id is still a foreign key; only the table it names is hidden"
        );
    }

    #[test]
    fn the_palette_offers_the_diagram_of_every_schema_with_a_table_the_default_first() {
        let schema = |name: &str, kind| Schema {
            name: name.into(),
            relations: vec![Relation {
                name: "t".into(),
                kind,
                partition_of: None,
                size: None,
                rows: None,
            }],
            routines: Vec::new(),
            sequences: Vec::new(),
        };
        let catalog = Catalog {
            schemas: vec![
                schema("archive", RelationKind::Table),
                schema("public", RelationKind::Table),
                schema("reports", RelationKind::View),
                schema("billing", RelationKind::PartitionedTable),
            ],
            database: None,
            ..Default::default()
        };
        let own = |name: &str| (None, name.to_string());
        assert_eq!(
            offered_schemas(None, Some("public".into()), Some(&catalog)),
            [own("public"), own("archive"), own("billing")]
        );
        assert_eq!(
            offered_schemas(None, Some("public".into()), None),
            [own("public")],
            "before the catalog is read"
        );
        assert!(offered_schemas(None, None, None).is_empty());
        assert_eq!(
            offered_schemas(
                Some("warehouse".into()),
                Some("public".into()),
                Some(&catalog)
            ),
            [
                (Some("warehouse".to_string()), "public".to_string()),
                own("archive"),
                own("public"),
                own("billing"),
            ],
            "another database's public keeps its database, and the connection's own public \
             is still offered"
        );
    }
}
