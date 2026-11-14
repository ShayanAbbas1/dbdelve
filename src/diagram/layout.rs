//! Where every card goes: a layered (Sugiyama) layout of a whole schema, and
//! the neighbourhood of one table. Both are pure functions of the graph, so
//! they run off the UI thread and are tested without a window.

use std::cmp::Reverse;
use std::collections::HashMap;

use crate::db::{KeyTarget, SchemaGraph};

use super::geometry::{
    CARD_WIDTH, ChildMark, Detail, LOOP_CONTROL, ParentMark, Point, card_height, row_offset,
};

/// The space between layers. An edge's curve and both its marks live in it,
/// and so does a self-reference's loop.
pub const LAYER_GAP: f32 = 96.0;
/// Sweeps of crossing reduction are capped, so the layout's work is bounded
/// by the graph's size and never by how stubborn its crossings are.
pub const MAX_SWEEPS: u32 = 8;
const PLACEMENT_PASSES: u32 = 8;
const CARD_GAP: f32 = 24.0;
/// Between a lane and the card above or below it in its layer.
const LANE_CLEARANCE: f32 = 12.0;
const LANE_GAP: f32 = 8.0;
const COMPONENT_GAP: f32 = 96.0;
/// Components are packed into rows about this much wider than tall.
const ASPECT: f32 = 1.6;
/// A lane pulls harder than a card towards its neighbours, so a long edge
/// runs straight rather than stepping at every layer.
const LANE_WEIGHT: f32 = 2.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// Cards were given room for their key columns only.
    pub keys_only: bool,
    pub cards: Vec<Card>,
    pub edges: Vec<Edge>,
    pub stats: Stats,
}

/// What a card stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subject {
    /// An index into [`SchemaGraph::tables`].
    Table(u32),
    /// A table in another schema, named by the first key into it (an index
    /// into [`SchemaGraph::keys`]). It shows its qualified name only.
    Stub(u32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub subject: Subject,
    /// Top-left, in world units.
    pub origin: Point,
    /// [`CARD_WIDTH`], or the width the user dragged it to.
    pub width: f32,
    /// The height the layout made room for, or the shorter one the user
    /// dragged it to. Every level of detail draws inside it.
    pub height: f32,
    pub columns: u32,
    /// The columns shown when only keys are, in column order: those in a
    /// primary, unique or foreign key, and those a foreign key references.
    pub key_rows: Vec<u32>,
    /// Edges that end at this card, so a drag reroutes those and no others.
    pub edges: Vec<u32>,
}

/// One foreign key, drawn from the referencing (child) table's first key
/// column to the referenced (parent) table's.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    /// An index into [`SchemaGraph::keys`].
    pub key: u32,
    pub child: u32,
    pub child_column: u32,
    pub parent: u32,
    /// `None` on a stub, which has no columns to point at.
    pub parent_column: Option<u32>,
    pub child_mark: ChildMark,
    pub parent_mark: ParentMark,
    /// The gaps it runs through in the layers it crosses, from the child's
    /// side to the parent's.
    pub lanes: Vec<Lane>,
}

/// A straight run across one layer, between the cards of that layer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lane {
    pub y: f32,
    pub left: f32,
    pub right: f32,
}

/// The layout's work, which is deterministic for a graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// The most layers in any component.
    pub layers: u32,
    /// Lanes over every edge.
    pub dummies: u32,
    /// The most sweeps in any component, never more than [`MAX_SWEEPS`].
    pub sweeps: u32,
    /// Crossings between adjacent layers in the starting order, and after.
    pub crossings_before: u64,
    pub crossings: u64,
}

