//! The diagram's shapes in world units, where one unit is one pixel at 100%,
//! and the transform that puts them on screen.

use super::layout::{Card, Edge, Layout};

pub const CARD_WIDTH: f32 = 216.0;
/// The narrowest a card can be dragged to: room for a few characters of its
/// name and the ellipsis.
pub const MIN_CARD_WIDTH: f32 = 120.0;
pub const MAX_CARD_WIDTH: f32 = 960.0;
pub const HEADER_HEIGHT: f32 = 34.0;
pub const ROW_HEIGHT: f32 = 24.0;
pub const BOTTOM_PAD: f32 = 8.0;
/// The most rows a card is laid out with, its "+N more" row included: a
/// table of hundreds of columns would otherwise stand as a wall that pushes
/// its layer apart and that Fit and the minimap have to make room for. A
/// drag can still make it as tall as its rows.
pub const AUTO_ROWS: usize = 30;
/// A name block's height on screen, at every zoom that draws one.
pub const NAME_BLOCK_HEIGHT: f32 = 26.0;
/// Labels are cut with an ellipsis below this size rather than drawn smaller.
pub const MIN_TEXT_SIZE: f32 = 11.0;
pub const MIN_ZOOM: f32 = 0.05;
pub const MAX_ZOOM: f32 = 4.0;
/// How far from the origin a card may be put, either way on either axis:
/// past any layout of a few thousand tables, and short of where `f32` loses
/// the precision a pixel needs.
pub const WORLD_LIMIT: f32 = 1.0e6;

/// Between the levels the design names (100%, about 70%, 45% and below).
const KEYS_BELOW: f32 = 0.85;
pub const NAMES_BELOW: f32 = 0.55;

/// The text sizes a zoom may pick, as multiples of the user's base size. gpui
/// keeps a raster per glyph per size and never evicts one, so text shaped at
/// the continuous zoom would grow its atlas without bound.
const TEXT_STEPS: [f32; 8] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];

/// How far a self-reference's control points reach right of its card. The
/// layer gap is wider, so the loop never reaches the next layer's cards.
pub(super) const LOOP_CONTROL: f32 = 40.0;
/// A curve's control points sit this share of the horizontal distance out
/// from each end. At most a half keeps both inside the gap the curve crosses,
/// which is what keeps it from passing under a card.
const TANGENT: f32 = 0.5;
const NAMES_TANGENT: f32 = 0.2;
const MIN_CONTROL: f32 = 24.0;
const CURVE_STEPS: usize = 16;

const MARK_HALF: f32 = 6.0;
const CROWS_FOOT: f32 = 12.0;
const BAR_NEAR: f32 = 8.0;
const BAR_FAR: f32 = 14.0;
const CIRCLE_AT: f32 = 18.0;
const CIRCLE_RADIUS: f32 = 4.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub fn centre(&self) -> Point {
        Point::new(self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    pub fn contains(&self, point: Point) -> bool {
        (self.x..=self.right()).contains(&point.x) && (self.y..=self.bottom()).contains(&point.y)
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }

    pub fn union(&self, other: &Rect) -> Rect {
        Rect::spanning(
            Point::new(self.x.min(other.x), self.y.min(other.y)),
            Point::new(
                self.right().max(other.right()),
                self.bottom().max(other.bottom()),
            ),
        )
    }

    pub fn inflate(&self, by: f32) -> Rect {
        Rect {
            x: self.x - by,
            y: self.y - by,
            width: self.width + 2.0 * by,
            height: self.height + 2.0 * by,
        }
    }

    fn spanning(a: Point, b: Point) -> Rect {
        Rect {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
            width: (a.x - b.x).abs(),
            height: (a.y - b.y).abs(),
        }
    }
}

/// What a card shows at a zoom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detail {
    /// Every column with its type, and the marks.
    Columns,
    /// The name and the key columns, without types. Keys-only is this at
    /// every zoom that shows rows at all.
    Keys,
    /// A name block. Edges meet its sides, with no marks.
    Names,
}

impl Detail {
    pub fn at(zoom: f32, keys_only: bool) -> Self {
        if zoom < NAMES_BELOW {
            Self::Names
        } else if keys_only || zoom < KEYS_BELOW {
            Self::Keys
        } else {
            Self::Columns
        }
    }

    pub fn types(self) -> bool {
        self == Self::Columns
    }

    pub fn marks(self) -> bool {
        self != Self::Names
    }
}

/// The text size for a zoom: the largest step that does not outgrow the
/// zoomed row, never under [`MIN_TEXT_SIZE`]. `base` is the user's own size.
pub fn text_size(zoom: f32, base: f32) -> f32 {
    let step = TEXT_STEPS
        .iter()
        .rev()
        .find(|&&step| step <= zoom)
        .unwrap_or(&TEXT_STEPS[0]);
    (base * step).max(MIN_TEXT_SIZE)
}

/// How a label fits the room it is given.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fit {
    Whole,
    /// Drawn up to the character that starts this far in, with the ellipsis
    /// there.
    Cut(f32),
    /// Not even the ellipsis fits.
    Hidden,
}

impl Fit {
    /// A label shaped `width` wide, cut so it and an `ellipsis` fit `room`.
    /// `starts` are where its characters start, in order; a cut never leaves
    /// a space before the ellipsis.
    pub fn of(
        text: &str,
        width: f32,
        room: f32,
        ellipsis: f32,
        starts: impl Iterator<Item = (usize, f32)>,
    ) -> Self {
        if width <= room {
            return Self::Whole;
        }
        if ellipsis > room {
            return Self::Hidden;
        }
        let mut cut = 0.0;
        for (at, x) in starts {
            if x + ellipsis > room {
                break;
            }
            if text.get(..at).is_some_and(|kept| {
                kept.chars()
                    .next_back()
                    .is_some_and(|last| !last.is_whitespace())
            }) {
                cut = x;
            }
        }
        Self::Cut(cut)
    }

    /// Whether the label as fitted shows at least `chars` of its
    /// characters, or the whole of a shorter one. `starts` are where its
    /// characters start, as [`Self::of`] takes them.
    pub fn shows(self, chars: usize, starts: impl Iterator<Item = (usize, f32)>) -> bool {
        match self {
            Self::Whole => true,
            Self::Cut(x) => starts.filter(|&(_, start)| start < x).count() >= chars,
            Self::Hidden => false,
        }
    }

    /// The label's width as drawn.
    pub fn width(self, width: f32, ellipsis: f32) -> f32 {
        match self {
            Self::Whole => width,
            Self::Cut(x) => x + ellipsis,
            Self::Hidden => 0.0,
        }
    }
}

/// The share of a row a column's type keeps when the name and type together
/// overflow it, so a long name cannot push the type out entirely.
const TYPE_SHARE: f32 = 0.4;

/// The room a column's type gets in a row `room` wide: all it needs when it
/// fits beside the name, else whatever the name leaves, but never under
/// [`TYPE_SHARE`] of the row. The name takes what the type does not.
pub fn type_room(room: f32, name: f32, label: f32, gap: f32) -> f32 {
    label
        .min((room - gap - name).max(room * TYPE_SHARE))
        .max(0.0)
}

pub fn card_height(rows: usize) -> f32 {
    match rows {
        0 => HEADER_HEIGHT,
        rows => HEADER_HEIGHT + rows as f32 * ROW_HEIGHT + BOTTOM_PAD,
    }
}

/// The height a card listing `rows` is laid out with: see [`AUTO_ROWS`].
pub fn auto_height(rows: usize) -> f32 {
    card_height(rows.min(AUTO_ROWS))
}

/// The fewest rows a card is cut to: a column and the "+N more" row after
/// it, since a card of a name and a count says nothing of its columns.
const MIN_ROWS: usize = 2;

