//! What a parsed plan means: one derived model per plan, read by everything that
//! shows or serializes it.
//!
//! Two invariants hold throughout. An [`Analysis`] is derived once, where the
//! plan lands or is restored, and then only read -- the plan view draws from it
//! and the compact form prints from the same field, so the two cannot disagree
//! about which node is hot. And nothing in it is NaN or infinite: a total that
//! cannot be divided by is `None` and every share under it is `None`, because a
//! snapshot compares whole `Explained` values and `NaN != NaN`.
//!
//! Every figure is scoped to the plan it was built from. Nothing here is shared
//! between plans, so no share is ever a fraction of another plan's total.

use super::{Plan, PlanNode, gathers};
use crate::db::Engine;

/// A plan's reading: what its numbers are, how heavy each node is, and what is
/// worth looking at first.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Analysis {
    pub(crate) engine: Engine,
    pub(crate) basis: Basis,
    /// The engine and what its numbers are, as the header prints it:
    /// `Postgres · measured (ms)`.
    pub(crate) label: String,
    /// What every share is a share of, on both bases. `None` when there is
    /// nothing to divide by: a plan without numbers, or one that sums to zero.
    pub(crate) total: Option<f64>,
    /// The `Execution Time` the server reported for the whole statement. Kept
    /// apart from `total`, which is the share denominator.
    pub(crate) execution_ms: Option<f64>,
    /// Measured time that belongs to no node: JIT, or the executor's setup and
    /// teardown. Present only when the reported execution time exceeds the
    /// nodes' total by more than 1 % of it.
    pub(crate) outside_nodes_ms: Option<f64>,
    /// One per plan node, in the plan's order.
    pub(crate) nodes: Vec<NodeAnalysis>,
    /// What to look at first: Hot and Danger nodes by share, then misestimated
    /// nodes by factor, ties in the plan's order.
    pub(crate) findings: Vec<Finding>,
}

/// Where a plan's numbers come from, decided by what the plan carries and not
/// by what was asked of the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Basis {
    /// At least one node reports how long it took.
    Measured,
    /// No timings, but nodes carry the planner's cost.
    PlannerCost,
    /// A shape and nothing to weigh it by.
    Structure,
}

/// One node's weight in its plan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeAnalysis {
    /// Milliseconds of its own on a measured plan, its own cost on a cost plan.
    pub(crate) value: Option<f64>,
    /// `value` over the plan's total, in `0.0..=1.0`.
    pub(crate) share: Option<f64>,
    pub(crate) tier: Option<Tier>,
    /// How far the planner's row count was from what happened, when it was off
    /// by ten times or more and the node did not simply stop early.
    pub(crate) misestimate: Option<Misestimate>,
    /// Set on a `Gather` and every node beneath it.
    pub(crate) parallel: Option<Parallel>,
}

/// A row estimate that missed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Misestimate {
    /// How many times over or under, at least 10.
    pub(crate) factor: f64,
    pub(crate) direction: Direction,
    pub(crate) tier: Tier,
}

/// Which way the planner was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// More rows than it expected.
    Under,
    /// Fewer rows than it expected.
    Over,
}

/// A node run by parallel workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Parallel {
    pub(crate) workers: u32,
    /// The `Gather` itself, as opposed to a node running in its workers.
    pub(crate) gathers: bool,
}

/// A node worth reading first, by index into the plan's nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Finding {
    Hot(usize),
    Misestimated(usize),
}

/// How loudly a figure is drawn. The same four names serve a node's share of
/// its plan and a misestimate's order of magnitude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    Quiet,
    Warm,
    Hot,
    Danger,
}

impl Tier {
    /// Quiet below 10 %, Warm to below 50 %, Hot through 90 % inclusive, Danger
    /// above. A lower bound belongs to the tier it starts, and 90 % to the one
    /// below it, so that a figure printed to one decimal and a colour drawn from
    /// the share fall on the same side.
    pub(crate) fn of_share(share: f64) -> Self {
        if share < 0.10 {
            Self::Quiet
        } else if share < 0.50 {
            Self::Warm
        } else if share <= 0.90 {
            Self::Hot
        } else {
            Self::Danger
        }
    }

    /// The tier of a misestimate's factor: none below 10x, Warm to below 100x,
    /// Hot through exactly 1000x, Danger only above.
    pub(crate) fn of_misestimate(factor: f64) -> Option<Self> {
        if factor < 10.0 {
            None
        } else if factor < 100.0 {
            Some(Self::Warm)
        } else if factor <= 1000.0 {
            Some(Self::Hot)
        } else {
            Some(Self::Danger)
        }
    }
}