/// The whole schema, layered left to right: referenced tables left of those
/// that reference them, long edges through lanes, tables with no keys packed
/// in a block after the rest.
pub fn schema(graph: &SchemaGraph, keys_only: bool) -> Layout {
    let mut builder = Builder::new(graph, keys_only);
    for table in 0..graph.tables.len() as u32 {
        builder.table(table);
    }
    for key in 0..graph.keys.len() as u32 {
        builder.edge(key);
    }
    let mut layout = builder.finish();

    let (components, keyless) = components(&layout);
    let sizes: Vec<(f32, f32)> = components
        .iter()
        .map(|component| layer_component(&mut layout, component))
        .collect();

    let mut packing: Vec<usize> = (0..components.len()).collect();
    packing.sort_by_key(|&index| {
        (
            Reverse(components[index].cards.len()),
            components[index].cards[0],
        )
    });
    let keyless_area: f32 = keyless
        .iter()
        .map(|&card| (CARD_WIDTH + LAYER_GAP) * (layout.cards[card as usize].height + CARD_GAP))
        .sum();
    let area: f32 = sizes
        .iter()
        .map(|(width, height)| (width + COMPONENT_GAP) * (height + COMPONENT_GAP))
        .sum::<f32>()
        + keyless_area;
    let widest = sizes.iter().map(|size| size.0).fold(CARD_WIDTH, f32::max);
    let row_width = (area * ASPECT).sqrt().max(widest);

    let (mut x, mut y, mut row_height) = (0.0, 0.0, 0.0f32);
    for index in packing {
        let (width, height) = sizes[index];
        if x > 0.0 && x + width > row_width {
            y += row_height + COMPONENT_GAP;
            (x, row_height) = (0.0, 0.0);
        }
        translate(&mut layout, &components[index], Point::new(x, y));
        x += width + COMPONENT_GAP;
        row_height = row_height.max(height);
    }

    let top = match components.is_empty() {
        true => 0.0,
        false => y + row_height + COMPONENT_GAP,
    };
    let columns = (((row_width + LAYER_GAP) / (CARD_WIDTH + LAYER_GAP)) as usize).max(1);
    let mut bottoms = vec![top; columns];
    for card in keyless {
        let (column, &bottom) = bottoms
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.total_cmp(b.1))
            .expect("at least one column");
        let card = &mut layout.cards[card as usize];
        card.origin = Point::new(column as f32 * (CARD_WIDTH + LAYER_GAP), bottom);
        bottoms[column] = bottom + card.height + CARD_GAP;
    }

    layout
}

/// One table in the middle, the tables it references stacked on its left and
/// those referencing it on its right. Only its own edges are drawn: an edge
/// between two neighbours would cross the middle column, under the focus.
pub fn neighbourhood(graph: &SchemaGraph, focus: u32, keys_only: bool) -> Layout {
    let mut builder = Builder::new(graph, keys_only);
    let centre = builder.table(focus);
    for (index, key) in graph.keys.iter().enumerate() {
        let references_focus =
            matches!(key.referenced, KeyTarget::Table { table, .. } if table == focus);
        if key.table == focus || references_focus {
            builder.edge(index as u32);
        }
    }
    let mut layout = builder.finish();

    let detail = rows_detail(keys_only);
    let focus_card = &layout.cards[centre as usize];
    let mut side: Vec<Option<usize>> = vec![None; layout.cards.len()];
    let mut columns: [Vec<(f32, u32)>; 2] = [Vec::new(), Vec::new()];
    for (column, as_child) in [(0, true), (1, false)] {
        for edge in &layout.edges {
            let (neighbour, port) = match as_child {
                true if edge.child == centre => (edge.parent, Some(edge.child_column)),
                false if edge.parent == centre => (edge.child, edge.parent_column),
                _ => continue,
            };
            if neighbour == centre || side[neighbour as usize].is_some() {
                continue;
            }
            side[neighbour as usize] = Some(column);
            columns[column].push((row_offset(focus_card, port, detail), neighbour));
        }
    }

    let middle = layout.cards[centre as usize].height / 2.0;
    layout.cards[centre as usize].origin = Point::new(CARD_WIDTH + LAYER_GAP, 0.0);
    for (index, mut column) in columns.into_iter().enumerate() {
        column.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let heights: Vec<f32> = column
            .iter()
            .map(|&(_, card)| layout.cards[card as usize].height)
            .collect();
        let total = heights.iter().sum::<f32>() + CARD_GAP * heights.len().saturating_sub(1) as f32;
        let x = index as f32 * 2.0 * (CARD_WIDTH + LAYER_GAP);
        let mut y = middle - total / 2.0;
        for (&(_, card), height) in column.iter().zip(heights) {
            layout.cards[card as usize].origin = Point::new(x, y);
            y += height + CARD_GAP;
        }
    }
    let top = layout
        .cards
        .iter()
        .map(|card| card.origin.y)
        .fold(0.0, f32::min);
    for card in &mut layout.cards {
        card.origin.y -= top;
    }
    layout
}

fn rows_detail(keys_only: bool) -> Detail {
    match keys_only {
        true => Detail::Keys,
        false => Detail::Columns,
    }
}

struct Builder<'a> {
    graph: &'a SchemaGraph,
    keys_only: bool,
    /// Per table, per column: some foreign key references it.
    referenced: Vec<Vec<bool>>,
    card_of_table: Vec<Option<u32>>,
    stubs: HashMap<(&'a str, &'a str), u32>,
    layout: Layout,
}

