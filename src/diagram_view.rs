//! The ER diagram on screen: one `canvas()` that paints what `diagram` laid
//! out, and the pointer that pans, zooms and drags it. Cards are quads, edges
//! are tessellated paths and text is shaped by hand, because a thousand cards
//! as divs would be a thousand layouts a frame (design §4.1).
//!
//! Nothing here runs without input: no animation, no timer. A frame is drawn
//! when a gesture or a load notifies the view, and the view is mounted cached,
//! so a redraw elsewhere in the window reuses its last frame.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    AppContext, BorderStyle, Bounds, BoxShadow, ContentMask, Context, CursorStyle, DispatchPhase,
    Entity, EventEmitter, FocusHandle, Focusable, Font, FontWeight, Hitbox, HitboxBehavior, Hsla,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Path, PathBuilder, PinchEvent, Pixels, Render, ScrollWheelEvent, ShapedLine,
    SharedString, StatefulInteractiveElement, Styled, Subscription, TextAlign, TextRun,
    TransformationMatrix, WeakEntity, Window, canvas, div, point, prelude::FluentBuilder, px, quad,
    size,
};

use gpui_component::{
    Disableable,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu, PopupMenu, PopupMenuItem},
};

use crate::actions::{
    DiagramFindNext, DiagramFindPick, DiagramFindPrevious, DiagramFocusDown, DiagramFocusLeft,
    DiagramFocusRight, DiagramFocusUp, FindInDiagram, OpenDiagramStructure, OpenDiagramTable,
    ResetEditorZoom, ZoomEditorIn, ZoomEditorOut,
};
use crate::db::{KeyTarget, SchemaGraph};
use crate::diagram::geometry::{
    self, CARD_WIDTH, Detail, Direction, Fit, HEADER_HEIGHT, Handle, MAX_CARD_WIDTH,
    MIN_CARD_WIDTH, Point, Rect, Segment, Stroke, Transform, WORLD_LIMIT,
};
use crate::diagram::index::{Placement, Scene};
use crate::diagram::layout::{Layout, Subject};
use crate::diagram::{mermaid, svg};
use crate::fuzzy::{Fuzzy, Query};
use crate::icons::icon;
use crate::store;
use crate::theme::{Theme, fonts, layout, popup_theme, theme};
use crate::ui::{self, Control, Tone};

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
/// How near an edge the pointer must come to light it, in pixels.
const EDGE_REACH: f32 = 4.0;
/// How near a card's right or bottom edge the pointer must come to resize
/// it, in pixels, either side of the edge.
const HANDLE_REACH: f32 = 5.0;
/// Dimmed ink keeps this much of its colour.
const DIM: f32 = 0.3;
/// The tint behind a joined row. Lighter than the accent's usual wash, because
/// the row's name and type are read through it, on glass over the worst backdrop.
pub(crate) const JOINED_ROW_WASH: f32 = 0.06;
/// The dot grid's spacing, in world units, doubled until the dots stand at
/// least [`DOT_MIN_GAP`] pixels apart, so zooming out never fills the screen
/// with dots.
const DOT_STEP: f32 = 32.0;
const DOT_MIN_GAP: f32 = 24.0;
const DOT_SIZE: f32 = 1.5;
/// Shaped lines kept before the cache starts again. Keys are text, size step
/// and face, so a session of zooming over one schema stays well under it.
const LINE_CACHE_LIMIT: usize = 8192;
const FOCUS_BAR_MAX_WIDTH: f32 = 560.0;
/// The zooms the toolbar's buttons and the zoom chords step through, so the
/// readout lands on round numbers whatever a pinch left it at.
const ZOOM_STEPS: [f32; 15] = [
    0.05, 0.1, 0.25, 0.33, 0.5, 0.67, 0.75, 0.9, 1.0, 1.25, 1.5, 2.0, 2.5, 3.0, 4.0,
];
/// Wide enough for "400%", so the toolbar does not shift as the zoom changes.
const ZOOM_READOUT_WIDTH: f32 = 52.0;
const FIND_WIDTH: f32 = 300.0;
/// Matches listed under the find field; typing narrows the rest.
const FIND_SHOWN: usize = 8;
const FIND_ROW_HEIGHT: f32 = 28.0;
const MINIMAP_WIDTH: f32 = 184.0;
const MINIMAP_MIN_HEIGHT: f32 = 64.0;
const MINIMAP_MAX_HEIGHT: f32 = 144.0;
/// Room around the drawn world inside the minimap, in pixels.
const MINIMAP_PAD: f32 = 8.0;
const MINIMAP_CARD_ALPHA: f32 = 0.6;
const ELLIPSIS: &str = "…";

pub enum Load {
    Loading,
    Failed(SharedString),
    Ready(Box<Ready>),
}

/// A table by its schema and name: what a card the user moved or sized is
/// remembered by, since card numbers change with every layout.
pub type TableKey = (SharedString, SharedString);

/// Which of the two views the scene is laid out as (design §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Schema,
    /// One table, by its index in the graph, with its parents and children.
    Neighbourhood(u32),
}

/// A read schema and its laid-out, indexed scene.
pub struct Ready {
    graph: Arc<SchemaGraph>,
    scene: Scene,
    view: View,
    /// The schema view and where it was looked at, kept while a neighbourhood
    /// is shown, so going back finds the cards where the user dragged them.
    /// Without a transform when the diagram came back from disk in a
    /// neighbourhood, and is fitted when it goes back.
    stash: Option<(Scene, Option<Transform>)>,
    /// What the scene covers, in world units: worked out once per layout and
    /// grown as cards are dragged, since the minimap reads it every frame.
    bounds: Rect,
    /// Each type label with the `?` a nullable column's type wears, made once
    /// so a frame never formats one.
    nullable_types: Vec<SharedString>,
    /// A stub's schema-qualified name, by the key into it, for the same reason.
    stub_names: HashMap<u32, SharedString>,
    /// Where the user dragged cards in the schema view and the sizes they
    /// gave them, by table, so a layout made again (keys only, or a new read)
    /// puts them back.
    placed: HashMap<TableKey, Placement>,
}

impl Ready {
    pub fn new(graph: SchemaGraph, scene: Scene) -> Self {
        let nullable_types = graph
            .types
            .iter()
            .map(|label| SharedString::from(format!("{label}?")))
            .collect();
        let stub_names = (graph.keys.iter().enumerate())
            .filter_map(|(key, graph_key)| match &graph_key.referenced {
                KeyTarget::Elsewhere { schema, table } => {
                    Some((key as u32, format!("{schema}.{table}").into()))
                }
                KeyTarget::Table { .. } => None,
            })
            .collect();
        Self {
            graph: Arc::new(graph),
            bounds: scene.layout().bounds(),
            scene,
            view: View::Schema,
            stash: None,
            nullable_types,
            stub_names,
            placed: HashMap::new(),
        }
    }

    /// The card that stands for a table, by its schema and name.
    fn card_of_table(&self, table: &TableKey) -> Option<u32> {
        (0..self.scene.layout().cards.len() as u32).find(|&card| self.table(card) == *table)
    }

    /// Takes over where the user left cards and the sizes they gave them,
    /// for the tables this read still has, and puts the cards there. What a
    /// dropped table had is forgotten.
    pub fn adopt(&mut self, mut placed: HashMap<TableKey, Placement>) {
        let tables: HashSet<TableKey> = (0..self.scene.layout().cards.len() as u32)
            .map(|card| self.table(card))
            .collect();
        placed.retain(|table, placement| tables.contains(table) && !placement.is_empty());
        self.placed = placed;
        self.place_saved();
        self.bounds = self.scene.layout().bounds();
    }

    /// Puts the cards the user dragged or sized back as they left them, on a
    /// scene laid out afresh.
    fn place_saved(&mut self) {
        if self.placed.is_empty() {
            return;
        }
        for card in 0..self.scene.layout().cards.len() as u32 {
            if let Some(placement) = self.placed.get(&self.table(card)) {
                self.scene.place(card, placement);
            }
        }
    }

    /// The width and height a card was given, none where it has the
    /// layout's own. The schema view's are remembered whatever the layout
    /// now shows; a neighbourhood's are read off its cards.
    fn size_of(&self, card: u32) -> (Option<f32>, Option<f32>) {
        match self.view {
            View::Schema => self
                .placed
                .get(&self.table(card))
                .map_or((None, None), |placement| {
                    (placement.width, placement.height)
                }),
            View::Neighbourhood(_) => {
                let layout = self.scene.layout();
                let shown = &layout.cards[card as usize];
                (
                    (shown.width != CARD_WIDTH).then_some(shown.width),
                    (shown.height != layout.auto_height(card)).then_some(shown.height),
                )
            }
        }
    }

    /// Sizes a card, remembering it in the schema view. Returns the edges
    /// that end at it, whose paths changed.
    fn resize(&mut self, card: u32, width: Option<f32>, height: Option<f32>) -> Vec<u32> {
        if self.view == View::Schema {
            let table = self.table(card);
            let placement = self.placed.entry(table.clone()).or_default();
            (placement.width, placement.height) = (width, height);
            if placement.is_empty() {
                self.placed.remove(&table);
            }
        }
        let edges = self.scene.resize_card(card, width, height).to_vec();
        let sized = self.scene.layout().card_rect(card, Detail::Columns, 1.0);
        self.bounds = self.bounds.union(&sized);
        edges
    }

    /// The schema and name of the table a card stands for, a stub's in its
    /// own schema.
    fn table(&self, card: u32) -> TableKey {
        let table = |table: u32| {
            (
                self.graph.schema.clone(),
                self.graph.tables[table as usize].name.clone(),
            )
        };
        match self.scene.layout().cards[card as usize].subject {
            Subject::Table(index) => table(index),
            Subject::Stub(key) => match &self.graph.keys[key as usize].referenced {
                KeyTarget::Elsewhere { schema, table } => (schema.clone(), table.clone()),
                KeyTarget::Table { table: index, .. } => table(*index),
            },
        }
    }

    fn name(&self, card: u32) -> SharedString {
        match self.scene.layout().cards[card as usize].subject {
            Subject::Table(table) => self.graph.tables[table as usize].name.clone(),
            Subject::Stub(key) => match &self.graph.keys[key as usize].referenced {
                KeyTarget::Elsewhere { .. } => self.stub_names[&key].clone(),
                KeyTarget::Table { table, .. } => self.graph.tables[*table as usize].name.clone(),
            },
        }
    }
}

/// The find field and what it matched.
struct Find {
    input: Entity<InputState>,
    query: Query,
    fuzzy: Fuzzy,
    /// Cards whose names match, best first.
    results: Vec<u32>,
    selected: usize,
    _changes: Subscription,
}

