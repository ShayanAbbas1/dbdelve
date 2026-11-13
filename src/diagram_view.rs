//! The ER diagram on screen: one `canvas()` that paints what `diagram` laid
//! out, and the pointer that pans, zooms and drags it. Cards are quads, edges
//! are tessellated paths and text is shaped by hand, because a thousand cards
//! as divs would be a thousand layouts a frame (design §4.1).
//!
//! Nothing here runs without input: no animation, no timer. A frame is drawn
//! when a gesture or a load notifies the view, and the view is mounted cached,
//! so a redraw elsewhere in the window reuses its last frame.

use std::collections::{HashMap, HashSet};

use gpui::{
    BorderStyle, Bounds, BoxShadow, ContentMask, Context, DispatchPhase, FocusHandle, Font,
    FontWeight, Hitbox, HitboxBehavior, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Path, PathBuilder, PinchEvent,
    Pixels, Render, ScrollWheelEvent, ShapedLine, SharedString, Styled, TextAlign, TextRun,
    TransformationMatrix, Window, canvas, div, point, px, quad, size,
};

use crate::db::{KeyTarget, SchemaGraph};
use crate::diagram::geometry::{
    self, Detail, HEADER_HEIGHT, Point, Rect, Segment, Stroke, Transform,
};
use crate::diagram::index::Scene;
use crate::diagram::layout::{Layout, Subject};
use crate::icons::icon;
use crate::theme::{Theme, fonts, layout, popup_theme, theme};

/// Room left around the diagram when it is fitted to the view, in pixels.
const FIT_MARGIN: f32 = 32.0;
const EDGE_WIDTH: f32 = 1.25;
const LIT_EDGE_WIDTH: f32 = 1.75;
/// A card's inner padding, in world units.
const CARD_PAD: f32 = 10.0;
const ICON_GAP: f32 = 6.0;
/// What a wheel's or trackpad's vertical pixels do to the zoom when it zooms:
/// one notch of a mouse wheel is about a tenth.
const WHEEL_ZOOM: f32 = 1.0 / 200.0;
/// How far a press may travel and still be a click, in pixels.
const CLICK_SLOP: f32 = 3.0;
const CIRCLE_STEPS: usize = 16;
/// Dimmed ink keeps this much of its colour.
const DIM: f32 = 0.3;
/// The dot grid's spacing, in world units, doubled until the dots stand at
/// least [`DOT_MIN_GAP`] pixels apart, so zooming out never fills the screen
/// with dots.
const DOT_STEP: f32 = 32.0;
const DOT_MIN_GAP: f32 = 24.0;
const DOT_SIZE: f32 = 1.5;
/// Shaped lines kept before the cache starts again. Keys are text, size step
/// and ink, so a session of zooming over one schema stays well under it.
const LINE_CACHE_LIMIT: usize = 8192;

pub enum Load {
    Loading,
    Failed(SharedString),
    Ready(Box<Ready>),
}

/// A read schema and its laid-out, indexed scene.
pub struct Ready {
    graph: SchemaGraph,
    scene: Scene,
    /// Each type label with the `?` a nullable column's type wears, made once
    /// so a frame never formats one.
    nullable_types: Vec<SharedString>,
}

impl Ready {
    pub fn new(graph: SchemaGraph, scene: Scene) -> Self {
        let nullable_types = graph
            .types
            .iter()
            .map(|label| SharedString::from(format!("{label}?")))
            .collect();
        Self {
            graph,
            scene,
            nullable_types,
        }
    }

    fn name(&self, card: u32) -> SharedString {
        match self.scene.layout().cards[card as usize].subject {
            Subject::Table(table) => self.graph.tables[table as usize].name.clone(),
            Subject::Stub(key) => match &self.graph.keys[key as usize].referenced {
                KeyTarget::Elsewhere { schema, table } => format!("{schema}.{table}").into(),
                KeyTarget::Table { table, .. } => self.graph.tables[*table as usize].name.clone(),
            },
        }
    }
}