impl<'a> Builder<'a> {
    fn new(graph: &'a SchemaGraph, keys_only: bool) -> Self {
        let mut referenced: Vec<Vec<bool>> = graph
            .tables
            .iter()
            .map(|table| vec![false; table.columns.len()])
            .collect();
        for key in &graph.keys {
            if let KeyTarget::Table { table, columns } = &key.referenced {
                for &column in columns {
                    referenced[*table as usize][column as usize] = true;
                }
            }
        }
        Self {
            graph,
            keys_only,
            referenced,
            card_of_table: vec![None; graph.tables.len()],
            stubs: HashMap::new(),
            layout: Layout {
                keys_only,
                cards: Vec::new(),
                edges: Vec::new(),
                stats: Stats::default(),
            },
        }
    }

    fn table(&mut self, table: u32) -> u32 {
        if let Some(card) = self.card_of_table[table as usize] {
            return card;
        }
        let columns = &self.graph.tables[table as usize].columns;
        let referenced = &self.referenced[table as usize];
        let key_rows: Vec<u32> = (columns.iter().zip(referenced).enumerate())
            .filter(|(_, (column, referenced))| {
                column.primary || column.foreign || column.unique || **referenced
            })
            .map(|(index, _)| index as u32)
            .collect();
        let rows = match self.keys_only {
            true => key_rows.len(),
            false => columns.len(),
        };
        let card = self.push(Card {
            subject: Subject::Table(table),
            origin: Point::default(),
            width: CARD_WIDTH,
            height: card_height(rows),
            columns: columns.len() as u32,
            key_rows,
            edges: Vec::new(),
        });
        self.card_of_table[table as usize] = Some(card);
        card
    }

    fn stub(&mut self, key: u32, schema: &'a str, table: &'a str) -> u32 {
        if let Some(&card) = self.stubs.get(&(schema, table)) {
            return card;
        }
        let card = self.push(Card {
            subject: Subject::Stub(key),
            origin: Point::default(),
            width: CARD_WIDTH,
            height: card_height(0),
            columns: 0,
            key_rows: Vec::new(),
            edges: Vec::new(),
        });
        self.stubs.insert((schema, table), card);
        card
    }

    fn push(&mut self, card: Card) -> u32 {
        self.layout.cards.push(card);
        self.layout.cards.len() as u32 - 1
    }

    fn edge(&mut self, index: u32) {
        let graph = self.graph;
        let key = &graph.keys[index as usize];
        let Some(&child_column) = key.columns.first() else {
            return;
        };
        let child = self.table(key.table);
        let (parent, parent_column) = match &key.referenced {
            KeyTarget::Table { table, columns } => (self.table(*table), columns.first().copied()),
            KeyTarget::Elsewhere { schema, table } => (self.stub(index, schema, table), None),
        };
        let columns = &graph.tables[key.table as usize].columns;
        let in_key = |column: &u32| &columns[*column as usize];
        let one_to_one = match key.columns[..] {
            [column] => columns[column as usize].unique,
            _ => {
                key.columns.iter().all(|column| in_key(column).primary)
                    && columns.iter().filter(|column| column.primary).count() == key.columns.len()
            }
        };
        self.layout.edges.push(Edge {
            key: index,
            child,
            child_column,
            parent,
            parent_column,
            child_mark: match one_to_one {
                true => ChildMark::One,
                false => ChildMark::Many,
            },
            parent_mark: match key.columns.iter().any(|column| in_key(column).nullable) {
                true => ParentMark::ZeroOrOne,
                false => ParentMark::ExactlyOne,
            },
            lanes: Vec::new(),
        });
    }

    fn finish(mut self) -> Layout {
        for (index, edge) in self.layout.edges.iter().enumerate() {
            self.layout.cards[edge.child as usize]
                .edges
                .push(index as u32);
            if edge.parent != edge.child {
                self.layout.cards[edge.parent as usize]
                    .edges
                    .push(index as u32);
            }
        }
        self.layout
    }
}

/// Cards joined by edges, and the edges between them other than loops.
struct Component {
    cards: Vec<u32>,
    edges: Vec<u32>,
}

