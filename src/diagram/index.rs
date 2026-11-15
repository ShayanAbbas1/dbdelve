//! A laid-out diagram indexed for the two questions a frame and a pointer ask:
//! what meets this rectangle, and what is under this point. A uniform grid in
//! world space, so neither scans every card.

use std::collections::HashMap;

use super::geometry::{self, CARD_WIDTH, Handle, Point, Rect};
use super::layout::Layout;

/// Wider than a card, so a pointer looks in one or two cells.
const CELL: f32 = 256.0;
/// A rectangle over more cells than this is not filed in them but kept
/// aside and offered to every query: only a card or edge stretched far past
/// anything a layout makes is, and filing it would allocate a cell for each.
const MAX_FILED_CELLS: i64 = 64 * 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    Card(u32),
    Edge(u32),
}

pub struct Scene {
    layout: Layout,
    /// Per edge, the rectangles it is filed under: they hold it at every
    /// level of detail, so zooming never refiles it.
    envelopes: Vec<Vec<Rect>>,
    cells: HashMap<(i32, i32), Vec<Entry>>,
    /// What spans too many cells to file ([`MAX_FILED_CELLS`]).
    wide: Vec<Entry>,
    /// The cells that hold anything, so a query zoomed far out visits only
    /// those rather than every cell of the viewport.
    occupied: Option<(i32, i32, i32, i32)>,
    /// Whether the grid and envelopes are built: not while the diagram is
    /// behind another tab ([`Self::release`]).
    filed: bool,
}

/// Where the user left a card and what size they gave it, each part only
/// where it differs from what the layout gave it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Placement {
    pub origin: Option<Point>,
    pub width: Option<f32>,
    /// A height the card is cut to, in whole rows.
    pub height: Option<f32>,
}

impl Placement {
    pub fn is_empty(&self) -> bool {
        self.origin.is_none() && self.width.is_none() && self.height.is_none()
    }
}

/// What a frame draws: cards and edges whose rectangles meet the viewport,
/// each in ascending order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Visible {
    pub cards: Vec<u32>,
    pub edges: Vec<u32>,
}

impl Scene {
    pub fn new(layout: Layout) -> Self {
        let mut scene = Self {
            layout,
            envelopes: Vec::new(),
            cells: HashMap::new(),
            wide: Vec::new(),
            occupied: None,
            filed: false,
        };
        scene.refile();
        scene
    }

    /// Drops the grid and the edges' envelopes and keeps the layout, for a
    /// diagram not on screen. Until [`Self::refile`], nothing is found.
    pub fn release(&mut self) {
        self.envelopes = Vec::new();
        self.cells = HashMap::new();
        self.wide = Vec::new();
        self.occupied = None;
        self.filed = false;
    }

    /// Builds the grid again after [`Self::release`]; nothing when built.
    pub fn refile(&mut self) {
        if self.filed {
            return;
        }
        self.filed = true;
        self.envelopes = (0..self.layout.edges.len() as u32)
            .map(|edge| self.layout.edge_envelopes(edge))
            .collect();
        for card in 0..self.layout.cards.len() as u32 {
            self.file(Entry::Card(card));
        }
        for edge in 0..self.layout.edges.len() as u32 {
            self.file(Entry::Edge(edge));
        }
    }

