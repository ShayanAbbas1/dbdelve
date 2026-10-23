//! The welcome surface: what the window is while there is no connection. A
//! field to paste a connection URL into, the engines to start from, and the
//! few things worth doing before connecting anywhere.

use std::path::Path;
use std::time::{Duration, Instant};

use gpui::{Animation, AnimationExt, Transformation, ease_in_out, ease_out_quint, size, svg};

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
    /// A path to a file of this engine's, which has no scheme to read: told
    /// apart by its extension.
    File(Engine),
    Unknown,
}

impl Recognised {
    pub(crate) fn engine(self) -> Option<Engine> {
        match self {
            Recognised::Url(engine) => Some(engine),
            Recognised::File(engine) => Some(engine),
            _ => None,
        }
    }
}

const FILE_EXTENSIONS: [(&str, Engine); 6] = [
    ("db", Engine::Sqlite),
    ("sqlite", Engine::Sqlite),
    ("sqlite3", Engine::Sqlite),
    ("db3", Engine::Sqlite),
    ("duckdb", Engine::DuckDb),
    ("ddb", Engine::DuckDb),
];

pub(crate) fn recognise(text: &str) -> Recognised {
    let text = text.trim();
    if text.is_empty() {
        return Recognised::Empty;
    }
    let path_like = text.starts_with('~') || Path::new(text).is_absolute();
    if path_like {
        let engine = Path::new(text)
            .extension()
            .and_then(|extension| extension.to_str())
            .and_then(|extension| {
                FILE_EXTENSIONS
                    .iter()
                    .find(|(known, _)| known.eq_ignore_ascii_case(extension))
            });
        return match engine {
            Some((_, engine)) => Recognised::File(*engine),
            None => Recognised::Typing,
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

/// Whether the first-run greeting has played. A file from before there was
/// one, holding connections, belongs to someone already past their first run.
pub(crate) fn welcomed(stored: Option<bool>, has_profiles: bool) -> bool {
    stored.unwrap_or(has_profiles)
}

/// The surface's entrance, read off one wall clock rather than an animation
/// per element: a dozen pieces step in on one timeline, and a single clock
/// keeps them in step and says exactly when the last has landed, which is when
/// the surface stops asking for frames.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entrance {
    started: Instant,
    /// The first run's, with its greeting ahead of the title.
    intro: bool,
}

/// Where one piece of the entrance is: how much of it shows, and how far
/// below its place it still sits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pose {
    pub(crate) opacity: f32,
    pub(crate) offset: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Part {
    Mark,
    Greeting,
    Title,
    Card,
    Tile(usize),
    Backdrop,
    /// The connection form's card, which rises in on the same curve as the
    /// welcome card, with nothing ahead of it.
    Form,
}

const FIELD_LOGO_MS: u64 = 160;
const RISE_MS: f32 = 340.;
const RISE_PX: f32 = 8.;
const SETTLE_MS: f32 = 260.;
const SETTLE_PX: f32 = 4.;
const TILES_AFTER_CARD_MS: f32 = 140.;
const TILE_STAGGER_MS: f32 = 28.;
const GREETING_AT_MS: f32 = 80.;
const GREETING_MS: f32 = 2000.;
const GREETING_PX: f32 = 6.;
/// The greeting is in by this share of its run, and starts out at the next.
const GREETING_IN: f32 = 0.14;
const GREETING_OUT: f32 = 0.72;
const BACKDROP_AT_MS: f32 = 250.;
const BACKDROP_MS: f32 = 900.;

fn title_at(intro: bool) -> f32 {
    if intro { 1480. } else { 60. }
}

fn card_at(intro: bool) -> f32 {
    if intro { 1500. } else { 120. }
}

fn tile_at(intro: bool, index: usize) -> f32 {
    card_at(intro) + TILES_AFTER_CARD_MS + index as f32 * TILE_STAGGER_MS
}

fn progress(elapsed: f32, at: f32, duration: f32) -> f32 {
    ((elapsed - at) / duration).clamp(0., 1.)
}

fn rise(elapsed: f32, at: f32, duration: f32, distance: f32) -> Pose {
    let eased = ease_out_quint()(progress(elapsed, at, duration));
    Pose {
        opacity: eased,
        offset: distance * (1. - eased),
    }
}

/// In, held, then out upwards, making room for the title in the same place.
fn greeting(elapsed: f32) -> Pose {
    let run = progress(elapsed, GREETING_AT_MS, GREETING_MS);
    let ease = ease_out_quint();
    if run <= GREETING_IN {
        let eased = ease(run / GREETING_IN);
        Pose {
            opacity: eased,
            offset: GREETING_PX * (1. - eased),
        }
    } else if run <= GREETING_OUT {
        Pose {
            opacity: 1.,
            offset: 0.,
        }
    } else {
        let eased = ease((run - GREETING_OUT) / (1. - GREETING_OUT));
        Pose {
            opacity: 1. - eased,
            offset: -GREETING_PX * eased,
        }
    }
}

pub(crate) fn pose(part: Part, intro: bool, elapsed: f32) -> Pose {
    match part {
        Part::Mark => rise(elapsed, 0., RISE_MS, RISE_PX),
        Part::Greeting if intro => greeting(elapsed),
        Part::Greeting => Pose {
            opacity: 0.,
            offset: 0.,
        },
        Part::Title => rise(elapsed, title_at(intro), RISE_MS, RISE_PX),
        Part::Card => rise(elapsed, card_at(intro), RISE_MS, RISE_PX),
        Part::Tile(index) => rise(elapsed, tile_at(intro, index), SETTLE_MS, SETTLE_PX),
        Part::Backdrop => Pose {
            opacity: ease_in_out(progress(elapsed, BACKDROP_AT_MS, BACKDROP_MS)),
            offset: 0.,
        },
        Part::Form => rise(elapsed, 0., RISE_MS, RISE_PX),
    }
}

/// When the connection form's entrance has landed: its card, and the
/// contours behind it unless they were already up.
pub(crate) fn form_entrance_ms(backdrop_shown: bool) -> f32 {
    if backdrop_shown {
        RISE_MS
    } else {
        RISE_MS.max(BACKDROP_AT_MS + BACKDROP_MS)
    }
}

/// When the last piece has landed.
pub(crate) fn entrance_ms(intro: bool) -> f32 {
    let tiles = tile_at(intro, Engine::ALL.len() - 1) + SETTLE_MS;
    let greeting = if intro {
        GREETING_AT_MS + GREETING_MS
    } else {
        0.
    };
    tiles
        .max(greeting)
        .max(card_at(intro) + RISE_MS)
        .max(BACKDROP_AT_MS + BACKDROP_MS)
}

/// A piece of the entrance where it is now. The caller positions it, relative
/// or absolute, so the offset moves it without moving anything beside it.
pub(super) fn posed(element: gpui::Div, pose: Pose) -> gpui::Div {
    element.top(px(pose.offset)).opacity(pose.opacity)
}

impl Workspace {
    /// Starts the entrance the first frame the surface is shown, and only
    /// then: a re-render mid-way must not start it over.
    pub(crate) fn begin_welcome(&mut self, cx: &mut Context<Self>) {
        if self.welcome_entrance.is_some() {
            return;
        }
        let intro = !self.settings.welcomed;
        self.welcome_entrance = Some(Entrance {
            started: Instant::now(),
            intro,
        });
        if intro {
            self.settings.welcomed = true;
            self.remember_profiles(cx);
        }
    }

    pub(crate) fn render_welcome(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = *theme(cx);
        let card_theme = popup_theme(cx);
        let viewport = window.viewport_size();
        let width = layout::WELCOME_WIDTH
            .min(viewport.width.as_f32() - 2. * layout::SPACE_LG)
            .max(0.);
        let top = if self.projects.is_empty() {
            layout::WELCOME_TOP
        } else {
            layout::WELCOME_TOP_WITH_PROJECTS
        };
        let (intro, elapsed) = match self.welcome_entrance {
            Some(entrance) if !cx.reduce_motion() => (
                entrance.intro,
                entrance.started.elapsed().as_secs_f32() * 1000.,
            ),
            _ => (false, f32::INFINITY),
        };
        if elapsed < entrance_ms(intro) {
            window.request_animation_frame();
        }
        let at = |part| pose(part, intro, elapsed);
        let heading = || {
            div()
                .text_size(px(layout::chrome(layout::WELCOME_TITLE_SIZE)))
                .line_height(px(layout::chrome(layout::WELCOME_TITLE_LINE)))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.text)
                .whitespace_nowrap()
        };

        let projects = self.render_welcome_projects(card_theme, cx);
        let surface = div()
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
                posed(div().relative().flex_shrink_0(), at(Part::Mark))
                    .child(gpui::img(crate::icons::BRAND).size(px(layout::WELCOME_MARK_SIZE))),
            )
            // The greeting and the title share one place, the one handing it
            // to the other.
            .child(
                div()
                    .relative()
                    .flex_shrink_0()
                    .w(px(width))
                    .mt(px(layout::WELCOME_MARK_GAP))
                    .mb(px(layout::WELCOME_TITLE_GAP))
                    .flex()
                    .justify_center()
                    .child(posed(heading().relative(), at(Part::Title)).child("Connect a database"))
                    .when(intro, |place| {
                        place.child(
                            posed(
                                heading()
                                    .absolute()
                                    .left_0()
                                    .w_full()
                                    .flex()
                                    .justify_center(),
                                at(Part::Greeting),
                            )
                            .child("Welcome to DBDelve"),
                        )
                    }),
            )
            .child(
                posed(div().relative(), at(Part::Card))
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
                    .child(self.render_engine_grid(width, card_theme, &at, cx))
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
            }));

        div()
            .relative()
            .size_full()
            .children(backdrop(t, viewport, at(Part::Backdrop)))
            .child(surface)
    }

    fn render_welcome_field(&self, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let recognised = recognise(&self.welcome_url.read(cx).value());
        let size = px(layout::chrome(layout::WELCOME_FIELD_ICON));
        let leading = match recognised.engine() {
            // Keyed by the engine, so a logo fades in each time the field
            // comes to mean a different one, and holds while it still does.
            Some(engine) => div()
                .flex_shrink_0()
                .child(
                    icon(engine.logo())
                        .size(size)
                        .text_color(theme::engine_brand(engine, t, &t.logo_planes())),
                )
                .with_animation(
                    gpui::SharedString::from(format!("welcome-field-{}", engine.as_str())),
                    Animation::new(Duration::from_millis(FIELD_LOGO_MS)).with_easing(ease_in_out),
                    |logo, delta| logo.opacity(delta),
                )
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

    fn render_engine_grid(
        &self,
        width: f32,
        t: Theme,
        at: &dyn Fn(Part) -> Pose,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                let tile = item(("welcome-engine", index))
                    .w_full()
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
                    }));
                posed(div().relative().min_w_0(), at(Part::Tile(index))).child(tile)
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
        if let Recognised::File(_) = recognised {
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

/// The contours and the light behind the card, on opaque themes only: a
/// glass theme already has the desktop behind it. Sized to cover the surface
/// at the asset's own proportions, and in steps, so a resize redraws the
/// bitmap a few times rather than on every pixel.
pub(super) fn backdrop(
    t: Theme,
    viewport: gpui::Size<gpui::Pixels>,
    pose: Pose,
) -> Option<gpui::Div> {
    if t.is_glass {
        return None;
    }
    let aspect = layout::BACKDROP_ASPECT;
    let tall = (viewport.height.as_f32() - layout::TITLEBAR_HEIGHT) * aspect;
    let wide = viewport.width.as_f32().max(tall);
    let width = (wide / layout::BACKDROP_STEP).ceil() * layout::BACKDROP_STEP;
    let height = width / aspect;
    let shrink = layout::LIFT_DOWNSCALE;
    Some(
        div()
            .absolute()
            .inset_0()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .opacity(pose.opacity)
            .child(
                div()
                    .relative()
                    .flex_shrink_0()
                    .w(px(width))
                    .h(px(height))
                    .child(
                        svg()
                            .path(crate::icons::LIFT)
                            .absolute()
                            .left(px((width - width / shrink) / 2.))
                            .top(px((height - height / shrink) / 2.))
                            .w(px(width / shrink))
                            .h(px(height / shrink))
                            .text_color(t.lift())
                            .with_transformation(Transformation::scale(size(shrink, shrink))),
                    )
                    .child(
                        svg()
                            .path(crate::icons::CONTOURS)
                            .absolute()
                            .inset_0()
                            .size_full()
                            .text_color(t.contour()),
                    ),
            ),
    )
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
    fn a_bare_database_file_path_is_a_file_and_any_other_path_is_still_being_typed() {
        // What counts as absolute is the platform's call: `/Users/…` has no
        // drive, so it is not an absolute path on Windows.
        #[cfg(windows)]
        let (absolute, other_absolute, partial) = (
            r"C:\Users\ana\app.db",
            r"D:\var\lib\app.db3",
            r"C:\Users\ana\ap",
        );
        #[cfg(not(windows))]
        let (absolute, other_absolute, partial) =
            ("/Users/ana/app.db", "/var/lib/app.db3", "/Users/ana/ap");
        for text in [
            absolute,
            "~/data/app.sqlite",
            "~/data/app.SQLITE3",
            other_absolute,
        ] {
            assert_eq!(recognise(text), Recognised::File(Engine::Sqlite), "{text}");
        }
        assert_eq!(
            recognise("~/data/app.duckdb"),
            Recognised::File(Engine::DuckDb)
        );
        for text in [partial, "~/data/app.csv", "~"] {
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
    fn the_greeting_plays_once_and_never_for_someone_already_connected() {
        assert!(!welcomed(None, false));
        assert!(welcomed(None, true));
        assert!(welcomed(Some(true), false));
        // Once it has played, deleting every connection does not replay it,
        // and a stored `false` is the file's word, not a guess.
        assert!(!welcomed(Some(false), true));
    }

    #[test]
    fn every_piece_starts_hidden_and_lands_by_the_end_of_the_entrance() {
        for intro in [true, false] {
            let end = entrance_ms(intro);
            let mut parts = vec![Part::Mark, Part::Title, Part::Card, Part::Backdrop];
            parts.extend((0..Engine::ALL.len()).map(Part::Tile));
            for part in parts {
                assert_eq!(pose(part, intro, 0.).opacity, 0., "{part:?}");
                assert_eq!(
                    pose(part, intro, end),
                    Pose {
                        opacity: 1.,
                        offset: 0.
                    },
                    "{part:?}"
                );
            }
            assert_eq!(pose(Part::Greeting, intro, end).opacity, 0.);
        }
    }

    #[test]
    fn the_greeting_hands_its_place_to_the_title_on_a_first_run_only() {
        let held = GREETING_AT_MS + GREETING_MS * (GREETING_IN + GREETING_OUT) / 2.;
        assert_eq!(pose(Part::Greeting, true, held).opacity, 1.);
        assert_eq!(pose(Part::Title, true, held).opacity, 0.);
        assert_eq!(pose(Part::Greeting, false, held).opacity, 0.);
        assert_eq!(pose(Part::Title, false, held).opacity, 1.);
        // On its way out it drifts up, not down.
        let leaving = GREETING_AT_MS + GREETING_MS * 0.9;
        assert!(pose(Part::Greeting, true, leaving).offset < 0.);
    }

    #[test]
    fn the_tiles_settle_one_after_another() {
        let first = tile_at(false, 0);
        let midway = first + SETTLE_MS / 2.;
        let early = pose(Part::Tile(0), false, midway);
        let late = pose(Part::Tile(5), false, midway);
        assert!(early.opacity > late.opacity);
        assert!(early.offset < late.offset);
    }

    #[test]
    fn the_form_rises_at_once_and_only_waits_on_contours_it_brought() {
        assert_eq!(pose(Part::Form, false, 0.).opacity, 0.);
        let landed = Pose {
            opacity: 1.,
            offset: 0.,
        };
        assert_eq!(pose(Part::Form, false, form_entrance_ms(true)), landed);
        assert_eq!(pose(Part::Backdrop, false, form_entrance_ms(false)), landed);
        assert!(form_entrance_ms(true) < form_entrance_ms(false));
    }

    #[test]
    fn a_later_visit_is_over_well_before_a_first_run() {
        assert_eq!(entrance_ms(false), BACKDROP_AT_MS + BACKDROP_MS);
        assert!(entrance_ms(true) > entrance_ms(false) + RISE_MS);
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
