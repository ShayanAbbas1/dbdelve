//! The diagram's shapes in world units, where one unit is one pixel at 100%,
//! and the transform that puts them on screen.

use super::layout::{Card, Edge, Layout};

pub const CARD_WIDTH: f32 = 216.0;
pub const HEADER_HEIGHT: f32 = 34.0;
pub const ROW_HEIGHT: f32 = 24.0;
pub const BOTTOM_PAD: f32 = 8.0;
/// A name block's height on screen, at every zoom that draws one.
pub const NAME_BLOCK_HEIGHT: f32 = 26.0;
/// Labels are cut with an ellipsis below this size rather than drawn smaller.
pub const MIN_TEXT_SIZE: f32 = 11.0;
pub const MIN_ZOOM: f32 = 0.05;
pub const MAX_ZOOM: f32 = 4.0;

/// Between the levels the design names (100%, about 70%, 45% and below).
const KEYS_BELOW: f32 = 0.85;
const NAMES_BELOW: f32 = 0.55;

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

pub fn card_height(rows: usize) -> f32 {
    match rows {
        0 => HEADER_HEIGHT,
        rows => HEADER_HEIGHT + rows as f32 * ROW_HEIGHT + BOTTOM_PAD,
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
            width: CARD_WIDTH,
            height: height.min(card.height),
        }
    }

    pub fn rows(&self, card: u32, detail: Detail) -> impl Iterator<Item = Row> + '_ {
        let card = &self.cards[card as usize];
        let every = (0..card.columns).filter(move |_| detail == Detail::Columns);
        let keys = (card.key_rows.iter().copied()).filter(move |_| detail == Detail::Keys);
        every.chain(keys).enumerate().map(|(row, column)| Row {
            column,
            rect: Rect {
                x: card.origin.x,
                y: card.origin.y + HEADER_HEIGHT + row as f32 * ROW_HEIGHT,
                width: CARD_WIDTH,
                height: ROW_HEIGHT,
            },
        })
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
            let x = self.cards[edge.child as usize].origin.x + CARD_WIDTH;
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
            let x = self.cards[edge.child as usize].origin.x + CARD_WIDTH;
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
        let centre = |card: u32| self.cards[card as usize].origin.x + CARD_WIDTH / 2.0;
        let lane_centre = |lane: &super::layout::Lane| (lane.left + lane.right) / 2.0;
        let toward_parent = edge.lanes.first().map_or(centre(edge.parent), lane_centre);
        let toward_child = edge.lanes.last().map_or(centre(edge.child), lane_centre);
        let side = |card: u32, toward: f32, (top, bottom): (f32, f32)| {
            let origin = self.cards[card as usize].origin.x;
            match toward < origin + CARD_WIDTH / 2.0 {
                true => Stop {
                    x: origin,
                    top,
                    bottom,
                    outward: -1.0,
                },
                false => Stop {
                    x: origin + CARD_WIDTH,
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
/// detail that shows rows. A stub, or a column the level hides, is met at
/// the header.
pub(super) fn row_offset(card: &Card, column: Option<u32>, detail: Detail) -> f32 {
    let row = column.and_then(|column| match detail {
        Detail::Columns => Some(column as usize),
        Detail::Keys => card.key_rows.binary_search(&column).ok(),
        Detail::Names => None,
    });
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