/// A press in progress, and what it moves.
#[derive(Clone, Copy, Debug)]
enum Gesture {
    Pan {
        start: Point,
        last: Point,
    },
    Card {
        card: u32,
        /// Where on the card it was grabbed, in world units from its origin.
        grab: Point,
        start: Point,
        moved: bool,
    },
}

/// How an edge is drawn under the current focus and hover.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum EdgeLook {
    Plain,
    /// The focus's or the hovered card's own: the accent, a little heavier.
    Lit,
    Dimmed,
}

/// The colour roles text is shaped in. A shaped line bakes its colour in, so
/// the role is part of the cache key, and a dimmed role is a role of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Ink {
    Text,
    Muted,
}

/// The three faces a card's text is set in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Face {
    Title,
    Column,
    Type,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct LineKey {
    text: SharedString,
    face: Face,
    size: u32,
    ink: Ink,
    dimmed: bool,
}

/// What the shaped lines were shaped against: a change of theme or font
/// throws them all away, since each holds its colour and family.
#[derive(Clone, Debug, PartialEq)]
struct Stamp {
    theme: &'static str,
    opacity: u32,
    chrome: SharedString,
    grid: SharedString,
}

/// One tessellated part of an edge, and what it is filled with.
type Piece = (Path<Pixels>, Paint);

/// What a cached path is filled with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Paint {
    Ink,
    /// The canvas tone, so a mark's circle is hollow over the edge it sits on.
    Hole,
}

pub struct DiagramView {
    focus_handle: FocusHandle,
    load: Load,
    transform: Transform,
    /// Whether the diagram has been fitted to the view yet: once, when it is
    /// first laid out with a size to fit.
    fitted: bool,
    viewport: Bounds<Pixels>,
    /// The card in focus. Its neighbours stay sharp and the rest dim.
    focus: Option<u32>,
    /// The card under the pointer, whose edges are lit while nothing is in
    /// focus.
    hover: Option<u32>,
    gesture: Option<Gesture>,
    lines: HashMap<LineKey, ShapedLine>,
    stamp: Option<Stamp>,
    /// Edges tessellated at `paths_zoom`, with no pan: panning offsets a copy,
    /// which `paint_path` makes anyway, and only a zoom tessellates again.
    paths: HashMap<(u32, EdgeLook), Vec<Piece>>,
    paths_zoom: f32,
}