/// The whole rows nearest a height a card is dragged to, never fewer than
/// [`MIN_ROWS`].
pub fn rows_for_height(height: f32) -> usize {
    (((height - HEADER_HEIGHT - BOTTOM_PAD) / ROW_HEIGHT)
        .round()
        .max(0.0) as usize)
        .max(MIN_ROWS)
}

/// An edge of a card that a drag resizes it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Right,
    Bottom,
    Corner,
}

impl Handle {
    pub fn widens(self) -> bool {
        self != Self::Bottom
    }

    pub fn heightens(self) -> bool {
        self != Self::Right
    }
}

/// The handle of a card drawn as `rect` that `point` is on: within `reach`
/// of its right or bottom edge, either side of it, and within twice that of
/// the corner. The reach never takes more than a quarter of the card, so a
/// card small on screen can still be grabbed to move. `heightens` is false
/// where the card's height cannot change, which leaves the right edge only.
pub fn handle_at(rect: Rect, point: Point, reach: f32, heightens: bool) -> Option<Handle> {
    let (quarter_x, quarter_y) = (rect.width / 4.0, rect.height / 4.0);
    let (reach_x, reach_y) = (reach.min(quarter_x), reach.min(quarter_y));
    let (dx, dy) = (
        (point.x - rect.right()).abs(),
        (point.y - rect.bottom()).abs(),
    );
    let across = point.x >= rect.x && point.x <= rect.right() + reach_x;
    let down = point.y >= rect.y && point.y <= rect.bottom() + reach_y;
    if heightens && dx <= (2.0 * reach).min(quarter_x) && dy <= (2.0 * reach).min(quarter_y) {
        return Some(Handle::Corner);
    }
    match (dx <= reach_x && down, heightens && dy <= reach_y && across) {
        (true, _) => Some(Handle::Right),
        (false, true) => Some(Handle::Bottom),
        (false, false) => None,
    }
}

/// Which way an arrow key moves the focus from card to card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// The card nearest `from` in `direction`, among `cards` (not `from` itself),
/// each by the rect it is drawn as: one whose centre lies that way of
/// `from`'s. Distance is along the direction between centres, plus twice the
/// gap across it between the cards, so a card in line, overlapping `from`
/// across the direction, beats one off to the side at the same distance, and
/// a near card slightly off line beats one in line far beyond it. Ties go to
/// the centre nearer across, then to the lower card number.
pub fn nearest_card(
    from: Rect,
    direction: Direction,
    cards: impl IntoIterator<Item = (u32, Rect)>,
) -> Option<u32> {
    let centre = from.centre();
    let gap = |start: f32, end: f32, from_start: f32, from_end: f32| {
        (start - from_end).max(from_start - end).max(0.0)
    };
    cards
        .into_iter()
        .filter_map(|(card, rect)| {
            let at = rect.centre();
            let (along, across, apart) = match direction {
                Direction::Left | Direction::Right => (
                    match direction {
                        Direction::Left => centre.x - at.x,
                        _ => at.x - centre.x,
                    },
                    (at.y - centre.y).abs(),
                    gap(rect.y, rect.bottom(), from.y, from.bottom()),
                ),
                Direction::Up | Direction::Down => (
                    match direction {
                        Direction::Up => centre.y - at.y,
                        _ => at.y - centre.y,
                    },
                    (at.x - centre.x).abs(),
                    gap(rect.x, rect.right(), from.x, from.right()),
                ),
            };
            (along > 0.0).then_some((along + 2.0 * apart, across, card))
        })
        .min_by(|a, b| {
            (a.0.total_cmp(&b.0))
                .then(a.1.total_cmp(&b.1))
                .then(a.2.cmp(&b.2))
        })
        .map(|(_, _, card)| card)
}

/// How a card's height cuts the rows a level of detail lists: `kept` of
/// `listed` are drawn, and a "+N more" row after them when that is fewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cut {
    listed: usize,
    kept: usize,
}

impl Card {
    /// The rows a layout made for keys only, or for every column, lists.
    fn laid_out_rows(&self, keys_only: bool) -> usize {
        match keys_only {
            true => self.key_rows.len(),
            false => self.columns as usize,
        }
    }

    fn cut(&self, detail: Detail) -> Cut {
        let listed = match detail {
            Detail::Columns => self.columns as usize,
            Detail::Keys => self.key_rows.len(),
            Detail::Names => 0,
        };
        // A hair of slack, so a height made of whole rows holds them all.
        let room = ((self.height - HEADER_HEIGHT - BOTTOM_PAD) / ROW_HEIGHT + 1e-3)
            .floor()
            .max(0.0) as usize;
        let kept = match room < listed {
            true => room.saturating_sub(1),
            false => listed,
        };
        Cut { listed, kept }
    }

    /// The row a kept column is drawn on with every column listed: the key
    /// columns are kept first, then the rest, and the kept ones are drawn in
    /// column order.
    fn column_row(&self, cut: Cut, column: u32) -> Option<usize> {
        let kept_keys = cut.kept.min(self.key_rows.len());
        let kept_others = cut.kept - kept_keys;
        let (keys_before, key) = match self.key_rows.binary_search(&column) {
            Ok(at) => (at, true),
            Err(at) => (at, false),
        };
        let others_before = column as usize - keys_before;
        let kept = match key {
            true => keys_before < kept_keys,
            false => others_before < kept_others,
        };
        kept.then(|| keys_before.min(kept_keys) + others_before.min(kept_others))
    }

    /// The row a column is met on at a level of detail: its own, or the
    /// "+N more" row when the card's height cut it. None where the level does
    /// not list it.
    fn row_of(&self, column: u32, detail: Detail) -> Option<usize> {
        let cut = self.cut(detail);
        let kept = match detail {
            Detail::Names => return None,
            Detail::Keys => {
                let at = self.key_rows.binary_search(&column).ok()?;
                (at < cut.kept).then_some(at)
            }
            Detail::Columns if column < self.columns => self.column_row(cut, column),
            Detail::Columns => return None,
        };
        Some(kept.unwrap_or(cut.kept))
    }

    fn row_rect(&self, row: usize) -> Rect {
        Rect {
            x: self.origin.x,
            y: self.origin.y + HEADER_HEIGHT + row as f32 * ROW_HEIGHT,
            width: self.width,
            height: ROW_HEIGHT,
        }
    }
}

/// World to screen: `screen = world * zoom + pan`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub zoom: f32,
    pub pan: Point,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: Point::default(),
        }
    }
}

impl Transform {
    pub fn to_screen(self, world: Point) -> Point {
        Point::new(
            world.x * self.zoom + self.pan.x,
            world.y * self.zoom + self.pan.y,
        )
    }

    pub fn to_world(self, screen: Point) -> Point {
        Point::new(
            (screen.x - self.pan.x) / self.zoom,
            (screen.y - self.pan.y) / self.zoom,
        )
    }

    /// The part of the world a screen rectangle (the viewport) shows.
    pub fn world_rect(&self, screen: Rect) -> Rect {
        let origin = self.to_world(Point::new(screen.x, screen.y));
        Rect {
            x: origin.x,
            y: origin.y,
            width: screen.width / self.zoom,
            height: screen.height / self.zoom,
        }
    }

    /// Zoomed to `zoom`, clamped, with the world point under `anchor` (a
    /// screen point: the pointer, or the viewport's centre) staying under it.
    pub fn zoomed_at(&self, anchor: Point, zoom: f32) -> Self {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let world = self.to_world(anchor);
        Self {
            zoom,
            pan: Point::new(anchor.x - world.x * zoom, anchor.y - world.y * zoom),
        }
    }

