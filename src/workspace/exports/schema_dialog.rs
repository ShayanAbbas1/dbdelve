//! The dialog a schema export is chosen in: which tables, and their structure,
//! their rows or both.

use std::collections::HashMap;

use gpui::{ElementId, SharedString};
use gpui_component::checkbox::Checkbox;

use super::schema::{SchemaRequest, dependency_order};
use super::*;
use crate::db::{ForeignKey, ForeignKeyReplay, RelationKind, Target};

pub(crate) struct SchemaExportDialog {
    pub(crate) schema: String,
    /// Every plain table, in catalog order, and whether it is ticked.
    pub(crate) tables: Vec<(String, bool)>,
    pub(crate) structure: bool,
    pub(crate) rows: bool,
    /// Each table's foreign keys, read on a session of its own when the dialog opens.
    pub(crate) keys: Loading<HashMap<String, Vec<ForeignKey>>>,
    /// Which read `keys` waits for, so a closed dialog's read is dropped.
    pub(crate) generation: u64,
}

pub(crate) enum Loading<T> {
    Reading,
    Read(T),
    Failed(String),
}

impl SchemaExportDialog {
    fn new(schema: String, tables: Vec<String>, generation: u64) -> Self {
        Self {
            schema,
            tables: tables.into_iter().map(|table| (table, true)).collect(),
            structure: true,
            rows: true,
            keys: Loading::Reading,
            generation,
        }
    }

    fn ticked(&self) -> impl Iterator<Item = &String> {
        self.tables
            .iter()
            .filter(|(_, ticked)| *ticked)
            .map(|(name, _)| name)
    }

    /// What Export would write, or why it is disabled.
    pub(crate) fn request(&self) -> Result<SchemaRequest, &'static str> {
        let tables: Vec<String> = self.ticked().cloned().collect();
        if tables.is_empty() {
            return Err("Tick at least one table.");
        }
        if !self.structure && !self.rows {
            return Err("Tick Structure, Rows or both.");
        }
        Ok(SchemaRequest {
            tables,
            structure: self.structure,
            rows: self.rows,
        })
    }

    /// The dialog's warnings: missing references and, rows only, cycles.
    pub(crate) fn warnings(&self, engine: Engine) -> Vec<String> {
        let Loading::Read(keys) = &self.keys else {
            return Vec::new();
        };
        let ticked = self
            .ticked()
            .map(|name| (name.clone(), keys.get(name).cloned().unwrap_or_default()))
            .collect();
        let ordered = dependency_order(&self.schema, ticked);
        let mut warnings = Vec::new();
        if !ordered.missing.is_empty() {
            warnings.push(format!(
                "Referenced but not exported: {}.",
                ordered.missing.join(", ")
            ));
        }
        // With the structure the keys come after the rows, and with the checks
        // off or none enforced no order of the rows fails.
        let order_matters = Target::built_in(engine)
            .exports_schema()
            .is_some_and(|export| {
                matches!(
                    export.foreign_keys,
                    ForeignKeyReplay::AddedAfterRows | ForeignKeyReplay::RefusesCycles
                )
            });
        if !self.structure && order_matters && !ordered.cyclic.is_empty() {
            warnings.push(format!(
                "These tables reference each other, so their rows may not run back in this order: {}.",
                ordered.cyclic.join(", ")
            ));
        }
        warnings
    }

    fn landed(&mut self, generation: u64, keys: Result<HashMap<String, Vec<ForeignKey>>, String>) {
        if generation != self.generation {
            return;
        }
        self.keys = match keys {
            Ok(keys) => Loading::Read(keys),
            Err(error) => Loading::Failed(error),
        };
    }
}

