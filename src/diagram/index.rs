//! A laid-out diagram indexed for the two questions a frame and a pointer ask:
//! what meets this rectangle, and what is under this point. A uniform grid in
//! world space, so neither scans every card.

use std::collections::HashMap;

use super::geometry::{Point, Rect};
use super::layout::Layout;

/// Wider than a card, so a pointer looks in one or two cells.
const CELL: f32 = 256.0;

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
    /// The cells that hold anything, so a query zoomed far out visits only
    /// those rather than every cell of the viewport.
    occupied: Option<(i32, i32, i32, i32)>,
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
        let envelopes = (0..layout.edges.len() as u32)
            .map(|edge| layout.edge_envelopes(edge))
            .collect();
        let mut scene = Self {
            layout,
            envelopes,
            cells: HashMap::new(),
            occupied: None,
        };
        for card in 0..scene.layout.cards.len() as u32 {
            scene.file(Entry::Card(card));
        }
        for edge in 0..scene.layout.edges.len() as u32 {
            scene.file(Entry::Edge(edge));
        }
        scene
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

    /// Moves a card and refiles it and the edges that end at it, and only
    /// those. Returns them, so their cached paths can be dropped.
    pub fn move_card(&mut self, card: u32, origin: Point) -> &[u32] {
        self.unfile(Entry::Card(card));
        let edges = self.layout.cards[card as usize].edges.clone();
        for &edge in &edges {
            self.unfile(Entry::Edge(edge));
        }
        self.layout.cards[card as usize].origin = origin;
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
                    width: super::geometry::CARD_WIDTH,
                    height: card.height,
                }]
            }
            Entry::Edge(edge) => self.envelopes[edge as usize].clone(),
        }
    }

    fn file(&mut self, entry: Entry) {
        for rect in self.rects(entry) {
            let (left, top, right, bottom) = cells(rect);
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
        for rect in self.rects(entry) {
            let (left, top, right, bottom) = cells(rect);
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
            .copied()
    }
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
    use crate::diagram::geometry::CARD_WIDTH;
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
}