    pub fn panned(&self, by: Point) -> Self {
        Self {
            zoom: self.zoom,
            pan: Point::new(self.pan.x + by.x, self.pan.y + by.y),
        }
    }

    /// Fits `world` inside the screen rectangle `viewport`, never zoomed past
    /// 100%: a small schema is shown at its size, not blown up.
    pub fn fitting(world: Rect, viewport: Rect, margin: f32) -> Self {
        let zoom = ((viewport.width - 2.0 * margin) / world.width.max(1.0))
            .min((viewport.height - 2.0 * margin) / world.height.max(1.0))
            .clamp(MIN_ZOOM, 1.0);
        Self::fitting_at(world, viewport, zoom)
    }

    /// `world` centred in `viewport` at `zoom`.
    pub fn fitting_at(world: Rect, viewport: Rect, zoom: f32) -> Self {
        let (from, to) = (world.centre(), viewport.centre());
        Self {
            zoom,
            pan: Point::new(to.x - from.x * zoom, to.y - from.y * zoom),
        }
    }
}

/// The end of a foreign key at the referencing table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildMark {
    /// A crow's foot.
    Many,
    /// A single bar: the key's columns are unique, so the reference is 1:1.
    One,
}

/// The end of a foreign key at the referenced table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParentMark {
    /// Two bars: every column of the key is `NOT NULL`.
    ExactlyOne,
    /// A bar and a circle: some column of the key is nullable.
    ZeroOrOne,
}

/// One step of an edge, in the order `PathBuilder` takes them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Segment {
    Move(Point),
    Line(Point),
    Cubic {
        control_a: Point,
        control_b: Point,
        to: Point,
    },
}

/// A cardinality mark's strokes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stroke {
    Line(Point, Point),
    Circle { centre: Point, radius: f32 },
}

/// An edge in world space at one level of detail: the path from the child's
/// end to the parent's, and the marks at both ends.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EdgePath {
    pub segments: Vec<Segment>,
    pub marks: Vec<Stroke>,
}

impl EdgePath {
    /// Every point and control point, and the marks: a curve lies inside the
    /// hull of its control points.
    pub fn bounds(&self) -> Rect {
        let mut points = self.segments.iter().flat_map(|segment| match *segment {
            Segment::Move(point) | Segment::Line(point) => vec![point],
            Segment::Cubic {
                control_a,
                control_b,
                to,
            } => vec![control_a, control_b, to],
        });
        let Some(first) = points.next() else {
            return Rect::default();
        };
        let mut bounds = Rect::spanning(first, first);
        for point in points {
            bounds = bounds.union(&Rect::spanning(point, point));
        }
        for stroke in &self.marks {
            bounds = bounds.union(&match *stroke {
                Stroke::Line(a, b) => Rect::spanning(a, b),
                Stroke::Circle { centre, radius } => Rect::spanning(centre, centre).inflate(radius),
            });
        }
        bounds
    }

    /// The path flattened to a polyline, each curve in a fixed number of steps.
    pub fn polyline(&self) -> Vec<Point> {
        let mut points: Vec<Point> = Vec::new();
        for segment in &self.segments {
            match *segment {
                Segment::Move(point) | Segment::Line(point) => points.push(point),
                Segment::Cubic {
                    control_a,
                    control_b,
                    to,
                } => {
                    let from = points.last().copied().unwrap_or(control_a);
                    points.extend((1..=CURVE_STEPS).map(|step| {
                        cubic(
                            from,
                            control_a,
                            control_b,
                            to,
                            step as f32 / CURVE_STEPS as f32,
                        )
                    }));
                }
            }
        }
        points
    }

    /// The distance from `point` to the path, marks aside.
    pub fn distance(&self, point: Point) -> f32 {
        self.polyline()
            .windows(2)
            .map(|pair| segment_distance(point, pair[0], pair[1]))
            .fold(f32::INFINITY, f32::min)
    }
}

fn cubic(a: Point, b: Point, c: Point, d: Point, t: f32) -> Point {
    let u = 1.0 - t;
    let (wa, wb, wc, wd) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    Point::new(
        wa * a.x + wb * b.x + wc * c.x + wd * d.x,
        wa * a.y + wb * b.y + wc * c.y + wd * d.y,
    )
}

fn segment_distance(point: Point, a: Point, b: Point) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let length = dx * dx + dy * dy;
    let t = if length > 0.0 {
        (((point.x - a.x) * dx + (point.y - a.y) * dy) / length).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (x, y) = (a.x + t * dx - point.x, a.y + t * dy - point.y);
    (x * x + y * y).sqrt()
}

/// A card's visible row: which column it shows, and where.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    pub column: u32,
    pub rect: Rect,
}

/// The row that ends a card cut short by its height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct More {
    /// The columns it stands for.
    pub hidden: usize,
    pub row: usize,
    pub rect: Rect,
}

/// Where an edge meets a card or a lane, with the direction its curve leaves
/// in. At a card's end, `top` and `bottom` bound where the anchor can be at
/// any level of detail, which is what the spatial index needs.
#[derive(Clone, Copy, Debug)]
struct Stop {
    x: f32,
    top: f32,
    bottom: f32,
    outward: f32,
}

impl Layout {
    /// The level of detail at `zoom`. Every other method takes the detail
    /// from here: a layout made for keys only has no room for every column.
    pub fn detail(&self, zoom: f32) -> Detail {
        Detail::at(zoom, self.keys_only)
    }

    /// A card at a level of detail. Every level draws inside the rectangle the
    /// layout made room for, top-aligned, so no level puts a card over a lane.
    pub fn card_rect(&self, card: u32, detail: Detail, zoom: f32) -> Rect {
        let card = &self.cards[card as usize];
        let height = match detail {
            Detail::Columns => card.height,
            Detail::Keys => card_height(card.key_rows.len()),
            Detail::Names => NAME_BLOCK_HEIGHT / zoom,
        };
        Rect {
            x: card.origin.x,
            y: card.origin.y,
            width: card.width,
            height: height.min(card.height),
        }
    }