impl Analysis {
    /// Read `plan`, as an `engine` printed it.
    pub(crate) fn new(plan: &Plan, engine: Engine) -> Self {
        let basis = if plan.nodes.iter().any(|node| node.actual.is_some()) {
            Basis::Measured
        } else if plan.nodes.iter().any(|node| node.estimated.is_some()) {
            Basis::PlannerCost
        } else {
            Basis::Structure
        };

        // A figure that is not finite is a figure the server's text got wrong,
        // and is left out rather than carried into a division.
        let values: Vec<Option<f64>> = match basis {
            Basis::Measured => plan.nodes.iter().map(|node| node.self_ms).collect(),
            Basis::PlannerCost => own_costs(&plan.nodes),
            Basis::Structure => vec![None; plan.nodes.len()],
        }
        .into_iter()
        .map(|value| value.filter(|value| value.is_finite()))
        .collect();
        let total = match basis {
            // A measured plan's clock time is its top nodes' inclusive times,
            // which `fill_self_ms` makes the sum of every node's own time.
            Basis::Measured => Some(
                plan.nodes
                    .iter()
                    .filter(|node| node.depth == 0)
                    .filter_map(|node| Some(node.actual?.inclusive_ms()))
                    .sum::<f64>(),
            ),
            // The sum of every node's own cost, so a cost plan's shares add to
            // 100 %. Without a `Limit` that is the top nodes' own estimate; with
            // one it is more, because Postgres prices a `Limit` as a fraction of
            // its child and that node's own cost floors at zero.
            Basis::PlannerCost => Some(values.iter().flatten().sum::<f64>()),
            Basis::Structure => None,
        }
        .filter(|total| total.is_finite() && *total > 0.0);

        let parallel = parallel_nodes(&plan.nodes);
        let nodes: Vec<NodeAnalysis> = values
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                // Bounded so malformed server text cannot draw past the track.
                let share = value
                    .zip(total)
                    .map(|(value, total)| (value / total).clamp(0.0, 1.0));
                NodeAnalysis {
                    value,
                    share,
                    tier: share.map(Tier::of_share),
                    misestimate: misestimate(&plan.nodes, index),
                    parallel: parallel[index],
                }
            })
            .collect();

        let execution_ms = plan
            .summary
            .iter()
            .find(|(label, _)| label.eq_ignore_ascii_case("Execution Time"))
            .and_then(|(_, value)| value.split_whitespace().next()?.parse::<f64>().ok())
            .filter(|ms| ms.is_finite());
        let outside_nodes_ms = execution_ms
            .zip(total)
            .filter(|_| basis == Basis::Measured)
            .filter(|(execution, total)| execution - total > total * 0.01)
            .map(|(execution, total)| execution - total);

        Self {
            engine,
            basis,
            label: label(engine, basis),
            total,
            execution_ms,
            outside_nodes_ms,
            findings: findings(&nodes),
            nodes,
        }
    }
}

/// Hot and Danger nodes by share, then misestimated nodes by factor, each
/// stable so that ties stay in the plan's order.
fn findings(nodes: &[NodeAnalysis]) -> Vec<Finding> {
    let mut hot: Vec<(usize, f64)> = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| matches!(node.tier, Some(Tier::Hot | Tier::Danger)))
        .filter_map(|(index, node)| Some((index, node.share?)))
        .collect();
    hot.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut missed: Vec<(usize, f64)> = nodes
        .iter()
        .enumerate()
        .filter_map(|(index, node)| Some((index, node.misestimate?.factor)))
        .collect();
    missed.sort_by(|a, b| b.1.total_cmp(&a.1));
    hot.into_iter()
        .map(|(index, _)| Finding::Hot(index))
        .chain(
            missed
                .into_iter()
                .map(|(index, _)| Finding::Misestimated(index)),
        )
        .collect()
}

/// How far a node's row count was from the planner's, unless it only ran short
/// because an ancestor stopped pulling rows. Estimates are for running to
/// completion, so a scan under a `Limit`, or on a semi or anti join's inner
/// side, reads as a huge overestimate on a healthy plan. Only a node that
/// reaches the `Limit` or the join through operators that pass rows along as
/// they get them counts: a `Sort` or an aggregate beneath it reads all of its
/// input first, so what is under one is read to the end.
///
/// Postgres reports both counts per loop, so they compare as they stand; zero
/// is read as one so neither side divides by it.
fn misestimate(nodes: &[PlanNode], index: usize) -> Option<Misestimate> {
    let node = &nodes[index];
    let (estimated, actual) = (node.estimated?.rows, node.actual?.rows);
    // A count that is NaN or too large for a float to hold is text the server
    // got wrong, not a figure to draw a chip from.
    if !estimated.is_finite() || !actual.is_finite() {
        return None;
    }
    let (estimated, actual) = (estimated.max(1.0), actual.max(1.0));
    let factor = (actual / estimated).max(estimated / actual);
    let tier = Tier::of_misestimate(factor)?;
    let direction = if actual > estimated {
        Direction::Under
    } else {
        Direction::Over
    };

    let stopped = stopped_early(nodes, index);
    (!(direction == Direction::Over && stopped)).then_some(Misestimate {
        factor,
        direction,
        tier,
    })
}

/// Whether an ancestor stops pulling rows from this node before it is
/// exhausted: a `Limit`, or a semi or anti join reached by its inner side (any
/// child but the first). The walk up ends at the first operator that reads all
/// of its input before it returns a row, since nothing above it can cut the
/// node short.
fn stopped_early(nodes: &[PlanNode], index: usize) -> bool {
    let mut depth = nodes[index].depth;
    // The child of the next ancestor found that the walk came up through.
    let mut branch = index;
    for (above, ancestor) in nodes[..index].iter().enumerate().rev() {
        if ancestor.depth >= depth {
            continue;
        }
        let label = ancestor.label.to_lowercase();
        if reads_all_input(&label) {
            return false;
        }
        let inner = branch != above + 1;
        if label.starts_with("limit")
            || (inner && (label.contains("semi join") || label.contains("anti join")))
        {
            return true;
        }
        depth = ancestor.depth;
        branch = above;
    }
    false
}