    #[cfg(test)]
    pub fn is_filed(&self) -> bool {
        self.filed
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn visible(&self, world: Rect, zoom: f32) -> Visible {
        let detail = self.layout.detail(zoom);
        let mut visible = Visible::default();
        for entry in self.entries(world) {
            match entry {
                Entry::Card(card) => {
                    if self.layout.card_rect(card, detail, zoom).intersects(&world) {
                        visible.cards.push(card);
                    }
                }
                Entry::Edge(edge) => {
                    if self.envelopes[edge as usize]
                        .iter()
                        .any(|rect| rect.intersects(&world))
                    {
                        visible.edges.push(edge);
                    }
                }
            }
        }
        for list in [&mut visible.cards, &mut visible.edges] {
            list.sort_unstable();
            list.dedup();
        }
        visible
    }

    /// The card under a world point. Cards are painted in index order, so of
    /// two that overlap the later one is on top.
    pub fn card_at(&self, point: Point, zoom: f32) -> Option<u32> {
        let detail = self.layout.detail(zoom);
        self.entries(Rect {
            x: point.x,
            y: point.y,
            width: 0.0,
            height: 0.0,
        })
        .filter_map(|entry| match entry {
            Entry::Card(card) => Some(card),
            Entry::Edge(_) => None,
        })
        .filter(|&card| self.layout.card_rect(card, detail, zoom).contains(point))
        .max()
    }

    /// The edge nearest a world point, within `tolerance` world units.
    pub fn edge_at(&self, point: Point, tolerance: f32, zoom: f32) -> Option<u32> {
        let detail = self.layout.detail(zoom);
        let probe = Rect {
            x: point.x,
            y: point.y,
            width: 0.0,
            height: 0.0,
        }
        .inflate(tolerance);
        let mut candidates: Vec<u32> = self
            .entries(probe)
            .filter_map(|entry| match entry {
                Entry::Edge(edge) => Some(edge),
                Entry::Card(_) => None,
            })
            .filter(|&edge| {
                self.envelopes[edge as usize]
                    .iter()
                    .any(|rect| rect.intersects(&probe))
            })
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        candidates
            .into_iter()
            .map(|edge| {
                (
                    self.layout.edge_path(edge, detail, zoom).distance(point),
                    edge,
                )
            })
            .filter(|&(distance, _)| distance <= tolerance)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, edge)| edge)
    }

    /// The resize handle under a world point, `reach` world units from an
    /// edge: on the card the point is on, else on the topmost card it is
    /// just outside of, so a handle never reaches over another card.
    pub fn handle_at(&self, point: Point, reach: f32, zoom: f32) -> Option<(u32, Handle)> {
        let detail = self.layout.detail(zoom);
        let on = self.card_at(point, zoom);
        let probe = Rect {
            x: point.x,
            y: point.y,
            width: 0.0,
            height: 0.0,
        }
        .inflate(2.0 * reach);
        self.entries(probe)
            .filter_map(|entry| match entry {
                Entry::Card(card) if on.is_none_or(|on| on == card) => Some(card),
                _ => None,
            })
            .filter_map(|card| {
                let rect = self.layout.card_rect(card, detail, zoom);
                let heightens = self.layout.heightens(card, detail);
                Some((card, geometry::handle_at(rect, point, reach, heightens)?))
            })
            .max_by_key(|&(card, _)| card)
    }

    /// Moves a card and refiles it and the edges that end at it, and only
    /// those. Returns them, so their cached paths can be dropped.
    pub fn move_card(&mut self, card: u32, origin: Point) -> &[u32] {
        self.reshape(card, |layout| layout.cards[card as usize].origin = origin)
    }

    /// Sizes a card ([`Layout::set_size`]), refiling as [`Self::move_card`]
    /// does: none is the size the layout gave it.
    pub fn resize_card(&mut self, card: u32, width: Option<f32>, height: Option<f32>) -> &[u32] {
        self.reshape(card, |layout| {
            layout.set_size(card, width.unwrap_or(CARD_WIDTH), height)
        })
    }

    /// Puts a card where the user left it, at the size they gave it.
    pub fn place(&mut self, card: u32, placement: &Placement) -> &[u32] {
        self.reshape(card, |layout| {
            if let Some(origin) = placement.origin {
                layout.cards[card as usize].origin = origin;
            }
            layout.set_size(
                card,
                placement.width.unwrap_or(CARD_WIDTH),
                placement.height,
            );
        })
    }

    fn reshape(&mut self, card: u32, change: impl FnOnce(&mut Layout)) -> &[u32] {
        if !self.filed {
            change(&mut self.layout);
            return &self.layout.cards[card as usize].edges;
        }
        self.unfile(Entry::Card(card));
        let edges = self.layout.cards[card as usize].edges.clone();
        for &edge in &edges {
            self.unfile(Entry::Edge(edge));
        }
        change(&mut self.layout);
        self.file(Entry::Card(card));
        for edge in edges {
            self.envelopes[edge as usize] = self.layout.edge_envelopes(edge);
            self.file(Entry::Edge(edge));
        }
        &self.layout.cards[card as usize].edges
    }

    fn rects(&self, entry: Entry) -> Vec<Rect> {
        match entry {
            Entry::Card(card) => {
                let card = &self.layout.cards[card as usize];
                vec![Rect {
                    x: card.origin.x,
                    y: card.origin.y,
                    width: card.width,
                    height: card.height,
                }]
            }
            Entry::Edge(edge) => self.envelopes[edge as usize].clone(),
        }
    }

    fn file(&mut self, entry: Entry) {
        for rect in self.rects(entry) {
            let (left, top, right, bottom) = cells(rect);
            if too_wide(left, top, right, bottom) {
                if !self.wide.contains(&entry) {
                    self.wide.push(entry);
                }
                continue;
            }
            self.occupied = Some(match self.occupied {
                None => (left, top, right, bottom),
                Some((l, t, r, b)) => (l.min(left), t.min(top), r.max(right), b.max(bottom)),
            });
            for x in left..=right {
                for y in top..=bottom {
                    let cell = self.cells.entry((x, y)).or_default();
                    if !cell.contains(&entry) {
                        cell.push(entry);
                    }
                }
            }
        }
    }

    fn unfile(&mut self, entry: Entry) {
        self.wide.retain(|filed| *filed != entry);
        for rect in self.rects(entry) {
            let (left, top, right, bottom) = cells(rect);
            if too_wide(left, top, right, bottom) {
                continue;
            }
            for x in left..=right {
                for y in top..=bottom {
                    if let Some(cell) = self.cells.get_mut(&(x, y)) {
                        cell.retain(|filed| *filed != entry);
                    }
                }
            }
        }
    }

    /// Every entry filed in a cell the rectangle meets, with repeats.
    fn entries(&self, rect: Rect) -> impl Iterator<Item = Entry> + '_ {
        let (left, top, right, bottom) = cells(rect);
        let (left, top, right, bottom) = match self.occupied {
            Some((l, t, r, b)) => (left.max(l), top.max(t), right.min(r), bottom.min(b)),
            None => (0, 0, -1, -1),
        };
        (left..=right)
            .flat_map(move |x| (top..=bottom).map(move |y| (x, y)))
            .filter_map(|cell| self.cells.get(&cell))
            .flatten()
            .chain(&self.wide)
            .copied()
    }
}