/// What the view asks of the workspace around it.
pub enum DiagramEvent {
    /// A table's rows, or its structure, in the tab the explorer would open.
    Open {
        schema: SharedString,
        table: SharedString,
        structure: bool,
    },
    /// The user moved or sized a card in the schema view, and the
    /// arrangement ([`DiagramView::arranged`]) is worth keeping.
    Arranged,
    /// The view, Keys only, the zoom or the pan changed, at the end of a
    /// gesture or on a command, and the tab is worth writing down.
    Viewed,
    /// The zoom or the pan moved under a wheel, a pinch or the keyboard, or
    /// a layout landed: worth writing down once it is still.
    Moved,
    /// The user asked to write the diagram to a file of this kind.
    Export(DiagramFile),
    /// The user asked for the diagram as Mermaid text on the clipboard.
    CopyMermaid,
}

/// A kind of file the diagram is written as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagramFile {
    Svg,
    Png,
    Mermaid,
}

impl DiagramFile {
    pub const ALL: [Self; 3] = [Self::Svg, Self::Png, Self::Mermaid];

    pub fn extension(self) -> &'static str {
        match self {
            Self::Svg => "svg",
            Self::Png => "png",
            Self::Mermaid => "mmd",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Svg => "SVG",
            Self::Png => "PNG",
            Self::Mermaid => "Mermaid",
        }
    }

    /// The kind a path's extension names, else `asked`: someone who typed
    /// `.png` over the suggested `.svg` asked for a PNG.
    pub fn for_path(path: &std::path::Path, asked: Self) -> Self {
        let extension = path.extension().and_then(|extension| extension.to_str());
        Self::ALL
            .into_iter()
            .find(|file| extension.is_some_and(|ext| ext.eq_ignore_ascii_case(file.extension())))
            .unwrap_or(asked)
    }
}

/// The diagram as it stands, taken on the UI thread to be written out off
/// it: where the user left each card, at the view and Keys only on screen.
pub struct Drawing {
    pub graph: Arc<SchemaGraph>,
    pub layout: Layout,
    pub style: svg::Style,
    /// What the file is named after: the schema, and the table in the middle
    /// of a neighbourhood.
    pub stem: String,
}

impl Drawing {
    pub fn svg(&self) -> String {
        svg::svg(&self.graph, &self.layout, &self.style)
    }

    pub fn mermaid(&self) -> String {
        mermaid::mermaid(&self.graph, &self.layout)
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
    /// The minimap held down: the view follows the pointer over it.
    Minimap,
    /// A card's edge or corner, dragged to size it.
    Resize {
        card: u32,
        handle: Handle,
        /// Where the pointer was pressed, in world units.
        from: Point,
        /// The card as drawn then, in world units.
        width: f32,
        height: f32,
        resized: bool,
    },
}

/// Where the minimap drew the world, so a press on it finds the point meant.
#[derive(Clone, Copy, Debug)]
struct Minimap {
    /// The window position of world `(0, 0)`.
    origin: gpui::Point<Pixels>,
    scale: f32,
}

impl Minimap {
    fn to_world(self, position: gpui::Point<Pixels>) -> Point {
        Point::new(
            f32::from(position.x - self.origin.x) / self.scale,
            f32::from(position.y - self.origin.y) / self.scale,
        )
    }

    fn to_screen(self, world: Rect) -> Bounds<Pixels> {
        Bounds {
            origin: point(
                self.origin.x + px(world.x * self.scale),
                self.origin.y + px(world.y * self.scale),
            ),
            size: size(
                px((world.width * self.scale).max(1.0)),
                px((world.height * self.scale).max(1.0)),
            ),
        }
    }
}

/// What the pointer is over, while nothing is pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hover {
    Card(u32),
    Edge(u32),
}

/// How an edge is drawn under the current focus and hover.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum EdgeLook {
    Plain,
    /// The focus's, the hovered card's own or the hovered edge: the accent, a
    /// little heavier.
    Lit,
    Dimmed,
}

/// The three faces a card's text is set in, each with its family, weight
/// and ink.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Face {
    Title,
    Column,
    Type,
    /// The "+N more" row of a card cut short.
    More,
}

/// How a line is set. A shaped line bakes its colour in, so the face and the
/// dimmed form of it are part of the cache key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Pen {
    face: Face,
    size: u32,
    dimmed: bool,
}

impl Pen {
    fn new(face: Face, size: f32, dimmed: bool) -> Self {
        Self {
            face,
            size: size.to_bits(),
            dimmed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct LineKey {
    text: SharedString,
    pen: Pen,
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
    /// The card under the pointer, whose edges are lit, or the edge, lit with
    /// the rows it joins, while nothing is in focus.
    hover: Option<Hover>,
    /// The resize handle under the pointer, which sets the cursor.
    handle: Option<Handle>,
    gesture: Option<Gesture>,
    lines: HashMap<LineKey, ShapedLine>,
    /// "+N more" by N, made once each so a frame formats none.
    more_labels: HashMap<usize, SharedString>,
    stamp: Option<Stamp>,
    /// Edges tessellated at `paths_zoom`, with no pan: panning offsets a copy,
    /// which `paint_path` makes anyway, and only a zoom tessellates again.
    paths: HashMap<(u32, EdgeLook), Vec<Piece>>,
    paths_zoom: f32,
    /// Keys only as asked for: the switch shows it at once, while the layout
    /// it asks for may still be on its way.
    keys_only: bool,
    find: Option<Find>,
    minimap: Option<Minimap>,
    /// Counts layouts asked for, so one that lands after a later one was
    /// asked for, or after a new read, is dropped.
    layouts: u64,
    /// How the tab was being looked at when it was written to disk, until
    /// its first read lands and it is applied.
    restore: Option<store::StoredDiagram>,
    /// Behind another tab or profile since it was last painted, so holding
    /// none of what only drawing needs ([`Self::hide`]).
    hidden: bool,
}

impl EventEmitter<DiagramEvent> for DiagramView {}

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
            handle: None,
            gesture: None,
            lines: HashMap::new(),
            more_labels: HashMap::new(),
            stamp: None,
            paths: HashMap::new(),
            paths_zoom: 0.0,
            keys_only: false,
            find: None,
            minimap: None,
            layouts: 0,
            restore: None,
            hidden: false,
        }
    }

    /// A diagram coming back from disk, looked at as it was once read.
    pub fn restoring(restore: store::StoredDiagram, cx: &mut Context<Self>) -> Self {
        Self {
            restore: Some(restore),
            ..Self::new(cx)
        }
    }

    /// How the tab is being looked at, for `profiles.toml`: as it came back
    /// from disk until it has been read again, so a session that never got
    /// that far loses nothing.
    pub fn stored(&self) -> store::StoredDiagram {
        if let Some(restore) = &self.restore {
            return restore.clone();
        }
        let Some(ready) = self.ready() else {
            return store::StoredDiagram::default();
        };
        let looked = self.fitted.then_some(self.transform);
        let rounded = |value: f32, scale: f64| (f64::from(value) * scale).round() / scale;
        store::StoredDiagram {
            centre: match ready.view {
                View::Neighbourhood(table) => {
                    Some(ready.graph.tables[table as usize].name.to_string())
                }
                View::Schema => None,
            },
            keys_only: Some(self.keys_only),
            zoom: looked.map(|looked| rounded(looked.zoom, 10_000.0)),
            x: looked.map_or(0.0, |looked| rounded(looked.pan.x, 10.0)),
            y: looked.map_or(0.0, |looked| rounded(looked.pan.y, 10.0)),
        }
    }

    /// The keys-only setting a read should lay the diagram out at: the one
    /// on screen, else the one it came back from disk with.
    pub fn wanted_keys_only(&self) -> Option<bool> {
        self.keys_only()
            .or_else(|| self.restore.as_ref()?.keys_only)
    }