/// Whether an operator, by its lowercased label, consumes every row beneath it
/// before it produces one. A `Hash` is the build side of a hash join; a group
/// aggregate streams its groups and is not listed.
///
/// ponytail: labels are matched by the engine's own words for these operators;
/// an operator not named here is read as passing rows along, which can only
/// hide a misestimate that a `Limit` above would have explained anyway.
fn reads_all_input(label: &str) -> bool {
    label == "hash"
        || label.starts_with("sort")
        || label.starts_with("materialize")
        || label.starts_with("temporary table")
        || (label.contains("aggregate") && !label.starts_with("group"))
}

/// A `Gather` and everything beneath it ran in workers. The count is what the
/// `Gather` says it launched, and failing that the participants its child ran
/// in, less the leader.
fn parallel_nodes(nodes: &[PlanNode]) -> Vec<Option<Parallel>> {
    // The depth of each `Gather` on the path down to the node, and its workers.
    let mut open: Vec<(usize, u32)> = Vec::new();
    let mut marks = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        while open.last().is_some_and(|(depth, _)| *depth >= node.depth) {
            open.pop();
        }
        let inside = open.last().map(|(_, workers)| Parallel {
            workers: *workers,
            gathers: false,
        });
        marks.push(
            match gathers(node).then(|| workers_of(nodes, index)).flatten() {
                Some(workers) => {
                    open.push((node.depth, workers));
                    Some(Parallel {
                        workers,
                        gathers: true,
                    })
                }
                None => inside,
            },
        );
    }
    marks
}

fn workers_of(nodes: &[PlanNode], gather: usize) -> Option<u32> {
    let node = &nodes[gather];
    let launched = node.detail.iter().find_map(|line| {
        line.strip_prefix("Workers Launched:")?
            .trim()
            .parse::<u32>()
            .ok()
    });
    launched.or_else(|| {
        let loops = nodes[gather + 1..]
            .iter()
            .take_while(|below| below.depth > node.depth)
            .find(|below| below.depth == node.depth + 1)?
            .actual?
            .loops;
        // The leader is one of the participants, and is not a worker.
        (loops.is_finite() && loops >= 1.0).then_some((loops - 1.0) as u32)
    })
}

/// Each node's own planner cost: its `total_cost` less its direct children's,
/// never below zero. A child with no estimate costs its parent nothing here.
fn own_costs(nodes: &[PlanNode]) -> Vec<Option<f64>> {
    // The node each one hangs from is the nearest one above it that is
    // shallower.
    let mut path: Vec<usize> = Vec::new();
    let mut children = vec![0.0; nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        while path
            .last()
            .is_some_and(|last| nodes[*last].depth >= node.depth)
        {
            path.pop();
        }
        if let (Some(parent), Some(estimated)) = (path.last(), node.estimated) {
            children[*parent] += estimated.total_cost;
        }
        path.push(index);
    }
    nodes
        .iter()
        .zip(children)
        .map(|(node, children)| Some((node.estimated?.total_cost - children).max(0.0)))
        .collect()
}