impl DiagramView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            load: Load::Loading,
            transform: Transform::default(),
            fitted: false,
            viewport: Bounds::default(),
            focus: None,
            hover: None,
            gesture: None,
            lines: HashMap::new(),
            stamp: None,
            paths: HashMap::new(),
            paths_zoom: 0.0,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.load, Load::Loading)
    }

    pub fn set_load(&mut self, load: Load, cx: &mut Context<Self>) {
        self.load = load;
        self.fitted = false;
        self.focus = None;
        self.hover = None;
        self.gesture = None;
        self.paths.clear();
        cx.notify();
    }

    fn ready(&self) -> Option<&Ready> {
        match &self.load {
            Load::Ready(ready) => Some(ready),
            _ => None,
        }
    }

    fn local(&self, position: gpui::Point<Pixels>) -> Point {
        Point::new(
            f32::from(position.x - self.viewport.origin.x),
            f32::from(position.y - self.viewport.origin.y),
        )
    }

    fn place(&mut self, bounds: Bounds<Pixels>) {
        self.viewport = bounds;
        if self.fitted || bounds.size.width <= px(0.0) || bounds.size.height <= px(0.0) {
            return;
        }
        let Some(ready) = self.ready() else {
            return;
        };
        let screen = Rect {
            x: 0.0,
            y: 0.0,
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        };
        self.transform = Transform::fitting(ready.scene.layout().bounds(), screen, FIT_MARGIN);
        self.fitted = true;
    }

    fn press(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let at = self.local(position);
        let zoom = self.transform.zoom;
        let world = self.transform.to_world(at);
        let card = self
            .ready()
            .and_then(|ready| ready.scene.card_at(world, zoom));
        self.gesture = Some(match card {
            Some(card) => {
                let origin = self.ready().map_or(Point::default(), |ready| {
                    ready.scene.layout().cards[card as usize].origin
                });
                Gesture::Card {
                    card,
                    grab: Point::new(world.x - origin.x, world.y - origin.y),
                    start: at,
                    moved: false,
                }
            }
            None => Gesture::Pan {
                start: at,
                last: at,
            },
        });
    }

    fn drag(&mut self, position: gpui::Point<Pixels>, hovered: bool, cx: &mut Context<Self>) {
        let at = self.local(position);
        match self.gesture {
            Some(Gesture::Pan { start, last }) => {
                self.transform = self
                    .transform
                    .panned(Point::new(at.x - last.x, at.y - last.y));
                self.gesture = Some(Gesture::Pan { start, last: at });
                cx.notify();
            }
            Some(Gesture::Card {
                card,
                grab,
                start,
                moved,
            }) => {
                if !moved && distance(start, at) < CLICK_SLOP {
                    return;
                }
                let world = self.transform.to_world(at);
                let origin = Point::new(world.x - grab.x, world.y - grab.y);
                if let Load::Ready(ready) = &mut self.load {
                    let edges = ready.scene.move_card(card, origin).to_vec();
                    self.paths.retain(|(edge, _), _| !edges.contains(edge));
                }
                self.gesture = Some(Gesture::Card {
                    card,
                    grab,
                    start,
                    moved: true,
                });
                cx.notify();
            }
            None => {
                let hover = match hovered {
                    true => {
                        let world = self.transform.to_world(at);
                        let zoom = self.transform.zoom;
                        self.ready()
                            .and_then(|ready| ready.scene.card_at(world, zoom))
                    }
                    false => None,
                };
                if hover != self.hover {
                    self.hover = hover;
                    if self.focus.is_none() {
                        cx.notify();
                    }
                }
            }
        }
    }

    fn release(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let at = self.local(position);
        match self.gesture.take() {
            Some(Gesture::Pan { start, .. }) if distance(start, at) < CLICK_SLOP => {
                self.set_focus(None, cx);
            }
            Some(Gesture::Card {
                card, moved: false, ..
            }) => self.set_focus(Some(card), cx),
            _ => {}
        }
    }

    fn set_focus(&mut self, focus: Option<u32>, cx: &mut Context<Self>) {
        if self.focus != focus {
            self.focus = focus;
            cx.notify();
        }
    }

    /// Whether a card is in focus, for `escape` to clear before it goes on to
    /// whatever else it closes.
    pub fn has_focus(&self) -> bool {
        self.focus.is_some()
    }

    pub fn clear_focus(&mut self, cx: &mut Context<Self>) {
        self.set_focus(None, cx);
    }

    /// Where a table's card is on screen, in window coordinates, once drawn.
    #[cfg(test)]
    pub fn table_on_screen(&self, table: u32) -> Option<gpui::Point<Pixels>> {
        let layout = self.ready()?.scene.layout();
        let card = layout
            .cards
            .iter()
            .position(|card| card.subject == Subject::Table(table))? as u32;
        let zoom = self.transform.zoom;
        let rect = layout.card_rect(card, layout.detail(zoom), zoom);
        let at = self.transform.to_screen(rect.centre());
        Some(point(
            self.viewport.origin.x + px(at.x),
            self.viewport.origin.y + px(at.y),
        ))
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, window: &Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(window.line_height());
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        self.transform = match event.modifiers.secondary() {
            true => self.transform.zoomed_at(
                self.local(event.position),
                self.transform.zoom * (dy * WHEEL_ZOOM).exp(),
            ),
            false => self.transform.panned(Point::new(dx, dy)),
        };
        cx.notify();
    }

    fn pinch(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        self.transform = self.transform.zoomed_at(
            self.local(event.position),
            self.transform.zoom * (1.0 + event.delta),
        );
        cx.notify();
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let t = *theme(cx);
        let inks = popup_theme(cx);
        let stamp = Stamp {
            theme: t.name,
            opacity: t.opacity.to_bits(),
            chrome: fonts(cx).chrome.clone(),
            grid: fonts(cx).grid.clone(),
        };
        if self.stamp.as_ref() != Some(&stamp) || self.lines.len() > LINE_CACHE_LIMIT {
            self.lines.clear();
            self.stamp = Some(stamp);
        }
        let transform = self.transform;
        let zoom = transform.zoom;
        if zoom != self.paths_zoom {
            self.paths.clear();
            self.paths_zoom = zoom;
        }
        let Self {
            load,
            lines,
            stamp,
            paths,
            focus,
            hover,
            ..
        } = self;
        let (focus, hover) = (*focus, *hover);
        let Load::Ready(ready) = &*load else {
            return;
        };
        let stamp = stamp.as_ref().expect("stamped above");
        let layout = ready.scene.layout();
        let detail = layout.detail(zoom);
        let screen = Rect {
            x: 0.0,
            y: 0.0,
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        };
        let visible = ready.scene.visible(transform.world_rect(screen), zoom);
        let origin = bounds.origin;
        let to_screen = |world: Point| {
            let at = transform.to_screen(world);
            point(origin.x + px(at.x), origin.y + px(at.y))
        };

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if !t.is_glass {
                paint_dots(bounds, transform, t, window);
            }

            let canvas_tone: Hsla = t.panel_glass().into();
            for &edge in &visible.edges {
                let look = edge_look(layout, focus, hover, edge);
                let colour: Hsla = match look {
                    EdgeLook::Plain => t.text_faint.into(),
                    EdgeLook::Lit => t.accent.into(),
                    EdgeLook::Dimmed => t.text_faint.alpha(DIM).into(),
                };
                let pan = point(
                    origin.x + px(transform.pan.x),
                    origin.y + px(transform.pan.y),
                );
                let pieces = paths
                    .entry((edge, look))
                    .or_insert_with(|| edge_paths(layout, edge, detail, zoom, look));
                for (path, paint) in pieces.iter() {
                    let fill = match paint {
                        Paint::Ink => colour,
                        Paint::Hole => canvas_tone,
                    };
                    window.paint_path(translated(path, pan), fill);
                }
            }

            let washed = joined_rows(ready, focus);
            for &card in &visible.cards {
                let dimmed = card_dimmed(layout, focus, card);
                let rect = layout.card_rect(card, detail, zoom);
                let top_left = to_screen(Point::new(rect.x, rect.y));
                let card_bounds = Bounds {
                    origin: top_left,
                    size: size(px(rect.width * zoom), px(rect.height * zoom)),
                };
                let radius = px(match detail {
                    Detail::Columns => 10.0,
                    Detail::Keys => 8.0,
                    Detail::Names => 6.0,
                });
                if !t.is_glass && detail != Detail::Names {
                    window.paint_drop_shadows(
                        card_bounds,
                        radius.into(),
                        &[BoxShadow {
                            color: t.scrim().into(),
                            offset: point(px(0.0), px(2.0)),
                            blur_radius: px(8.0),
                            spread_radius: px(-2.0),
                            inset: false,
                        }],
                    );
                }
                let focused = focus == Some(card);
                window.paint_quad(quad(
                    card_bounds,
                    radius,
                    Hsla::from(t.overlay_glass()),
                    px(match focused {
                        true => 2.0,
                        false => 1.0,
                    }),
                    match focused {
                        true => Hsla::from(t.accent),
                        false => Hsla::from(t.border),
                    },
                    BorderStyle::Solid,
                ));
                let header_height = match detail {
                    Detail::Names => rect.height * zoom,
                    _ => HEADER_HEIGHT * zoom,
                };
                let header = Bounds {
                    origin: top_left,
                    size: size(card_bounds.size.width, px(header_height)),
                };
                window.paint_quad(quad(
                    header,
                    gpui::Corners {
                        top_left: radius,
                        top_right: radius,
                        bottom_left: match detail {
                            Detail::Names => radius,
                            _ => px(0.0),
                        },
                        bottom_right: match detail {
                            Detail::Names => radius,
                            _ => px(0.0),
                        },
                    },
                    Hsla::from(t.element_hover),
                    px(0.0),
                    Hsla::from(t.border),
                    BorderStyle::Solid,
                ));

                let pad = CARD_PAD * zoom;
                let name_size = geometry::text_size(zoom, layout::chrome(layout::TEXT_MD));
                let title = shape(
                    lines,
                    stamp,
                    ready.name(card),
                    Face::Title,
                    name_size,
                    Ink::Text,
                    dimmed,
                    &inks,
                    window,
                );
                let inner = Bounds {
                    origin: point(header.origin.x + px(pad), header.origin.y),
                    size: size(header.size.width - px(2.0 * pad), header.size.height),
                };
                window.with_content_mask(Some(ContentMask { bounds: inner }), |window| {
                    let _ = title.paint(
                        inner.origin,
                        inner.size.height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                });

                let Subject::Table(table) = layout.cards[card as usize].subject else {
                    continue;
                };
                let columns = &ready.graph.tables[table as usize].columns;
                let type_size = geometry::text_size(zoom, layout::grid(layout::TEXT_SM));
                for row in layout.rows(card, detail) {
                    let column = &columns[row.column as usize];
                    let row_bounds = Bounds {
                        origin: to_screen(Point::new(row.rect.x, row.rect.y)),
                        size: size(px(row.rect.width * zoom), px(row.rect.height * zoom)),
                    };
                    if washed.contains(&(card, row.column)) {
                        window.paint_quad(quad(
                            row_bounds,
                            px(0.0),
                            Hsla::from(inks.accent.alpha(layout::ACCENT_WASH)),
                            px(0.0),
                            Hsla::from(t.border),
                            BorderStyle::Solid,
                        ));
                    }
                    let icon_size = (name_size * 0.9).min(row.rect.height * zoom * 0.7);
                    let icon_at = point(
                        row_bounds.origin.x + px(pad),
                        row_bounds.origin.y + (row_bounds.size.height - px(icon_size)) / 2.0,
                    );
                    let marker = match (column.primary, column.foreign) {
                        (true, _) => Some((icon::PRIMARY_KEY, inks.accent)),
                        (false, true) => Some((icon::LINK, inks.syntax_type)),
                        _ => None,
                    };
                    if let Some((path, colour)) = marker {
                        let colour: Hsla = match dimmed {
                            true => colour.alpha(DIM).into(),
                            false => colour.into(),
                        };
                        let _ = window.paint_svg(
                            Bounds {
                                origin: icon_at,
                                size: size(px(icon_size), px(icon_size)),
                            },
                            SharedString::from(path),
                            None,
                            TransformationMatrix::unit(),
                            colour,
                            cx,
                        );
                    }
                    let name_x = icon_at.x + px(icon_size + ICON_GAP * zoom);
                    let mut name_right = row_bounds.origin.x + row_bounds.size.width - px(pad);
                    if detail.types() {
                        let label = match column.nullable {
                            true => ready.nullable_types[column.type_label as usize].clone(),
                            false => ready.graph.types[column.type_label as usize].clone(),
                        };
                        let shaped = shape(
                            lines,
                            stamp,
                            label,
                            Face::Type,
                            type_size,
                            Ink::Muted,
                            dimmed,
                            &inks,
                            window,
                        );
                        let x = name_right - shaped.width();
                        let _ = shaped.paint(
                            point(x, row_bounds.origin.y),
                            row_bounds.size.height,
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                        name_right = x - px(ICON_GAP * zoom);
                    }
                    let shaped = shape(
                        lines,
                        stamp,
                        column.name.clone(),
                        Face::Column,
                        name_size,
                        Ink::Text,
                        dimmed,
                        &inks,
                        window,
                    );
                    let clip = Bounds {
                        origin: point(name_x, row_bounds.origin.y),
                        size: size((name_right - name_x).max(px(0.0)), row_bounds.size.height),
                    };
                    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                        let _ = shaped.paint(
                            clip.origin,
                            clip.size.height,
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    });
                }
            }
        });
    }

    fn canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let placed = view.clone();
        canvas(
            move |bounds, window, cx| {
                placed.update(cx, |view, _| view.place(bounds));
                window.insert_hitbox(bounds, HitboxBehavior::Normal)
            },
            move |bounds, hitbox: Hitbox, window, cx| {
                view.update(cx, |diagram, cx| diagram.paint(bounds, window, cx));
                listen(&view, &hitbox, window);
            },
        )
        .size_full()
    }
}