fn too_wide(left: i32, top: i32, right: i32, bottom: i32) -> bool {
    let span = |from: i32, to: i32| (i64::from(to) - i64::from(from) + 1).max(0);
    span(left, right) * span(top, bottom) > MAX_FILED_CELLS
}

fn cells(rect: Rect) -> (i32, i32, i32, i32) {
    let cell = |at: f32| (at / CELL).floor() as i32;
    (
        cell(rect.x),
        cell(rect.y),
        cell(rect.right()),
        cell(rect.bottom()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagram::geometry::{CARD_WIDTH, Detail, Handle};
    use crate::diagram::layout::{LAYER_GAP, schema};
    use crate::diagram::tests::graph;

    /// Table 1 references table 0: 0 in the first layer, 1 in the second.
    fn scene() -> Scene {
        Scene::new(schema(&graph(2, &[(1, 0)]), false))
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn culling_keeps_what_meets_the_viewport_and_nothing_else() {
        let scene = scene();
        let gap = CARD_WIDTH + LAYER_GAP / 2.0;
        assert_eq!(
            scene.visible(rect(0.0, 0.0, 100.0, 40.0), 1.0),
            Visible {
                cards: vec![0],
                edges: vec![]
            }
        );
        assert_eq!(
            scene.visible(rect(gap - 4.0, 0.0, 8.0, 200.0), 1.0),
            Visible {
                cards: vec![],
                edges: vec![0]
            }
        );
        assert_eq!(
            scene.visible(rect(-10.0, -10.0, 2000.0, 2000.0), 1.0),
            Visible {
                cards: vec![0, 1],
                edges: vec![0]
            }
        );
        assert_eq!(
            scene.visible(rect(5000.0, 5000.0, 100.0, 100.0), 1.0),
            Visible::default()
        );
        // A name block is shorter than the card it stands for.
        let card = &scene.layout().cards[0];
        let below_block = rect(card.origin.x, card.origin.y + card.height - 4.0, 50.0, 2.0);
        assert_eq!(scene.visible(below_block, 1.0).cards, vec![0]);
        assert_eq!(scene.visible(below_block, 0.5).cards, Vec::<u32>::new());
    }

    #[test]
    fn hit_testing_finds_the_card_and_the_edge_and_follows_a_drag() {
        let mut scene = scene();
        let centre = |scene: &Scene, card: u32| {
            scene
                .layout()
                .card_rect(card, scene.layout().detail(1.0), 1.0)
                .centre()
        };
        let middle = |scene: &Scene| {
            let points = scene
                .layout()
                .edge_path(0, scene.layout().detail(1.0), 1.0)
                .polyline();
            points[points.len() / 2]
        };
        assert_eq!(scene.card_at(centre(&scene, 0), 1.0), Some(0));
        assert_eq!(scene.card_at(centre(&scene, 1), 1.0), Some(1));
        assert_eq!(scene.card_at(Point::new(-50.0, -50.0), 1.0), None);
        let on_edge = middle(&scene);
        assert_eq!(scene.edge_at(on_edge, 4.0, 1.0), Some(0));
        assert_eq!(
            scene.edge_at(Point::new(on_edge.x, on_edge.y + 40.0), 4.0, 1.0),
            None
        );

        let before = centre(&scene, 1);
        assert_eq!(scene.move_card(1, Point::new(2000.0, 1500.0)), &[0]);
        assert_eq!(scene.card_at(before, 1.0), None);
        assert_eq!(scene.card_at(centre(&scene, 1), 1.0), Some(1));
        assert_eq!(scene.edge_at(on_edge, 4.0, 1.0), None);
        assert_eq!(scene.edge_at(middle(&scene), 4.0, 1.0), Some(0));
        assert_eq!(
            scene.visible(rect(1990.0, 1490.0, 20.0, 20.0), 1.0).cards,
            vec![1]
        );
    }

    #[test]
    fn a_handle_is_found_on_the_card_under_the_pointer_and_a_resize_refiles_it() {
        // Three columns, so its height can be cut.
        let mut graph = graph(2, &[(1, 0)]);
        let note = graph.tables[1].columns[1].clone();
        graph.tables[1].columns.push(note);
        let mut scene = Scene::new(schema(&graph, false));
        let card = scene.layout().card_rect(1, Detail::Columns, 1.0);
        let corner = Point::new(card.right() - 1.0, card.bottom() - 1.0);
        assert_eq!(scene.handle_at(corner, 5.0, 1.0), Some((1, Handle::Corner)));
        assert_eq!(
            scene.handle_at(Point::new(card.right() + 3.0, card.y + 4.0), 5.0, 1.0),
            Some((1, Handle::Right))
        );
        assert_eq!(scene.handle_at(card.centre(), 5.0, 1.0), None);

        let wider = Point::new(card.right() + 100.0, card.centre().y);
        assert_eq!(scene.card_at(wider, 1.0), None);
        assert_eq!(scene.resize_card(1, Some(card.width + 200.0), None), &[0]);
        assert_eq!(
            scene.card_at(wider, 1.0),
            Some(1),
            "refiled at its new width"
        );
        assert_eq!(
            scene.visible(rect(wider.x, wider.y, 1.0, 1.0), 1.0).cards,
            vec![1]
        );
        scene.resize_card(1, None, None);
        assert_eq!(scene.layout().cards[1].width, CARD_WIDTH);
    }

    #[test]
    fn a_handle_never_reaches_over_another_card() {
        // Zoomed far out, a handle's reach in world units is wider than the
        // gap between two cards.
        let mut scene = scene();
        let left = scene.layout().card_rect(0, Detail::Columns, 1.0);
        let gap = 10.0;
        scene.move_card(1, Point::new(left.right() + gap, left.y));
        let on_right_card = Point::new(left.right() + gap + 2.0, left.y + 10.0);
        assert_eq!(scene.handle_at(on_right_card, 40.0, 1.0), None);
        let in_gap = Point::new(left.right() + gap / 2.0, left.y + 10.0);
        assert_eq!(scene.handle_at(in_gap, 40.0, 1.0), Some((0, Handle::Right)));
    }

    #[test]
    fn a_card_put_absurdly_far_away_is_kept_aside_rather_than_filed_in_every_cell() {
        let mut scene = scene();
        // The edge between the two now spans a trillion cells each way.
        scene.move_card(1, Point::new(f32::MAX / 2.0, f32::MAX / 2.0));
        assert!(scene.cells.values().map(Vec::len).sum::<usize>() < 64);
        assert_eq!(
            scene.visible(rect(-10.0, -10.0, 2000.0, 2000.0), 1.0),
            Visible {
                cards: vec![0],
                edges: vec![0]
            },
            "the edge is still found where it passes"
        );
        scene.move_card(1, Point::new(600.0, 0.0));
        assert!(scene.wide.is_empty(), "and is filed again once back");
        assert_eq!(
            scene.visible(rect(-10.0, -10.0, 2000.0, 2000.0), 1.0).cards,
            vec![0, 1]
        );
    }

    #[test]
    fn a_released_scene_keeps_its_layout_and_files_it_again_as_it_was_moved() {
        let mut scene = scene();
        let everything = rect(-10.0, -10.0, 4000.0, 4000.0);
        let whole = scene.visible(everything, 1.0);
        scene.release();
        assert!(!scene.is_filed() && scene.cells.is_empty() && scene.envelopes.is_empty());
        assert_eq!(scene.visible(everything, 1.0), Visible::default());
        assert_eq!(scene.layout().cards.len(), 2, "the layout stays");

        assert_eq!(scene.move_card(1, Point::new(2000.0, 1500.0)), &[0]);
        scene.refile();
        assert_eq!(scene.visible(everything, 1.0), whole);
        assert_eq!(
            scene.visible(rect(1990.0, 1490.0, 20.0, 20.0), 1.0).cards,
            vec![1],
            "filed where it was moved while released"
        );
    }

    #[test]
    fn an_edge_hit_is_the_nearest_edge_at_the_level_drawn_and_a_loop_counts() {
        // Tables 1 and 2 both reference 0, and 0 references itself.
        let scene = Scene::new(schema(&graph(3, &[(1, 0), (2, 0), (0, 0)]), false));
        let layout = scene.layout();
        for zoom in [1.0, 0.3] {
            let detail = layout.detail(zoom);
            for edge in 0..layout.edges.len() as u32 {
                let points = layout.edge_path(edge, detail, zoom).polyline();
                let middle = points[points.len() / 2];
                assert_eq!(scene.edge_at(middle, 4.0, zoom), Some(edge), "{zoom}");
            }
        }

        // Between the two keys into table 0, nearer the first.
        let path = |edge: u32| layout.edge_path(edge, Detail::Columns, 1.0).polyline();
        let (a, b) = (path(0), path(1));
        let (a, b) = (a[a.len() / 2], b[b.len() / 2]);
        let nearer_a = Point::new(a.x + (b.x - a.x) * 0.3, a.y + (b.y - a.y) * 0.3);
        let reach = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
        assert_eq!(scene.edge_at(nearer_a, reach, 1.0), Some(0));
    }
}
