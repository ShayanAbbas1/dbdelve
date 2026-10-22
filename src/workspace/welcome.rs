//! The welcome surface: what the window is while there is no connection. A
//! field to paste a connection URL into, the engines to start from, and the
//! few things worth doing before connecting anywhere.

use std::path::Path;

use super::*;
use crate::import::Source;
use crate::scroller::{SmoothScrollable, smooth_scoped};
use crate::ui::{chord_for, keycaps, raised_card, search_field};

use super::forms::{name_field, welcome_row};

/// What the URL field holds, read the way `ConnectionConfig::from_url` will.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recognised {
    Empty,
    /// The start of a scheme or a path, too short to say anything about yet.
    Typing,
    /// A URL whose scheme names this engine.
    Url(Engine),
    /// A path to a SQLite file, which has no scheme to read.
    File,
    Unknown,
}

impl Recognised {
    pub(crate) fn engine(self) -> Option<Engine> {
        match self {
            Recognised::Url(engine) => Some(engine),
            Recognised::File => Some(Engine::Sqlite),
            _ => None,
        }
    }
}

const SQLITE_EXTENSIONS: [&str; 4] = ["db", "sqlite", "sqlite3", "db3"];

pub(crate) fn recognise(text: &str) -> Recognised {
    let text = text.trim();
    if text.is_empty() {
        return Recognised::Empty;
    }
    let path_like = text.starts_with('~') || Path::new(text).is_absolute();
    if path_like {
        let sqlite = Path::new(text)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                SQLITE_EXTENSIONS
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(extension))
            });
        return if sqlite {
            Recognised::File
        } else {
            Recognised::Typing
        };
    }
    // The scheme the way `from_url` finds it, so the field and the form never
    // disagree about what was pasted.
    let scheme = text
        .split_once("://")
        .or_else(|| text.split_once(':'))
        .map(|(scheme, _)| scheme)
        .filter(|scheme| !scheme.is_empty());
    match scheme {
        Some(scheme) => Engine::parse(scheme).map_or(Recognised::Unknown, Recognised::Url),
        None => {
            let typed = text.to_ascii_lowercase();
            if Engine::SCHEMES
                .iter()
                .any(|(spelling, _)| spelling.starts_with(&typed))
            {
                Recognised::Typing
            } else {
                Recognised::Unknown
            }
        }
    }
}

/// One of the quiet rows under the engines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    NewProject,
    Import(Source),
    Theme,
    Settings,
}

impl Row {
    fn label(self) -> String {
        match self {
            Row::NewProject => "New project".into(),
            Row::Import(source) => format!("Import from {}", source.label()),
            Row::Theme => "Theme".into(),
            Row::Settings => "Settings".into(),
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Row::NewProject => icon::ADD_TO_PROJECT,
            Row::Import(_) => icon::IMPORT,
            Row::Theme => icon::THEME,
            Row::Settings => icon::SETTINGS,
        }
    }

    /// The keybinding whose chord the row wears.
    fn action(self) -> &'static str {
        match self {
            Row::NewProject => "new_project",
            Row::Import(Source::DBeaver) => "import_dbeaver",
            Row::Import(Source::TablePlus) => "import_tableplus",
            Row::Theme => "cycle_theme",
            Row::Settings => "open_settings",
        }
    }
}

/// An import is offered only for a client that is installed here.
pub(crate) fn rows(importable: &[Source]) -> Vec<Row> {
    std::iter::once(Row::NewProject)
        .chain(importable.iter().copied().map(Row::Import))
        .chain([Row::Theme, Row::Settings])
        .collect()
}