/// A share as the percentage the header and the compact form print, so that the
/// figure and the tier drawn from the same share fall on the same side of every
/// tier boundary.
///
/// Floored to a tenth of a percent, not rounded: a share just under a boundary
/// must not print as the boundary. The one band flooring cannot fix is just
/// above 90 %, which is Danger and would print `90.0%`, a figure that is Hot, so
/// there the next tenth is printed. Works in whole tenths so that a `{:.1}`
/// format never rounds a floored value again.
pub(crate) fn percent_text(share: f64) -> String {
    let mut tenths = (share.clamp(0.0, 1.0) * 1000.0).floor() as u32;
    if tenths == 900 && Tier::of_share(share) == Tier::Danger {
        tenths = 901;
    }
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// The engine and what its numbers are: `Postgres · measured (ms)`.
fn label(engine: Engine, basis: Basis) -> String {
    let words = engine.plan_basis();
    let name = engine.label();
    match basis {
        Basis::Measured => format!("{name} · {}", words.measured.unwrap_or("measured")),
        Basis::PlannerCost => format!(
            "{name} · {} · no measurements",
            words.cost.unwrap_or("planner cost")
        ),
        Basis::Structure => format!("{name} · structure only"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::{Row, rows},
        explain,
    };

    fn one_column(text: &str) -> Plan {
        explain::parse(
            &["QUERY PLAN".to_string()],
            &rows(vec![vec![Some(text.to_string())]]),
        )
    }

    /// One row per line, as ClickHouse answers.
    fn line_per_row(column: &str, text: &str) -> Plan {
        explain::parse(
            &[column.to_string()],
            &rows(
                text.lines()
                    .map(|line| vec![Some(line.to_string())])
                    .collect(),
            ),
        )
    }

    /// What the driver hands back for a MongoDB `explain`.
    fn explain_reply(json: &str) -> Plan {
        let reply: mongodb::bson::Document = serde_json::from_str(json).unwrap();
        let columns: Vec<String> = reply.keys().cloned().collect();
        let row = Row::new(reply.values().map(|value| match value {
            mongodb::bson::Bson::String(text) => Some(text.clone()),
            other => serde_json::to_string(other).ok(),
        }))
        .unwrap();
        explain::parse(&columns, &[row])
    }

    fn tabular(columns: &[&str], cells: &[&str]) -> Plan {
        explain::parse(
            &columns
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>(),
            &rows(vec![
                cells.iter().map(|cell| Some(cell.to_string())).collect(),
            ]),
        )
    }

    const HASH_JOIN: &str = "Hash Join  (cost=0.00..100.00 rows=10 width=8)
  ->  Seq Scan on a  (cost=0.00..30.00 rows=10 width=8)
  ->  Seq Scan on b  (cost=0.00..50.00 rows=10 width=8)";

    /// No number anywhere in the analysis may be NaN or infinite.
    #[track_caller]
    fn assert_finite(analysis: &Analysis) {
        let figures = analysis.total.into_iter().chain(
            analysis
                .nodes
                .iter()
                .flat_map(|node| [node.value, node.share].into_iter().flatten()),
        );
        for figure in figures {
            assert!(figure.is_finite(), "{analysis:?}");
        }
    }

    #[test]
    fn a_measured_plan_gives_each_node_its_share_of_the_root() {
        let plan = one_column(include_str!("../testdata/plan-postgres-parallel.txt"));
        let analysis = Analysis::new(&plan, Engine::Postgres);

        assert_eq!(analysis.basis, Basis::Measured);
        assert_eq!(analysis.label, "Postgres · measured (ms)");
        assert_eq!(analysis.nodes.len(), plan.nodes.len());
        let shares: Vec<f64> = analysis
            .nodes
            .iter()
            .map(|node| node.share.expect("a measured node has a share"))
            .collect();
        assert!(shares.iter().all(|share| (0.0..=1.0).contains(share)));
        let sum: f64 = shares.iter().sum();
        assert!((sum - 1.0).abs() < 0.01, "shares add up to {sum}");
    }

    #[test]
    fn share_tiers_change_exactly_at_ten_fifty_and_ninety_percent() {
        for (share, tier) in [
            (0.0, Tier::Quiet),
            (0.0999, Tier::Quiet),
            (0.10, Tier::Warm),
            (0.4999, Tier::Warm),
            (0.50, Tier::Hot),
            (0.90, Tier::Hot),
            (0.9001, Tier::Danger),
            (1.0, Tier::Danger),
        ] {
            assert_eq!(Tier::of_share(share), tier, "{share}");
        }
        // Equal shares are equally loud.
        assert_eq!(Tier::of_share(0.3), Tier::of_share(0.3));
    }

    #[test]
    fn a_plan_with_no_timings_falls_back_to_each_nodes_own_planner_cost() {
        let values = |analysis: &Analysis| -> Vec<f64> {
            analysis
                .nodes
                .iter()
                .map(|node| node.value.unwrap())
                .collect()
        };
        let shares = |analysis: &Analysis| -> Vec<f64> {
            analysis
                .nodes
                .iter()
                .map(|node| node.share.unwrap())
                .collect()
        };

        let analysis = Analysis::new(&one_column(HASH_JOIN), Engine::Postgres);
        assert_eq!(analysis.basis, Basis::PlannerCost);
        assert_eq!(values(&analysis), [20.0, 30.0, 50.0]);
        assert_eq!(shares(&analysis), [0.2, 0.3, 0.5]);
        assert_eq!(analysis.total, Some(100.0));

        // MySQL's `FORMAT=TREE` prints one cost per node.
        let mysql = Analysis::new(
            &one_column(
                "-> Nested loop inner join  (cost=3.10 rows=6)
    -> Table scan on a  (cost=0.85 rows=6)
    -> Single-row index lookup on b using PRIMARY (id=a.id)  (cost=0.38 rows=1)",
            ),
            Engine::MySql,
        );
        assert_eq!(mysql.basis, Basis::PlannerCost);
        let sum: f64 = shares(&mysql).iter().sum();
        assert!((sum - 1.0).abs() < 1e-9, "{sum}");
        assert!((mysql.nodes[0].value.unwrap() - (3.10 - 0.85 - 0.38)).abs() < 1e-9);

        // Postgres prices a `Limit` as a fraction of its child, so its own cost
        // (0.35 less 100.00) floors at zero and the shares are still of 100.
        let limited = one_column(&format!(
            "Limit  (cost=0.00..0.35 rows=1 width=8)\n  ->  {}",
            HASH_JOIN.replace('\n', "\n    ")
        ));
        let analysis = Analysis::new(&limited, Engine::Postgres);
        assert_eq!(values(&analysis), [0.0, 20.0, 30.0, 50.0]);
        assert_eq!(analysis.total, Some(100.0));
        let sum: f64 = shares(&analysis).iter().sum();
        assert!((sum - 1.0).abs() < 0.01, "{sum}");
        assert!(shares(&analysis).iter().all(|share| *share < 1.0));
        let tiers: Vec<Tier> = analysis
            .nodes
            .iter()
            .map(|node| node.tier.unwrap())
            .collect();
        assert_eq!(tiers, [Tier::Quiet, Tier::Warm, Tier::Warm, Tier::Hot]);
        assert_eq!(analysis.nodes[0].share, Some(0.0));
    }

    #[test]
    fn a_plan_with_neither_timings_nor_costs_draws_no_bars() {
        let sqlite = explain::parse(
            &["id", "parent", "notused", "detail"].map(str::to_string),
            &rows(vec![
                ["2", "0", "0", "SCAN accounts"]
                    .map(|cell| Some(cell.to_string()))
                    .to_vec(),
                ["6", "2", "0", "SEARCH orders USING INDEX orders_account"]
                    .map(|cell| Some(cell.to_string()))
                    .to_vec(),
            ]),
        );
        let mysql = tabular(
            &[
                "id",
                "select_type",
                "table",
                "partitions",
                "type",
                "possible_keys",
                "key",
                "key_len",
                "ref",
                "rows",
                "filtered",
                "Extra",
            ],
            &[
                "1",
                "SIMPLE",
                "accounts",
                "NULL",
                "ALL",
                "NULL",
                "NULL",
                "NULL",
                "NULL",
                "12",
                "100.00",
                "Using where",
            ],
        );
        let mariadb = tabular(
            &[
                "id",
                "select_type",
                "table",
                "type",
                "possible_keys",
                "key",
                "key_len",
                "ref",
                "rows",
                "r_rows",
                "filtered",
                "r_filtered",
                "Extra",
            ],
            &[
                "1",
                "SIMPLE",
                "accounts",
                "ALL",
                "NULL",
                "NULL",
                "NULL",
                "NULL",
                "12",
                "12.00",
                "100.00",
                "100.00",
                "Using where",
            ],
        );
        let clickhouse = line_per_row(
            "explain",
            "Expression ((Project names + Projection))\n  Limit (preliminary LIMIT)\n│  Limit 3\n│  Offset 0\n└──ReadFromSystemNumbers",
        );
        let mongodb = explain_reply(
            r#"{"explainVersion":"1","queryPlanner":{"winningPlan":{"stage":"LIMIT","limitAmount":2,"inputStage":{"stage":"FETCH","inputStage":{"stage":"IXSCAN","indexName":"_id_"}}}},"ok":1}"#,
        );

        for (plan, engine) in [
            (sqlite, Engine::Sqlite),
            (mysql, Engine::MySql),
            (mariadb, Engine::MariaDb),
            (clickhouse, Engine::ClickHouse),
            (mongodb, Engine::MongoDb),
        ] {
            let analysis = Analysis::new(&plan, engine);
            assert_eq!(analysis.basis, Basis::Structure, "{}", engine.label());
            assert_eq!(analysis.total, None, "{}", engine.label());
            assert!(
                analysis
                    .nodes
                    .iter()
                    .all(|node| node.share.is_none() && node.value.is_none()),
                "{}",
                engine.label()
            );
        }
    }

    #[test]
    fn the_header_names_the_engine_and_what_its_numbers_are() {
        let label = |plan: &Plan, engine| Analysis::new(plan, engine).label;

        let measured = one_column(include_str!("../testdata/plan-postgres-parallel.txt"));
        assert_eq!(
            label(&measured, Engine::Postgres),
            "Postgres · measured (ms)"
        );
        assert_eq!(
            label(&one_column(HASH_JOIN), Engine::Postgres),
            "Postgres · planner cost, arbitrary units · no measurements"
        );

        let tree = "-> Limit: 10 row(s)  (cost=1.25 rows=10) (actual time=0.021..0.030 rows=10 loops=1)\n    -> Table scan on accounts  (cost=2.50 rows=12) (actual time=0.019..0.025 rows=12 loops=1)";
        assert_eq!(
            label(&one_column(tree), Engine::MySql),
            "MySQL · measured (ms)"
        );
        let costs = "-> Limit: 10 row(s)  (cost=1.25 rows=10)\n    -> Table scan on accounts  (cost=2.50 rows=12)";
        assert_eq!(
            label(&one_column(costs), Engine::MySql),
            "MySQL · planner cost, single figure · no measurements"
        );
        let tabular_row = tabular(
            &["id", "select_type", "table"],
            &["1", "SIMPLE", "accounts"],
        );
        assert_eq!(label(&tabular_row, Engine::MySql), "MySQL · structure only");
        assert_eq!(
            label(&tabular_row, Engine::MariaDb),
            "MariaDB · structure only"
        );
        assert_eq!(
            label(&tabular_row, Engine::Sqlite),
            "SQLite · structure only"
        );
        assert_eq!(
            label(&tabular_row, Engine::ClickHouse),
            "ClickHouse · structure only"
        );

        let stats = explain_reply(
            r#"{"queryPlanner":{"winningPlan":{"stage":"LIMIT","inputStage":{"stage":"IXSCAN","indexName":"_id_"}}},"executionStats":{"nReturned":3,"executionStages":{"stage":"LIMIT","nReturned":3,"executionTimeMillisEstimate":4,"inputStage":{"stage":"IXSCAN","nReturned":3,"executionTimeMillisEstimate":1,"indexName":"_id_"}}},"ok":1}"#,
        );
        assert_eq!(
            label(&stats, Engine::MongoDb),
            "MongoDB · integer-millisecond estimates"
        );
        let planner =
            explain_reply(r#"{"queryPlanner":{"winningPlan":{"stage":"COLLSCAN"}},"ok":1}"#);
        assert_eq!(label(&planner, Engine::MongoDb), "MongoDB · structure only");

        // A server that said nothing a tree could be read from still gets a
        // header that names the engine.
        assert_eq!(
            label(&Plan::default(), Engine::Postgres),
            "Postgres · structure only"
        );
    }

    #[test]
    fn a_zero_or_empty_plan_has_no_share_and_no_nan() {
        for text in [
            "",
            "Seq Scan on a  (cost=0.00..0.00 rows=1 width=1)",
            "Seq Scan on a  (actual time=0.000..0.000 rows=1 loops=1)",
            "Seq Scan on a  (cost=0.00..1e999 rows=1 width=1)",
            "Seq Scan on a  (cost=NaN..NaN rows=NaN width=1)",
            "Seq Scan on a  (actual time=NaN..NaN rows=1 loops=1)",
            "Seq Scan on a  (cost=0.00..1.00 rows=1e999 width=1) (actual time=0.000..0.000 rows=1e999 loops=1)",
        ] {
            let analysis = Analysis::new(&one_column(text), Engine::Postgres);
            assert_eq!(analysis.total, None, "{text}");
            assert!(
                analysis.nodes.iter().all(|node| node.share.is_none()),
                "{text}"
            );
            assert_finite(&analysis);
        }
        let empty = Analysis::new(&Plan::default(), Engine::Postgres);
        assert!(empty.nodes.is_empty());

        let single = Analysis::new(
            &one_column("Seq Scan on a  (actual time=0.000..2.000 rows=1 loops=1)"),
            Engine::Postgres,
        );
        assert_eq!(single.nodes[0].share, Some(1.0));
        assert_eq!(single.nodes[0].tier, Some(Tier::Danger));
        assert_finite(&single);
    }

    #[test]
    fn a_printed_share_never_crosses_its_tier_boundary() {
        for (share, text) in [
            (0.0996, "9.9%"),
            (0.1, "10.0%"),
            (0.5, "50.0%"),
            (0.9, "90.0%"),
            (0.9001, "90.1%"),
            (0.9009, "90.1%"),
            (f64::from_bits(0.9f64.to_bits() + 1), "90.1%"),
            (0.901, "90.1%"),
            (1.0, "100.0%"),
        ] {
            assert_eq!(percent_text(share), text, "{share}");
        }
        assert_eq!(Tier::of_share(0.9), Tier::Hot);
        assert_eq!(Tier::of_share(0.9001), Tier::Danger);

        for step in 0..=10_000 {
            let share = step as f64 / 10_000.0;
            let printed: f64 = percent_text(share).trim_end_matches('%').parse().unwrap();
            assert_eq!(
                Tier::of_share(printed / 100.0),
                Tier::of_share(share),
                "{share} prints as {}",
                percent_text(share)
            );
        }
    }

    #[test]
    fn no_state_is_shared_between_plans() {
        // The same plan read twice, around a very different one, reads the same.
        let first = Analysis::new(&one_column(HASH_JOIN), Engine::Postgres);
        let big = Analysis::new(
            &one_column("Seq Scan on a  (cost=0.00..1000000.00 rows=1 width=1)"),
            Engine::Postgres,
        );
        let second = Analysis::new(&one_column(HASH_JOIN), Engine::Postgres);
        assert_eq!(first, second);
        assert_eq!(big.nodes[0].share, Some(1.0));
    }

    /// A node that estimated `estimated` rows and returned `actual`.
    fn node(label: &str, depth: usize, estimated: f64, actual: f64) -> PlanNode {
        PlanNode {
            label: label.into(),
            depth,
            estimated: Some(explain::Estimated {
                startup_cost: 0.0,
                total_cost: 1.0,
                rows: estimated,
                width: 0,
            }),
            actual: Some(explain::Actual {
                startup_ms: 0.0,
                total_ms: 1.0,
                rows: actual,
                loops: 1.0,
            }),
            ..PlanNode::default()
        }
    }

    fn missed(estimated: f64, actual: f64) -> Option<(f64, Direction, Tier)> {
        let plan = Plan {
            nodes: vec![node("Seq Scan on t", 0, estimated, actual)],
            ..Plan::default()
        };
        let analysis = Analysis::new(&plan, Engine::Postgres);
        analysis.nodes[0]
            .misestimate
            .map(|miss| (miss.factor, miss.direction, miss.tier))
    }

    #[test]
    fn a_misestimate_names_its_direction_and_order_of_magnitude() {
        use Direction::{Over, Under};
        assert_eq!(missed(10.0, 1000.0), Some((100.0, Under, Tier::Hot)));
        assert_eq!(missed(5000.0, 50.0), Some((100.0, Over, Tier::Hot)));
        // Zero is read as one, so nothing divides by it.
        assert_eq!(missed(0.0, 20.0), Some((20.0, Under, Tier::Warm)));
        assert_eq!(missed(1.0, 0.0), None);
        assert_eq!(missed(100.0, 1000.0), Some((10.0, Under, Tier::Warm)));
        assert_eq!(missed(100.0, 999.0), None);
        let (factor, direction, tier) = missed(10.0, 999.0).unwrap();
        assert!((factor - 99.9).abs() < 1e-9);
        assert_eq!((direction, tier), (Under, Tier::Warm));
        // Exactly 1000x is the top of Hot; only more than that is Danger.
        assert_eq!(missed(1.0, 1000.0), Some((1000.0, Under, Tier::Hot)));
        assert_eq!(missed(1.0, 1000.5), Some((1000.5, Under, Tier::Danger)));
        assert_eq!(missed(1000.0, 1.0), Some((1000.0, Over, Tier::Hot)));
        // Counts too large for a float to hold are no figure at all.
        assert_eq!(missed(1.0, f64::INFINITY), None);
        assert_eq!(missed(f64::INFINITY, f64::INFINITY), None);
        assert_eq!(missed(f64::NAN, 50.0), None);
        assert_eq!(missed(50.0, f64::NAN), None);

        for (factor, tier) in [
            (9.99, None),
            (10.0, Some(Tier::Warm)),
            (99.99, Some(Tier::Warm)),
            (100.0, Some(Tier::Hot)),
            (1000.0, Some(Tier::Hot)),
            (1000.01, Some(Tier::Danger)),
        ] {
            assert_eq!(Tier::of_misestimate(factor), tier, "{factor}");
        }
        assert_eq!(
            Analysis::new(&Plan::default(), Engine::Postgres).findings,
            []
        );
    }

    #[test]
    fn a_short_scan_under_a_limit_or_semi_join_is_not_a_misestimate() {
        let miss = |nodes: Vec<PlanNode>, index: usize| {
            let plan = Plan {
                nodes,
                ..Plan::default()
            };
            Analysis::new(&plan, Engine::Postgres).nodes[index]
                .misestimate
                .map(|miss| miss.factor)
        };
        let limit = vec![
            node("Limit", 0, 10.0, 10.0),
            node("Seq Scan on big", 1, 1_000_000.0, 10.0),
        ];
        assert_eq!(miss(limit, 1), None);
        let mysql = vec![
            node("Limit: 10 row(s)", 0, 10.0, 10.0),
            node("Table scan on big", 1, 1_000_000.0, 10.0),
        ];
        assert_eq!(miss(mysql, 1), None);
        let semi = vec![
            node("Nested Loop Semi Join", 0, 5.0, 5.0),
            node("Seq Scan on a", 1, 5.0, 5.0),
            node("Hash Anti Join", 1, 5.0, 5.0),
            node("Index Scan on b", 2, 100_000.0, 1.0),
        ];
        assert_eq!(miss(semi, 3), None);
        // Stopping early explains too few rows, never too many.
        let overshoot = vec![
            node("Limit", 0, 10.0, 10.0),
            node("Seq Scan on big", 1, 10.0, 5000.0),
        ];
        assert_eq!(miss(overshoot, 1), Some(500.0));
        // A `Limit` beside the node, not above it, explains nothing.
        let sibling = vec![
            node("Append", 0, 1.0, 1.0),
            node("Limit", 1, 10.0, 10.0),
            node("Seq Scan on a", 2, 10.0, 10.0),
            node("Seq Scan on b", 1, 1_000_000.0, 10.0),
        ];
        assert_eq!(miss(sibling, 3), Some(100_000.0));
        let plain = vec![
            node("Sort", 0, 10.0, 10.0),
            node("Seq Scan on big", 1, 1_000_000.0, 10.0),
        ];
        assert_eq!(miss(plain, 1), Some(100_000.0));
    }

    #[test]
    fn a_scan_a_sort_or_aggregate_reads_to_the_end_is_a_misestimate_under_a_limit() {
        let miss = |nodes: Vec<PlanNode>, index: usize| {
            let plan = Plan {
                nodes,
                ..Plan::default()
            };
            Analysis::new(&plan, Engine::Postgres).nodes[index]
                .misestimate
                .map(|miss| miss.factor)
        };
        // The shape of the spill fixture: the `Sort` reads the scan to the end.
        let sorted = vec![
            node("Limit", 0, 10.0, 10.0),
            node("Sort", 1, 10.0, 10.0),
            node("Seq Scan on big", 2, 1_000_000.0, 10.0),
        ];
        assert_eq!(miss(sorted, 2), Some(100_000.0));
        let aggregated = vec![
            node("Limit", 0, 10.0, 10.0),
            node("HashAggregate", 1, 10.0, 10.0),
            node("Seq Scan on big", 2, 1_000_000.0, 10.0),
        ];
        assert_eq!(miss(aggregated, 2), Some(100_000.0));
        // Rows pass through a join and a group aggregate as they are read.
        let streamed = vec![
            node("Limit", 0, 10.0, 10.0),
            node("GroupAggregate", 1, 10.0, 10.0),
            node("Nested Loop", 2, 10.0, 10.0),
            node("Seq Scan on a", 3, 10.0, 10.0),
            node("Seq Scan on b", 3, 1_000_000.0, 10.0),
        ];
        assert_eq!(miss(streamed, 4), None);
        // The outer side of a semi join is read to the end, the inner is not.
        let semi = vec![
            node("Nested Loop Semi Join", 0, 5.0, 5.0),
            node("Seq Scan on outer_side", 1, 1_000_000.0, 5.0),
            node("Index Scan on inner_side", 1, 100_000.0, 1.0),
        ];
        assert_eq!(miss(semi.clone(), 1), Some(200_000.0));
        assert_eq!(miss(semi, 2), None);
        // A hash join's inner side is the `Hash`, which reads all of it.
        let hashed = vec![
            node("Hash Semi Join", 0, 5.0, 5.0),
            node("Seq Scan on a", 1, 5.0, 5.0),
            node("Hash", 1, 5.0, 5.0),
            node("Seq Scan on b", 2, 100_000.0, 1.0),
        ];
        assert_eq!(miss(hashed, 3), Some(100_000.0));
    }

    #[test]
    fn a_parallel_subtree_is_marked_with_its_workers() {
        let plan = one_column(include_str!("../testdata/plan-postgres-parallel.txt"));
        let analysis = Analysis::new(&plan, Engine::Postgres);
        let gather = plan.nodes.iter().position(gathers).unwrap();
        let depth = plan.nodes[gather].depth;

        assert_eq!(
            analysis.nodes[gather].parallel,
            Some(Parallel {
                workers: 2,
                gathers: true
            })
        );
        for (index, node) in plan.nodes.iter().enumerate() {
            let inside = index > gather && node.depth > depth && {
                plan.nodes[gather + 1..index]
                    .iter()
                    .all(|between| between.depth > depth)
            };
            match (index == gather, inside) {
                (true, _) => {}
                (false, true) => assert_eq!(
                    analysis.nodes[index].parallel,
                    Some(Parallel {
                        workers: 2,
                        gathers: false
                    }),
                    "{}",
                    node.label
                ),
                (false, false) => {
                    assert_eq!(analysis.nodes[index].parallel, None, "{}", node.label)
                }
            }
        }
        assert!(
            analysis
                .nodes
                .iter()
                .any(|node| node.parallel.is_some_and(|p| !p.gathers))
        );

        // With no `Workers Launched` line the workers are the participants its
        // child ran in, less the leader.
        let bare = one_column(
            "Gather  (actual time=0.000..9.000 rows=1 loops=1)
  ->  Parallel Seq Scan on t  (actual time=0.000..8.000 rows=1 loops=3)",
        );
        let analysis = Analysis::new(&bare, Engine::Postgres);
        assert_eq!(analysis.nodes[0].parallel.map(|p| p.workers), Some(2));
        assert_eq!(analysis.nodes[1].parallel.map(|p| p.gathers), Some(false));
    }

    #[test]
    fn findings_list_hot_nodes_then_misestimates_in_a_stable_order() {
        let plan = one_column(
            "Append  (actual time=0.000..10.000 rows=1 loops=1)
  ->  Seq Scan on a  (cost=0.00..1.00 rows=100 width=1) (actual time=0.000..5.000 rows=10 loops=1)
  ->  Seq Scan on b  (cost=0.00..1.00 rows=1 width=1) (actual time=0.000..5.000 rows=1000 loops=1)
  ->  Seq Scan on c  (cost=0.00..1.00 rows=1 width=1) (actual time=0.000..0.000 rows=10 loops=1)",
        );
        let analysis = Analysis::new(&plan, Engine::Postgres);
        // `a` and `b` share the time equally and tie; `b`'s miss is the largest
        // and `a` and `c` tie on theirs.
        assert_eq!(
            analysis.findings,
            [
                Finding::Hot(1),
                Finding::Hot(2),
                Finding::Misestimated(2),
                Finding::Misestimated(1),
                Finding::Misestimated(3),
            ]
        );

        let danger = one_column("Seq Scan on a  (actual time=0.000..2.000 rows=1 loops=1)");
        assert_eq!(
            Analysis::new(&danger, Engine::Postgres).findings,
            [Finding::Hot(0)]
        );
        let quiet = one_column(HASH_JOIN);
        assert_eq!(
            Analysis::new(&quiet, Engine::Postgres).findings,
            [Finding::Hot(2)]
        );
    }

    #[test]
    fn time_spent_outside_every_node_is_reported() {
        let plan = one_column(include_str!("../testdata/plan-postgres-cte-subplan.txt"));
        let analysis = Analysis::new(&plan, Engine::Postgres);
        let execution = analysis
            .execution_ms
            .expect("the fixture reports an Execution Time");
        let total = analysis.total.unwrap();
        let outside = analysis
            .outside_nodes_ms
            .expect("JIT time sits outside every node");
        assert!(outside > 0.0);
        assert!((outside - (execution - total)).abs() < 0.01);

        // Inside one percent of the total is rounding, not a missing node.
        let close = one_column(
            "Seq Scan on a  (actual time=0.000..100.000 rows=1 loops=1)
Execution Time: 100.5 ms",
        );
        let analysis = Analysis::new(&close, Engine::Postgres);
        assert_eq!(analysis.execution_ms, Some(100.5));
        assert_eq!(analysis.outside_nodes_ms, None);
        let apart = one_column(
            "Seq Scan on a  (actual time=0.000..100.000 rows=1 loops=1)
Execution Time: 102 ms",
        );
        assert_eq!(
            Analysis::new(&apart, Engine::Postgres).outside_nodes_ms,
            Some(2.0)
        );
    }
}