    /// Puts a diagram just read back as it was looked at when written down:
    /// the neighbourhood it centred on, if that table is still there, and the
    /// zoom and pan, or else a fit.
    fn apply_restore(&mut self, restore: store::StoredDiagram) {
        let Load::Ready(ready) = &mut self.load else {
            return;
        };
        let centre = restore.centre.as_deref().map(|name| {
            (ready.graph.tables.iter())
                .position(|table| table.name.as_ref() == name)
                .map(|table| table as u32)
        });
        if let Some(Some(table)) = centre {
            let around = crate::diagram::layout::neighbourhood(&ready.graph, table, self.keys_only);
            let schema = std::mem::replace(&mut ready.scene, Scene::new(around));
            ready.stash = Some((schema, None));
            ready.view = View::Neighbourhood(table);
            ready.bounds = ready.scene.layout().bounds();
            self.focus = card_of(ready.scene.layout(), Subject::Table(table));
        }
        // A neighbourhood whose table is gone was looked at somewhere this
        // schema view has nothing.
        let looked = match centre {
            Some(None) => None,
            _ => restore.zoom,
        };
        if let Some(zoom) = looked
            && zoom.is_finite()
            && zoom > 0.0
            && restore.x.is_finite()
            && restore.y.is_finite()
        {
            self.transform = Transform {
                zoom: (zoom as f32).clamp(geometry::MIN_ZOOM, geometry::MAX_ZOOM),
                pan: Point::new(restore.x as f32, restore.y as f32),
            };
            self.fitted = true;
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// The tab went behind another, or its profile did: drops the shaped
    /// lines, the tessellated edges and both scenes' grids, keeping the
    /// graph, the layouts and where the cards are (design §6.6). The next
    /// paint builds what it draws again, the grid in about 5 ms on a
    /// thousand tables.
    pub fn hide(&mut self) {
        self.hidden = true;
        self.lines = HashMap::new();
        self.more_labels = HashMap::new();
        self.stamp = None;
        self.paths = HashMap::new();
        self.hover = None;
        if let Load::Ready(ready) = &mut self.load {
            ready.scene.release();
            if let Some((schema, _)) = &mut ready.stash {
                schema.release();
            }
        }
    }

    /// A scene that lands while the tab is hidden is laid out with its grid,
    /// which nothing draws from until it is shown.
    fn release_if_hidden(&mut self) {
        if self.hidden {
            self.hide();
        }
    }

    /// Whether keys only is on, once there is a diagram to show it on.
    pub fn keys_only(&self) -> Option<bool> {
        self.ready().map(|_| self.keys_only)
    }

    /// A read landed. Over a diagram already drawn, it is that diagram read
    /// again ([`Self::refresh`]); otherwise it starts the view afresh.
    pub fn set_load(&mut self, load: Load, cx: &mut Context<Self>) {
        let load = match (load, &self.load) {
            (Load::Ready(fresh), Load::Ready(_)) => return self.refresh(*fresh, cx),
            (load, _) => load,
        };
        self.load = load;
        self.keys_only = self
            .ready()
            .is_some_and(|ready| ready.scene.layout().keys_only);
        self.layouts += 1;
        self.find = None;
        self.fitted = false;
        self.focus = None;
        self.hover = None;
        self.gesture = None;
        self.paths.clear();
        if self.ready().is_some()
            && let Some(restore) = self.restore.take()
        {
            self.apply_restore(restore);
        }
        self.release_if_hidden();
        cx.notify();
    }

    /// Swaps in a diagram read again, keeping what the user made of the old
    /// one: the cards they dragged, the view and the table it centres on, the
    /// focus and where they were looking, for every table still there.
    fn refresh(&mut self, mut fresh: Ready, cx: &mut Context<Self>) {
        let Load::Ready(old) = &mut self.load else {
            return;
        };
        let focus = self.focus.map(|card| old.table(card));
        let centre = match old.view {
            View::Neighbourhood(table) => Some(old.graph.tables[table as usize].name.clone()),
            View::Schema => None,
        };
        let looked = old
            .stash
            .as_ref()
            .map_or(Some(self.transform), |(_, at)| *at);
        fresh.adopt(std::mem::take(&mut old.placed));
        // Keys only switched while the read was out: its scene is at the
        // setting it was asked with, and the switch's own layout was of the
        // old graph.
        let switched = fresh.scene.layout().keys_only != self.keys_only;
        let centre = centre.and_then(|name| {
            (fresh.graph.tables.iter())
                .position(|table| table.name == name)
                .map(|table| table as u32)
        });
        match centre {
            Some(table) => {
                let around =
                    crate::diagram::layout::neighbourhood(&fresh.graph, table, self.keys_only);
                let schema = std::mem::replace(&mut fresh.scene, Scene::new(around));
                fresh.stash = (!switched).then_some((schema, looked));
                fresh.view = View::Neighbourhood(table);
            }
            None => match looked {
                Some(looked) => self.transform = looked,
                None => self.fitted = false,
            },
        }
        fresh.bounds = fresh.scene.layout().bounds();
        self.focus = focus.and_then(|table| fresh.card_of_table(&table));
        let view = fresh.view;
        self.load = Load::Ready(Box::new(fresh));
        self.layouts += 1;
        self.hover = None;
        self.gesture = None;
        self.paths.clear();
        if let Some(text) = self.find.as_ref().map(|find| find.input.read(cx).value()) {
            self.rank(&text);
        }
        if switched && view == View::Schema {
            self.lay_out(view, cx);
        }
        self.release_if_hidden();
        cx.emit(DiagramEvent::Moved);
        cx.notify();
    }

    fn ready(&self) -> Option<&Ready> {
        match &self.load {
            Load::Ready(ready) => Some(ready),
            _ => None,
        }
    }

    /// The diagram as it is drawn, for an export. None until it is read.
    pub fn drawing(&self, cx: &gpui::App) -> Option<Drawing> {
        let ready = self.ready()?;
        let graph = &ready.graph;
        let stem = match ready.view {
            View::Schema => graph.schema.to_string(),
            View::Neighbourhood(table) => {
                format!("{}-{}", graph.schema, graph.tables[table as usize].name)
            }
        };
        let fonts = fonts(cx);
        Some(Drawing {
            graph: graph.clone(),
            layout: ready.scene.layout().clone(),
            style: svg::Style {
                palette: file_palette(*theme(cx)),
                chrome: fonts.chrome.to_string(),
                grid: fonts.grid.to_string(),
                name_size: geometry::text_size(1.0, layout::chrome(layout::TEXT_MD)),
                type_size: geometry::text_size(1.0, layout::grid(layout::TEXT_SM)),
            },
            stem: stem.replace(['/', '\\', ':'], "_"),
        })
    }

    fn local(&self, position: gpui::Point<Pixels>) -> Point {
        Point::new(
            f32::from(position.x - self.viewport.origin.x),
            f32::from(position.y - self.viewport.origin.y),
        )
    }

    /// Takes the canvas's bounds as laid out. A fit or a new size changes
    /// what the chrome, rendered before this, shows (the zoom readout, whether
    /// the minimap is wanted), so either asks to render once more.
    fn place(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let mut stale = bounds.size != self.viewport.size;
        self.viewport = bounds;
        if !self.fitted
            && bounds.size.width > px(0.0)
            && bounds.size.height > px(0.0)
            && let Some(ready) = self.ready()
        {
            self.transform = ready
                .scene
                .layout()
                .fitting(fit_area(self.screen()), FIT_MARGIN);
            self.fitted = true;
            stale = true;
        }
        // A notify while this view draws is lost with the frame it is in.
        if stale {
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        }
    }

    fn screen(&self) -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: f32::from(self.viewport.size.width),
            height: f32::from(self.viewport.size.height),
        }
    }

    /// Zoomed to `zoom` about the middle of the view.
    fn zoom_to(&mut self, zoom: f32, cx: &mut Context<Self>) {
        self.transform = self.transform.zoomed_at(self.screen().centre(), zoom);
        cx.emit(DiagramEvent::Viewed);
        cx.notify();
    }

    fn step_zoom(&mut self, zoom_in: bool, cx: &mut Context<Self>) {
        self.zoom_to(zoom_step(self.transform.zoom, zoom_in), cx);
    }

    /// The whole scene in view, never zoomed past 100%.
    fn fit(&mut self, cx: &mut Context<Self>) {
        let Some(ready) = self.ready() else {
            return;
        };
        self.transform = ready
            .scene
            .layout()
            .fitting(fit_area(self.screen()), FIT_MARGIN);
        cx.emit(DiagramEvent::Viewed);
        cx.notify();
    }

    fn set_keys_only(&mut self, keys_only: bool, cx: &mut Context<Self>) {
        if self.keys_only == keys_only {
            return;
        }
        self.keys_only = keys_only;
        cx.emit(DiagramEvent::Viewed);
        let Some(view) = self.ready().map(|ready| ready.view) else {
            return;
        };
        if let Load::Ready(ready) = &mut self.load {
            ready.stash = None;
        }
        self.lay_out(view, cx);
        cx.notify();
    }

    /// The schema view, or the neighbourhood of the table in focus. Asked
    /// again on a neighbourhood, it moves the centre to the focused table.
    fn show(&mut self, neighbourhood: bool, cx: &mut Context<Self>) {
        let Some(ready) = self.ready() else {
            return;
        };
        let view = match neighbourhood {
            false => View::Schema,
            true => match self
                .focus
                .map(|card| ready.scene.layout().cards[card as usize].subject)
            {
                Some(Subject::Table(table)) => View::Neighbourhood(table),
                _ => return,
            },
        };
        if view == ready.view {
            return;
        }
        let Load::Ready(ready) = &mut self.load else {
            return;
        };
        cx.emit(DiagramEvent::Viewed);
        if view == View::Schema
            && let Some((scene, transform)) = ready.stash.take()
        {
            self.install(scene, view, transform, cx);
            if transform.is_none() {
                self.fitted = false;
            }
            return;
        }
        self.lay_out(view, cx);
    }

    /// Shows a table's neighbourhood with the table in focus, from outside
    /// the diagram: the explorer or a Structure tab. Before the first read it
    /// is held for when the read lands; a table the diagram does not draw (a
    /// view, a partition) leaves the view as it is.
    pub fn centre_on(&mut self, table: &str, cx: &mut Context<Self>) {
        let Some(ready) = self.ready() else {
            self.restore.get_or_insert_with(Default::default).centre = Some(table.to_string());
            return;
        };
        let Some(index) = (ready.graph.tables.iter())
            .position(|drawn| drawn.name.as_ref() == table)
            .map(|index| index as u32)
        else {
            return;
        };
        let view = View::Neighbourhood(index);
        if view == ready.view {
            self.focus = card_of(ready.scene.layout(), Subject::Table(index));
            cx.notify();
            return;
        }
        cx.emit(DiagramEvent::Viewed);
        self.lay_out(view, cx);
    }

    /// Lays `view` out at the keys-only setting asked for. A neighbourhood is
    /// a handful of cards and is laid out at once; a schema can be thousands,
    /// and is laid out off the UI thread while the old scene stays on screen.
    fn lay_out(&mut self, view: View, cx: &mut Context<Self>) {
        let Some(graph) = self.ready().map(|ready| ready.graph.clone()) else {
            return;
        };
        let keys_only = self.keys_only;
        self.layouts += 1;
        let asked = self.layouts;
        match view {
            View::Neighbourhood(table) => {
                let scene = Scene::new(crate::diagram::layout::neighbourhood(
                    &graph, table, keys_only,
                ));
                self.install(scene, view, None, cx);
            }
            View::Schema => {
                let task = cx.background_executor().spawn(async move {
                    Scene::new(crate::diagram::layout::schema(&graph, keys_only))
                });
                cx.spawn(async move |this, cx| {
                    let scene = task.await;
                    this.update(cx, |this, cx| {
                        if this.layouts == asked {
                            this.install(scene, view, None, cx);
                            cx.emit(DiagramEvent::Moved);
                        }
                    })
                    .ok();
                })
                .detach();
            }
        }
    }

    /// Puts a newly laid-out scene on screen, keeping the focus on the same
    /// table. A neighbourhood is fitted; a schema view is looked at where it
    /// was (`transform`), else with the focused card held where it stood.
    fn install(
        &mut self,
        scene: Scene,
        view: View,
        transform: Option<Transform>,
        cx: &mut Context<Self>,
    ) {
        let Load::Ready(ready) = &mut self.load else {
            return;
        };
        let zoom = self.transform.zoom;
        let focused = self.focus.map(|card| {
            let layout = ready.scene.layout();
            (
                layout.cards[card as usize].subject,
                layout.cards[card as usize].origin,
            )
        });
        let focus = match (view, focused) {
            (View::Neighbourhood(table), _) => card_of(scene.layout(), Subject::Table(table)),
            (View::Schema, Some((subject, _))) => card_of(scene.layout(), subject),
            (View::Schema, None) => None,
        };
        let old = std::mem::replace(&mut ready.scene, scene);
        if view == View::Schema && transform.is_none() {
            ready.place_saved();
        }
        ready.bounds = ready.scene.layout().bounds();
        if ready.view == View::Schema && matches!(view, View::Neighbourhood(_)) {
            ready.stash = Some((old, Some(self.transform)));
        }
        ready.view = view;
        match (view, transform, focused.zip(focus)) {
            (View::Neighbourhood(_), _, _) => self.fitted = false,
            (View::Schema, Some(transform), _) => self.transform = transform,
            (View::Schema, None, Some(((_, was), card))) => {
                let now = ready.scene.layout().cards[card as usize].origin;
                self.transform = self
                    .transform
                    .panned(Point::new((was.x - now.x) * zoom, (was.y - now.y) * zoom));
            }
            (View::Schema, None, None) => {}
        }
        self.focus = focus;
        self.hover = None;
        self.gesture = None;
        self.paths.clear();
        // The matches were card numbers in the scene just replaced.
        if let Some(text) = self.find.as_ref().map(|find| find.input.read(cx).value()) {
            self.rank(&text);
        }
        self.release_if_hidden();
        cx.notify();
    }