impl Workspace {
    pub(crate) fn render_welcome(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = *theme(cx);
        let card_theme = popup_theme(cx);
        let viewport = window.viewport_size().width.as_f32();
        let width = layout::WELCOME_WIDTH
            .min(viewport - 2. * layout::SPACE_LG)
            .max(0.);
        let top = if self.projects.is_empty() {
            layout::WELCOME_TOP
        } else {
            layout::WELCOME_TOP_WITH_PROJECTS
        };

        let projects = self.render_welcome_projects(card_theme, cx);
        div()
            .id("welcome-scroll")
            .size_full()
            .overflow_y_scroll()
            .smooth_scroll(&smooth_scoped("welcome-scroll", "welcome", cx))
            .pt(px(top))
            .px(px(layout::SPACE_LG))
            .pb(px(layout::SPACE_LG))
            .flex()
            .flex_col()
            .items_center()
            .child(
                gpui::img(crate::icons::BRAND)
                    .flex_shrink_0()
                    .size(px(layout::WELCOME_MARK_SIZE)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .mt(px(layout::WELCOME_MARK_GAP))
                    .mb(px(layout::WELCOME_TITLE_GAP))
                    .text_size(px(layout::chrome(layout::WELCOME_TITLE_SIZE)))
                    .line_height(px(layout::chrome(layout::WELCOME_TITLE_LINE)))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.text)
                    .child("Connect a database"),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(width))
                    .flex()
                    .flex_col()
                    .text_size(px(layout::chrome(layout::TEXT_MD)))
                    .text_color(card_theme.text)
                    .border_1()
                    .border_color(card_theme.border_strong)
                    .rounded(px(layout::RADIUS_CARD))
                    .map(|card| raised_card(card, card_theme))
                    .overflow_hidden()
                    .child(self.render_welcome_field(card_theme, cx))
                    .child(self.render_engine_grid(width, card_theme, cx))
                    .children(projects)
                    .child(self.render_welcome_rows(card_theme, cx)),
            )
            .children(self.welcome_notice.clone().map(|message| {
                div()
                    .flex_shrink_0()
                    .max_w(px(width))
                    .mt(px(layout::SPACE_MD))
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .text_color(t.text_muted)
                    .child(message)
            }))
    }

    fn render_welcome_field(&self, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let recognised = recognise(&self.welcome_url.read(cx).value());
        let size = px(layout::chrome(layout::WELCOME_FIELD_ICON));
        let leading = match recognised.engine() {
            Some(engine) => icon(engine.logo())
                .size(size)
                .text_color(theme::engine_brand(engine, t, &t.logo_planes()))
                .into_any_element(),
            None => icon(icon::LINK)
                .size(size)
                .text_color(t.text_faint)
                .into_any_element(),
        };
        let hint = |text: &'static str| {
            div()
                .flex_shrink_0()
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .text_color(t.text_faint)
                .child(text)
        };
        let trailing = match recognised.engine() {
            Some(engine) => Some(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .child(hint(engine.label()))
                    .child(keycaps("enter", t))
                    .into_any_element(),
            ),
            None if recognised == Recognised::Unknown => Some(hint("Not a URL").into_any_element()),
            None => None,
        };
        search_field(leading, &self.welcome_url, t).children(trailing)
    }

    fn render_engine_grid(&self, width: f32, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let columns = if width < layout::WELCOME_NARROW {
            layout::WELCOME_GRID_COLUMNS - 1
        } else {
            layout::WELCOME_GRID_COLUMNS
        };
        let planes = t.logo_planes();
        div()
            .grid()
            .grid_cols(columns)
            .gap(px(layout::WELCOME_ITEM_GAP))
            .p(px(layout::WELCOME_SECTION_PADDING))
            .children(Engine::ALL.into_iter().enumerate().map(|(index, engine)| {
                let brand = theme::engine_brand(engine, t, &planes);
                item(("welcome-engine", index))
                    .child(
                        item_content(t)
                            .child(
                                item_tile().bg(brand.alpha(layout::ACCENT_WASH)).child(
                                    icon(engine.logo())
                                        .size(px(layout::chrome(layout::ICON_SIZE)))
                                        .text_color(brand),
                                ),
                            )
                            .child(item_label(engine.label())),
                    )
                    .on_click(cx.listener(move |workspace, _, window, cx| {
                        workspace.new_connection_on(engine, window, cx);
                    }))
            }))
    }

    /// The projects made before any connection: each empty, each a way into
    /// the form with itself already picked.
    fn render_welcome_projects(&self, t: Theme, cx: &mut Context<Self>) -> Option<gpui::Div> {
        if self.projects.is_empty() {
            return None;
        }
        let workspace = cx.entity().downgrade();
        let mut rows = Vec::new();
        for (index, project) in self.projects.iter().enumerate() {
            if self.renaming_project.as_deref() == Some(project.name.as_str())
                && let Some(input) = &self.project_name
            {
                rows.push(name_field(input));
                continue;
            }
            let add = {
                let workspace = workspace.clone();
                let name = project.name.clone();
                move |window: &mut Window, cx: &mut App| {
                    _ = workspace.update(cx, |workspace, cx| {
                        workspace.new_connection_in(Some(name.clone()), window, cx);
                    });
                }
            };
            let add_from_row = add.clone();
            rows.push(
                welcome_row(("welcome-project", index), t)
                    .group(format!("project-row-{index}"))
                    .child(row_icon(t, icon::PROJECT))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(project.name.clone()),
                    )
                    // Every project is empty here: with no connections at all
                    // there is nothing for one to hold.
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                            .text_color(t.text_faint)
                            .child("No connections yet"),
                    )
                    .child(self.project_actions(index, &project.name, t, cx))
                    .child(
                        button(
                            ("welcome-add-connection", index),
                            "Add connection",
                            Tone::Quiet,
                            Control::Compact,
                            t,
                        )
                        .on_click(move |_, window, cx| {
                            // The row would open the same form again.
                            cx.stop_propagation();
                            add(window, cx);
                        }),
                    )
                    .on_click(move |_, window, cx| add_from_row(window, cx))
                    .into_any_element(),
            );
        }
        Some(
            div()
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(t.border)
                .p(px(layout::WELCOME_SECTION_PADDING))
                .child(
                    div()
                        .px(px(layout::SPACE_SM))
                        .py(px(layout::SPACE_XS))
                        .child(section_label(t, "Projects")),
                )
                .children(rows),
        )
    }

    fn render_welcome_rows(&self, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let overrides = &self.settings.custom_keybindings;
        let naming = self.project_name.is_some() && self.renaming_project.is_none();
        div()
            .flex()
            .flex_col()
            .gap(px(layout::WELCOME_ITEM_GAP))
            .border_t_1()
            .border_color(t.border)
            .p(px(layout::WELCOME_SECTION_PADDING))
            .children(
                rows(&self.importable)
                    .into_iter()
                    .enumerate()
                    .map(|(index, row)| {
                        if row == Row::NewProject && naming {
                            return self.render_project_naming(t, cx).into_any_element();
                        }
                        let detail = (row == Row::Theme).then(|| {
                            div()
                                .flex_shrink_0()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_faint)
                                .child(theme(cx).name)
                        });
                        item(("welcome-row", index))
                            .when(matches!(row, Row::Import(_)), |item| {
                                item.disabled(self.importing)
                            })
                            .child(
                                item_content(t)
                                    .child(
                                        item_tile().bg(t.element_active).child(
                                            icon(row.icon())
                                                .size(px(layout::chrome(layout::ICON_SIZE)))
                                                .text_color(t.text_muted),
                                        ),
                                    )
                                    .child(item_label(row.label()))
                                    .child(div().flex_1())
                                    .children(detail)
                                    .children(
                                        chord_for(row.action(), overrides)
                                            .map(|chord| keycaps(&chord, t)),
                                    ),
                            )
                            .on_click(cx.listener(move |workspace, _, window, cx| match row {
                                Row::NewProject => workspace.new_project(&NewProject, window, cx),
                                Row::Import(source) => {
                                    workspace.import_connections(source, window, cx)
                                }
                                Row::Theme => workspace.select_theme(&SelectTheme, window, cx),
                                Row::Settings => workspace.open_settings(&OpenSettings, window, cx),
                            }))
                            .into_any_element()
                    }),
            )
    }

    /// The New project row while a name is being typed into it: the field in
    /// the row's place, so it opens where it was asked for.
    fn render_project_naming(&self, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let input = self
            .project_name
            .clone()
            .expect("naming is drawn only while the field is open");
        let typed = input.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(layout::SPACE_XS))
            .p(px(layout::SPACE_XS))
            .child(
                div()
                    .flex()
                    .gap(px(layout::SPACE_SM))
                    .child(div().flex_1().min_w_0().child(Input::new(&input).w_full()))
                    .child(
                        button(
                            "create-project",
                            "Create",
                            Tone::Primary,
                            Control::Standard,
                            t,
                        )
                        .on_click(cx.listener(
                            move |workspace, _, _, cx| {
                                let name = typed.read(cx).value().trim().to_string();
                                workspace.create_project(name, cx);
                            },
                        )),
                    )
                    .child(
                        icon_button(
                            "cancel-new-project",
                            icon::CLOSE,
                            Tone::Quiet,
                            Control::Standard,
                            t,
                        )
                        .tooltip("Cancel")
                        .on_click(cx.listener(|workspace, _, _, cx| {
                            workspace.drop_project_name();
                            cx.notify();
                        })),
                    ),
            )
            .children(self.project_name_error.clone().map(|message| {
                div()
                    .px(px(layout::SPACE_XS))
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .text_color(t.danger)
                    .child(message)
            }))
    }

    /// A fresh form for `engine`, its first field focused: `set_engine` moves
    /// focus whenever the field set changes from the form's default.
    pub(crate) fn new_connection_on(
        &mut self,
        engine: Engine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_connection_in(None, window, cx);
        if let Some(form) = &mut self.form {
            form.set_engine(engine);
        }
    }

    /// Enter in the URL field. Opens the form filled from what was pasted
    /// rather than connecting: credentials, a tunnel and the mode still
    /// deserve a look before anything is sent.
    pub(crate) fn submit_welcome_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.welcome_url.read(cx).value().trim().to_string();
        let recognised = recognise(&text);
        let Some(engine) = recognised.engine() else {
            return;
        };
        self.new_connection_on(engine, window, cx);
        let Some(form) = &self.form else {
            return;
        };
        if recognised == Recognised::File {
            let name = default_profile_name(&ConnectionConfig::Sqlite {
                path: text.clone(),
                statement_timeout: 0,
            });
            let (path, name_input) = (form.path.clone(), form.name.clone());
            path.update(cx, |input, cx| input.set_value(text, window, cx));
            name_input.update(cx, |input, cx| input.set_value(name, window, cx));
        } else if engine.fields() != Fields::Account {
            let url = form.url.clone();
            url.update(cx, |input, cx| input.set_value(text, window, cx));
            self.fill_from_url(window, cx);
        }
        self.welcome_url
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }
}

