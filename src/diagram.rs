//! The ER diagram's model side: where each card goes, what each level of
//! detail draws, and what lies under the pointer, all in world units. Nothing
//! here touches gpui, so the layout runs on a background thread and all of it
//! is tested without a window. The view paints what this computes.

pub mod geometry;
pub mod index;
pub mod layout;
pub mod mermaid;
pub mod png;
pub mod svg;

#[cfg(test)]
pub(crate) mod tests {
    use crate::db::{GraphColumn, GraphKey, GraphTable, KeyTarget, SchemaGraph};

    use super::geometry::{Point, Rect};
    use super::layout::{MAX_SWEEPS, schema};

    fn column(name: &str, primary: bool) -> GraphColumn {
        GraphColumn {
            name: name.to_string().into(),
            type_label: 0,
            nullable: false,
            primary,
            foreign: !primary,
            unique: primary,
        }
    }

    /// `tables` tables with an `id` primary key each, and a foreign key per
    /// `(child, parent)` pair, from a column of its own to the parent's `id`.
    pub(crate) fn graph(tables: usize, keys: &[(u32, u32)]) -> SchemaGraph {
        let mut graph = SchemaGraph {
            schema: "public".into(),
            tables: (0..tables)
                .map(|table| GraphTable {
                    name: format!("t{table}").into(),
                    columns: vec![column("id", true)],
                })
                .collect(),
            types: vec!["bigint".into()],
            keys: Vec::new(),
        };
        for (index, &(child, parent)) in keys.iter().enumerate() {
            let columns = &mut graph.tables[child as usize].columns;
            columns.push(column(&format!("t{parent}_id"), false));
            graph.keys.push(GraphKey {
                name: format!("fk_{index}").into(),
                table: child,
                columns: vec![columns.len() as u32 - 1],
                referenced: KeyTarget::Table {
                    table: parent,
                    columns: vec![0],
                },
            });
        }
        graph
    }