    fn press(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        self.find = None;
        let at = self.local(event.position);
        let zoom = self.transform.zoom;
        let world = self.transform.to_world(at);
        let handle = self
            .ready()
            .and_then(|ready| ready.scene.handle_at(world, HANDLE_REACH / zoom, zoom));
        if let Some((card, handle)) = handle {
            self.gesture = None;
            match event.click_count {
                1 => {
                    let rect = self.ready().map_or(Rect::default(), |ready| {
                        let layout = ready.scene.layout();
                        layout.card_rect(card, layout.detail(zoom), zoom)
                    });
                    self.gesture = Some(Gesture::Resize {
                        card,
                        handle,
                        from: world,
                        width: rect.width,
                        height: rect.height,
                        resized: false,
                    });
                }
                _ => self.unsize(card, handle, cx),
            }
            return;
        }
        let card = self
            .ready()
            .and_then(|ready| ready.scene.card_at(world, zoom));
        // The first click of the pair already focused the card.
        if event.click_count >= 2
            && let Some(card) = card
        {
            self.gesture = None;
            self.set_focus(Some(card), cx);
            self.open(false, cx);
            return;
        }
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
            Some(Gesture::Minimap) => self.follow_minimap(position, cx),
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
                let origin = Point::new(
                    (world.x - grab.x).clamp(-WORLD_LIMIT, WORLD_LIMIT),
                    (world.y - grab.y).clamp(-WORLD_LIMIT, WORLD_LIMIT),
                );
                if let Load::Ready(ready) = &mut self.load {
                    if ready.view == View::Schema {
                        let table = ready.table(card);
                        ready.placed.entry(table).or_default().origin = Some(origin);
                    }
                    let edges = ready.scene.move_card(card, origin).to_vec();
                    let moved = ready.scene.layout().card_rect(card, Detail::Columns, 1.0);
                    ready.bounds = ready.bounds.union(&moved);
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
            Some(Gesture::Resize {
                card,
                handle,
                from,
                width,
                height,
                ..
            }) => {
                let world = self.transform.to_world(at);
                let zoom = self.transform.zoom;
                if let Load::Ready(ready) = &mut self.load {
                    let (mut sized_width, mut sized_height) = ready.size_of(card);
                    if handle.widens() {
                        sized_width =
                            Some((width + world.x - from.x).clamp(MIN_CARD_WIDTH, MAX_CARD_WIDTH));
                    }
                    if handle.heightens() {
                        let layout = ready.scene.layout();
                        let rows = geometry::rows_for_height(height + world.y - from.y);
                        sized_height = layout.dragged_height(card, rows, layout.detail(zoom));
                    }
                    if (sized_width, sized_height) != ready.size_of(card) {
                        let edges = ready.resize(card, sized_width, sized_height);
                        self.paths.retain(|(edge, _), _| !edges.contains(edge));
                        self.gesture = Some(Gesture::Resize {
                            card,
                            handle,
                            from,
                            width,
                            height,
                            resized: true,
                        });
                        cx.notify();
                    }
                }
            }
            None => {
                let (hover, handle) = match hovered {
                    true => {
                        let world = self.transform.to_world(at);
                        let zoom = self.transform.zoom;
                        self.ready().map_or((None, None), |ready| {
                            (
                                hover_at(&ready.scene, world, zoom),
                                (ready.scene.handle_at(world, HANDLE_REACH / zoom, zoom))
                                    .map(|(_, handle)| handle),
                            )
                        })
                    }
                    false => (None, None),
                };
                if handle != self.handle {
                    self.handle = handle;
                    cx.notify();
                }
                if hover != self.hover {
                    self.hover = hover;
                    if self.focus.is_none() {
                        cx.notify();
                    }
                }
            }
        }
    }

    fn press_minimap(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        self.gesture = Some(Gesture::Minimap);
        self.follow_minimap(position, cx);
    }

    /// Centres the view on the world point under the pointer on the minimap.
    fn follow_minimap(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(minimap) = self.minimap else {
            return;
        };
        let world = minimap.to_world(position);
        let (middle, zoom) = (self.screen().centre(), self.transform.zoom);
        self.transform.pan = Point::new(middle.x - world.x * zoom, middle.y - world.y * zoom);
        cx.notify();
    }

    fn release(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let at = self.local(position);
        match self.gesture.take() {
            Some(Gesture::Pan { start, .. }) if distance(start, at) < CLICK_SLOP => {
                self.set_focus(None, cx);
            }
            Some(Gesture::Pan { .. } | Gesture::Minimap) => cx.emit(DiagramEvent::Viewed),
            Some(Gesture::Card {
                card, moved: false, ..
            }) => self.set_focus(Some(card), cx),
            Some(Gesture::Card { moved: true, .. }) => self.arranged_now(cx),
            Some(Gesture::Resize { resized, .. }) => {
                // The cursor it held goes back.
                cx.notify();
                if resized {
                    self.arranged_now(cx);
                }
            }
            None => {}
        }
    }

    /// Says the schema view's cards were moved or sized; a neighbourhood's
    /// are not kept.
    fn arranged_now(&self, cx: &mut Context<Self>) {
        if self.ready().is_some_and(|ready| ready.view == View::Schema) {
            cx.emit(DiagramEvent::Arranged);
        }
    }

    /// The cards the user moved or sized in the schema view, by table.
    pub fn arranged(&self) -> impl Iterator<Item = (&TableKey, &Placement)> {
        self.ready().into_iter().flat_map(|ready| &ready.placed)
    }

    /// Gives a card back the layout's size along the handle double-clicked:
    /// its width, its height, or at the corner both.
    fn unsize(&mut self, card: u32, handle: Handle, cx: &mut Context<Self>) {
        let Load::Ready(ready) = &mut self.load else {
            return;
        };
        let (mut width, mut height) = ready.size_of(card);
        if handle.widens() {
            width = None;
        }
        if handle.heightens() {
            height = None;
        }
        let edges = ready.resize(card, width, height);
        self.paths.retain(|(edge, _), _| !edges.contains(edge));
        self.arranged_now(cx);
        cx.notify();
    }

    /// The cursor a resize handle wears, and whether a drag holds it, which
    /// keeps it however far the pointer strays.
    fn cursor(&self) -> Option<(CursorStyle, bool)> {
        let (handle, held) = match self.gesture {
            Some(Gesture::Resize { handle, .. }) => (handle, true),
            Some(_) => return None,
            None => (self.handle?, false),
        };
        let style = match handle {
            Handle::Right => CursorStyle::ResizeLeftRight,
            Handle::Bottom => CursorStyle::ResizeUpDown,
            Handle::Corner => CursorStyle::ResizeUpLeftDownRight,
        };
        Some((style, held))
    }

    fn set_focus(&mut self, focus: Option<u32>, cx: &mut Context<Self>) {
        if self.focus != focus {
            self.focus = focus;
            cx.notify();
        }
    }

    /// The focused card's table, opened by the workspace. Nothing in focus,
    /// nothing opens.
    fn open(&mut self, structure: bool, cx: &mut Context<Self>) {
        let Some((schema, table)) = self.focus.and_then(|card| Some(self.ready()?.table(card)))
        else {
            return;
        };
        cx.emit(DiagramEvent::Open {
            schema,
            table,
            structure,
        });
    }