/// The connected components in order of their first card, and the cards with
/// no edge but a loop, which are packed rather than layered.
fn components(layout: &Layout) -> (Vec<Component>, Vec<u32>) {
    let mut root: Vec<u32> = (0..layout.cards.len() as u32).collect();
    fn find(root: &mut [u32], mut card: u32) -> u32 {
        while root[card as usize] != card {
            root[card as usize] = root[root[card as usize] as usize];
            card = root[card as usize];
        }
        card
    }
    let mut joined = vec![false; layout.cards.len()];
    for edge in layout.edges.iter().filter(|edge| edge.child != edge.parent) {
        let (a, b) = (find(&mut root, edge.child), find(&mut root, edge.parent));
        root[a.max(b) as usize] = a.min(b);
        joined[edge.child as usize] = true;
        joined[edge.parent as usize] = true;
    }
    let mut components: Vec<Component> = Vec::new();
    let mut index_of: HashMap<u32, usize> = HashMap::new();
    let mut keyless = Vec::new();
    for card in 0..layout.cards.len() as u32 {
        if !joined[card as usize] {
            keyless.push(card);
            continue;
        }
        let root = find(&mut root, card);
        let index = *index_of.entry(root).or_insert_with(|| {
            components.push(Component {
                cards: Vec::new(),
                edges: Vec::new(),
            });
            components.len() - 1
        });
        components[index].cards.push(card);
    }
    for (index, edge) in layout.edges.iter().enumerate() {
        if edge.child != edge.parent {
            let root = find(&mut root, edge.child);
            components[index_of[&root]].edges.push(index as u32);
        }
    }
    (components, keyless)
}

/// Moves a laid-out component, its cards and its lanes, by `by`.
fn translate(layout: &mut Layout, component: &Component, by: Point) {
    for &card in &component.cards {
        let origin = &mut layout.cards[card as usize].origin;
        (origin.x, origin.y) = (origin.x + by.x, origin.y + by.y);
    }
    for &edge in &component.edges {
        for lane in &mut layout.edges[edge as usize].lanes {
            lane.y += by.y;
            lane.left += by.x;
            lane.right += by.x;
        }
    }
}

/// An edge between adjacent layers, from a node on the left to one on the
/// right, each met at a port: the row's offset below the node's top.
struct Link {
    left: usize,
    left_port: f32,
    right: usize,
    right_port: f32,
}

/// The nodes of one component's layered drawing: its cards, then a dummy per
/// layer each long edge crosses.
struct Nodes {
    layer: Vec<usize>,
    height: Vec<f32>,
    dummy: Vec<bool>,
    links: Vec<Link>,
    /// Per node, the links meeting it from the layer on its left, and those
    /// leaving it for the layer on its right.
    from_left: Vec<Vec<usize>>,
    to_right: Vec<Vec<usize>>,
}