/// An engine tile or a row: a quiet control laid out the way a palette row is.
fn item(id: impl Into<gpui::ElementId>) -> gpui_component::button::Button {
    ui::control(id, Tone::Quiet, Control::Standard)
        .h(px(layout::chrome(layout::PALETTE_ROW_HEIGHT)))
        .min_w_0()
        .px(px(layout::WELCOME_ITEM_INSET))
        .rounded(px(layout::WELCOME_ITEM_RADIUS))
}

/// The button centres what it holds, and a row reads from its left edge.
fn item_content(t: Theme) -> gpui::Div {
    div()
        .size_full()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(layout::WELCOME_ITEM_INSET))
        .text_size(px(layout::chrome(layout::TEXT_MD)))
        .font_weight(FontWeight::NORMAL)
        .text_color(t.text)
}

fn item_tile() -> gpui::Div {
    let side = layout::chrome(layout::PALETTE_TILE_SIZE);
    div()
        .flex_shrink_0()
        .size(px(side))
        .rounded(px(layout::RADIUS_CONTROL))
        .flex()
        .items_center()
        .justify_center()
}

fn item_label(label: impl Into<gpui::SharedString>) -> gpui::Div {
    div()
        .min_w_0()
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .child(label.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_is_recognised_by_the_scheme_the_form_reads() {
        let cases = [
            ("postgresql://ana@db.internal:5432/app", Engine::Postgres),
            ("postgres://db/app", Engine::Postgres),
            ("  MySQL://root@localhost/shop  ", Engine::MySql),
            ("mariadb://db/app", Engine::MariaDb),
            ("sqlite:///tmp/app.db", Engine::Sqlite),
            ("sqlite:app.db", Engine::Sqlite),
            ("file:app.db", Engine::Sqlite),
            ("turso://db.example", Engine::Turso),
            ("mssql://sa@db/master", Engine::SqlServer),
            ("sqlserver://sa@db/master", Engine::SqlServer),
            ("mongodb+srv://cluster0.example.net/app", Engine::MongoDb),
            ("rediss://cache:6380", Engine::Redis),
            ("snowflake://account", Engine::Snowflake),
        ];
        for (text, engine) in cases {
            assert_eq!(recognise(text), Recognised::Url(engine), "{text}");
        }
    }

    #[test]
    fn a_bare_sqlite_path_is_a_file_and_any_other_path_is_still_being_typed() {
        for text in [
            "/Users/ana/app.db",
            "~/data/app.sqlite",
            "~/data/app.SQLITE3",
            "/var/lib/app.db3",
        ] {
            assert_eq!(recognise(text), Recognised::File, "{text}");
        }
        for text in ["/Users/ana/ap", "~/data/app.csv", "~"] {
            assert_eq!(recognise(text), Recognised::Typing, "{text}");
        }
        // Relative paths are not offered: the form could not say where from.
        assert_eq!(recognise("app.db"), Recognised::Unknown);
    }

    #[test]
    fn the_start_of_a_scheme_says_nothing_until_it_cannot_be_one() {
        assert_eq!(recognise(""), Recognised::Empty);
        assert_eq!(recognise("   "), Recognised::Empty);
        for text in ["p", "post", "POSTGRESQL", "mongodb+", "redi"] {
            assert_eq!(recognise(text), Recognised::Typing, "{text}");
        }
        for text in ["localhost:5432", "http://example.com", "hello world", "xyz"] {
            assert_eq!(recognise(text), Recognised::Unknown, "{text}");
        }
    }

    #[test]
    fn a_recognised_field_names_the_engine_it_will_open() {
        assert_eq!(recognise("redis://cache").engine(), Some(Engine::Redis));
        assert_eq!(recognise("~/app.db").engine(), Some(Engine::Sqlite));
        assert_eq!(recognise("post").engine(), None);
        assert_eq!(recognise("nope://x").engine(), None);
    }

    #[test]
    fn an_import_row_is_offered_only_for_a_client_that_is_installed() {
        assert_eq!(rows(&[]), [Row::NewProject, Row::Theme, Row::Settings]);
        assert_eq!(
            rows(&[Source::TablePlus]),
            [
                Row::NewProject,
                Row::Import(Source::TablePlus),
                Row::Theme,
                Row::Settings
            ]
        );
        assert_eq!(
            Row::Import(Source::DBeaver).label(),
            "Import from DBeaver".to_string()
        );
    }

    #[test]
    fn every_row_wears_the_chord_of_an_action_that_exists() {
        let rows = [
            Row::NewProject,
            Row::Import(Source::DBeaver),
            Row::Import(Source::TablePlus),
            Row::Theme,
            Row::Settings,
        ];
        for row in rows {
            assert!(
                keybindings::REGISTRY
                    .iter()
                    .any(|spec| spec.id == row.action()),
                "{row:?}"
            );
        }
        let overrides = std::collections::HashMap::new();
        assert_eq!(
            chord_for(Row::Settings.action(), &overrides).as_deref(),
            Some("secondary-,")
        );
        assert_eq!(chord_for(Row::NewProject.action(), &overrides), None);
    }
}