    /// The view switch, Keys only, the zoom and Fit, at the top left.
    fn toolbar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let ready = self.ready()?;
        let (t, inks) = (*theme(cx), popup_theme(cx));
        let neighbourhood = matches!(ready.view, View::Neighbourhood(_));
        let centrable = self.focus.is_some_and(|card| {
            matches!(
                ready.scene.layout().cards[card as usize].subject,
                Subject::Table(_)
            )
        });
        let zoom = self.transform.zoom;
        let tool = |id: &'static str, path: &'static str| {
            ui::icon_button(id, path, Tone::Quiet, Control::Compact, inks)
        };
        Some(
            floating(t)
                .top(px(layout::SPACE_MD))
                .left(px(layout::SPACE_MD))
                .gap(px(layout::SPACE_XS))
                .p(px(layout::SPACE_XS))
                .child(
                    ui::segmented(inks)
                        .child(
                            ui::segmented_choice("diagram-schema", "Schema", !neighbourhood, inks)
                                .on_click(cx.listener(|view, _, _, cx| view.show(false, cx))),
                        )
                        .child(
                            ui::segmented_choice(
                                "diagram-neighbourhood",
                                "Neighbourhood",
                                neighbourhood,
                                inks,
                            )
                            .disabled(!centrable)
                            .tooltip(match (centrable, neighbourhood) {
                                (false, _) => "Focus a table to see its neighbourhood",
                                (true, false) => "The focused table, its parents and children",
                                (true, true) => "Centre on the focused table",
                            })
                            .on_click(cx.listener(|view, _, _, cx| view.show(true, cx))),
                        ),
                )
                .child(rule(inks))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .px(px(layout::SPACE_SM))
                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                        .text_color(inks.text_muted)
                        .whitespace_nowrap()
                        .child("Keys only")
                        .child(
                            ui::switch("diagram-keys-only", self.keys_only, inks).on_click(
                                cx.listener(|view, _, _, cx| {
                                    view.set_keys_only(!view.keys_only, cx)
                                }),
                            ),
                        ),
                )
                .child(rule(inks))
                .child(
                    tool("diagram-zoom-out", icon::MINIMIZE)
                        .tooltip_with_action("Zoom out", &ZoomEditorOut, Some("Diagram"))
                        .on_click(cx.listener(|view, _, _, cx| view.step_zoom(false, cx))),
                )
                .child(
                    ui::button(
                        "diagram-zoom",
                        format!("{:.0}%", zoom * 100.0),
                        Tone::Quiet,
                        Control::Compact,
                        inks,
                    )
                    .w(px(layout::chrome(ZOOM_READOUT_WIDTH)))
                    .tooltip("Zoom to 100%")
                    .on_click(cx.listener(|view, _, _, cx| view.zoom_to(1.0, cx))),
                )
                .child(
                    tool("diagram-zoom-in", icon::PLUS)
                        .tooltip_with_action("Zoom in", &ZoomEditorIn, Some("Diagram"))
                        .on_click(cx.listener(|view, _, _, cx| view.step_zoom(true, cx))),
                )
                .child(
                    tool("diagram-fit", icon::FIT)
                        .tooltip_with_action("Fit", &ResetEditorZoom, Some("Diagram"))
                        .on_click(cx.listener(|view, _, _, cx| view.fit(cx))),
                )
                .child(rule(inks))
                .child(
                    tool("diagram-find", icon::SEARCH)
                        .tooltip_with_action("Find table", &FindInDiagram, Some("Diagram"))
                        .on_click(cx.listener(|view, _, window, cx| view.find(window, cx))),
                )
                .child({
                    let view = cx.entity().downgrade();
                    tool("diagram-export", icon::EXPORT)
                        .debug_selector(|| "diagram-export".into())
                        .tooltip("Export")
                        .dropdown_menu(move |menu, _, _| export_menu(menu, &view))
                }),
        )
    }

    /// Every card as a block and the view as a frame over them, at the bottom
    /// right; a press or a drag on it moves the view. Not shown while every
    /// card, as the zoom draws it, is in view, when there is nowhere to go.
    fn minimap(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let ready = self.ready()?;
        let world = ready.bounds;
        let seen = self.transform.world_rect(self.screen());
        let layout = ready.scene.layout();
        let zoom = self.transform.zoom;
        let drawn = layout.cards_bounds(layout.detail(zoom), zoom);
        if seen.x <= drawn.x
            && seen.y <= drawn.y
            && seen.right() >= drawn.right()
            && seen.bottom() >= drawn.bottom()
        {
            return None;
        }
        let t = *theme(cx);
        let width = layout::chrome(MINIMAP_WIDTH);
        let height = (width * world.height / world.width.max(1.0)).clamp(
            layout::chrome(MINIMAP_MIN_HEIGHT),
            layout::chrome(MINIMAP_MAX_HEIGHT),
        );
        let view = cx.entity();
        Some(
            floating(t)
                .right(px(layout::SPACE_MD))
                .bottom(px(layout::SPACE_MD))
                .w(px(width))
                .h(px(height))
                .overflow_hidden()
                .child(
                    canvas(
                        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                        move |bounds, hitbox: Hitbox, window, cx| {
                            view.update(cx, |diagram, cx| {
                                diagram.paint_minimap(bounds, window, cx)
                            });
                            window.on_mouse_event({
                                let view = view.clone();
                                move |event: &MouseDownEvent, phase, window, cx| {
                                    if phase == DispatchPhase::Bubble
                                        && event.button == MouseButton::Left
                                        && hitbox.is_hovered(window)
                                    {
                                        cx.stop_propagation();
                                        view.update(cx, |view, cx| {
                                            view.press_minimap(event.position, window, cx)
                                        });
                                    }
                                }
                            });
                        },
                    )
                    .size_full(),
                ),
        )
    }

    fn paint_minimap(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let inks = popup_theme(cx);
        let screen = self.screen();
        let Some(ready) = self.ready() else {
            return;
        };
        let world = ready.bounds;
        let room = size(
            f32::from(bounds.size.width) - 2.0 * MINIMAP_PAD,
            f32::from(bounds.size.height) - 2.0 * MINIMAP_PAD,
        );
        let scale = (room.width / world.width.max(1.0)).min(room.height / world.height.max(1.0));
        let drawn = size(world.width * scale, world.height * scale);
        let minimap = Minimap {
            origin: point(
                bounds.origin.x
                    + px(MINIMAP_PAD + (room.width - drawn.width) / 2.0 - world.x * scale),
                bounds.origin.y
                    + px(MINIMAP_PAD + (room.height - drawn.height) / 2.0 - world.y * scale),
            ),
            scale,
        };
        let layout = ready.scene.layout();
        let card: Hsla = inks.text_faint.alpha(MINIMAP_CARD_ALPHA).into();
        let focused: Hsla = inks.accent.into();
        let seen = self.transform.world_rect(screen);
        let focus = self.focus;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for index in 0..layout.cards.len() as u32 {
                let rect = layout.card_rect(index, Detail::Columns, 1.0);
                let fill = match focus == Some(index) {
                    true => focused,
                    false => card,
                };
                window.paint_quad(gpui::fill(minimap.to_screen(rect), fill));
            }
            window.paint_quad(quad(
                minimap.to_screen(seen),
                px(2.0),
                Hsla::from(inks.accent.alpha(layout::ACCENT_WASH)),
                px(1.0),
                Hsla::from(inks.accent),
                BorderStyle::Solid,
            ));
        });
        self.minimap = Some(minimap);
    }

    /// The card in focus, its name and Open data and Structure, at the bottom
    /// left, out of the minimap's corner.
    fn focus_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let card = self.focus?;
        let ready = self.ready()?;
        let (t, inks) = (*theme(cx), popup_theme(cx));
        let (schema, _) = ready.table(card);
        let detail: SharedString = match ready.scene.layout().cards[card as usize].subject {
            Subject::Table(table) => match ready.graph.tables[table as usize].columns.len() {
                1 => "1 column".into(),
                columns => format!("{columns} columns").into(),
            },
            Subject::Stub(_) => format!("in {schema}").into(),
        };
        let action = |id: &'static str, path, label: &'static str, structure: bool| {
            let binding: &dyn gpui::Action = match structure {
                true => &OpenDiagramStructure,
                false => &OpenDiagramTable,
            };
            ui::icon_label_button(id, path, label, Tone::Quiet, Control::Compact, inks)
                .tooltip_with_action(label, binding, Some("Diagram"))
                .on_click(cx.listener(move |view, _, _, cx| view.open(structure, cx)))
        };
        Some(
            floating(t)
                .left(px(layout::SPACE_MD))
                .bottom(px(layout::SPACE_MD))
                .max_w(px(layout::chrome(FOCUS_BAR_MAX_WIDTH)))
                .gap(px(layout::SPACE_SM))
                .py(px(layout::SPACE_XS))
                .pl(px(layout::SPACE_MD))
                .pr(px(layout::SPACE_XS))
                .child(
                    icon(icon::TABLE)
                        .flex_none()
                        .size(px(layout::chrome(layout::ICON_SIZE)))
                        .text_color(inks.accent),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex()
                        .items_baseline()
                        .gap(px(layout::SPACE_SM))
                        .whitespace_nowrap()
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_size(px(layout::chrome(layout::TEXT_MD)))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(inks.text)
                                .child(ready.name(card)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(inks.text_muted)
                                .child(detail),
                        ),
                )
                .child(rule(inks))
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .gap(px(layout::SPACE_XS))
                        .child(action("diagram-open-data", icon::TABLE, "Open data", false))
                        .child(action(
                            "diagram-structure",
                            icon::STRUCTURE,
                            "Structure",
                            true,
                        )),
                ),
        )
    }

    /// Opens the find field, or takes the keyboard back to it.
    fn find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ready().is_none() {
            return;
        }
        let find = match &mut self.find {
            Some(find) => find,
            None => {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find a table…"));
                let changes = cx.subscribe_in(&input, window, Self::find_event);
                self.find.insert(Find {
                    input,
                    query: Query::new(""),
                    fuzzy: Fuzzy::new(),
                    results: Vec::new(),
                    selected: 0,
                    _changes: changes,
                })
            }
        };
        find.input.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn find_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, InputEvent::Change) {
            let text = input.read(cx).value();
            self.search(&text, cx);
        }
    }

    /// Ranks the cards against `text` and jumps to the best.
    fn search(&mut self, text: &str, cx: &mut Context<Self>) {
        self.rank(text);
        match self.find.as_ref().and_then(|find| find.results.first()) {
            Some(&card) => self.jump(card, cx),
            None => cx.notify(),
        }
    }

    /// The find field's matches among the cards on screen, best first.
    fn rank(&mut self, text: &str) {
        let (Load::Ready(ready), Some(find)) = (&self.load, &mut self.find) else {
            return;
        };
        find.query = Query::new(text);
        find.selected = 0;
        find.results = match find.query.is_empty() {
            true => Vec::new(),
            false => {
                let cards = (0..ready.scene.layout().cards.len() as u32)
                    .map(|card| (card, ready.name(card)));
                let no_aliases: &[String] = &[];
                find.fuzzy
                    .rank(
                        &find.query,
                        cards,
                        |(_, name)| name.as_ref(),
                        |_| no_aliases,
                    )
                    .into_iter()
                    .map(|((card, _), _)| card)
                    .collect()
            }
        };
    }

    /// The next match, or the previous one, wrapping.
    fn step_find(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else {
            return;
        };
        let count = find.results.len().min(FIND_SHOWN);
        if count == 0 {
            return;
        }
        find.selected = match forward {
            true => (find.selected + 1) % count,
            false => (find.selected + count - 1) % count,
        };
        let card = find.results[find.selected];
        self.jump(card, cx);
    }

    fn pick_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.find.as_ref().map_or(0, |find| find.selected);
        self.pick(selected, window, cx);
    }

    fn pick(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(&card) = self.find.as_ref().and_then(|find| find.results.get(index)) {
            self.jump(card, cx);
        }
        self.close_find(window, cx);
    }

    /// Puts a card in focus and in the middle of the view, at a zoom its
    /// columns can be read at.
    fn jump(&mut self, card: u32, cx: &mut Context<Self>) {
        let Some(centred) = self.centring(card, self.transform.zoom.max(1.0)) else {
            return;
        };
        self.transform = centred;
        self.focus = Some(card);
        self.hover = None;
        cx.emit(DiagramEvent::Moved);
        cx.notify();
    }

    /// The view at `zoom` with a card in the middle; a card taller than the
    /// view shows its top.
    fn centring(&self, card: u32, zoom: f32) -> Option<Transform> {
        let layout = self.ready()?.scene.layout();
        let rect = layout.card_rect(card, layout.detail(zoom), zoom);
        let area = fit_area(self.screen());
        let middle = area.centre();
        let top = match rect.height * zoom > area.height - 2.0 * FIT_MARGIN {
            true => area.y + FIT_MARGIN - rect.y * zoom,
            false => middle.y - rect.centre().y * zoom,
        };
        Some(Transform {
            zoom,
            pan: Point::new(middle.x - rect.centre().x * zoom, top),
        })
    }

    /// Moves the focus to the nearest card `direction` of the focused one,
    /// or with none in focus, to the card nearest the middle of the view; the
    /// view follows a card that is not wholly on screen.
    fn step_focus(&mut self, direction: Direction, cx: &mut Context<Self>) {
        // The field's caret hands an arrow on at the end of its text, and
        // walking the cards from under the user's typing is not what it meant.
        if self.find.is_some() {
            return;
        }
        let Some(ready) = self.ready() else {
            return;
        };
        let layout = ready.scene.layout();
        let zoom = self.transform.zoom;
        let detail = layout.detail(zoom);
        let rect = |card: u32| layout.card_rect(card, detail, zoom);
        let cards = 0..layout.cards.len() as u32;
        let next = match self.focus {
            Some(from) => geometry::nearest_card(
                rect(from),
                direction,
                cards
                    .filter(|&card| card != from)
                    .map(|card| (card, rect(card))),
            ),
            None => {
                let middle = self.transform.to_world(self.screen().centre());
                cards.min_by(|&a, &b| {
                    distance(rect(a).centre(), middle)
                        .total_cmp(&distance(rect(b).centre(), middle))
                })
            }
        };
        let Some(card) = next else {
            return;
        };
        let (rect, screen) = (rect(card), self.transform.world_rect(self.screen()));
        let seen = screen.x <= rect.x
            && screen.y <= rect.y
            && rect.right() <= screen.right()
            && rect.bottom() <= screen.bottom();
        if !seen && let Some(centred) = self.centring(card, zoom) {
            self.transform = centred;
            cx.emit(DiagramEvent::Moved);
        }
        self.focus = Some(card);
        self.hover = None;
        cx.notify();
    }

    /// Whether the find field is open, for `escape` to close first.
    pub fn finding(&self) -> bool {
        self.find.is_some()
    }

    /// Closes the find field, leaving its match in focus and the keyboard on
    /// the canvas, where `enter` opens it.
    pub fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        self.focus_handle.focus(window, cx);
        cx.emit(DiagramEvent::Viewed);
        cx.notify();
    }

    /// The find field and its best matches, at the top right.
    fn find_card(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (t, inks) = (*theme(cx), popup_theme(cx));
        let (Load::Ready(ready), Some(find)) = (&self.load, &mut self.find) else {
            return None;
        };
        let count: SharedString = match (find.query.is_empty(), find.results.len()) {
            (true, _) => "".into(),
            (false, 0) => "None".into(),
            (false, matches) => matches.to_string().into(),
        };
        let rows = find
            .results
            .iter()
            .take(FIND_SHOWN)
            .enumerate()
            .map(|(index, &card)| {
                let name = ready.name(card);
                let ranges = find.fuzzy.highlights(&find.query, &name);
                let selected = index == find.selected;
                div()
                    .id(("diagram-find-match", index))
                    .h(px(layout::chrome(FIND_ROW_HEIGHT)))
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .px(px(layout::SPACE_SM))
                    .rounded(px(layout::RADIUS_CONTROL))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_size(px(layout::chrome(layout::TEXT_MD)))
                    .text_color(inks.text)
                    .map(|row| match selected {
                        true => row.bg(inks.element_active),
                        false => row.hover(|style| style.bg(inks.element_hover)),
                    })
                    .child(
                        icon(icon::TABLE)
                            .flex_none()
                            .size(px(layout::chrome(layout::ICON_SIZE)))
                            .text_color(match selected {
                                true => inks.accent,
                                false => inks.text_faint,
                            }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(ui::highlighted(name, &ranges, inks)),
                    )
                    .on_click(cx.listener(move |view, _, window, cx| view.pick(index, window, cx)))
            })
            .collect::<Vec<_>>();
        let listed = !rows.is_empty();
        Some(
            floating(t)
                .key_context("DiagramFind")
                .top(px(layout::SPACE_MD))
                .right(px(layout::SPACE_MD))
                .w(px(layout::chrome(FIND_WIDTH)))
                .flex_col()
                .items_stretch()
                .child(
                    div()
                        .h(px(layout::chrome(layout::CONTROL_HEIGHT)))
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .px(px(layout::SPACE_MD))
                        .when(listed, |row| row.border_b_1().border_color(inks.border))
                        .child(
                            icon(icon::SEARCH)
                                .flex_none()
                                .size(px(layout::chrome(layout::ICON_SIZE)))
                                .text_color(inks.text_muted),
                        )
                        .child(
                            div().flex_1().min_w_0().overflow_hidden().child(
                                Input::new(&find.input).appearance(false).w_full().min_w_0(),
                            ),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(inks.text_muted)
                                .child(count),
                        ),
                )
                .when(listed, |card| {
                    card.child(
                        div()
                            .flex()
                            .flex_col()
                            .p(px(layout::SPACE_XS))
                            .children(rows),
                    )
                }),
        )
    }

    /// Whether a card is in focus, for `escape` to clear before it goes on to
    /// whatever else it closes.
    pub fn has_focus(&self) -> bool {
        self.focus.is_some()
    }

    pub fn clear_focus(&mut self, cx: &mut Context<Self>) {
        self.set_focus(None, cx);
    }

    #[cfg(test)]
    pub fn focused_name(&self) -> Option<SharedString> {
        Some(self.ready()?.name(self.focus?))
    }

    /// Where the minimap drew the middle of the scene, while it is shown.
    #[cfg(test)]
    pub fn minimap_middle(&self) -> Option<gpui::Point<Pixels>> {
        let (minimap, world) = (self.minimap?, self.ready()?.bounds);
        Some(minimap.to_screen(world).center())
    }

    /// How far the middle of the view is from the middle of the scene, in
    /// world units.
    #[cfg(test)]
    pub fn off_middle(&self) -> f32 {
        let world = self.ready().map_or(Rect::default(), |ready| ready.bounds);
        distance(
            self.transform.to_world(self.screen().centre()),
            world.centre(),
        )
    }

    #[cfg(test)]
    pub fn zoom(&self) -> f32 {
        self.transform.zoom
    }

    /// Whether anything only drawing needs is held: shaped lines,
    /// tessellated edges, a scene's grid.
    #[cfg(test)]
    pub fn holds_render_caches(&self) -> bool {
        !self.lines.is_empty()
            || !self.paths.is_empty()
            || self.ready().is_some_and(|ready| {
                ready.scene.is_filed()
                    || (ready.stash.as_ref()).is_some_and(|(schema, _)| schema.is_filed())
            })
    }

    /// The tables on screen and whether their cards were given room for keys
    /// only, once laid out.
    #[cfg(test)]
    pub fn laid_out(&self) -> Option<(Vec<SharedString>, bool)> {
        let ready = self.ready()?;
        let layout = ready.scene.layout();
        let names = (0..layout.cards.len() as u32).map(|card| ready.name(card));
        Some((names.collect(), layout.keys_only))
    }

    #[cfg(test)]
    pub fn show_neighbourhood(&mut self, neighbourhood: bool, cx: &mut Context<Self>) {
        self.show(neighbourhood, cx);
    }

    #[cfg(test)]
    pub fn toggle_keys_only(&mut self, cx: &mut Context<Self>) {
        self.set_keys_only(!self.keys_only, cx);
    }

    /// Where a table's card is in the world, its top-left corner.
    #[cfg(test)]
    pub fn table_origin(&self, name: &str) -> Option<(f32, f32)> {
        let ready = self.ready()?;
        let layout = ready.scene.layout();
        let card = (0..layout.cards.len() as u32).find(|&card| ready.name(card) == name)?;
        let origin = layout.cards[card as usize].origin;
        Some((origin.x, origin.y))
    }

    /// A table's card's width and height, in world units.
    #[cfg(test)]
    pub fn table_size(&self, name: &str) -> Option<(f32, f32)> {
        let ready = self.ready()?;
        let layout = ready.scene.layout();
        let card = (0..layout.cards.len() as u32).find(|&card| ready.name(card) == name)?;
        let card = &layout.cards[card as usize];
        Some((card.width, card.height))
    }

    /// Just inside a table's card's bottom-right corner, in window
    /// coordinates, once drawn.
    #[cfg(test)]
    pub fn table_corner_on_screen(&self, name: &str) -> Option<gpui::Point<Pixels>> {
        let ready = self.ready()?;
        let layout = ready.scene.layout();
        let card = (0..layout.cards.len() as u32).find(|&card| ready.name(card) == name)?;
        let zoom = self.transform.zoom;
        let rect = layout.card_rect(card, layout.detail(zoom), zoom);
        let at = self
            .transform
            .to_screen(Point::new(rect.right(), rect.bottom()));
        Some(point(
            self.viewport.origin.x + px(at.x - 1.0),
            self.viewport.origin.y + px(at.y - 1.0),
        ))
    }

    /// Where a table's card is on screen, in window coordinates, once drawn.
    #[cfg(test)]
    pub fn table_on_screen(&self, name: &str) -> Option<gpui::Point<Pixels>> {
        let ready = self.ready()?;
        let layout = ready.scene.layout();
        let card = (0..layout.cards.len() as u32).find(|&card| ready.name(card) == name)?;
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
        cx.emit(DiagramEvent::Moved);
        cx.notify();
    }

    fn pinch(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        self.transform = self.transform.zoomed_at(
            self.local(event.position),
            self.transform.zoom * (1.0 + event.delta),
        );
        cx.emit(DiagramEvent::Moved);
        cx.notify();
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.hidden = false;
        if let Load::Ready(ready) = &mut self.load {
            ready.scene.refile();
        }
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
            more_labels,
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
                    EdgeLook::Plain => t.edge_ink().into(),
                    EdgeLook::Lit => t.accent.into(),
                    EdgeLook::Dimmed => t.edge_ink().alpha(DIM).into(),
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

            let washed = joined_rows(ready, focus, hover);
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
                let (title, title_ellipsis) = label(
                    lines,
                    stamp,
                    &inks,
                    ready.name(card),
                    Pen::new(Face::Title, name_size, dimmed),
                    window,
                );
                let room = header.size.width - px(2.0 * pad);
                paint_label(
                    &title,
                    &title_ellipsis,
                    fit(&title, &title_ellipsis, room),
                    point(header.origin.x + px(pad), header.origin.y),
                    header.size.height,
                    window,
                    cx,
                );

                let Subject::Table(table) = layout.cards[card as usize].subject else {
                    continue;
                };
                let columns = &ready.graph.tables[table as usize].columns;
                let type_size = geometry::text_size(zoom, layout::grid(layout::TEXT_SM));
                let (column_pen, type_pen) = (
                    Pen::new(Face::Column, name_size, dimmed),
                    Pen::new(Face::Type, type_size, dimmed),
                );
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
                            Hsla::from(inks.accent.alpha(JOINED_ROW_WASH)),
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
                    let right = row_bounds.origin.x + row_bounds.size.width - px(pad);
                    let mut room = right - name_x;
                    let (name, name_ellipsis) =
                        label(lines, stamp, &inks, column.name.clone(), column_pen, window);
                    if detail.types() {
                        let type_label = match column.nullable {
                            true => ready.nullable_types[column.type_label as usize].clone(),
                            false => ready.graph.types[column.type_label as usize].clone(),
                        };
                        let (label, label_ellipsis) =
                            label(lines, stamp, &inks, type_label, type_pen, window);
                        let gap = ICON_GAP * zoom;
                        let label_room = geometry::type_room(
                            f32::from(room),
                            f32::from(name.width()),
                            f32::from(label.width()),
                            gap,
                        );
                        let label_fit = fit(&label, &label_ellipsis, px(label_room));
                        let drawn = px(label_fit
                            .width(f32::from(label.width()), f32::from(label_ellipsis.width())));
                        paint_label(
                            &label,
                            &label_ellipsis,
                            label_fit,
                            point(right - drawn, row_bounds.origin.y),
                            row_bounds.size.height,
                            window,
                            cx,
                        );
                        if drawn > px(0.0) {
                            room -= drawn + px(gap);
                        }
                    }
                    paint_label(
                        &name,
                        &name_ellipsis,
                        fit(&name, &name_ellipsis, room),
                        point(name_x, row_bounds.origin.y),
                        row_bounds.size.height,
                        window,
                        cx,
                    );
                }
                if let Some(more) = layout.more(card, detail) {
                    let row_bounds = Bounds {
                        origin: to_screen(Point::new(more.rect.x, more.rect.y)),
                        size: size(px(more.rect.width * zoom), px(more.rect.height * zoom)),
                    };
                    // A key into a column the cut hid ends here, so this row
                    // is what lights for it.
                    if washed.iter().any(|&(washed_card, column)| {
                        washed_card == card && layout.row_of(card, column, detail) == Some(more.row)
                    }) {
                        window.paint_quad(quad(
                            row_bounds,
                            px(0.0),
                            Hsla::from(inks.accent.alpha(JOINED_ROW_WASH)),
                            px(0.0),
                            Hsla::from(t.border),
                            BorderStyle::Solid,
                        ));
                    }
                    let text = more_labels
                        .entry(more.hidden)
                        .or_insert_with(|| format!("+{} more", more.hidden).into())
                        .clone();
                    let (line, ellipsis) = label(
                        lines,
                        stamp,
                        &inks,
                        text,
                        Pen::new(Face::More, name_size, dimmed),
                        window,
                    );
                    let icon_size = (name_size * 0.9).min(more.rect.height * zoom * 0.7);
                    let x = row_bounds.origin.x + px(pad + icon_size + ICON_GAP * zoom);
                    let room = row_bounds.origin.x + row_bounds.size.width - px(pad) - x;
                    paint_label(
                        &line,
                        &ellipsis,
                        fit(&line, &ellipsis, room),
                        point(x, row_bounds.origin.y),
                        row_bounds.size.height,
                        window,
                        cx,
                    );
                }
            }
        });
    }

    fn canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let placed = view.clone();
        canvas(
            move |bounds, window, cx| {
                placed.update(cx, |view, cx| view.place(bounds, window, cx));
                window.insert_hitbox(bounds, HitboxBehavior::Normal)
            },
            move |bounds, hitbox: Hitbox, window, cx| {
                view.update(cx, |diagram, cx| diagram.paint(bounds, window, cx));
                match view.read(cx).cursor() {
                    Some((style, true)) => window.set_window_cursor_style(style),
                    Some((style, false)) => window.set_cursor_style(style, &hitbox),
                    None => {}
                }
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
                view.update(cx, |view, cx| view.press(event, window, cx));
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
        let surface =
            div()
                .id("diagram")
                .key_context("Diagram")
                .track_focus(&self.focus_handle)
                .on_action(cx.listener(|view, _: &OpenDiagramTable, _, cx| view.open(false, cx)))
                .on_action(cx.listener(|view, _: &OpenDiagramStructure, _, cx| view.open(true, cx)))
                // The app's zoom chords, which zoom whatever is in front: here,
                // the diagram, and Reset Zoom fits it.
                .on_action(cx.listener(|view, _: &ZoomEditorIn, _, cx| view.step_zoom(true, cx)))
                .on_action(cx.listener(|view, _: &ZoomEditorOut, _, cx| view.step_zoom(false, cx)))
                .on_action(cx.listener(|view, _: &ResetEditorZoom, _, cx| view.fit(cx)))
                .on_action(cx.listener(|view, _: &FindInDiagram, window, cx| view.find(window, cx)))
                .on_action(cx.listener(|view, _: &DiagramFocusLeft, _, cx| {
                    view.step_focus(Direction::Left, cx)
                }))
                .on_action(cx.listener(|view, _: &DiagramFocusRight, _, cx| {
                    view.step_focus(Direction::Right, cx)
                }))
                .on_action(
                    cx.listener(|view, _: &DiagramFocusUp, _, cx| {
                        view.step_focus(Direction::Up, cx)
                    }),
                )
                .on_action(cx.listener(|view, _: &DiagramFocusDown, _, cx| {
                    view.step_focus(Direction::Down, cx)
                }))
                .on_action(cx.listener(|view, _: &DiagramFindNext, _, cx| view.step_find(true, cx)))
                .on_action(
                    cx.listener(|view, _: &DiagramFindPrevious, _, cx| view.step_find(false, cx)),
                )
                .on_action(cx.listener(|view, _: &DiagramFindPick, window, cx| {
                    view.pick_selected(window, cx)
                }))
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
            Load::Ready(_) => {
                let minimap = self.minimap(cx);
                if minimap.is_none() {
                    self.minimap = None;
                }
                surface
                    .relative()
                    .child(self.canvas(cx))
                    .children(self.toolbar(cx))
                    .children(self.find_card(cx))
                    .children(self.focus_bar(cx))
                    .children(minimap)
            }
        }
    }
}