/// Lays one component out with its top-left at the origin, writing its cards'
/// positions and its edges' lanes, and returns its size.
fn layer_component(layout: &mut Layout, component: &Component) -> (f32, f32) {
    let detail = rows_detail(layout.keys_only);
    let local: HashMap<u32, usize> = (component.cards.iter().enumerate())
        .map(|(index, &card)| (card, index))
        .collect();
    let count = component.cards.len();
    let arcs: Vec<(usize, usize)> = (component.edges.iter())
        .map(|&edge| {
            let edge = &layout.edges[edge as usize];
            (local[&edge.parent], local[&edge.child])
        })
        .collect();

    let sequence = acyclic_order(count, &arcs);
    let mut rank = vec![0; count];
    for (position, &node) in sequence.iter().enumerate() {
        rank[node] = position;
    }
    // A key the cycle breaking turned around keeps its true direction when
    // drawn; only its layering runs the other way.
    let reversed: Vec<bool> = arcs
        .iter()
        .map(|&(parent, child)| rank[parent] > rank[child])
        .collect();
    let dag: Vec<(usize, usize)> = (arcs.iter().zip(&reversed))
        .map(|(&(parent, child), &reversed)| match reversed {
            true => (child, parent),
            false => (parent, child),
        })
        .collect();

    let mut into = vec![Vec::new(); count];
    let mut out_of = vec![Vec::new(); count];
    for (index, &(left, right)) in dag.iter().enumerate() {
        out_of[left].push(index);
        into[right].push(index);
    }
    let mut layer = vec![0usize; count];
    for &node in &sequence {
        for &arc in &into[node] {
            layer[node] = layer[node].max(layer[dag[arc].0] + 1);
        }
    }
    // Longest path stacks every source in the first layer. Pulling each one
    // up to just left of its nearest child shortens its edges, and with them
    // the lanes, without moving anything else.
    for &node in sequence.iter().rev() {
        if into[node].is_empty()
            && let Some(nearest) = out_of[node].iter().map(|&arc| layer[dag[arc].1]).min()
        {
            layer[node] = nearest - 1;
        }
    }

    let card_of = |node: usize| &layout.cards[component.cards[node] as usize];
    let mut nodes = Nodes {
        layer: layer.clone(),
        height: (0..count).map(|node| card_of(node).height).collect(),
        dummy: vec![false; count],
        links: Vec::new(),
        from_left: Vec::new(),
        to_right: Vec::new(),
    };
    let mut chains: Vec<Vec<usize>> = Vec::with_capacity(dag.len());
    for (index, &(left, right)) in dag.iter().enumerate() {
        let edge = &layout.edges[component.edges[index] as usize];
        let child_port = row_offset(card_of(local[&edge.child]), Some(edge.child_column), detail);
        let parent_port = row_offset(card_of(local[&edge.parent]), edge.parent_column, detail);
        let (left_port, right_port) = match reversed[index] {
            true => (child_port, parent_port),
            false => (parent_port, child_port),
        };
        let mut chain = Vec::new();
        let (mut previous, mut previous_port) = (left, left_port);
        for between in layer[left] + 1..layer[right] {
            let dummy = nodes.layer.len();
            nodes.layer.push(between);
            nodes.height.push(0.0);
            nodes.dummy.push(true);
            nodes.links.push(Link {
                left: previous,
                left_port: previous_port,
                right: dummy,
                right_port: 0.0,
            });
            chain.push(dummy);
            (previous, previous_port) = (dummy, 0.0);
        }
        nodes.links.push(Link {
            left: previous,
            left_port: previous_port,
            right,
            right_port,
        });
        chains.push(chain);
    }
    let total = nodes.layer.len();
    nodes.from_left = vec![Vec::new(); total];
    nodes.to_right = vec![Vec::new(); total];
    for (index, link) in nodes.links.iter().enumerate() {
        nodes.to_right[link.left].push(index);
        nodes.from_left[link.right].push(index);
    }

    let layers = layer.iter().max().map_or(0, |last| last + 1);
    let mut order: Vec<Vec<usize>> = vec![Vec::new(); layers];
    for node in 0..total {
        order[nodes.layer[node]].push(node);
    }
    let mut gaps: Vec<Vec<usize>> = vec![Vec::new(); layers];
    for (index, link) in nodes.links.iter().enumerate() {
        gaps[nodes.layer[link.left]].push(index);
    }

    let mut y = vec![0.0f32; total];
    for nodes_in_layer in &order {
        stack(nodes_in_layer, &nodes, &mut y);
    }
    let before = crossings(&gaps, &nodes.links, &y);
    let mut best = (before, order.clone());
    let mut sweeps = 0;
    while sweeps < MAX_SWEEPS && best.0 > 0 {
        let from_left = sweeps % 2 == 0;
        let sequence: Vec<usize> = match from_left {
            true => (1..layers).collect(),
            false => (0..layers.saturating_sub(1)).rev().collect(),
        };
        for at in sequence {
            reorder(&mut order[at], &nodes, &y, from_left);
            stack(&order[at], &nodes, &mut y);
        }
        sweeps += 1;
        let now = crossings(&gaps, &nodes.links, &y);
        if now < best.0 {
            best = (now, order.clone());
        }
    }
    let (after, order) = best;
    for nodes_in_layer in &order {
        stack(nodes_in_layer, &nodes, &mut y);
    }

    for pass in 0..PLACEMENT_PASSES {
        let from_left = pass % 2 == 0;
        let sequence: Vec<usize> = match from_left {
            true => (1..layers).collect(),
            false => (0..layers.saturating_sub(1)).rev().collect(),
        };
        for at in sequence {
            place(&order[at], &nodes, &mut y, from_left);
        }
    }
    let top = y.iter().copied().fold(f32::INFINITY, f32::min);
    let top = if top.is_finite() { top } else { 0.0 };
    let x = |layer: usize| layer as f32 * (CARD_WIDTH + LAYER_GAP);

    for (node, &card) in component.cards.iter().enumerate() {
        layout.cards[card as usize].origin = Point::new(x(nodes.layer[node]), y[node] - top);
    }
    for (index, chain) in chains.iter().enumerate() {
        let lanes = chain.iter().map(|&dummy| Lane {
            y: y[dummy] - top,
            left: x(nodes.layer[dummy]),
            right: x(nodes.layer[dummy]) + CARD_WIDTH,
        });
        let edge = &mut layout.edges[component.edges[index] as usize];
        // A chain runs left to right; lanes run from the child's side.
        edge.lanes = match reversed[index] {
            true => lanes.collect(),
            false => lanes.rev().collect(),
        };
    }

    let stats = &mut layout.stats;
    stats.layers = stats.layers.max(layers as u32);
    stats.dummies += (total - count) as u32;
    stats.sweeps = stats.sweeps.max(sweeps);
    stats.crossings_before += before;
    stats.crossings += after;

    let width = x(layers.saturating_sub(1)) + CARD_WIDTH + LOOP_CONTROL;
    let height = (0..total)
        .map(|node| y[node] - top + nodes.height[node])
        .fold(0.0, f32::max);
    (width, height)
}