    /// The column rows a level of detail draws, less any the card's height
    /// cut: see [`Self::more`].
    pub fn rows(&self, card: u32, detail: Detail) -> impl Iterator<Item = Row> + '_ {
        let card = &self.cards[card as usize];
        let cut = card.cut(detail);
        let every = (0..card.columns).filter(move |&column| {
            detail == Detail::Columns && card.column_row(cut, column).is_some()
        });
        let keys = (card.key_rows.iter().copied())
            .take(cut.kept)
            .filter(move |_| detail == Detail::Keys);
        every.chain(keys).enumerate().map(|(row, column)| Row {
            column,
            rect: card.row_rect(row),
        })
    }

    /// The "+N more" row a card cut short by its height ends with.
    pub fn more(&self, card: u32, detail: Detail) -> Option<More> {
        let card = &self.cards[card as usize];
        let cut = card.cut(detail);
        (cut.kept < cut.listed).then(|| More {
            hidden: cut.listed - cut.kept,
            row: cut.kept,
            rect: card.row_rect(cut.kept),
        })
    }

    /// The row a column is met on: see [`row_offset`].
    pub fn row_of(&self, card: u32, column: u32, detail: Detail) -> Option<usize> {
        self.cards[card as usize].row_of(column, detail)
    }

    /// A card's height with nothing cut: room for every row it shows.
    pub fn full_height(&self, card: u32) -> f32 {
        card_height(self.cards[card as usize].laid_out_rows(self.keys_only))
    }

    /// The height a card is laid out with: every row it shows, up to
    /// [`AUTO_ROWS`].
    pub fn auto_height(&self, card: u32) -> f32 {
        auto_height(self.cards[card as usize].laid_out_rows(self.keys_only))
    }

    /// The height a drag to `rows` rows asks for at a level of detail, none
    /// where that is [`Self::auto_height`]: dragged past every row the level
    /// lists, the card shows them all, and never fewer than the cap does.
    pub fn dragged_height(&self, card: u32, rows: usize, detail: Detail) -> Option<f32> {
        let (listed, auto) = (self.listed(card, detail), self.auto_height(card));
        let height = match rows < listed {
            true => card_height(rows),
            false => card_height(listed).max(auto),
        };
        (height != auto).then_some(height)
    }

    /// Whether a drag can change a card's height at a level of detail: the
    /// level lists more than the rows a card keeps however short.
    pub fn heightens(&self, card: u32, detail: Detail) -> bool {
        self.cards[card as usize].cut(detail).listed > MIN_ROWS
    }

    /// How many rows a level of detail lists on a card before any cut.
    pub fn listed(&self, card: u32, detail: Detail) -> usize {
        self.cards[card as usize].cut(detail).listed
    }

    /// Sizes a card: its width held between the narrowest and widest a card
    /// can be, and its height cut to `height` in whole rows, never taller
    /// than [`Self::full_height`]; none is [`Self::auto_height`].
    pub(super) fn set_size(&mut self, card: u32, width: f32, height: Option<f32>) {
        let (full, auto) = (self.full_height(card), self.auto_height(card));
        let card = &mut self.cards[card as usize];
        card.width = width.clamp(MIN_CARD_WIDTH, MAX_CARD_WIDTH);
        card.height = match height {
            Some(height) => card_height(rows_for_height(height)).min(full),
            None => auto,
        };
    }

    /// An edge at a level of detail, from its child's end to its parent's.
    pub fn edge_path(&self, edge: u32, detail: Detail, zoom: f32) -> EdgePath {
        let edge = &self.edges[edge as usize];
        let anchor = |card: u32, column: Option<u32>| {
            let y = self.anchor_y(card, column, detail, zoom);
            (y, y)
        };
        let mut path = EdgePath::default();
        let (child, parent) = if edge.child == edge.parent {
            let (child, _) = anchor(edge.child, Some(edge.child_column));
            let (parent, _) = anchor(edge.parent, edge.parent_column);
            let child_card = &self.cards[edge.child as usize];
            let x = child_card.origin.x + child_card.width;
            let spread = match (parent - child).abs() < ROW_HEIGHT / 2.0 {
                true => ROW_HEIGHT / 2.0,
                false => 0.0,
            };
            path.segments.push(Segment::Move(Point::new(x, child)));
            path.segments.push(Segment::Cubic {
                control_a: Point::new(x + LOOP_CONTROL, child - spread),
                control_b: Point::new(x + LOOP_CONTROL, parent + spread),
                to: Point::new(x, parent),
            });
            (Point::new(x, child), Point::new(x, parent))
        } else {
            let tangent = match detail {
                Detail::Names => NAMES_TANGENT,
                _ => TANGENT,
            };
            let stops = self.stops(edge, anchor);
            path.segments
                .push(Segment::Move(Point::new(stops[0].x, stops[0].top)));
            for (index, pair) in stops.windows(2).enumerate() {
                let (from, to) = (pair[0], pair[1]);
                path.segments.push(match index % 2 {
                    0 => {
                        let reach = control_reach(from, to, tangent);
                        Segment::Cubic {
                            control_a: Point::new(from.x + from.outward * reach, from.top),
                            control_b: Point::new(to.x + to.outward * reach, to.top),
                            to: Point::new(to.x, to.top),
                        }
                    }
                    _ => Segment::Line(Point::new(to.x, to.top)),
                });
            }
            let last = stops[stops.len() - 1];
            (
                Point::new(stops[0].x, stops[0].top),
                Point::new(last.x, last.top),
            )
        };
        if detail.marks() {
            let outward = |end: Point, card: u32| match end.x <= self.cards[card as usize].origin.x
            {
                true => -1.0,
                false => 1.0,
            };
            child_mark(
                edge.child_mark,
                child,
                outward(child, edge.child),
                &mut path.marks,
            );
            parent_mark(
                edge.parent_mark,
                parent,
                outward(parent, edge.parent),
                &mut path.marks,
            );
        }
        path
    }

    /// Rectangles that hold the edge at every level of detail and zoom, one
    /// per curve or lane: the spatial index files the edge under these, so it
    /// is built once per layout and not again on zoom.
    pub(super) fn edge_envelopes(&self, edge: u32) -> Vec<Rect> {
        let edge = &self.edges[edge as usize];
        let extent = |card: u32, _: Option<u32>| {
            let card = &self.cards[card as usize];
            (card.origin.y, card.origin.y + card.height)
        };
        if edge.child == edge.parent {
            let (top, bottom) = extent(edge.child, None);
            let card = &self.cards[edge.child as usize];
            let x = card.origin.x + card.width;
            return vec![Rect {
                x,
                y: top - ROW_HEIGHT / 2.0,
                width: LOOP_CONTROL,
                height: bottom - top + ROW_HEIGHT,
            }];
        }
        let stops = self.stops(edge, extent);
        stops
            .windows(2)
            .map(|pair| {
                let (from, to) = (pair[0], pair[1]);
                let reach = control_reach(from, to, TANGENT);
                let xs = [
                    from.x,
                    to.x,
                    from.x + from.outward * reach,
                    to.x + to.outward * reach,
                ];
                let left = xs.iter().copied().fold(f32::INFINITY, f32::min);
                let right = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let top = from.top.min(to.top);
                Rect {
                    x: left,
                    y: top,
                    width: right - left,
                    height: from.bottom.max(to.bottom) - top,
                }
            })
            .collect()
    }

    /// Everything the layout draws at 100%: cards, lanes and loops.
    pub fn bounds(&self) -> Rect {
        let cards =
            (0..self.cards.len() as u32).map(|card| self.card_rect(card, Detail::Columns, 1.0));
        let edges = (0..self.edges.len() as u32).flat_map(|edge| self.edge_envelopes(edge));
        cards
            .chain(edges)
            .reduce(|bounds, rect| bounds.union(&rect))
            .unwrap_or_default()
    }

    /// The cards as a level of detail draws them at `zoom`.
    pub fn cards_bounds(&self, detail: Detail, zoom: f32) -> Rect {
        (0..self.cards.len() as u32)
            .map(|card| self.card_rect(card, detail, zoom))
            .reduce(|bounds, rect| bounds.union(&rect))
            .unwrap_or_default()
    }

    /// What a level of detail draws at `zoom`: its cards and its edges.
    fn drawn_bounds(&self, detail: Detail, zoom: f32) -> Rect {
        (0..self.edges.len() as u32)
            .map(|edge| self.edge_path(edge, detail, zoom).bounds())
            .fold(self.cards_bounds(detail, zoom), |bounds, rect| {
                bounds.union(&rect)
            })
    }

    /// The view that shows the whole layout in `viewport`, never past 100%.
    /// What has to fit is what the fitted zoom draws: one tall card can pull
    /// a fit of every column down to a zoom that draws name blocks instead,
    /// a sliver of the view, so each level is fitted at the zooms that draw
    /// it.
    pub fn fitting(&self, viewport: Rect, margin: f32) -> Transform {
        // Tracing every edge costs a tenth of a second on thousands of
        // tables, so the cards alone rule a level out first: the edges only
        // ever add to them.
        let cards_fit = |detail: Detail| {
            let cards = self.cards_bounds(detail, 1.0);
            Transform::fitting(cards, viewport, margin).zoom
        };
        if self.detail(cards_fit(Detail::Columns)) == Detail::Columns {
            let columns = self.drawn_bounds(Detail::Columns, 1.0);
            let fit = Transform::fitting(columns, viewport, margin);
            if self.detail(fit.zoom) == Detail::Columns {
                return fit;
            }
        }
        // Keys draw the same at every zoom.
        if self.detail(cards_fit(Detail::Keys)) != Detail::Names {
            let keys = self.drawn_bounds(Detail::Keys, 1.0);
            let fit = Transform::fitting(keys, viewport, margin);
            if self.detail(fit.zoom) != Detail::Names {
                let zoom = match self.keys_only {
                    true => fit.zoom,
                    false => fit.zoom.min(KEYS_BELOW - 0.01),
                };
                return Transform::fitting_at(keys, viewport, zoom);
            }
        }
        // A name block is the same height on screen at any zoom, so its
        // height in the world, and what fits, changes with the zoom. The
        // halving goes by the cards alone and the edges have the last word.
        let (mut fits, mut over) = (MIN_ZOOM, NAMES_BELOW);
        for _ in 0..16 {
            let zoom = (fits + over) / 2.0;
            let cards = self.cards_bounds(Detail::Names, zoom);
            if Transform::fitting(cards, viewport, margin).zoom >= zoom {
                fits = zoom;
            } else {
                over = zoom;
            }
        }
        let names = self.drawn_bounds(Detail::Names, fits);
        let zoom = fits.min(Transform::fitting(names, viewport, margin).zoom);
        Transform::fitting_at(names, viewport, zoom)
    }

    fn anchor_y(&self, card: u32, column: Option<u32>, detail: Detail, zoom: f32) -> f32 {
        let origin = self.cards[card as usize].origin.y;
        match detail {
            Detail::Names => origin + self.card_rect(card, detail, zoom).height / 2.0,
            _ => origin + row_offset(&self.cards[card as usize], column, detail),
        }
    }

    /// The child's end, then the near and far end of each lane, then the
    /// parent's end. Pairs alternate: a curve, a lane, a curve, and so on.
    fn stops(&self, edge: &Edge, anchor: impl Fn(u32, Option<u32>) -> (f32, f32)) -> Vec<Stop> {
        let centre = |card: u32| {
            let card = &self.cards[card as usize];
            card.origin.x + card.width / 2.0
        };
        let lane_centre = |lane: &super::layout::Lane| (lane.left + lane.right) / 2.0;
        let toward_parent = edge.lanes.first().map_or(centre(edge.parent), lane_centre);
        let toward_child = edge.lanes.last().map_or(centre(edge.child), lane_centre);
        let side = |card: u32, toward: f32, (top, bottom): (f32, f32)| {
            let Card { origin, width, .. } = self.cards[card as usize];
            let origin = origin.x;
            match toward < origin + width / 2.0 {
                true => Stop {
                    x: origin,
                    top,
                    bottom,
                    outward: -1.0,
                },
                false => Stop {
                    x: origin + width,
                    top,
                    bottom,
                    outward: 1.0,
                },
            }
        };
        let mut stops = vec![side(
            edge.child,
            toward_parent,
            anchor(edge.child, Some(edge.child_column)),
        )];
        for lane in &edge.lanes {
            let lane_stop = |x: f32, outward: f32| Stop {
                x,
                top: lane.y,
                bottom: lane.y,
                outward,
            };
            let from = stops[stops.len() - 1].x;
            match from >= lane_centre(lane) {
                true => stops.extend([lane_stop(lane.right, 1.0), lane_stop(lane.left, -1.0)]),
                false => stops.extend([lane_stop(lane.left, -1.0), lane_stop(lane.right, 1.0)]),
            }
        }
        stops.push(side(
            edge.parent,
            toward_child,
            anchor(edge.parent, edge.parent_column),
        ));
        stops
    }
}