/// How far down the canvas the floating toolbar reaches: its offset, its
/// border and padding, and the segmented track, its tallest control.
fn toolbar_bottom() -> f32 {
    layout::SPACE_MD
        + 2.0 * (1.0 + layout::SPACE_XS + layout::SEGMENT_INSET)
        + layout::chrome(layout::CONTROL_HEIGHT_COMPACT)
}

/// The part of the canvas a fit fills: below the toolbar, which floats over
/// the top of it. The focus bar comes and goes with the focus, and the
/// minimap keeps to a corner, so neither is reserved.
fn fit_area(screen: Rect) -> Rect {
    let top = toolbar_bottom().min(screen.height);
    Rect {
        y: screen.y + top,
        height: screen.height - top,
        ..screen
    }
}

/// The toolbar's Export menu, laid out as the result grid's is: the
/// clipboard first, then the files.
fn export_menu(menu: PopupMenu, view: &WeakEntity<DiagramView>) -> PopupMenu {
    let copy = view.clone();
    let menu = menu
        .item(
            PopupMenuItem::new("Copy as Mermaid").on_click(move |_, _, cx| {
                _ = copy.update(cx, |_, cx| cx.emit(DiagramEvent::CopyMermaid));
            }),
        )
        .separator();
    DiagramFile::ALL.into_iter().fold(menu, |menu, file| {
        let view = view.clone();
        menu.item(
            PopupMenuItem::new(format!("Export {}\u{2026}", file.label())).on_click(
                move |_, _, cx| {
                    _ = view.update(cx, |_, cx| cx.emit(DiagramEvent::Export(file)));
                },
            ),
        )
    })
}