/// An order of the nodes in which few arcs point backwards: Eades, Lin and
/// Smyth's greedy heuristic. Sinks go to the end, sources to the front, and
/// when neither is left, the node with the most outgoing over incoming arcs
/// goes to the front. Reversing the backward arcs breaks every cycle.
fn acyclic_order(count: usize, arcs: &[(usize, usize)]) -> Vec<usize> {
    let mut out_degree = vec![0i64; count];
    let mut in_degree = vec![0i64; count];
    let mut out_of = vec![Vec::new(); count];
    let mut into = vec![Vec::new(); count];
    for &(from, to) in arcs {
        out_degree[from] += 1;
        in_degree[to] += 1;
        out_of[from].push(to);
        into[to].push(from);
    }
    let mut removed = vec![false; count];
    let mut front = Vec::with_capacity(count);
    let mut back = Vec::new();
    let mut sinks: Vec<usize> = (0..count)
        .filter(|&node| out_degree[node] == 0)
        .rev()
        .collect();
    let mut sources: Vec<usize> = (0..count)
        .filter(|&node| in_degree[node] == 0)
        .rev()
        .collect();
    let mut left = count;

    let remove = |node: usize,
                  removed: &mut [bool],
                  out_degree: &mut [i64],
                  in_degree: &mut [i64],
                  sinks: &mut Vec<usize>,
                  sources: &mut Vec<usize>| {
        removed[node] = true;
        for &from in &into[node] {
            out_degree[from] -= 1;
            if out_degree[from] == 0 && !removed[from] {
                sinks.push(from);
            }
        }
        for &to in &out_of[node] {
            in_degree[to] -= 1;
            if in_degree[to] == 0 && !removed[to] {
                sources.push(to);
            }
        }
    };

    while left > 0 {
        if let Some(node) = sinks.pop() {
            if !removed[node] {
                remove(
                    node,
                    &mut removed,
                    &mut out_degree,
                    &mut in_degree,
                    &mut sinks,
                    &mut sources,
                );
                back.push(node);
                left -= 1;
            }
            continue;
        }
        if let Some(node) = sources.pop() {
            if !removed[node] {
                remove(
                    node,
                    &mut removed,
                    &mut out_degree,
                    &mut in_degree,
                    &mut sinks,
                    &mut sources,
                );
                front.push(node);
                left -= 1;
            }
            continue;
        }
        let node = (0..count)
            .filter(|&node| !removed[node])
            .max_by_key(|&node| (out_degree[node] - in_degree[node], Reverse(node)))
            .expect("a node is left");
        remove(
            node,
            &mut removed,
            &mut out_degree,
            &mut in_degree,
            &mut sinks,
            &mut sources,
        );
        front.push(node);
        left -= 1;
    }
    front.extend(back.into_iter().rev());
    front
}

fn gap(nodes: &Nodes, above: usize, below: usize) -> f32 {
    match (nodes.dummy[above], nodes.dummy[below]) {
        (true, true) => LANE_GAP,
        (false, false) => CARD_GAP,
        _ => LANE_CLEARANCE,
    }
}

/// Stacks a layer's nodes top to bottom in their order, as close as allowed.
fn stack(layer: &[usize], nodes: &Nodes, y: &mut [f32]) {
    let mut cursor = 0.0;
    for (index, &node) in layer.iter().enumerate() {
        if index > 0 {
            cursor += gap(nodes, layer[index - 1], node);
        }
        y[node] = cursor;
        cursor += nodes.height[node];
    }
}

/// Where a node's top would sit to meet its neighbours on one side level, by
/// the rows its links attach to rather than its centre.
fn barycentre(node: usize, nodes: &Nodes, y: &[f32], from_left: bool) -> Option<f32> {
    let links = match from_left {
        true => &nodes.from_left[node],
        false => &nodes.to_right[node],
    };
    if links.is_empty() {
        return None;
    }
    let sum: f32 = links
        .iter()
        .map(|&index| {
            let link = &nodes.links[index];
            match from_left {
                true => y[link.left] + link.left_port - link.right_port,
                false => y[link.right] + link.right_port - link.left_port,
            }
        })
        .sum();
    Some(sum / links.len() as f32)
}

fn reorder(layer: &mut [usize], nodes: &Nodes, y: &[f32], from_left: bool) {
    let mut keyed: Vec<(f32, usize)> = layer
        .iter()
        .map(|&node| {
            (
                barycentre(node, nodes, y, from_left).unwrap_or(y[node]),
                node,
            )
        })
        .collect();
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (slot, (_, node)) in layer.iter_mut().zip(keyed) {
        *slot = node;
    }
}