fn control_reach(from: Stop, to: Stop, tangent: f32) -> f32 {
    ((to.x - from.x).abs() * tangent).max(MIN_CONTROL)
}

/// Where a column's row centre sits below the card's top, at a level of
/// detail that shows rows. A stub, or a column the level does not list, is
/// met at the header, and one the card's height cut at its "+N more" row.
pub(super) fn row_offset(card: &Card, column: Option<u32>, detail: Detail) -> f32 {
    let row = column.and_then(|column| card.row_of(column, detail));
    row.map_or(HEADER_HEIGHT / 2.0, |row| {
        HEADER_HEIGHT + (row as f32 + 0.5) * ROW_HEIGHT
    })
}

fn bar(at: Point, outward: f32, distance: f32) -> Stroke {
    let x = at.x + outward * distance;
    Stroke::Line(
        Point::new(x, at.y - MARK_HALF),
        Point::new(x, at.y + MARK_HALF),
    )
}

fn child_mark(mark: ChildMark, at: Point, outward: f32, strokes: &mut Vec<Stroke>) {
    match mark {
        ChildMark::Many => {
            let heel = Point::new(at.x + outward * CROWS_FOOT, at.y);
            strokes.extend(
                [-MARK_HALF, 0.0, MARK_HALF]
                    .map(|toe| Stroke::Line(heel, Point::new(at.x, at.y + toe))),
            );
        }
        ChildMark::One => strokes.push(bar(at, outward, BAR_NEAR)),
    }
}