/// A theme's tones for a file. A file has no backdrop for glass to show, so
/// every plane is the theme's own opaque tone, and the washes are flattened
/// onto the card they sit on.
fn file_palette(t: Theme) -> svg::Palette {
    let card = t.overlay;
    svg::Palette {
        canvas: t.panel,
        card,
        header: t.element_hover.flatten(card),
        border: t.border.flatten(card),
        text: t.text,
        muted: t.text_muted,
        edge: t.edge_ink(),
        primary: t.accent,
        foreign: t.syntax_type,
    }
}

/// The hairline between groups of a floating card's controls.
fn rule(t: Theme) -> gpui::Div {
    div()
        .flex_none()
        .w(px(1.0))
        .h(px(
            layout::chrome(layout::CONTROL_HEIGHT_COMPACT) - layout::SPACE_SM
        ))
        .mx(px(layout::SPACE_XS))
        .bg(t.border)
}

/// A frosted card floating over the canvas. `occlude`, so a press on it never
/// reaches the canvas beneath and starts a pan.
fn floating(t: Theme) -> gpui::Div {
    div()
        .absolute()
        .occlude()
        .flex()
        .items_center()
        .rounded(px(layout::RADIUS_PANEL))
        .border_1()
        .border_color(t.border)
        .map(|card| ui::frosted_card(card, t))
}

/// How a shaped label fits `room`, cut with `ellipsis` when it must be.
fn fit(line: &ShapedLine, ellipsis: &ShapedLine, room: Pixels) -> Fit {
    let starts = (line.runs.iter())
        .flat_map(|run| &run.glyphs)
        .map(|glyph| (glyph.index, f32::from(glyph.position.x)));
    Fit::of(
        &line.text,
        f32::from(line.width()),
        f32::from(room),
        f32::from(ellipsis.width()),
        starts,
    )
}

/// A label painted from `origin` as `fit` says: whole, or clipped where its
/// cut falls with the ellipsis after it, so a cut never shapes new text.
fn paint_label(
    line: &ShapedLine,
    ellipsis: &ShapedLine,
    fit: Fit,
    origin: gpui::Point<Pixels>,
    height: Pixels,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    match fit {
        Fit::Whole => {
            let _ = line.paint(origin, height, TextAlign::Left, None, window, cx);
        }
        Fit::Cut(x) => {
            let kept = Bounds {
                origin,
                size: size(px(x), height),
            };
            window.with_content_mask(Some(ContentMask { bounds: kept }), |window| {
                let _ = line.paint(origin, height, TextAlign::Left, None, window, cx);
            });
            let at = point(origin.x + px(x), origin.y);
            let _ = ellipsis.paint(at, height, TextAlign::Left, None, window, cx);
        }
        Fit::Hidden => {}
    }
}

/// A label shaped in `pen`, and the ellipsis it is cut with, both cached.
fn label(
    lines: &mut HashMap<LineKey, ShapedLine>,
    stamp: &Stamp,
    inks: &Theme,
    text: SharedString,
    pen: Pen,
    window: &mut Window,
) -> (ShapedLine, ShapedLine) {
    (
        shape(lines, stamp, inks, text, pen, window),
        shape(lines, stamp, inks, ELLIPSIS.into(), pen, window),
    )
}