    /// A schema shaped like a large real one, the same on every run: mostly
    /// keys into earlier tables, some into later ones (cycles), self
    /// references, composite keys, keys into another schema, nullable and
    /// one-to-one keys, and every eleventh table with no key either way.
    pub(super) fn synthetic(tables: usize, keys: usize) -> SchemaGraph {
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move |bound: usize| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize % bound
        };
        let keyless = |table: usize| table % 11 == 5;
        let composite = |table: usize| table % 7 == 3;
        let mut generated = graph(tables, &[]);
        for table in 0..tables {
            let columns = &mut generated.tables[table].columns;
            if composite(table) {
                columns[0].unique = false;
                columns.push(GraphColumn {
                    unique: false,
                    ..column("part", true)
                });
            }
            columns.extend((0..3).map(|payload| GraphColumn {
                nullable: true,
                foreign: false,
                ..column(&format!("payload_{payload}"), false)
            }));
        }
        while generated.keys.len() < keys {
            let child = next(tables);
            let roll = next(100);
            let parent = match roll {
                0..2 => None,
                2..4 => Some(child),
                4..10 => Some(next(tables)),
                _ if child > 0 => Some(next(child)),
                _ => continue,
            };
            if keyless(child) || parent.is_some_and(keyless) {
                continue;
            }
            let nullable = next(4) == 0;
            let one_to_one = next(20) == 0;
            let index = generated.keys.len();
            let width = match parent {
                Some(parent) if composite(parent) => 2,
                _ => 1,
            };
            let columns = &mut generated.tables[child].columns;
            let first = columns.len() as u32;
            for part in 0..width {
                columns.push(GraphColumn {
                    nullable,
                    unique: one_to_one && width == 1,
                    ..column(&format!("fk_{index}_{part}"), false)
                });
            }
            generated.keys.push(GraphKey {
                name: format!("fk_{index}").into(),
                table: child as u32,
                columns: (first..first + width).collect(),
                referenced: match parent {
                    Some(parent) => KeyTarget::Table {
                        table: parent as u32,
                        columns: (0..width).collect(),
                    },
                    None => KeyTarget::Elsewhere {
                        schema: "elsewhere".into(),
                        table: format!("remote_{}", next(5)).into(),
                    },
                },
            });
        }
        generated
    }

    /// Whether the segment from `a` to `b` passes through the rectangle
    /// (Liang-Barsky clipping).
    fn crosses(a: Point, b: Point, rect: Rect) -> bool {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let (mut enter, mut leave) = (0.0f32, 1.0f32);
        for (p, q) in [
            (-dx, a.x - rect.x),
            (dx, rect.right() - a.x),
            (-dy, a.y - rect.y),
            (dy, rect.bottom() - a.y),
        ] {
            if p == 0.0 {
                if q < 0.0 {
                    return false;
                }
            } else if p < 0.0 {
                enter = enter.max(q / p);
            } else {
                leave = leave.min(q / p);
            }
        }
        enter <= leave
    }

    /// Cards are glass: an edge under one would show through it. So for every
    /// edge and every card it does not end at, at every level of detail, the
    /// flattened path stays out of the card.
    #[test]
    fn no_edge_passes_under_a_card_it_does_not_join() {
        let graph = synthetic(300, 600);
        for keys_only in [false, true] {
            let layout = schema(&graph, keys_only);
            assert!(
                layout.stats.dummies > 0,
                "the graph has long edges to route"
            );
            for zoom in [1.0, 0.7, 0.3] {
                let detail = layout.detail(zoom);
                let rects: Vec<Rect> = (0..layout.cards.len() as u32)
                    .map(|card| layout.card_rect(card, detail, zoom))
                    .collect();
                for (index, edge) in layout.edges.iter().enumerate() {
                    let path = layout.edge_path(index as u32, detail, zoom);
                    let bounds = path.bounds();
                    let points = path.polyline();
                    for (card, rect) in rects.iter().enumerate() {
                        let card = card as u32;
                        if card == edge.child || card == edge.parent || !rect.intersects(&bounds) {
                            continue;
                        }
                        let inside = rect.inflate(-0.5);
                        assert!(
                            !points
                                .windows(2)
                                .any(|pair| crosses(pair[0], pair[1], inside)),
                            "edge {index} passes under card {card} at {zoom} (keys only: {keys_only})"
                        );
                    }
                }
            }
        }
    }

    /// The work bound CI holds a large layout to: sweeps are capped, the lanes
    /// stay within a budget, and the result is the same on every run.
    #[test]
    fn a_thousand_table_layout_does_bounded_work_and_the_same_work_every_time() {
        let graph = synthetic(1000, 2000);
        let layout = schema(&graph, false);
        let stats = layout.stats;
        assert!(stats.sweeps <= MAX_SWEEPS, "{stats:?}");
        // Measured at 10,338 lanes for 2,000 keys over 35 layers.
        assert!(stats.dummies as usize <= 6 * graph.keys.len(), "{stats:?}");
        assert!(stats.crossings <= stats.crossings_before, "{stats:?}");
        assert_eq!(layout, schema(&graph, false));
        let keys_only = schema(&graph, true);
        assert_eq!(
            keys_only.stats.dummies, stats.dummies,
            "keys only keeps the layering"
        );
    }

    /// Layout time against its budget. A debug build of this crate is
    /// unoptimized, so this is compiled in release only:
    /// `cargo test --release -- --ignored a_thousand_table_layout_fits`.
    #[cfg(not(debug_assertions))]
    #[test]
    #[ignore = "a wall-clock budget, meaningful in a release build only"]
    fn a_thousand_table_layout_fits_its_time_budget() {
        use std::time::{Duration, Instant};

        use super::geometry::MIN_ZOOM;
        use super::index::Scene;

        let graph = synthetic(1000, 2000);
        let started = Instant::now();
        let layout = schema(&graph, false);
        let laid_out = started.elapsed();
        let scene = Scene::new(layout);
        let indexed = started.elapsed() - laid_out;
        let started = Instant::now();
        let bounds = scene.layout().bounds();
        let visible = scene.visible(bounds, MIN_ZOOM);
        let culled = started.elapsed();
        println!(
            "{} tables, {} keys: layout {laid_out:?}, index {indexed:?}, \
             cull everything {culled:?}; {:?}, {} cards and {} edges in view",
            graph.tables.len(),
            graph.keys.len(),
            scene.layout().stats,
            visible.cards.len(),
            visible.edges.len(),
        );
        assert!(laid_out < Duration::from_millis(50), "{laid_out:?}");
    }

    /// What a frame and a drag cost on the large graph, against the 8 ms frame
    /// budget (design §6.7). The painting itself needs a window and is not
    /// measured here: these are the model's share of it.
    #[cfg(not(debug_assertions))]
    #[test]
    #[ignore = "a wall-clock budget, meaningful in a release build only"]
    fn a_thousand_table_frame_and_drag_fit_their_time_budgets() {
        use std::time::{Duration, Instant};

        use super::geometry::{Point, Rect};
        use super::index::Scene;

        let graph = synthetic(1000, 2000);
        let mut scene = Scene::new(schema(&graph, false));
        let world = scene.layout().bounds();
        let tries = 200u32;
        let at = |step: u32, width: f32, height: f32| Rect {
            x: world.x + (world.width - width).max(0.0) * (step % 20) as f32 / 19.0,
            y: world.y + (world.height - height).max(0.0) * (step / 20 % 10) as f32 / 9.0,
            width,
            height,
        };

        for zoom in [1.0f32, 0.7, 0.3] {
            let (width, height) = (1600.0 / zoom, 1000.0 / zoom);
            let (mut cull, mut paths, mut hits) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
            let (mut cards, mut edges) = (0, 0);
            let detail = scene.layout().detail(zoom);
            for step in 0..tries {
                let view = at(step, width, height);
                let started = Instant::now();
                let visible = scene.visible(view, zoom);
                cull += started.elapsed();
                let started = Instant::now();
                for &edge in &visible.edges {
                    std::hint::black_box(scene.layout().edge_path(edge, detail, zoom));
                }
                paths += started.elapsed();
                let started = Instant::now();
                std::hint::black_box(scene.card_at(
                    Point::new(view.x + width / 2.0, view.y + height / 2.0),
                    zoom,
                ));
                hits += started.elapsed();
                cards += visible.cards.len();
                edges += visible.edges.len();
            }
            let (cull, paths, hits) = (cull / tries, paths / tries, hits / tries);
            println!(
                "zoom {zoom}: a 1600x1000 viewport culls in {cull:?} to {} cards and {} edges \
                 on average; their edge paths take {paths:?}; a hit test {hits:?}",
                cards / tries as usize,
                edges / tries as usize,
            );
            assert!(cull < Duration::from_millis(1), "{cull:?}");
            assert!(paths < Duration::from_millis(4), "{paths:?}");
            assert!(hits < Duration::from_micros(500), "{hits:?}");
        }

        let count = scene.layout().cards.len() as u32;
        let (mut moved, mut resized) = (Duration::ZERO, Duration::ZERO);
        for step in 0..tries {
            let card = step * 5 % count;
            let origin = scene.layout().cards[card as usize].origin;
            let started = Instant::now();
            scene.move_card(card, Point::new(origin.x + 40.0, origin.y - 25.0));
            moved += started.elapsed();
            let started = Instant::now();
            scene.resize_card(card, Some(300.0), None);
            resized += started.elapsed();
        }
        let (moved, resized) = (moved / tries, resized / tries);
        println!("a card moves in {moved:?} and is resized in {resized:?}, edges refiled");
        assert!(moved < Duration::from_millis(1), "{moved:?}");
        assert!(resized < Duration::from_millis(1), "{resized:?}");
    }
}