/// Moves a layer's nodes as near their barycentres as their order and gaps
/// allow: the least-squares placement, by pooling adjacent violators.
fn place(layer: &[usize], nodes: &Nodes, y: &mut [f32], from_left: bool) {
    let mut offsets = Vec::with_capacity(layer.len());
    let mut cursor = 0.0;
    for (index, &node) in layer.iter().enumerate() {
        if index > 0 {
            cursor += gap(nodes, layer[index - 1], node);
        }
        offsets.push(cursor);
        cursor += nodes.height[node];
    }
    // (first index, weight, mean) of each pooled block.
    let mut blocks: Vec<(usize, f32, f32)> = Vec::new();
    for (index, &node) in layer.iter().enumerate() {
        let target = barycentre(node, nodes, y, from_left).unwrap_or(y[node]) - offsets[index];
        let weight = match nodes.dummy[node] {
            true => LANE_WEIGHT,
            false => 1.0,
        };
        let mut block = (index, weight, target);
        while let Some(&(first, previous_weight, mean)) = blocks.last() {
            if mean <= block.2 {
                break;
            }
            let weight = previous_weight + block.1;
            block = (
                first,
                weight,
                (mean * previous_weight + block.2 * block.1) / weight,
            );
            blocks.pop();
        }
        blocks.push(block);
    }
    for (number, &(first, _, mean)) in blocks.iter().enumerate() {
        let end = blocks.get(number + 1).map_or(layer.len(), |next| next.0);
        for index in first..end {
            y[layer[index]] = mean + offsets[index];
        }
    }
}

/// Pairs of links between adjacent layers that cross, counted at the rows
/// they attach to. Links that share a port do not cross.
fn crossings(gaps: &[Vec<usize>], links: &[Link], y: &[f32]) -> u64 {
    gaps.iter()
        .map(|gap| {
            let mut ends: Vec<(f32, f32)> = gap
                .iter()
                .map(|&index| {
                    let link = &links[index];
                    (
                        y[link.left] + link.left_port,
                        y[link.right] + link.right_port,
                    )
                })
                .collect();
            ends.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
            let mut rights: Vec<f32> = ends.into_iter().map(|end| end.1).collect();
            let mut scratch = rights.clone();
            inversions(&mut rights, &mut scratch)
        })
        .sum()
}