fn shape(
    lines: &mut HashMap<LineKey, ShapedLine>,
    stamp: &Stamp,
    inks: &Theme,
    text: SharedString,
    pen: Pen,
    window: &mut Window,
) -> ShapedLine {
    let key = LineKey { text, pen };
    if let Some(line) = lines.get(&key) {
        return line.clone();
    }
    let family = match pen.face {
        Face::Type => stamp.grid.clone(),
        Face::Title | Face::Column | Face::More => stamp.chrome.clone(),
    };
    let mut font: Font = gpui::font(family);
    if pen.face == Face::Title {
        font.weight = FontWeight::SEMIBOLD;
    }
    let colour = match pen.face {
        Face::Title | Face::Column => inks.text,
        Face::Type | Face::More => inks.text_muted,
    };
    let colour: Hsla = match pen.dimmed {
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
    let line = window.text_system().shape_line(
        key.text.clone(),
        px(f32::from_bits(pen.size)),
        &[run],
        None,
    );
    lines.insert(key, line.clone());
    line
}

/// The next step in or out from `zoom`, which need not be a step itself.
fn zoom_step(zoom: f32, zoom_in: bool) -> f32 {
    // A pinch can leave the zoom a hair off a step; that counts as on it.
    const NEAR: f32 = 0.005;
    match zoom_in {
        true => ZOOM_STEPS
            .into_iter()
            .find(|&step| step > zoom + NEAR)
            .unwrap_or(geometry::MAX_ZOOM),
        false => ZOOM_STEPS
            .into_iter()
            .rev()
            .find(|&step| step < zoom - NEAR)
            .unwrap_or(geometry::MIN_ZOOM),
    }
}

fn card_of(layout: &Layout, subject: Subject) -> Option<u32> {
    layout
        .cards
        .iter()
        .position(|card| card.subject == subject)
        .map(|card| card as u32)
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

/// What lies under a world point: a card, else the nearest edge within
/// [`EDGE_REACH`] pixels.
fn hover_at(scene: &Scene, world: Point, zoom: f32) -> Option<Hover> {
    match scene.card_at(world, zoom) {
        Some(card) => Some(Hover::Card(card)),
        None => scene
            .edge_at(world, EDGE_REACH / zoom, zoom)
            .map(Hover::Edge),
    }
}

/// An edge under the focus, or with nothing in focus, under the hover.
fn edge_look(layout: &Layout, focus: Option<u32>, hover: Option<Hover>, edge: u32) -> EdgeLook {
    let touches = |card: u32| other_end(layout, edge, card).is_some();
    match (focus, hover) {
        (Some(focus), _) if touches(focus) => EdgeLook::Lit,
        (Some(_), _) => EdgeLook::Dimmed,
        (None, Some(Hover::Card(card))) if touches(card) => EdgeLook::Lit,
        (None, Some(Hover::Edge(hovered))) if hovered == edge => EdgeLook::Lit,
        (None, _) => EdgeLook::Plain,
    }
}

/// The column rows at both ends of the focus's keys, or with nothing in
/// focus of the hovered edge's, every column of a composite key included:
/// `(card, column)`.
fn joined_rows(ready: &Ready, focus: Option<u32>, hover: Option<Hover>) -> HashSet<(u32, u32)> {
    let layout = ready.scene.layout();
    let edges = match (focus, hover) {
        (Some(focus), _) => layout.cards[focus as usize].edges.as_slice(),
        (None, Some(Hover::Edge(edge))) => &[edge][..],
        (None, _) => return HashSet::new(),
    };
    let mut rows = HashSet::new();
    for &edge in edges {
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
    use crate::theme::color::contrast_ratio;

    /// A file has no glass, so its tones are the theme's opaque ones, and
    /// what is read on them holds the floors the canvas holds to.
    #[test]
    fn a_files_inks_hold_on_its_opaque_cards_in_every_theme() {
        for t in Theme::all() {
            let p = file_palette(t);
            for (what, ink, plane, floor) in [
                ("a name on a card", p.text, p.card, 4.5),
                ("a name on a header", p.text, p.header, 4.5),
                ("a type on a card", p.muted, p.card, 3.0),
                ("a primary key icon", p.primary, p.card, 3.0),
                ("a foreign key icon", p.foreign, p.card, 3.0),
                ("an edge on the canvas", p.edge, p.canvas, 3.0),
            ] {
                let ratio = contrast_ratio(ink, plane);
                assert!(ratio >= floor, "{}: {what} at {ratio:.2}", t.name);
            }
        }
    }

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
    fn a_sized_card_keeps_its_size_through_a_new_layout_until_its_table_goes() {
        let mut graph = crate::diagram::tests::graph(3, &[(1, 0), (2, 1)]);
        let note = graph.tables[1].columns[0].clone();
        graph.tables[1].columns.push(crate::db::GraphColumn {
            name: "note".into(),
            primary: false,
            unique: false,
            ..note
        });
        let mut ready = Ready::new(graph.clone(), Scene::new(layout::schema(&graph, false)));
        let short = geometry::card_height(2);
        let t1 = card_of(ready.scene.layout(), 1);
        ready.resize(t1, Some(320.0), Some(short));
        assert_eq!(ready.size_of(t1), (Some(320.0), Some(short)));
        let sized = |ready: &Ready| {
            let layout = ready.scene.layout();
            let card = &layout.cards[card_of(layout, 1) as usize];
            (card.width, card.height)
        };
        assert_eq!(sized(&ready), (320.0, short));

        ready.scene = Scene::new(layout::schema(&graph, true));
        ready.place_saved();
        assert_eq!(sized(&ready), (320.0, short), "keys only keeps the size");

        let t1 = card_of(ready.scene.layout(), 1);
        ready.resize(t1, None, Some(short));
        assert_eq!(sized(&ready).0, CARD_WIDTH, "the width alone given back");
        ready.resize(t1, None, None);
        assert!(ready.placed.is_empty(), "nothing left to remember");

        ready.resize(t1, Some(300.0), None);
        let mut placed = ready.placed.clone();
        let gone: TableKey = ("public".into(), "gone".into());
        placed.insert(
            gone.clone(),
            Placement {
                origin: Some(Point::new(5.0, 5.0)),
                ..Placement::default()
            },
        );
        let fewer = crate::diagram::tests::graph(2, &[(1, 0)]);
        let mut fresh = Ready::new(fewer.clone(), Scene::new(layout::schema(&fewer, false)));
        fresh.adopt(placed);
        assert_eq!(sized(&fresh).0, 300.0, "a new read puts the size back");
        assert!(
            !fresh.placed.contains_key(&gone),
            "a dropped table is forgotten"
        );
    }

    #[test]
    fn a_fit_puts_the_diagram_below_the_toolbar() {
        let screen = Rect {
            x: 0.0,
            y: 0.0,
            width: 1200.0,
            height: 800.0,
        };
        for graph in [
            crate::diagram::tests::graph(3, &[(1, 0), (2, 1)]),
            crate::diagram::tests::synthetic(300, 600),
        ] {
            let layout = layout::schema(&graph, false);
            let fit = layout.fitting(fit_area(screen), FIT_MARGIN);
            let detail = layout.detail(fit.zoom);
            let top = layout.cards_bounds(detail, fit.zoom).y * fit.zoom + fit.pan.y;
            assert!(
                top >= toolbar_bottom() + FIT_MARGIN - 0.5,
                "{top} at {}",
                fit.zoom
            );
        }
    }

    #[test]
    fn zoom_steps_land_on_round_numbers_from_anywhere() {
        assert_eq!(zoom_step(1.0, true), 1.25);
        assert_eq!(zoom_step(1.0, false), 0.9);
        assert_eq!(
            zoom_step(0.8, true),
            0.9,
            "a pinch's zoom goes to the next step"
        );
        assert_eq!(zoom_step(0.8, false), 0.75);
        assert_eq!(zoom_step(1.002, true), 1.25, "a hair off a step is on it");
        assert_eq!(zoom_step(geometry::MAX_ZOOM, true), geometry::MAX_ZOOM);
        assert_eq!(zoom_step(geometry::MIN_ZOOM, false), geometry::MIN_ZOOM);
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
        let card = |card| Some(Hover::Card(card));
        assert_eq!(edge_look(&layout, None, None, near), EdgeLook::Plain);
        assert_eq!(edge_look(&layout, None, card(t0), near), EdgeLook::Lit);
        assert_eq!(edge_look(&layout, None, card(t0), far), EdgeLook::Plain);
        assert_eq!(edge_look(&layout, Some(t0), None, near), EdgeLook::Lit);
        assert_eq!(
            edge_look(&layout, Some(t0), card(t2), far),
            EdgeLook::Dimmed,
            "the hover gives way to the focus"
        );
        assert_eq!(edge_look(&layout, Some(t3), None, near), EdgeLook::Dimmed);

        let hovered = Some(Hover::Edge(far));
        assert_eq!(edge_look(&layout, None, hovered, far), EdgeLook::Lit);
        assert_eq!(edge_look(&layout, None, hovered, near), EdgeLook::Plain);
        assert_eq!(edge_look(&layout, Some(t0), hovered, far), EdgeLook::Dimmed);
    }

    #[test]
    fn the_pointer_finds_a_card_before_an_edge_and_an_edge_within_reach() {
        let graph = crate::diagram::tests::graph(2, &[(1, 0)]);
        let scene = Scene::new(layout::schema(&graph, false));
        let layout = scene.layout();
        for zoom in [1.0, 0.7, 0.3] {
            let detail = layout.detail(zoom);
            let on_card = layout.card_rect(1, detail, zoom).centre();
            assert_eq!(hover_at(&scene, on_card, zoom), Some(Hover::Card(1)));
            let points = layout.edge_path(0, detail, zoom).polyline();
            let middle = points[points.len() / 2];
            assert_eq!(hover_at(&scene, middle, zoom), Some(Hover::Edge(0)));
            // The reach is in pixels, so it covers more of the world zoomed out.
            let near = Point::new(middle.x, middle.y + (EDGE_REACH - 1.0) / zoom);
            let far = Point::new(middle.x, middle.y + (EDGE_REACH + 2.0) / zoom);
            assert_eq!(hover_at(&scene, near, zoom), Some(Hover::Edge(0)), "{zoom}");
            assert_eq!(hover_at(&scene, far, zoom), None, "{zoom}");
        }
    }

    #[test]
    fn a_hovered_edge_washes_the_rows_it_joins_and_the_focus_wins() {
        // `t2.t0_id` and `t1.t0_id` both reference `t0.id`.
        let graph = crate::diagram::tests::graph(3, &[(1, 0), (2, 0)]);
        let scene = Scene::new(layout::schema(&graph, false));
        let ready = Ready::new(graph, scene);
        let layout = ready.scene.layout();
        let [t0, t1, t2] = [0, 1, 2].map(|table| card_of(layout, table));
        let edge = layout
            .edges
            .iter()
            .position(|edge| edge.child == t2)
            .unwrap() as u32;
        let rows = |focus, hover| {
            let mut rows: Vec<_> = joined_rows(&ready, focus, hover).into_iter().collect();
            rows.sort();
            rows
        };
        assert!(rows(None, None).is_empty());
        assert!(rows(None, Some(Hover::Card(t2))).is_empty());
        let mut joined = vec![(t0, 0), (t2, 1)];
        joined.sort();
        assert_eq!(rows(None, Some(Hover::Edge(edge))), joined);
        let mut focused = vec![(t0, 0), (t1, 1)];
        focused.sort();
        assert_eq!(rows(Some(t1), Some(Hover::Edge(edge))), focused);
    }
}