impl Workspace {
    /// The dialog over `schema`'s plain tables, every one ticked. A view, a
    /// partition or anything else the catalog lists is left out, as the
    /// file's header says. Each table's foreign keys are read behind it, so
    /// it can say what the tables ticked point at and leave unwritten.
    ///
    /// ponytail: asked from the explorer's schema row alone, with no palette
    /// command, until #188 settles where this is asked from.
    pub(crate) fn open_schema_export(&mut self, schema: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let engine = profile.config.engine();
        let refused = |reason: &str| format!("The schema was not exported: {reason}");
        if profile.config.target().exports_schema().is_none() {
            let reason = "this engine's structure does not run back as written.";
            self.note(refused(reason), cx);
            return;
        }
        let Some(catalog) = self.catalog() else {
            self.note(refused("its tables have not been read yet."), cx);
            return;
        };
        let tables: Vec<String> = catalog
            .schemas
            .iter()
            .find(|each| each.name == schema)
            .map(|found| {
                found
                    .relations
                    .iter()
                    .filter(|relation| {
                        matches!(relation.kind, RelationKind::Table)
                            && relation.partition_of.is_none()
                    })
                    .map(|relation| relation.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        if tables.is_empty() {
            self.note(refused("it has no tables."), cx);
            return;
        }
        let refusal = tables.iter().find_map(|table| {
            relation_refetch(engine, &schema, table, "", &[], &[])
                .and_then(|refetch| self.cleared(refetch))
                .err()
        });
        if let Some(reason) = refusal {
            self.note(refused(reason), cx);
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(connection) = profile.connection() else {
            self.note(refused("the connection is not open."), cx);
            return;
        };
        let (id, generation) = (profile.id.clone(), profile.generation);
        profile.session.last_schema_export += 1;
        let read = profile.session.last_schema_export;
        profile.session.schema_export = Some(SchemaExportDialog::new(
            schema.clone(),
            tables.clone(),
            read,
        ));
        cx.notify();

        let task = spawn_blocking(cx.background_executor(), move || {
            read_keys(&connection, &schema, &tables)
        });
        cx.spawn(async move |workspace, cx| {
            let keys = task.await;
            _ = workspace.update(cx, |workspace, cx| {
                let Some(profile) = workspace.issued_to(&id, generation) else {
                    return;
                };
                if let Some(dialog) = &mut profile.session.schema_export {
                    dialog.landed(read, keys);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn edit_schema_export(
        &mut self,
        edit: impl FnOnce(&mut SchemaExportDialog),
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = self
            .profile_mut()
            .and_then(|profile| profile.session.schema_export.as_mut())
        {
            edit(dialog);
            cx.notify();
        }
    }

    /// Export, once the keys are read and something is ticked: the save
    /// panel asks where, and the export runs as the dialog left it.
    pub(crate) fn answer_schema_export(&mut self, cx: &mut Context<Self>) {
        // Unconditional, as `answer_pending_export`'s is.
        cx.notify();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(dialog) = &profile.session.schema_export else {
            return;
        };
        if matches!(dialog.keys, Loading::Reading) {
            return;
        }
        let Ok(request) = dialog.request() else {
            return;
        };
        let schema = dialog.schema.clone();
        profile.session.schema_export = None;
        let (id, generation) = (profile.id.clone(), profile.generation);
        let chosen = choose_path(&schema, FileAs::Inserts, cx);
        cx.spawn(async move |workspace, cx| {
            let Some(path) = chosen.await else {
                return;
            };
            _ = workspace.update(cx, |workspace, cx| {
                workspace.start_schema_export(&id, generation, schema, request, path, cx);
            });
        })
        .detach();
    }

    pub(crate) fn cancel_schema_export(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled = self
            .profile_mut()
            .is_some_and(|profile| profile.session.schema_export.take().is_some());
        if cancelled {
            cx.notify();
        }
        cancelled
    }

    pub(crate) fn render_schema_export(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let profile = self.profile()?;
        let open = profile.session.schema_export.as_ref()?;
        let request = open.request();
        let reading = matches!(open.keys, Loading::Reading);
        let exports = !reading && request.is_ok();
        let mut notes = match &open.keys {
            Loading::Reading => vec![("Reading each table's foreign keys…".into(), t.text_muted)],
            Loading::Failed(error) => vec![(
                format!("Could not read the foreign keys: {error}"),
                t.danger,
            )],
            Loading::Read(_) => Vec::new(),
        };
        notes.extend(
            open.warnings(profile.config.engine())
                .into_iter()
                .map(|warning| (warning, t.text)),
        );
        if let Err(reason) = request {
            notes.push((reason.to_string(), t.text_muted));
        }
        let muted = |text: String| {
            div()
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .text_color(t.text_muted)
                .child(text)
        };
        let select = |id: &'static str, label: &'static str, on: bool| {
            button(id, label, Tone::Quiet, Control::Compact, t).on_click(cx.listener(
                move |workspace, _, _, cx| {
                    workspace.edit_schema_export(
                        |dialog| {
                            for (_, ticked) in &mut dialog.tables {
                                *ticked = on;
                            }
                        },
                        cx,
                    );
                },
            ))
        };
        let (select_all, select_none) = (
            select("schema-export-all", "Select all", true),
            select("schema-export-none", "Select none", false),
        );

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    dialog(t)
                        .child(section_label(t, "Export Schema as SQL"))
                        .child(muted(open.schema.clone()))
                        .child(
                            div()
                                .flex()
                                .gap(px(layout::SPACE_MD))
                                .child(checkbox(
                                    "schema-export-structure",
                                    "Structure",
                                    open.structure,
                                    |dialog, on| dialog.structure = on,
                                    cx,
                                ))
                                .child(checkbox(
                                    "schema-export-rows",
                                    "Rows",
                                    open.rows,
                                    |dialog, on| dialog.rows = on,
                                    cx,
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(layout::SPACE_SM))
                                .child(select_all)
                                .child(select_none),
                        )
                        .child(
                            div()
                                .id("schema-export-tables")
                                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .gap(px(layout::SPACE_XS))
                                .children(open.tables.iter().enumerate().map(
                                    |(index, (name, ticked))| {
                                        checkbox(
                                            ("schema-export-table", index),
                                            name.clone(),
                                            *ticked,
                                            move |dialog, on| {
                                                if let Some((_, ticked)) =
                                                    dialog.tables.get_mut(index)
                                                {
                                                    *ticked = on;
                                                }
                                            },
                                            cx,
                                        )
                                    },
                                )),
                        )
                        .children(
                            notes
                                .into_iter()
                                .map(|(note, color)| muted(note).text_color(color)),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    button(
                                        "cancel-schema-export",
                                        "Cancel",
                                        Tone::Quiet,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(cx.listener(
                                        |workspace, _, _, cx| {
                                            workspace.cancel_schema_export(cx);
                                        },
                                    )),
                                )
                                // Quiet while disabled, as Run is in the
                                // apply review.
                                .child(
                                    button(
                                        "schema-export",
                                        "Export",
                                        if exports { Tone::Primary } else { Tone::Quiet },
                                        Control::Standard,
                                        t,
                                    )
                                    .disabled(!exports)
                                    .on_click(cx.listener(
                                        |workspace, _, _, cx| {
                                            workspace.answer_schema_export(cx);
                                        },
                                    )),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// Dressed as the import dialog's.
fn checkbox(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
    edit: impl Fn(&mut SchemaExportDialog, bool) + 'static,
    cx: &mut Context<Workspace>,
) -> Checkbox {
    Checkbox::new(id)
        .text_size(px(layout::chrome(layout::TEXT_SM)))
        .font_weight(FontWeight::MEDIUM)
        .label(label.into())
        .checked(checked)
        .on_click(cx.listener(move |workspace, on: &bool, _, cx| {
            let on = *on;
            workspace.edit_schema_export(|dialog| edit(dialog, on), cx);
        }))
}

/// Each of `tables`' foreign keys, on a session of its own, as the export
/// reads them again.
fn read_keys(
    connection: &Connection,
    schema: &str,
    tables: &[String],
) -> Result<HashMap<String, Vec<ForeignKey>>, String> {
    let own = connection.apart().map_err(|error| error.to_string())?;
    tables
        .iter()
        .map(|name| {
            let structure = own
                .structure(schema, name)
                .map_err(|error| format!("{name}: {error}"))?;
            Ok((name.clone(), structure.foreign_keys))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(table: &str) -> ForeignKey {
        ForeignKey {
            column: "ref_id".into(),
            referenced_schema: "s".into(),
            referenced_table: table.into(),
            referenced_column: "id".into(),
        }
    }

    fn dialog(keys: &[(&str, Vec<ForeignKey>)]) -> SchemaExportDialog {
        let mut dialog = SchemaExportDialog::new(
            "s".into(),
            keys.iter().map(|(name, _)| name.to_string()).collect(),
            1,
        );
        dialog.landed(
            1,
            Ok(keys
                .iter()
                .map(|(name, keys)| (name.to_string(), keys.clone()))
                .collect()),
        );
        dialog
    }

    fn tick(dialog: &mut SchemaExportDialog, table: &str, ticked: bool) {
        for (name, on) in &mut dialog.tables {
            if name == table {
                *on = ticked;
            }
        }
    }

    #[test]
    fn nothing_ticked_exports_nothing() {
        let mut dialog = dialog(&[("a", vec![]), ("b", vec![])]);
        assert_eq!(
            dialog.request(),
            Ok(SchemaRequest {
                tables: vec!["a".into(), "b".into()],
                structure: true,
                rows: true,
            })
        );
        dialog.structure = false;
        dialog.rows = false;
        assert_eq!(dialog.request(), Err("Tick Structure, Rows or both."));
        dialog.rows = true;
        tick(&mut dialog, "a", false);
        tick(&mut dialog, "b", false);
        assert_eq!(dialog.request(), Err("Tick at least one table."));
    }

    #[test]
    fn the_dialog_names_tables_a_ticked_one_points_at() {
        let mut dialog = dialog(&[("customers", vec![]), ("orders", vec![key("customers")])]);
        assert_eq!(dialog.warnings(Engine::Postgres), Vec::<String>::new());
        tick(&mut dialog, "customers", false);
        assert_eq!(
            dialog.warnings(Engine::Postgres),
            vec!["Referenced but not exported: customers.".to_string()]
        );
    }

    #[test]
    fn a_rows_only_cycle_is_warned_where_order_matters() {
        let mut dialog = dialog(&[("a", vec![key("b")]), ("b", vec![key("a")])]);
        let cycle = "These tables reference each other, so their rows may not run back in this order: a, b.";
        // With the structure, the keys come after the rows.
        assert_eq!(dialog.warnings(Engine::Postgres), Vec::<String>::new());
        dialog.structure = false;
        assert_eq!(dialog.warnings(Engine::Postgres), vec![cycle.to_string()]);
        assert_eq!(dialog.warnings(Engine::MySql), Vec::<String>::new());
    }

    #[test]
    fn keys_landing_for_a_closed_dialog_are_dropped() {
        let mut dialog = SchemaExportDialog::new("s".into(), vec!["a".into()], 2);
        dialog.landed(1, Ok(HashMap::new()));
        assert!(matches!(dialog.keys, Loading::Reading));
        dialog.landed(2, Err("gone".into()));
        assert!(matches!(&dialog.keys, Loading::Failed(error) if error == "gone"));
    }
}