/// Sorts `values`, counting the pairs that were strictly out of order.
fn inversions(values: &mut [f32], scratch: &mut [f32]) -> u64 {
    let length = values.len();
    if length < 2 {
        return 0;
    }
    let middle = length / 2;
    let mut count = inversions(&mut values[..middle], &mut scratch[..middle])
        + inversions(&mut values[middle..], &mut scratch[middle..]);
    let (mut left, mut right) = (0, middle);
    for slot in scratch.iter_mut().take(length) {
        if right < length && (left >= middle || values[right] < values[left]) {
            if left < middle {
                count += (middle - left) as u64;
            }
            *slot = values[right];
            right += 1;
        } else {
            *slot = values[left];
            left += 1;
        }
    }
    values.copy_from_slice(&scratch[..length]);
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{GraphColumn, GraphKey, KeyTarget};
    use crate::diagram::geometry::HEADER_HEIGHT;
    use crate::diagram::tests::graph;

    fn x(layout: &Layout, table: u32) -> f32 {
        layout.cards[table as usize].origin.x
    }

    #[test]
    fn referenced_tables_sit_left_of_the_tables_that_reference_them() {
        // 0 <- 1 <- 2, 0 <- 2, 3 <- 2
        let layout = schema(&graph(4, &[(1, 0), (2, 1), (2, 0), (2, 3)]), false);
        assert!(x(&layout, 0) < x(&layout, 1));
        assert!(x(&layout, 1) < x(&layout, 2));
        assert!(x(&layout, 3) < x(&layout, 2));
        // The source 3 is pulled up next to its only child, not left in the
        // first layer.
        assert_eq!(x(&layout, 3), x(&layout, 1));
        // 0 -> 2 spans a layer, through a lane in it.
        let long = layout
            .edges
            .iter()
            .find(|edge| edge.child == 2 && edge.parent == 0)
            .unwrap();
        assert_eq!(long.lanes.len(), 1);
        assert_eq!(long.lanes[0].left, x(&layout, 1));
        assert_eq!(layout.stats.dummies, 1);
    }

    #[test]
    fn a_cycle_is_laid_out_with_every_card_in_a_layer_of_its_own() {
        let layout = schema(&graph(3, &[(1, 0), (2, 1), (0, 2), (0, 0)]), false);
        let mut xs: Vec<f32> = (0..3).map(|table| x(&layout, table)).collect();
        xs.sort_by(f32::total_cmp);
        xs.dedup();
        assert_eq!(xs.len(), 3);
    }

    #[test]
    fn crossing_reduction_never_ends_worse_than_the_order_it_started_from() {
        // Parents 0 and 1, children 2 and 3 cross-wired, and 4 joining both
        // pairs: in table order 2 -> 1 crosses 3 -> 0, and reordering either
        // layer uncrosses them.
        let layout = schema(&graph(5, &[(2, 1), (3, 0), (4, 0), (4, 1)]), false);
        assert!(layout.stats.crossings_before > 0, "{:?}", layout.stats);
        assert_eq!(layout.stats.crossings, 0, "{:?}", layout.stats);
        assert!(layout.stats.sweeps >= 1 && layout.stats.sweeps <= MAX_SWEEPS);
    }

    #[test]
    fn tables_with_no_keys_are_packed_below_the_layered_ones() {
        let layout = schema(&graph(5, &[(1, 0), (3, 3)]), false);
        let layered_bottom = (0..2)
            .map(|table| layout.cards[table].origin.y + layout.cards[table].height)
            .fold(0.0, f32::max);
        for table in 2..5 {
            assert!(
                layout.cards[table].origin.y > layered_bottom,
                "table {table}"
            );
        }
        assert_eq!(layout.cards[2].origin.x, 0.0);
        assert_ne!(layout.cards[3].origin.x, layout.cards[2].origin.x);
    }

    #[test]
    fn a_key_into_another_schema_points_at_one_stub_per_table() {
        let mut graph = graph(2, &[]);
        for child in 0..2 {
            graph.tables[child].columns.push(GraphColumn {
                name: "account_id".into(),
                type_label: 0,
                nullable: false,
                primary: false,
                foreign: true,
                unique: false,
            });
            graph.keys.push(GraphKey {
                name: format!("fk_{child}").into(),
                table: child as u32,
                columns: vec![1],
                referenced: KeyTarget::Elsewhere {
                    schema: "billing".into(),
                    table: "accounts".into(),
                },
            });
        }
        let layout = schema(&graph, false);
        assert_eq!(layout.cards.len(), 3);
        assert_eq!(layout.cards[2].subject, Subject::Stub(0));
        assert_eq!(layout.cards[2].height, HEADER_HEIGHT);
        assert!(
            layout
                .edges
                .iter()
                .all(|edge| edge.parent == 2 && edge.parent_column.is_none())
        );
        assert!(x(&layout, 2) < x(&layout, 0));
    }

    #[test]
    fn the_neighbourhood_puts_parents_left_children_right_and_only_the_focus_edges() {
        // 1 is the focus: it references 0 and 3, and 2 references it. 2 also
        // references 0, which the neighbourhood leaves out.
        let layout = neighbourhood(&graph(5, &[(1, 0), (1, 3), (2, 1), (2, 0)]), 1, false);
        let card = |table: u32| {
            (layout.cards.iter())
                .find(|card| card.subject == Subject::Table(table))
                .unwrap()
        };
        assert_eq!(layout.cards.len(), 4);
        assert_eq!(layout.edges.len(), 3);
        assert!(card(0).origin.x < card(1).origin.x && card(3).origin.x == card(0).origin.x);
        assert!(card(2).origin.x > card(1).origin.x);
        let focus_middle = card(1).origin.y + card(1).height / 2.0;
        let parents_middle = (card(0).origin.y + card(3).origin.y + card(3).height) / 2.0;
        assert!((focus_middle - parents_middle).abs() < 1e-3);
        assert!(
            card(0).origin.y < card(3).origin.y,
            "ordered by the focus's own rows"
        );
    }

    #[test]
    fn cardinality_marks_come_from_uniqueness_and_nullability() {
        let mut graph = graph(3, &[(1, 0), (2, 0)]);
        graph.tables[1].columns[1].unique = true;
        graph.tables[2].columns[1].nullable = true;
        let layout = schema(&graph, false);
        assert_eq!(layout.edges[0].child_mark, ChildMark::One);
        assert_eq!(layout.edges[0].parent_mark, ParentMark::ExactlyOne);
        assert_eq!(layout.edges[1].child_mark, ChildMark::Many);
        assert_eq!(layout.edges[1].parent_mark, ParentMark::ZeroOrOne);
    }

    #[test]
    fn inversions_count_strictly_out_of_order_pairs() {
        let mut values = [3.0, 1.0, 2.0, 2.0, 0.0];
        let mut scratch = values;
        assert_eq!(inversions(&mut values, &mut scratch), 7);
        assert_eq!(values, [0.0, 1.0, 2.0, 2.0, 3.0]);
    }
}