fn listen(view: &gpui::Entity<DiagramView>, hitbox: &Hitbox, window: &mut Window) {
    window.on_mouse_event({
        let (view, hitbox) = (view.clone(), hitbox.clone());
        move |event: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble
                && event.button == MouseButton::Left
                && hitbox.is_hovered(window)
            {
                view.update(cx, |view, cx| view.press(event.position, window, cx));
            }
        }
    });
    window.on_mouse_event({
        let (view, hitbox) = (view.clone(), hitbox.clone());
        move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                let hovered = hitbox.is_hovered(window);
                view.update(cx, |view, cx| view.drag(event.position, hovered, cx));
            }
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                view.update(cx, |view, cx| view.release(event.position, cx));
            }
        }
    });
    window.on_mouse_event({
        let (view, hitbox) = (view.clone(), hitbox.clone());
        move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.should_handle_scroll(window) {
                cx.stop_propagation();
                view.update(cx, |view, cx| view.scroll(event, window, cx));
            }
        }
    });
    window.on_mouse_event({
        let (view, hitbox) = (view.clone(), hitbox.clone());
        move |event: &PinchEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                cx.stop_propagation();
                view.update(cx, |view, cx| view.pinch(event, cx));
            }
        }
    });
}

impl Render for DiagramView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
        let surface = div()
            .id("diagram")
            .key_context("Diagram")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .bg(t.panel_glass());
        let note = |text: SharedString, colour: Hsla| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p(px(layout::SPACE_LG))
                .text_size(px(layout::chrome(layout::TEXT_MD)))
                .text_color(colour)
                .child(text)
        };
        match &self.load {
            Load::Loading => surface.child(note("Reading the schema…".into(), t.text_muted.into())),
            Load::Failed(message) => surface.child(note(message.clone(), t.danger.into())),
            Load::Ready(_) => surface.child(self.canvas(cx)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn shape(
    lines: &mut HashMap<LineKey, ShapedLine>,
    stamp: &Stamp,
    text: SharedString,
    face: Face,
    size: f32,
    ink: Ink,
    dimmed: bool,
    inks: &Theme,
    window: &mut Window,
) -> ShapedLine {
    let key = LineKey {
        text,
        face,
        size: size.to_bits(),
        ink,
        dimmed,
    };
    if let Some(line) = lines.get(&key) {
        return line.clone();
    }
    let family = match face {
        Face::Type => stamp.grid.clone(),
        Face::Title | Face::Column => stamp.chrome.clone(),
    };
    let mut font: Font = gpui::font(family);
    if face == Face::Title {
        font.weight = FontWeight::SEMIBOLD;
    }
    let colour = match ink {
        Ink::Text => inks.text,
        Ink::Muted => inks.text_muted,
    };
    let colour: Hsla = match dimmed {
        true => colour.alpha(DIM).into(),
        false => colour.into(),
    };
    let run = TextRun {
        len: key.text.len(),
        font,
        color: colour,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window
        .text_system()
        .shape_line(key.text.clone(), px(size), &[run], None);
    lines.insert(key, line.clone());
    line
}

fn distance(a: Point, b: Point) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// The card at an edge's other end from `card`, when the edge touches it.
fn other_end(layout: &Layout, edge: u32, card: u32) -> Option<u32> {
    let edge = &layout.edges[edge as usize];
    match (edge.child == card, edge.parent == card) {
        (true, _) => Some(edge.parent),
        (_, true) => Some(edge.child),
        _ => None,
    }
}

/// Whether a card fades under the focus: every card but the focus and the
/// cards it shares an edge with.
fn card_dimmed(layout: &Layout, focus: Option<u32>, card: u32) -> bool {
    let Some(focus) = focus else {
        return false;
    };
    focus != card
        && !layout.cards[focus as usize]
            .edges
            .iter()
            .any(|&edge| other_end(layout, edge, focus) == Some(card))
}

/// An edge under the focus, or with nothing in focus, under the hover.
fn edge_look(layout: &Layout, focus: Option<u32>, hover: Option<u32>, edge: u32) -> EdgeLook {
    let touches = |card: u32| other_end(layout, edge, card).is_some();
    match (focus, hover) {
        (Some(focus), _) if touches(focus) => EdgeLook::Lit,
        (Some(_), _) => EdgeLook::Dimmed,
        (None, Some(hover)) if touches(hover) => EdgeLook::Lit,
        (None, _) => EdgeLook::Plain,
    }
}

/// The column rows at both ends of the focus's keys, every column of a
/// composite key included: `(card, column)`.
fn joined_rows(ready: &Ready, focus: Option<u32>) -> HashSet<(u32, u32)> {
    let Some(focus) = focus else {
        return HashSet::new();
    };
    let layout = ready.scene.layout();
    let mut rows = HashSet::new();
    for &edge in &layout.cards[focus as usize].edges {
        let edge = &layout.edges[edge as usize];
        let key = &ready.graph.keys[edge.key as usize];
        rows.extend(key.columns.iter().map(|&column| (edge.child, column)));
        if let KeyTarget::Table { columns, .. } = &key.referenced {
            rows.extend(columns.iter().map(|&column| (edge.parent, column)));
        }
    }
    rows
}

fn screen_point(world: Point, zoom: f32) -> gpui::Point<Pixels> {
    point(px(world.x * zoom), px(world.y * zoom))
}

/// An edge and its marks tessellated at `zoom` with no pan, in the order they
/// are painted: the line, the marks' strokes, then each circle's hole and its
/// ring.
fn edge_paths(layout: &Layout, edge: u32, detail: Detail, zoom: f32, look: EdgeLook) -> Vec<Piece> {
    let shape = layout.edge_path(edge, detail, zoom);
    let width = px(match look {
        EdgeLook::Lit => LIT_EDGE_WIDTH,
        EdgeLook::Plain | EdgeLook::Dimmed => EDGE_WIDTH,
    });
    let mut pieces = Vec::new();
    let mut line = PathBuilder::stroke(width);
    for segment in &shape.segments {
        match *segment {
            Segment::Move(to) => line.move_to(screen_point(to, zoom)),
            Segment::Line(to) => line.line_to(screen_point(to, zoom)),
            Segment::Cubic {
                control_a,
                control_b,
                to,
            } => line.cubic_bezier_to(
                screen_point(to, zoom),
                screen_point(control_a, zoom),
                screen_point(control_b, zoom),
            ),
        }
    }
    pieces.extend(line.build().ok().map(|path| (path, Paint::Ink)));
    let mut strokes = PathBuilder::stroke(width);
    let mut circles = Vec::new();
    for mark in &shape.marks {
        match *mark {
            Stroke::Line(a, b) => {
                strokes.move_to(screen_point(a, zoom));
                strokes.line_to(screen_point(b, zoom));
            }
            Stroke::Circle { centre, radius } => circles.push(
                (0..CIRCLE_STEPS)
                    .map(|step| {
                        let angle = step as f32 / CIRCLE_STEPS as f32 * std::f32::consts::TAU;
                        screen_point(
                            Point::new(
                                centre.x + radius * angle.cos(),
                                centre.y + radius * angle.sin(),
                            ),
                            zoom,
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
        }
    }
    pieces.extend(strokes.build().ok().map(|path| (path, Paint::Ink)));
    for ring in circles {
        let mut hole = PathBuilder::fill();
        hole.add_polygon(&ring, true);
        pieces.extend(hole.build().ok().map(|path| (path, Paint::Hole)));
        let mut outline = PathBuilder::stroke(width);
        outline.add_polygon(&ring, true);
        pieces.extend(outline.build().ok().map(|path| (path, Paint::Ink)));
    }
    pieces
}

fn translated(path: &Path<Pixels>, by: gpui::Point<Pixels>) -> Path<Pixels> {
    let mut path = path.clone();
    path.bounds.origin += by;
    for vertex in &mut path.vertices {
        vertex.xy_position += by;
    }
    path
}

/// The faint dot grid behind an opaque theme's cards, in world space so it
/// moves with the diagram.
fn paint_dots(bounds: Bounds<Pixels>, transform: Transform, t: Theme, window: &mut Window) {
    let mut step = DOT_STEP;
    while step * transform.zoom < DOT_MIN_GAP {
        step *= 2.0;
    }
    let screen = Rect {
        x: 0.0,
        y: 0.0,
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
    };
    let world = transform.world_rect(screen);
    let colour: Hsla = t.border.into();
    let mut y = (world.y / step).floor() * step;
    while y <= world.bottom() {
        let mut x = (world.x / step).floor() * step;
        while x <= world.right() {
            let at = transform.to_screen(Point::new(x, y));
            window.paint_quad(gpui::fill(
                Bounds {
                    origin: point(
                        bounds.origin.x + px(at.x - DOT_SIZE / 2.0),
                        bounds.origin.y + px(at.y - DOT_SIZE / 2.0),
                    ),
                    size: size(px(DOT_SIZE), px(DOT_SIZE)),
                },
                colour,
            ));
            x += step;
        }
        y += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagram::layout;

    /// Three tables in a chain, `t2 -> t1 -> t0`, and one on its own.
    fn chain() -> Layout {
        let graph = crate::diagram::tests::graph(4, &[(1, 0), (2, 1)]);
        layout::schema(&graph, false)
    }

    fn card_of(layout: &Layout, table: u32) -> u32 {
        layout
            .cards
            .iter()
            .position(|card| card.subject == Subject::Table(table))
            .unwrap() as u32
    }

    #[test]
    fn focus_keeps_its_neighbours_sharp_and_dims_the_rest() {
        let layout = chain();
        let [t0, t1, t2, t3] = [0, 1, 2, 3].map(|table| card_of(&layout, table));
        assert!(!card_dimmed(&layout, None, t3), "nothing in focus");
        assert!(!card_dimmed(&layout, Some(t1), t1));
        assert!(!card_dimmed(&layout, Some(t1), t0));
        assert!(!card_dimmed(&layout, Some(t1), t2));
        assert!(card_dimmed(&layout, Some(t1), t3));
        assert!(card_dimmed(&layout, Some(t0), t2), "two steps away");
    }

    #[test]
    fn edges_light_for_the_focus_else_for_the_hover() {
        let layout = chain();
        let [t0, _, t2, t3] = [0, 1, 2, 3].map(|table| card_of(&layout, table));
        let edge = |child: u32| {
            layout
                .edges
                .iter()
                .position(|edge| edge.child == child)
                .unwrap() as u32
        };
        let (near, far) = (edge(card_of(&layout, 1)), edge(t2));
        assert_eq!(edge_look(&layout, None, None, near), EdgeLook::Plain);
        assert_eq!(edge_look(&layout, None, Some(t0), near), EdgeLook::Lit);
        assert_eq!(edge_look(&layout, None, Some(t0), far), EdgeLook::Plain);
        assert_eq!(edge_look(&layout, Some(t0), None, near), EdgeLook::Lit);
        assert_eq!(
            edge_look(&layout, Some(t0), Some(t2), far),
            EdgeLook::Dimmed,
            "the hover gives way to the focus"
        );
        assert_eq!(edge_look(&layout, Some(t3), None, near), EdgeLook::Dimmed);
    }
}