fn parent_mark(mark: ParentMark, at: Point, outward: f32, strokes: &mut Vec<Stroke>) {
    strokes.push(bar(at, outward, BAR_NEAR));
    strokes.push(match mark {
        ParentMark::ExactlyOne => bar(at, outward, BAR_FAR),
        ParentMark::ZeroOrOne => Stroke::Circle {
            centre: Point::new(at.x + outward * CIRCLE_AT, at.y),
            radius: CIRCLE_RADIUS,
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::GraphColumn;
    use crate::diagram::layout::schema;
    use crate::diagram::tests::graph;

    #[test]
    fn an_arrow_finds_the_nearest_card_that_way_and_prefers_one_in_line() {
        let card = |x: f32, y: f32| Rect {
            x,
            y,
            width: 200.0,
            height: 100.0,
        };
        let from = card(0.0, 0.0);
        // A grid of layers: one to the right in line, one up and right a
        // little off line, one far right in line, one below, one above.
        let cards = [
            (1, card(300.0, 40.0)),
            (2, card(300.0, -130.0)),
            (3, card(1200.0, 0.0)),
            (4, card(0.0, 160.0)),
            (5, card(10.0, -300.0)),
        ];
        let towards = |direction| nearest_card(from, direction, cards);
        assert_eq!(
            towards(Direction::Right),
            Some(1),
            "in line before off line"
        );
        assert_eq!(towards(Direction::Down), Some(4));
        assert_eq!(
            towards(Direction::Up),
            Some(5),
            "in line, though further along than 2"
        );
        assert_eq!(towards(Direction::Left), None, "nothing that way");

        let without_one = cards.into_iter().filter(|&(card, _)| card != 1);
        assert_eq!(
            nearest_card(from, Direction::Right, without_one),
            Some(2),
            "a near card a little off line before one in line far beyond"
        );

        let level = [(7, card(300.0, 30.0)), (6, card(300.0, -30.0))];
        assert_eq!(
            nearest_card(from, Direction::Right, level),
            Some(6),
            "equally near across: the lower card"
        );
        let stacked = [(8, card(0.0, 0.0)), (9, card(100.0, 0.0))];
        assert_eq!(
            nearest_card(from, Direction::Down, stacked),
            None,
            "a centre level with the card's is not below it"
        );
    }

    /// A table of 200 columns and no keys among short ones, which every-column bounds
    /// would fit at a zoom that draws a sliver of name blocks.
    #[test]
    fn a_fit_fits_what_its_zoom_draws_and_a_tall_card_does_not_shrink_it() {
        let mut tall = graph(
            12,
            &[
                (1, 0),
                (2, 1),
                (3, 0),
                (4, 3),
                (5, 4),
                (6, 0),
                (7, 6),
                (8, 7),
                (9, 8),
                (10, 9),
            ],
        );
        tall.tables[11]
            .columns
            .extend((0..200).map(|column| GraphColumn {
                name: format!("metric_{column}").into(),
                type_label: 0,
                nullable: true,
                primary: false,
                foreign: false,
                unique: false,
            }));
        let viewport = Rect {
            x: 0.0,
            y: 0.0,
            width: 1200.0,
            height: 800.0,
        };
        for keys_only in [false, true] {
            let layout = schema(&tall, keys_only);
            let fit = layout.fitting(viewport, 32.0);
            let drawn = layout.drawn_bounds(layout.detail(fit.zoom), fit.zoom);
            let (width, height) = (drawn.width * fit.zoom, drawn.height * fit.zoom);
            assert!(width <= 1200.0 - 64.0 + 0.5, "{width} wide at {}", fit.zoom);
            assert!(
                height <= 800.0 - 64.0 + 0.5,
                "{height} high at {}",
                fit.zoom
            );
            let centre = drawn.centre();
            let on_screen = Point::new(
                centre.x * fit.zoom + fit.pan.x,
                centre.y * fit.zoom + fit.pan.y,
            );
            assert!((on_screen.x - 600.0).abs() < 0.5 && (on_screen.y - 400.0).abs() < 0.5);
            assert!(
                width >= 1200.0 * 0.6 || height >= 800.0 * 0.6,
                "{width} by {height} at {} leaves the view empty",
                fit.zoom
            );
        }
        // Opened to every column by a drag, the card is as tall as ever.
        let mut layout = schema(&tall, false);
        layout.set_size(11, CARD_WIDTH, Some(f32::MAX));
        let every_column = Transform::fitting(layout.bounds(), viewport, 32.0);
        assert!(layout.fitting(viewport, 32.0).zoom > 2.0 * every_column.zoom);
    }

    fn note(name: String) -> GraphColumn {
        GraphColumn {
            name: name.into(),
            type_label: 0,
            nullable: true,
            primary: false,
            foreign: false,
            unique: false,
        }
    }

    #[test]
    fn a_tall_card_is_laid_out_at_thirty_rows_keys_first_and_a_drag_opens_it() {
        // Table 1: `id`, 100 notes, then its key into table 0.
        let mut tall = graph(2, &[(1, 0)]);
        let columns = &mut tall.tables[1].columns;
        let key = columns.pop().unwrap();
        columns.extend((0..100).map(|column| note(format!("note_{column}"))));
        columns.push(key);
        tall.keys[0].columns = vec![101];

        let mut layout = schema(&tall, false);
        assert_eq!(layout.cards[1].height, card_height(AUTO_ROWS));
        assert_eq!(layout.auto_height(1), card_height(AUTO_ROWS));
        assert_eq!(layout.full_height(1), card_height(102));
        let shown: Vec<u32> = layout
            .rows(1, Detail::Columns)
            .map(|row| row.column)
            .collect();
        assert_eq!(shown.len(), AUTO_ROWS - 1);
        assert_eq!(shown[0], 0);
        assert_eq!(
            shown.last(),
            Some(&101),
            "the key is kept, drawn in its own place"
        );
        let more = layout.more(1, Detail::Columns).expect("the rest counted");
        assert_eq!(
            (more.hidden, more.row),
            (102 - (AUTO_ROWS - 1), AUTO_ROWS - 1)
        );
        assert_eq!(
            layout.row_of(1, 101, Detail::Columns),
            Some(AUTO_ROWS - 2),
            "its edge meets its own row"
        );
        assert_eq!(layout.row_of(1, 60, Detail::Columns), Some(AUTO_ROWS - 1));

        layout.set_size(1, CARD_WIDTH, Some(card_height(50)));
        assert_eq!(layout.cards[1].height, card_height(50), "taller by a drag");
        layout.set_size(1, CARD_WIDTH, Some(f32::MAX));
        assert_eq!(layout.cards[1].height, layout.full_height(1));
        assert_eq!(layout.more(1, Detail::Columns), None);
        layout.set_size(1, CARD_WIDTH, None);
        assert_eq!(
            layout.cards[1].height,
            card_height(AUTO_ROWS),
            "the cap back"
        );
        let drag = |rows| layout.dragged_height(1, rows, Detail::Columns);
        assert_eq!(drag(12), Some(card_height(12)));
        assert_eq!(drag(AUTO_ROWS), None, "the cap is the card's own size");
        assert_eq!(drag(60), Some(card_height(60)));
        assert_eq!(drag(500), Some(card_height(102)), "past the end: every row");
        assert_eq!(
            layout.dragged_height(1, 500, Detail::Keys),
            None,
            "past its two key rows at keys: the cap"
        );

        // Keys only caps a card of more key rows than that the same way.
        for column in &mut tall.tables[1].columns[1..=40] {
            column.unique = true;
        }
        let keys = schema(&tall, true);
        assert_eq!(keys.cards[1].key_rows.len(), 42);
        assert_eq!(keys.cards[1].height, card_height(AUTO_ROWS));
        assert_eq!(
            keys.more(1, Detail::Keys).map(|more| more.hidden),
            Some(42 - (AUTO_ROWS - 1))
        );
        let full = schema(&tall, false);
        assert_eq!(
            full.rows(1, Detail::Keys).count(),
            AUTO_ROWS - 1,
            "and the key rows drawn zoomed out of every column"
        );
    }

    #[test]
    fn a_fit_lands_on_the_most_detailed_level_that_fits() {
        let viewport = Rect {
            x: 0.0,
            y: 0.0,
            width: 1200.0,
            height: 800.0,
        };
        let level = |graph: &crate::db::SchemaGraph, keys_only: bool| {
            let layout = schema(graph, keys_only);
            let fit = layout.fitting(viewport, 32.0);
            (layout.detail(fit.zoom), fit.zoom)
        };
        let small = graph(3, &[(1, 0), (2, 1)]);
        assert_eq!(level(&small, false), (Detail::Columns, 1.0));
        assert_eq!(level(&small, true), (Detail::Keys, 1.0));
        // Five layers of cards too wide for every column at a zoom that
        // draws them.
        let mut wide = graph(5, &[(1, 0), (2, 1), (3, 2), (4, 3)]);
        for table in &mut wide.tables {
            table
                .columns
                .extend((0..40).map(|column| note(format!("c{column}"))));
        }
        let (detail, zoom) = level(&wide, false);
        assert_eq!(detail, Detail::Keys);
        assert!(zoom < KEYS_BELOW, "{zoom} would draw every column");
        let many = graph(400, &[]);
        let (detail, zoom) = level(&many, false);
        assert_eq!(detail, Detail::Names);
        assert!(zoom > MIN_ZOOM, "{zoom}");
        let empty = graph(0, &[]);
        assert_eq!(level(&empty, false).1, 1.0);
    }

    #[test]
    fn the_level_of_detail_follows_the_zoom_and_keys_only_caps_it() {
        assert_eq!(Detail::at(1.0, false), Detail::Columns);
        assert_eq!(Detail::at(MAX_ZOOM, false), Detail::Columns);
        assert_eq!(Detail::at(0.7, false), Detail::Keys);
        assert_eq!(Detail::at(0.45, false), Detail::Names);
        assert_eq!(Detail::at(MIN_ZOOM, false), Detail::Names);
        assert_eq!(Detail::at(2.0, true), Detail::Keys);
        assert_eq!(Detail::at(0.45, true), Detail::Names);
        assert!(Detail::Columns.types() && !Detail::Keys.types());
        assert!(Detail::Keys.marks() && !Detail::Names.marks());
    }

    #[test]
    fn text_sizes_come_from_a_few_steps_and_never_drop_below_eleven() {
        for base in [11.0, 13.0, 16.0, 20.0] {
            let mut steps: Vec<f32> = TEXT_STEPS.iter().map(|step| base * step).collect();
            steps.push(MIN_TEXT_SIZE);
            let mut previous = 0.0;
            for tenth in 0..=400 {
                let zoom = MIN_ZOOM + (MAX_ZOOM - MIN_ZOOM) * tenth as f32 / 400.0;
                let size = text_size(zoom, base);
                assert!(size >= MIN_TEXT_SIZE, "{size} at {zoom}");
                assert!(steps.contains(&size), "{size} at {zoom} is not a step");
                assert!(size >= previous, "shrank to {size} zooming in to {zoom}");
                assert!(
                    size <= (base * zoom).max(MIN_TEXT_SIZE),
                    "{size} outgrows {zoom}"
                );
                previous = size;
            }
        }
        assert_eq!(text_size(1.0, 13.0), 13.0);
    }

    #[test]
    fn a_long_label_is_cut_at_a_character_with_room_for_the_ellipsis() {
        // Every character 10 wide, the ellipsis 8.
        let starts = |text: &str| {
            text.char_indices()
                .enumerate()
                .map(|(n, (at, _))| (at, n as f32 * 10.0))
                .collect::<Vec<_>>()
        };
        let fit = |text: &str, room: f32| {
            let width = text.chars().count() as f32 * 10.0;
            Fit::of(text, width, room, 8.0, starts(text).into_iter())
        };
        assert_eq!(fit("orders", 60.0), Fit::Whole);
        assert_eq!(fit("orders", 100.0), Fit::Whole);
        assert_eq!(fit("orders", 59.0), Fit::Cut(50.0));
        assert_eq!(fit("orders", 48.0), Fit::Cut(40.0));
        assert_eq!(
            fit("order lines", 68.0),
            Fit::Cut(50.0),
            "no space before the ellipsis"
        );
        assert_eq!(fit("orders", 12.0), Fit::Cut(0.0));
        assert_eq!(fit("orders", 7.0), Fit::Hidden);
        assert_eq!(
            fit("åäöü", 25.0),
            Fit::Cut(10.0),
            "cut on a character, not a byte"
        );
        let shows = |text: &str, room: f32| fit(text, room).shows(4, starts(text).into_iter());
        assert!(shows("orders", 59.0), "five characters and the ellipsis");
        assert!(shows("orders", 48.0), "four");
        assert!(!shows("orders", 38.0), "three");
        assert!(shows("t01", 30.0), "a short name whole");
        assert!(!shows("orders", 7.0), "nothing");
        assert_eq!(Fit::Cut(40.0).width(60.0, 8.0), 48.0);
        assert_eq!(Fit::Whole.width(60.0, 8.0), 60.0);
        assert_eq!(Fit::Hidden.width(60.0, 8.0), 0.0);
    }

    #[test]
    fn a_columns_type_keeps_a_share_of_a_row_its_name_overflows() {
        assert_eq!(type_room(200.0, 80.0, 60.0, 6.0), 60.0, "both fit");
        assert_eq!(type_room(200.0, 150.0, 120.0, 6.0), 80.0, "never under 40%");
        assert_eq!(
            type_room(200.0, 150.0, 60.0, 6.0),
            60.0,
            "nor over its width"
        );
        assert_eq!(
            type_room(200.0, 100.0, 120.0, 6.0),
            94.0,
            "what the name leaves"
        );
        assert_eq!(type_room(200.0, 20.0, 300.0, 6.0), 174.0);
        assert_eq!(type_room(0.0, 20.0, 30.0, 6.0), 0.0);
    }

    #[test]
    fn zooming_keeps_the_point_under_the_anchor_where_it_was() {
        let transform = Transform {
            zoom: 0.8,
            pan: Point::new(40.0, -12.0),
        };
        let anchor = Point::new(300.0, 200.0);
        let under = transform.to_world(anchor);
        let zoomed = transform.zoomed_at(anchor, 1.6);
        let after = zoomed.to_screen(under);
        assert!((after.x - anchor.x).abs() < 1e-3 && (after.y - anchor.y).abs() < 1e-3);
        assert_eq!(transform.zoomed_at(anchor, 100.0).zoom, MAX_ZOOM);

        let panned = zoomed.panned(Point::new(10.0, 5.0));
        let moved = panned.to_screen(under);
        assert!((moved.x - anchor.x - 10.0).abs() < 1e-3);

        let viewport = Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let shown = panned.world_rect(viewport);
        assert_eq!(
            panned.to_screen(Point::new(shown.right(), shown.bottom())),
            Point::new(800.0, 600.0)
        );

        let world = Rect {
            x: 100.0,
            y: 100.0,
            width: 4000.0,
            height: 1000.0,
        };
        let fit = Transform::fitting(world, viewport, 20.0);
        assert_eq!(fit.zoom, 0.19);
        let centre = fit.to_screen(world.centre());
        assert!((centre.x - 400.0).abs() < 1e-3 && (centre.y - 300.0).abs() < 1e-3);
    }

    #[test]
    fn each_level_shows_its_rows_inside_the_room_the_layout_made() {
        // Table 1: its key, its reference to 0, and a column in no key.
        let mut graph = graph(2, &[(1, 0)]);
        graph.tables[1].columns.push(GraphColumn {
            name: "note".into(),
            type_label: 0,
            nullable: true,
            primary: false,
            foreign: false,
            unique: false,
        });
        let layout = schema(&graph, false);
        let columns = |detail| {
            layout
                .rows(1, detail)
                .map(|row| row.column)
                .collect::<Vec<_>>()
        };
        assert_eq!(columns(Detail::Columns), [0, 1, 2]);
        assert_eq!(columns(Detail::Keys), [0, 1]);
        assert!(columns(Detail::Names).is_empty());

        let room = layout.card_rect(1, Detail::Columns, 1.0);
        assert_eq!(room.height, card_height(3));
        assert_eq!(
            layout.card_rect(1, Detail::Keys, 0.7).height,
            card_height(2)
        );
        assert_eq!(
            layout.card_rect(1, Detail::Names, 0.5).height,
            NAME_BLOCK_HEIGHT / 0.5
        );
        assert_eq!(layout.card_rect(1, Detail::Names, 0.1).height, room.height);

        let key_row = layout.rows(1, Detail::Keys).nth(1).unwrap().rect;
        let Segment::Move(start) = layout.edge_path(0, Detail::Keys, 0.7).segments[0] else {
            panic!("a path starts with a move");
        };
        assert_eq!(start, Point::new(key_row.x, key_row.centre().y));
        let bounds = layout.bounds();
        for card in 0..2 {
            let rect = layout.card_rect(card, Detail::Columns, 1.0);
            assert_eq!(bounds.union(&rect), bounds);
        }
    }

    /// Table 1: `id`, two notes, its key into table 0, and a third note.
    /// Its key rows are `id` and the key, columns 0 and 3.
    fn keyed_between_notes() -> Layout {
        let mut graph = graph(2, &[(1, 0)]);
        let note = |name: &str| GraphColumn {
            name: name.to_string().into(),
            type_label: 0,
            nullable: true,
            primary: false,
            foreign: false,
            unique: false,
        };
        let columns = &mut graph.tables[1].columns;
        columns.insert(1, note("note_a"));
        columns.insert(2, note("note_b"));
        columns.push(note("note_c"));
        graph.keys[0].columns = vec![3];
        schema(&graph, false)
    }

    fn shown(layout: &Layout, detail: Detail) -> Vec<u32> {
        layout.rows(1, detail).map(|row| row.column).collect()
    }

    #[test]
    fn a_card_cut_short_keeps_its_keys_first_and_ends_with_the_rest_counted() {
        let mut layout = keyed_between_notes();
        assert_eq!(layout.cards[1].key_rows, [0, 3]);
        assert_eq!(shown(&layout, Detail::Columns), [0, 1, 2, 3, 4]);
        assert_eq!(layout.more(1, Detail::Columns), None);

        // Four rows: the two keys, the first other column, then "+2 more".
        layout.set_size(1, CARD_WIDTH, Some(card_height(4)));
        assert_eq!(
            shown(&layout, Detail::Columns),
            [0, 1, 3],
            "in column order"
        );
        let more = layout
            .more(1, Detail::Columns)
            .expect("the rest are counted");
        assert_eq!((more.hidden, more.row), (2, 3));
        assert_eq!(
            more.rect.y,
            layout.cards[1].origin.y + HEADER_HEIGHT + 3.0 * ROW_HEIGHT
        );
        assert_eq!(
            layout.card_rect(1, Detail::Columns, 1.0).height,
            card_height(4)
        );
        assert_eq!(layout.row_of(1, 3, Detail::Columns), Some(2));
        assert_eq!(
            layout.row_of(1, 2, Detail::Columns),
            Some(3),
            "met at +N more"
        );
        assert_eq!(layout.row_of(1, 4, Detail::Columns), Some(3));
        assert_eq!(shown(&layout, Detail::Keys), [0, 3], "keys only still fit");
        assert_eq!(layout.more(1, Detail::Keys), None);

        // Two rows: one key and "+4 more", and both keys at keys only.
        layout.set_size(1, CARD_WIDTH, Some(card_height(2)));
        assert_eq!(shown(&layout, Detail::Columns), [0]);
        assert_eq!(
            layout.more(1, Detail::Columns).map(|more| more.hidden),
            Some(4)
        );
        assert_eq!(shown(&layout, Detail::Keys), [0, 3]);
        assert_eq!(layout.row_of(1, 3, Detail::Keys), Some(1));

        // Two rows are the shortest a card goes: one asked for still shows a
        // column.
        layout.set_size(1, CARD_WIDTH, Some(card_height(1)));
        assert_eq!(layout.cards[1].height, card_height(2));
        assert_eq!(shown(&layout, Detail::Columns), [0]);
        assert_eq!(
            layout.more(1, Detail::Columns).map(|more| more.hidden),
            Some(4)
        );
        assert_eq!(
            layout.row_of(1, 1, Detail::Keys),
            None,
            "keys only lists no note"
        );
        assert_eq!(layout.row_of(1, 3, Detail::Names), None);
    }

    #[test]
    fn an_edge_into_a_column_the_cut_hid_meets_the_more_row() {
        let mut layout = keyed_between_notes();
        layout.set_size(1, CARD_WIDTH, Some(card_height(2)));
        let more = layout.more(1, Detail::Columns).unwrap();
        let Segment::Move(start) = layout.edge_path(0, Detail::Columns, 1.0).segments[0] else {
            panic!("a path starts with a move");
        };
        assert_eq!(start.y, more.rect.centre().y);
        assert!(
            layout
                .edge_envelopes(0)
                .iter()
                .any(|envelope| envelope.contains(start)),
            "the index still files the edge where it now starts"
        );
    }

    #[test]
    fn a_card_is_sized_within_its_bounds_and_in_whole_rows() {
        let mut layout = keyed_between_notes();
        let full = layout.full_height(1);
        assert_eq!(full, card_height(5));
        layout.set_size(1, 10.0, Some(0.0));
        assert_eq!(layout.cards[1].width, MIN_CARD_WIDTH);
        assert_eq!(
            layout.cards[1].height,
            card_height(2),
            "a header, a column and the rest counted"
        );
        layout.set_size(0, CARD_WIDTH, Some(0.0));
        assert_eq!(
            layout.cards[0].height,
            card_height(1),
            "a header and a row where there is nothing to count"
        );
        layout.set_size(1, 5000.0, Some(10_000.0));
        assert_eq!(layout.cards[1].width, MAX_CARD_WIDTH);
        assert_eq!(layout.cards[1].height, full, "never taller than its rows");
        layout.set_size(1, 300.0, Some(card_height(3) + 0.4 * ROW_HEIGHT));
        assert_eq!(layout.cards[1].height, card_height(3));
        layout.set_size(1, 300.0, Some(card_height(3) + 0.6 * ROW_HEIGHT));
        assert_eq!(layout.cards[1].height, card_height(4));
        layout.set_size(1, 300.0, None);
        assert_eq!(layout.cards[1].height, full);

        let rect = layout.card_rect(1, Detail::Columns, 1.0);
        assert_eq!(rect.width, 300.0);
        assert!(
            layout
                .rows(1, Detail::Columns)
                .all(|row| row.rect.width == 300.0)
        );
        let Segment::Move(start) = layout.edge_path(0, Detail::Columns, 1.0).segments[0] else {
            panic!("a path starts with a move");
        };
        assert!(
            start.x == rect.x || start.x == rect.right(),
            "an edge leaves from a side of the card as sized"
        );

        assert!(layout.heightens(1, Detail::Columns));
        assert!(
            !layout.heightens(1, Detail::Keys),
            "two key rows, which the shortest card holds"
        );
        assert!(
            !layout.heightens(1, Detail::Names),
            "a name block's height is fixed"
        );
        assert!(
            !layout.heightens(0, Detail::Columns),
            "one row cannot be cut"
        );
    }

    #[test]
    fn a_handle_is_the_edge_or_corner_under_the_pointer_either_side() {
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 100.0,
        };
        let at = |x, y| handle_at(rect, Point::new(x, y), 5.0, true);
        assert_eq!(at(198.0, 50.0), Some(Handle::Right));
        assert_eq!(at(204.0, 50.0), Some(Handle::Right), "just outside");
        assert_eq!(at(206.0, 50.0), None);
        assert_eq!(at(100.0, 103.0), Some(Handle::Bottom));
        assert_eq!(
            at(192.0, 92.0),
            Some(Handle::Corner),
            "the corner reaches further"
        );
        assert_eq!(at(205.0, 105.0), Some(Handle::Corner));
        assert_eq!(at(100.0, 50.0), None, "the body moves the card");
        assert_eq!(at(198.0, -3.0), None, "not above the card");
        assert_eq!(
            handle_at(rect, Point::new(198.0, 98.0), 5.0, false),
            Some(Handle::Right),
            "no height to change leaves the right edge"
        );
        assert_eq!(handle_at(rect, Point::new(100.0, 101.0), 5.0, false), None);
        let small = Rect {
            width: 12.0,
            height: 8.0,
            ..rect
        };
        assert_eq!(
            handle_at(small, Point::new(6.0, 4.0), 5.0, true),
            None,
            "the middle of a card small on screen still moves it"
        );
    }

    #[test]
    fn the_distance_to_a_path_is_measured_along_its_curve() {
        let path = EdgePath {
            segments: vec![
                Segment::Move(Point::new(0.0, 0.0)),
                Segment::Cubic {
                    control_a: Point::new(50.0, 0.0),
                    control_b: Point::new(50.0, 100.0),
                    to: Point::new(100.0, 100.0),
                },
                Segment::Line(Point::new(200.0, 100.0)),
            ],
            marks: Vec::new(),
        };
        assert!(path.distance(Point::new(50.0, 50.0)) < 1.0);
        assert!((path.distance(Point::new(150.0, 110.0)) - 10.0).abs() < 1e-3);
        assert!(path.distance(Point::new(0.0, 100.0)) > 30.0);
        assert_eq!(
            path.bounds(),
            Rect {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 100.0
            }
        );
    }
}
