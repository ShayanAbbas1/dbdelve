//! Turning the rows an EXPLAIN returns into a tree.
//!
//! Three servers print three different things and none of them is a data
//! structure: Postgres a block of indented text in one `QUERY PLAN` column,
//! MySQL the same idea crammed into a single cell with embedded newlines, and
//! SQLite a four-column table whose shape lives in `id`/`parent` rather than in
//! whitespace. There is no structured format they share — `FORMAT JSON` is
//! Postgres-only, and asking for it would mean rewriting the statement the user
//! asked to run, which rule 1 forbids. So we parse what arrived.
//!
//! Which means this parses text from servers across versions nobody here has
//! seen. Nothing in it may panic and nothing may be dropped: a line that makes
//! no sense becomes a node carrying its own raw text, and a metric that does
//! not parse stays in the label where the user can still read it.

pub(crate) mod analysis;
pub(crate) mod compact;

use crate::db::Row;

/// A parsed EXPLAIN, ready to render.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    /// Flattened depth-first, which is the order the server printed them and
    /// the order they render in.
    pub nodes: Vec<PlanNode>,
    /// The trailing `Planning Time: … ms` / `Execution Time: … ms` style lines,
    /// as label and value.
    pub summary: Vec<(String, String)>,
    /// The server's plan exactly as it arrived.
    pub text: String,
}

/// One operator in the plan.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanNode {
    pub depth: usize,
    /// The operator and its target: `Seq Scan on accounts`.
    pub label: String,
    /// The qualifier lines printed under the node — `Filter: …`,
    /// `Index Cond: …`, `Buffers: …` — verbatim and in order.
    pub detail: Vec<String>,
    /// The `SubPlan 2` / `InitPlan 1 (returns $0)` / `CTE big` line that
    /// introduced this node, verbatim. Postgres prints it above the subtree it
    /// owns, one level off from the owner's ordinary children.
    pub heading: Option<String>,
    /// `cost=0.00..35.50 rows=2550 width=4`, when the plan carried estimates.
    pub estimated: Option<Estimated>,
    /// `actual time=0.01..0.02 rows=10 loops=1`, when it was an ANALYZE.
    pub actual: Option<Actual>,
    /// Time in this node alone — its own total less its children's — in
    /// milliseconds, and what the bar is drawn from. `None` without timings.
    pub self_ms: Option<f64>,
}

/// What the planner guessed. MySQL prints one cost and no width, so
/// `startup_cost` and `total_cost` are the same number and `width` is 0. `rows`
/// is a float because MySQL estimates fractions (`rows=1.73`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimated {
    pub startup_cost: f64,
    pub total_cost: f64,
    pub rows: f64,
    pub width: u64,
}

/// What actually happened. `rows` and `loops` are floats because Postgres
/// prints an average over the loops, which is fractional.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Actual {
    pub startup_ms: f64,
    pub total_ms: f64,
    pub rows: f64,
    pub loops: f64,
}

impl Actual {
    /// The time the node took across every loop, which is what a parent's own
    /// total already contains and so what a parent subtracts.
    pub(crate) fn inclusive_ms(&self) -> f64 {
        self.total_ms * self.loops
    }
}

/// Parse the rows an EXPLAIN returned into a plan.
pub fn parse(columns: &[String], rows: &[Row]) -> Plan {
    if let Some(plan) = parse_linked(columns, rows).or_else(|| parse_document(columns, rows)) {
        return plan;
    }

    let text = rows
        .iter()
        .flat_map(Row::iter)
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    parse_indented(text)
}

/// SQLite's `EXPLAIN QUERY PLAN`, which is a table and not a drawing: the tree
/// is in the `id`/`parent` linkage, and indentation would be a guess.
fn parse_linked(columns: &[String], rows: &[Row]) -> Option<Plan> {
    if columns.len() != 4 {
        return None;
    }
    let column = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let (id, parent, detail) = (column("id")?, column("parent")?, column("detail")?);
    column("notused")?;

    let mut depths: Vec<(i64, usize)> = Vec::new();
    let mut nodes = Vec::new();
    for row in rows {
        let cell = |index: usize| row.get(index).unwrap_or_default().trim();
        // A parent of 0 is a root, and so is a parent we have not seen — a
        // forward reference would otherwise have no depth to hang from.
        let depth = cell(parent)
            .parse::<i64>()
            .ok()
            .and_then(|parent| depths.iter().find(|(id, _)| *id == parent))
            .map_or(0, |(_, depth)| depth + 1);
        if let Ok(id) = cell(id).parse::<i64>() {
            depths.push((id, depth));
        }
        nodes.push(PlanNode {
            depth,
            label: cell(detail).to_string(),
            ..PlanNode::default()
        });
    }

    let text = nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Some(Plan {
        nodes,
        text,
        ..Plan::default()
    })
}

/// MongoDB's `explain`, which is one document: each column of the one row is a
/// top-level field, rendered as Relaxed Extended JSON. The tree is in
/// `inputStage`/`inputStages` nesting, and an aggregate's pipeline in `stages`.
fn parse_document(columns: &[String], rows: &[Row]) -> Option<Plan> {
    use mongodb::bson::{Bson, Document};

    let [row] = rows else {
        return None;
    };
    let reply: Document = columns
        .iter()
        .zip(row.iter())
        .filter_map(|(name, cell)| {
            let cell = cell?;
            // Only a document or an array is JSON; a scalar's cell is bare text.
            let value = match cell.starts_with(['{', '[']) {
                true => serde_json::from_str(cell).ok(),
                false => None,
            };
            Some((
                name.clone(),
                value.unwrap_or_else(|| Bson::String(cell.into())),
            ))
        })
        .collect();
    if !reply.contains_key("queryPlanner") && !reply.contains_key("stages") {
        return None;
    }

    let mut plan = Plan::default();
    match reply.get_array("stages") {
        Ok(stages) => {
            let documents: Vec<&Document> = stages.iter().filter_map(Bson::as_document).collect();
            for (at, stage) in documents.iter().enumerate().rev() {
                let depth = documents.len() - 1 - at;
                // The leading `$cursor` is the query the pipeline reads from,
                // and holds a plan of its own beneath it.
                let cursor = stage.get_document("$cursor").ok();
                let name = stage.keys().next().map_or("stage", String::as_str);
                plan.nodes.push(PlanNode {
                    depth,
                    label: name.to_string(),
                    detail: match cursor {
                        Some(_) => Vec::new(),
                        None => stage.values().next().map(shown).into_iter().collect(),
                    },
                    actual: actual_of(stage),
                    ..PlanNode::default()
                });
                if let Some(cursor) = cursor {
                    plan_of(cursor, depth + 1, &mut plan);
                }
            }
        }
        Err(_) => plan_of(&reply, 0, &mut plan),
    }

    fill_self_ms(&mut plan.nodes);
    plan.text = serde_json::to_string_pretty(&reply).unwrap_or_default();
    Some(plan)
}

/// The winning plan of one `queryPlanner`, with what each stage did where the
/// reply came from an `executionStats` run.
fn plan_of(holder: &mongodb::bson::Document, depth: usize, plan: &mut Plan) {
    let winning = holder
        .get_document("queryPlanner")
        .and_then(|planner| planner.get_document("winningPlan"));
    let executed = holder
        .get_document("executionStats")
        .and_then(|stats| stats.get_document("executionStages"));
    // Newer servers' slot-based plans keep the stage tree one level down.
    let root = executed.ok().or_else(|| {
        let winning = winning.ok()?;
        winning.get_document("queryPlan").ok().or(Some(winning))
    });
    if let Some(root) = root {
        push_stage(root, depth, plan);
    }
    if let Ok(stats) = holder.get_document("executionStats") {
        for (label, key) in [
            ("Documents returned", "nReturned"),
            ("Keys examined", "totalKeysExamined"),
            ("Documents examined", "totalDocsExamined"),
        ] {
            if let Some(value) = stats.get(key).and_then(millis) {
                plan.summary.push((label.into(), value.to_string()));
            }
        }
    }
}

fn push_stage(stage: &mongodb::bson::Document, depth: usize, plan: &mut Plan) {
    use mongodb::bson::Bson;

    const COUNTERS: [&str; 19] = [
        "stage",
        "nReturned",
        "executionTimeMillisEstimate",
        "works",
        "advanced",
        "needTime",
        "needYield",
        "saveState",
        "restoreState",
        "isEOF",
        "isCached",
        "planNodeId",
        "opens",
        "closes",
        "inputStage",
        "inputStages",
        "outerStage",
        "innerStage",
        "queryPlan",
    ];
    let name = stage.get_str("stage").unwrap_or("stage");
    let label = match stage.get_str("indexName") {
        Ok(index) => format!("{name} on {index}"),
        Err(_) => name.to_string(),
    };
    plan.nodes.push(PlanNode {
        depth,
        label,
        detail: stage
            .iter()
            .filter(|(key, _)| !COUNTERS.contains(&key.as_str()))
            .map(|(key, value)| format!("{key}: {}", shown(value)))
            .collect(),
        actual: actual_of(stage),
        ..PlanNode::default()
    });
    for key in ["inputStage", "outerStage", "innerStage"] {
        if let Ok(child) = stage.get_document(key) {
            push_stage(child, depth + 1, plan);
        }
    }
    if let Ok(children) = stage.get_array("inputStages") {
        for child in children.iter().filter_map(Bson::as_document) {
            push_stage(child, depth + 1, plan);
        }
    }
}

/// What a stage reports having done. `executionTimeMillisEstimate` includes
/// the stage's inputs, as Postgres' `actual time` does, so `fill_self_ms`
/// subtracts it the same way. Absent from a `queryPlanner` reply.
fn actual_of(stage: &mongodb::bson::Document) -> Option<Actual> {
    Some(Actual {
        startup_ms: 0.0,
        total_ms: millis(stage.get("executionTimeMillisEstimate")?)?,
        rows: millis(stage.get("nReturned")?)?,
        loops: 1.0,
    })
}

fn millis(value: &mongodb::bson::Bson) -> Option<f64> {
    use mongodb::bson::Bson;
    match value {
        Bson::Int32(n) => Some(f64::from(*n)),
        Bson::Int64(n) => Some(*n as f64),
        Bson::Double(n) => Some(*n),
        _ => None,
    }
}

fn shown(value: &mongodb::bson::Bson) -> String {
    match value {
        mongodb::bson::Bson::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Postgres and MySQL, where depth is drawn rather than stated.
fn parse_indented(text: String) -> Plan {
    let mut nodes: Vec<PlanNode> = Vec::new();
    let mut summary: Vec<(String, String)> = Vec::new();
    // The indent column of every node on the path down to the last one. Depth
    // is that path's length, which holds for Postgres' 2-then-4 steps, MySQL's
    // 4 and SQLite's 3 without any of them being written down here.
    let mut ancestors: Vec<usize> = Vec::new();
    // A `SubPlan` / `InitPlan` / `CTE` line, held until the next line says
    // whether it introduces a node: its column, and the line itself.
    let mut heading: Option<(usize, String)> = None;

    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        // A drawn plan carries the column's own name as its first line. Left in,
        // it becomes the root and adopts the whole tree one level too deep.
        if nodes.is_empty() && line.trim().eq_ignore_ascii_case("QUERY PLAN") {
            continue;
        }
        let (mut indent, body, opens_node) = strip_marker(line);

        // The subtree under a heading is drawn two columns deeper than the
        // owner's ordinary children, so read at the heading's own column it is
        // their sibling. A heading that is not followed by that arrow keeps
        // the old reading: a line of the node above.
        let mut introduced = None;
        if let Some((column, text)) = heading.take() {
            if opens_node && indent == column + 2 {
                indent = column;
                introduced = Some(text);
            } else if let Some(node) = nodes.last_mut() {
                node.detail.push(text);
            }
        }

        if opens_node || nodes.is_empty() {
            while ancestors.last().is_some_and(|last| *last >= indent) {
                ancestors.pop();
            }
            let (label, estimated, actual) = split_metrics(body);
            nodes.push(PlanNode {
                depth: ancestors.len(),
                label,
                heading: introduced,
                estimated,
                actual,
                ..PlanNode::default()
            });
            ancestors.push(indent);
        } else if indent == 0
            && let Some((label, value)) = body.split_once(':')
        {
            summary.push((label.trim().to_string(), value.trim().to_string()));
        } else if indent > 0
            && let Some((_, value)) = summary.last_mut()
        {
            // `Planning:` and `JIT:` print their figures on indented lines
            // after the plan; they belong to the summary, not to the last node.
            if !value.is_empty() {
                value.push_str("; ");
            }
            value.push_str(body);
        } else if indent > 0 && is_heading(body) {
            heading = Some((indent, body.to_string()));
        } else if let Some(node) = nodes.last_mut() {
            node.detail.push(body.to_string());
        }
    }
    if let Some((_, text)) = heading
        && let Some(node) = nodes.last_mut()
    {
        node.detail.push(text);
    }

    fill_self_ms(&mut nodes);

    Plan {
        nodes,
        summary,
        text,
    }
}

/// Whether a line is one of Postgres' headers over a subtree it owns.
fn is_heading(body: &str) -> bool {
    ["SubPlan ", "InitPlan ", "CTE "]
        .iter()
        .any(|prefix| body.starts_with(prefix))
}

/// The indent column, the line without its tree drawing, and whether that
/// drawing opened a node. Postgres and MySQL draw with `->`, SQLite's indented
/// form with `|--` and `` `-- `` under runs of `|  `.
fn strip_marker(line: &str) -> (usize, &str, bool) {
    let indent = line
        .bytes()
        .take_while(|byte| *byte == b' ' || *byte == b'|')
        .count();
    let rest = &line[indent..];

    for marker in ["->", "`--", "--"] {
        if let Some(body) = rest.strip_prefix(marker) {
            return (indent, body.trim_start(), true);
        }
    }
    (indent, rest, false)
}

/// Cut the `(cost=…)` and `(actual …)` parentheticals off a node's line; what
/// is left is the label. One that does not parse is left where it was, because
/// a number we cannot read is still a number the user can.
fn split_metrics(line: &str) -> (String, Option<Estimated>, Option<Actual>) {
    let mut label = line.to_string();
    let estimated = cut(&mut label, "(cost=", parse_estimated);
    let actual = cut(&mut label, "(actual ", parse_actual);
    let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
    (label, estimated, actual)
}

/// The metrics are the last parentheticals of a line, so the match is the
/// rightmost one: the text before them can hold the same words, as in a MySQL
/// `Index lookup on t using idx_cost (cost=a.cost)`.
fn cut<T>(label: &mut String, open: &str, parse: fn(&str) -> Option<T>) -> Option<T> {
    let start = label.rfind(open)?;
    let close = start + label[start..].find(')')?;
    let parsed = parse(&label[start + open.len()..close])?;
    label.replace_range(start..=close, "");
    Some(parsed)
}

/// `0.00..35.50 rows=2550 width=8`, and MySQL's `1.25 rows=10`.
fn parse_estimated(inner: &str) -> Option<Estimated> {
    let mut fields = inner.split_whitespace();
    let (startup_cost, total_cost) = pair(fields.next()?)?;
    let (mut rows, mut width) = (0.0, 0);
    for field in fields {
        if let Some(value) = field.strip_prefix("rows=") {
            rows = value.parse().ok()?;
        } else if let Some(value) = field.strip_prefix("width=") {
            width = value.parse().ok()?;
        }
    }
    Some(Estimated {
        startup_cost,
        total_cost,
        rows,
        width,
    })
}

/// `time=0.015..0.019 rows=10 loops=1`. Without a `time=` there is nothing to
/// draw a bar from — `(never executed)` and `TIMING OFF` both land here — so
/// the parenthetical stays in the label instead.
fn parse_actual(inner: &str) -> Option<Actual> {
    let mut time = None;
    let (mut rows, mut loops) = (0.0, 1.0);
    for field in inner.split_whitespace() {
        if let Some(value) = field.strip_prefix("time=") {
            time = Some(pair(value)?);
        } else if let Some(value) = field.strip_prefix("rows=") {
            rows = value.parse().ok()?;
        } else if let Some(value) = field.strip_prefix("loops=") {
            loops = value.parse().ok()?;
        }
    }
    let (startup_ms, total_ms) = time?;
    Some(Actual {
        startup_ms,
        total_ms,
        rows,
        loops,
    })
}

/// `A..B`, or MySQL's bare `A` where startup and total are the same number.
fn pair(value: &str) -> Option<(f64, f64)> {
    match value.split_once("..") {
        Some((start, end)) => Some((start.parse().ok()?, end.parse().ok()?)),
        None => {
            let single = value.parse().ok()?;
            Some((single, single))
        }
    }
}

/// Whether a node runs its children in workers: `Gather` and `Gather Merge`.
/// Its children's times overlap, so they are read in wall time.
pub(crate) fn gathers(node: &PlanNode) -> bool {
    node.label.starts_with("Gather")
}

/// What a node cost by the clock: its own time less what its children cost.
/// Below a `Gather` every time is divided by the number of participants, the
/// `loops` of the Gather's direct child, because the workers ran at once.
/// The Gather itself ran once, in the leader, so it keeps its own time whole
/// and is what its children's divided times are taken out of.
// ponytail: the divisor is an average over the workers, so a skewed split
// reads a little off. The upgrade is the per-worker lines `VERBOSE` prints.
// ponytail: each Gather scans down for its first child and each CTE scans the
// whole plan for its consumer; plans are tens of nodes, and a name-to-consumer
// map built in one pass is the upgrade if one ever is not.
fn fill_self_ms(nodes: &mut [PlanNode]) {
    // The depth of each Gather on the path down to the node, and its divisor.
    let mut gathered: Vec<(usize, f64)> = Vec::new();
    let mut wall: Vec<Option<f64>> = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        while gathered
            .last()
            .is_some_and(|(depth, _)| *depth >= node.depth)
        {
            gathered.pop();
        }
        let divisor = gathered.last().map_or(1.0, |(_, workers)| *workers);
        wall.push(node.actual.map(|actual| actual.inclusive_ms() / divisor));
        if gathers(node) {
            let child = nodes[index + 1..]
                .iter()
                .take_while(|below| below.depth > node.depth)
                .find(|below| below.depth == node.depth + 1)
                .and_then(|below| below.actual);
            if let Some(workers) = child
                .map(|actual| actual.loops)
                .filter(|loops| *loops >= 1.0)
            {
                gathered.push((node.depth, workers));
            }
        }
    }

    // The node each one hangs from: the nearest one above it that is shallower.
    let mut path: Vec<usize> = Vec::new();
    let mut parent: Vec<Option<usize>> = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        while path
            .last()
            .is_some_and(|last| nodes[*last].depth >= node.depth)
        {
            path.pop();
        }
        parent.push(path.last().copied());
        path.push(index);
    }

    // What each node's children cost. A CTE is read when its first consumer
    // asks for a row, so the producer's time is inside that consumer's own and
    // is taken out of it, not out of the node the CTE is printed under. With
    // no consumer to be found it stays where it is printed.
    // Assumed, not read from the server: the first consumer in document order
    // is the one that started it (RESEARCH A2). It held on every captured
    // plan, where it was checked by self times adding up to the root's time.
    let mut children = vec![0.0; nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        let Some(owner) = parent[index] else {
            continue;
        };
        let consumer = node
            .heading
            .as_deref()
            .and_then(|heading| heading.strip_prefix("CTE "))
            .and_then(|name| {
                let scan = format!("CTE Scan on {}", name.trim());
                nodes.iter().position(|other| {
                    other
                        .label
                        .strip_prefix(&scan)
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
                })
            });
        children[consumer.unwrap_or(owner)] += wall[index].unwrap_or_default();
    }

    for (index, node) in nodes.iter_mut().enumerate() {
        // Estimates and loop averaging make small negatives normal, and a
        // negative bar is nonsense.
        node.self_ms = wall[index].map(|own| (own - children[index]).max(0.0));
    }
}

/// What a stretch of a node's label names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelPart {
    Operator,
    /// The ` on ` and ` using ` that join the operator to its targets.
    Keyword,
    Relation,
    Index,
    /// A trailing parenthetical, like `(never executed)`.
    Aside,
}

/// A label cut into consecutive parts that together are all of it:
/// `Index Scan` `using` `accounts_pkey` `on` `accounts a`.
pub fn label_parts(label: &str) -> Vec<(std::ops::Range<usize>, LabelPart)> {
    let head_end = match label.find(" (") {
        Some(at) if label.ends_with(')') => at,
        _ => label.len(),
    };
    let head = &label[..head_end];
    let mut parts = Vec::new();
    let mut part = LabelPart::Operator;
    let mut at = 0;
    while let Some((found, keyword)) = [" on ", " using "]
        .into_iter()
        .filter_map(|keyword| Some((at + head[at..].find(keyword)?, keyword)))
        .min()
    {
        parts.push((at..found, part));
        parts.push((found..found + keyword.len(), LabelPart::Keyword));
        part = match keyword {
            " using " => LabelPart::Index,
            _ if head.starts_with("Bitmap Index Scan") => LabelPart::Index,
            _ => LabelPart::Relation,
        };
        at = found + keyword.len();
    }
    parts.push((at..head_end, part));
    parts.push((head_end..label.len(), LabelPart::Aside));
    parts.retain(|(range, _)| !range.is_empty());
    parts
}

/// Whether a qualifier's value is an SQL expression, worth highlighting as one.
pub fn is_expression(key: &str) -> bool {
    matches!(
        key,
        "Filter"
            | "Index Cond"
            | "Recheck Cond"
            | "Hash Cond"
            | "Merge Cond"
            | "TID Cond"
            | "Join Filter"
            | "One-Time Filter"
            | "Run Condition"
            | "Sort Key"
            | "Presorted Key"
            | "Group Key"
            | "Hash Key"
            | "Cache Key"
            | "Order By"
            | "Output"
    )
}

/// The least a node must have thrown away, per loop, for [`wasteful`] to say it
/// threw away far more than it kept. It must also have thrown away more than it
/// kept: the compact form prints this number and that rule as the fact it
/// states.
pub(crate) const WASTEFUL_MIN_REMOVED: f64 = 1000.0;

/// Whether a `Rows Removed by …` qualifier says the node threw away far more
/// than it kept. Both counts are per loop, as Postgres prints them.
// ponytail: more removed than kept, and at least a thousand; a per-operator
// selectivity threshold is the upgrade if this flags healthy plans.
pub fn wasteful(line: &str, actual: Option<Actual>) -> bool {
    let Some((key, value)) = line.split_once(": ") else {
        return false;
    };
    let Ok(removed) = value.trim().parse::<f64>() else {
        return false;
    };
    key.starts_with("Rows Removed by ")
        && removed >= WASTEFUL_MIN_REMOVED
        && removed > actual.map_or(0.0, |actual| actual.rows)
}

/// Where the numbers are in a qualifier's value: `hit=12 read=3`, `9kB`. A
/// digit glued to a name, as in `idx_1`, is part of the name.
pub fn numbers(text: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let glued = at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
        if !bytes[at].is_ascii_digit() || glued {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len()
            && (bytes[at].is_ascii_digit()
                || bytes[at] == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit))
        {
            at += 1;
        }
        found.push(start..at);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Engine, rows};
    use analysis::{Analysis, Basis};

    fn one_column(text: &str) -> Plan {
        parse(
            &["QUERY PLAN".to_string()],
            &rows(vec![vec![Some(text.to_string())]]),
        )
    }

    /// Timings never subtract cleanly in binary, so assert to a tolerance far
    /// tighter than any difference the parser could introduce.
    #[track_caller]
    fn close(actual: Option<f64>, expected: f64) {
        let Some(actual) = actual else {
            panic!("expected {expected}, got None");
        };
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    /// What the driver hands back for an `explain`: one row, a column per
    /// top-level field, each document or array as one line of JSON.
    fn explain_reply(json: &str) -> Plan {
        let reply: mongodb::bson::Document = serde_json::from_str(json).unwrap();
        let columns: Vec<String> = reply.keys().cloned().collect();
        let row = Row::new(reply.values().map(|value| match value {
            mongodb::bson::Bson::String(text) => Some(text.clone()),
            other => serde_json::to_string(other).ok(),
        }))
        .unwrap();
        parse(&columns, &[row])
    }

    #[test]
    fn a_find_with_execution_stats_nests_by_input_stage_and_carries_its_counts() {
        // dbdelve_dev, MongoDB 8.2: find({_id: {$gt: 0}}).sort({_id: 1}).limit(3)
        let plan = explain_reply(
            r#"{"explainVersion":"1","queryPlanner":{"namespace":"dbdelve_dev.wide_metrics","winningPlan":{"stage":"LIMIT","limitAmount":3,"inputStage":{"stage":"FETCH","inputStage":{"stage":"IXSCAN","keyPattern":{"_id":1},"indexName":"_id_","direction":"forward"}}},"rejectedPlans":[]},"executionStats":{"executionSuccess":true,"nReturned":3,"executionTimeMillis":4,"totalKeysExamined":3,"totalDocsExamined":3,"executionStages":{"stage":"LIMIT","nReturned":3,"executionTimeMillisEstimate":4,"works":4,"limitAmount":3,"inputStage":{"stage":"FETCH","nReturned":3,"executionTimeMillisEstimate":3,"docsExamined":3,"inputStage":{"stage":"IXSCAN","nReturned":3,"executionTimeMillisEstimate":1,"indexName":"_id_","keysExamined":3,"direction":"forward"}}}},"ok":1}"#,
        );

        let shape: Vec<(usize, &str)> = plan
            .nodes
            .iter()
            .map(|node| (node.depth, node.label.as_str()))
            .collect();
        assert_eq!(shape, [(0, "LIMIT"), (1, "FETCH"), (2, "IXSCAN on _id_")]);
        assert_eq!(plan.nodes[0].detail, ["limitAmount: 3"]);
        assert_eq!(plan.nodes[1].detail, ["docsExamined: 3"]);
        assert_eq!(
            plan.nodes[2].detail,
            ["indexName: _id_", "keysExamined: 3", "direction: forward"]
        );
        assert_eq!(plan.nodes[2].actual.map(|actual| actual.rows), Some(3.0));
        close(plan.nodes[0].self_ms, 1.0);
        close(plan.nodes[1].self_ms, 2.0);
        close(plan.nodes[2].self_ms, 1.0);
        close(Analysis::new(&plan, Engine::MongoDb).total, 4.0);
        assert_eq!(
            plan.summary,
            [
                ("Documents returned".to_string(), "3".to_string()),
                ("Keys examined".to_string(), "3".to_string()),
                ("Documents examined".to_string(), "3".to_string()),
            ]
        );
        assert!(plan.text.contains("\"explainVersion\": \"1\""));
    }

    #[test]
    fn a_query_planner_reply_has_a_shape_and_no_timings() {
        // aggregate([{$match}, {$sort}, {$limit}]) collapsed into one cursor.
        let plan = explain_reply(
            r#"{"explainVersion":"1","queryPlanner":{"winningPlan":{"stage":"LIMIT","limitAmount":2,"inputStage":{"stage":"FETCH","filter":{"plan":{"$eq":"free"}},"inputStage":{"stage":"IXSCAN","indexName":"_id_"}}}},"ok":1}"#,
        );
        assert_eq!(plan.nodes.len(), 3);
        assert_eq!(plan.nodes[1].detail, [r#"filter: {"plan":{"$eq":"free"}}"#]);
        assert!(plan.nodes.iter().all(|node| node.actual.is_none()));
        // No stage carries a time, so nothing is weighed (D-20).
        assert_eq!(Analysis::new(&plan, Engine::MongoDb).total, None);
        assert!(plan.summary.is_empty());
    }

    #[test]
    fn a_pipeline_reads_last_stage_first_with_its_cursor_plan_beneath() {
        // events.aggregate([{$match}, {$group}, {$limit}]) on a slot-based plan.
        let plan = explain_reply(
            r#"{"explainVersion":"2","stages":[{"$cursor":{"queryPlanner":{"winningPlan":{"isCached":false,"queryPlan":{"stage":"GROUP","planNodeId":3,"inputStage":{"stage":"PROJECTION_COVERED","planNodeId":2,"inputStage":{"stage":"IXSCAN","planNodeId":1,"indexName":"account_id_1_occurred_at_-1"}}},"slotBasedPlan":{"slots":"$$RESULT=s7"}}},"executionStats":{"nReturned":0,"executionTimeMillis":6,"totalKeysExamined":0,"totalDocsExamined":0,"executionStages":{"stage":"project","nReturned":0,"executionTimeMillisEstimate":5,"inputStage":{"stage":"ixseek","nReturned":0,"executionTimeMillisEstimate":2,"indexName":"account_id_1_occurred_at_-1"}}}},"nReturned":0,"executionTimeMillisEstimate":5},{"$limit":2,"nReturned":0,"executionTimeMillisEstimate":6}],"ok":1}"#,
        );

        let shape: Vec<(usize, &str)> = plan
            .nodes
            .iter()
            .map(|node| (node.depth, node.label.as_str()))
            .collect();
        assert_eq!(
            shape,
            [
                (0, "$limit"),
                (1, "$cursor"),
                (2, "project"),
                (3, "ixseek on account_id_1_occurred_at_-1"),
            ]
        );
        assert_eq!(plan.nodes[0].detail, ["2"]);
        close(plan.nodes[0].self_ms, 1.0);
        close(plan.nodes[1].self_ms, 0.0);
        close(plan.nodes[2].self_ms, 3.0);
        close(Analysis::new(&plan, Engine::MongoDb).total, 6.0);
    }

    #[test]
    fn a_slot_based_query_plan_without_stats_is_read_from_query_plan() {
        let plan = explain_reply(
            r#"{"queryPlanner":{"winningPlan":{"queryPlan":{"stage":"GROUP","inputStage":{"stage":"COLLSCAN"}},"slotBasedPlan":{"slots":"x"}}}}"#,
        );
        let labels: Vec<&str> = plan.nodes.iter().map(|node| node.label.as_str()).collect();
        assert_eq!(labels, ["GROUP", "COLLSCAN"]);
    }

    #[test]
    fn an_or_plan_lists_every_input_stage() {
        let plan = explain_reply(
            r#"{"queryPlanner":{"winningPlan":{"stage":"SUBPLAN","inputStage":{"stage":"OR","inputStages":[{"stage":"IXSCAN","indexName":"a_1"},{"stage":"IXSCAN","indexName":"b_1"}]}}}}"#,
        );
        let shape: Vec<(usize, &str)> = plan
            .nodes
            .iter()
            .map(|node| (node.depth, node.label.as_str()))
            .collect();
        assert_eq!(
            shape,
            [
                (0, "SUBPLAN"),
                (1, "OR"),
                (2, "IXSCAN on a_1"),
                (2, "IXSCAN on b_1")
            ]
        );
    }

    #[test]
    fn a_reply_with_no_plan_in_it_is_not_taken_for_one() {
        let columns = ["ok".to_string()];
        let plan = parse(&columns, &rows(vec![vec![Some("1".into())]]));
        assert!(plan.nodes.len() <= 1);
        assert_eq!(plan.text, "1");
    }

    #[test]
    fn garbage_in_a_mongo_reply_loses_nothing_and_panics_nowhere() {
        let plan = explain_reply(
            r#"{"queryPlanner":{"winningPlan":{"stage":7,"inputStage":"x"}},"stages":3}"#,
        );
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(plan.nodes[0].label, "stage");
    }

    #[test]
    fn postgres_nests_by_indentation_and_keeps_its_qualifiers() {
        let plan = one_column(
            r#"Limit  (cost=0.00..0.29 rows=10 width=8) (actual time=0.015..0.019 rows=10 loops=1)
  ->  Nested Loop  (cost=0.00..1.00 rows=10 width=8) (actual time=0.014..0.018 rows=10 loops=1)
        ->  Seq Scan on accounts  (cost=0.00..35.50 rows=2550 width=8) (actual time=0.013..0.015 rows=10 loops=1)
              Filter: (id > 5)
              Rows Removed by Filter: 5
        ->  Index Scan using orders_pkey on orders  (cost=0.29..8.31 rows=1 width=4) (actual time=0.001..0.002 rows=1 loops=1)
Planning Time: 0.083 ms
Execution Time: 0.041 ms"#,
        );

        let shape: Vec<(usize, &str)> = plan
            .nodes
            .iter()
            .map(|node| (node.depth, node.label.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (0, "Limit"),
                (1, "Nested Loop"),
                (2, "Seq Scan on accounts"),
                (2, "Index Scan using orders_pkey on orders"),
            ]
        );

        // A qualifier belongs to the node above it however far it is indented
        // past it, and comes back verbatim.
        assert_eq!(
            plan.nodes[2].detail,
            vec!["Filter: (id > 5)", "Rows Removed by Filter: 5"]
        );
        assert!(plan.nodes[3].detail.is_empty());

        assert_eq!(
            plan.nodes[2].estimated,
            Some(Estimated {
                startup_cost: 0.00,
                total_cost: 35.50,
                rows: 2550.0,
                width: 8,
            })
        );
        assert_eq!(
            plan.nodes[0].actual,
            Some(Actual {
                startup_ms: 0.015,
                total_ms: 0.019,
                rows: 10.0,
                loops: 1.0,
            })
        );

        assert_eq!(
            plan.summary,
            vec![
                ("Planning Time".to_string(), "0.083 ms".to_string()),
                ("Execution Time".to_string(), "0.041 ms".to_string()),
            ]
        );
        // The reported execution time is kept as `execution_ms`; it is no
        // longer the share denominator, which is the root's own inclusive time.
        let analysis = Analysis::new(&plan, Engine::Postgres);
        close(analysis.execution_ms, 0.041);
        close(analysis.total, 0.019);
        assert!(plan.text.starts_with("Limit  (cost="));
    }

    #[test]
    fn a_nodes_own_time_excludes_the_time_of_its_children() {
        let plan = one_column(
            r#"Nested Loop  (cost=0.00..2.00 rows=10 width=8) (actual time=0.000..1.000 rows=10 loops=1)
  ->  Seq Scan on a  (cost=0.00..1.00 rows=10 width=8) (actual time=0.000..0.400 rows=10 loops=1)
  ->  Index Scan using b_pkey on b  (cost=0.00..1.00 rows=1 width=8) (actual time=0.000..0.020 rows=1 loops=10)
        Index Cond: (b.id = a.id)"#,
        );

        // The inner scan ran ten times, so it cost its parent 0.2 and not 0.02.
        close(plan.nodes[2].self_ms, 0.2);
        close(plan.nodes[1].self_ms, 0.4);
        close(plan.nodes[0].self_ms, 1.0 - 0.4 - 0.2);
        // Without an Execution Time line the root's inclusive time is the whole.
        let analysis = Analysis::new(&plan, Engine::Postgres);
        close(analysis.total, 1.0);
        assert_eq!(analysis.execution_ms, None);
        assert_eq!(analysis.outside_nodes_ms, None);

        let impossible = one_column(
            r#"Nested Loop  (actual time=0.000..1.000 rows=1 loops=1)
  ->  Seq Scan on a  (actual time=0.000..2.000 rows=1 loops=1)"#,
        );
        assert_eq!(impossible.nodes[0].self_ms, Some(0.0));
        assert_eq!(impossible.nodes[0].estimated, None);
    }

    #[test]
    fn mysql_arrives_as_one_cell_of_newlines_and_a_single_cost() {
        let plan = parse(
            &["EXPLAIN".to_string()],
            &rows(vec![vec![Some(
                "-> Limit: 10 row(s)  (cost=1.25 rows=10) (actual time=0.021..0.030 rows=10 loops=1)\n    -> Table scan on accounts  (cost=2.50 rows=12) (actual time=0.019..0.025 rows=12 loops=1)\n"
                    .to_string(),
            )]]),
        );

        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.nodes[0].depth, 0);
        assert_eq!(plan.nodes[0].label, "Limit: 10 row(s)");
        assert_eq!(plan.nodes[1].depth, 1);
        assert_eq!(plan.nodes[1].label, "Table scan on accounts");

        // One cost, so startup and total are it, and there is no width.
        assert_eq!(
            plan.nodes[1].estimated,
            Some(Estimated {
                startup_cost: 2.50,
                total_cost: 2.50,
                rows: 12.0,
                width: 0,
            })
        );
        close(Analysis::new(&plan, Engine::MySql).total, 0.030);
        close(plan.nodes[0].self_ms, 0.030 - 0.025);
    }

    #[test]
    fn a_node_label_that_holds_a_cost_parenthetical_keeps_its_real_metrics() {
        let plan = parse(
            &["EXPLAIN".to_string()],
            &rows(vec![vec![Some(
                "-> Index lookup on t using idx_cost (cost=a.cost)  (cost=0.35 rows=1) (actual time=0.010..0.012 rows=1 loops=1)\n    -> Index lookup on u using idx (cost=5)  (cost=2.50 rows=12)\n"
                    .to_string(),
            )]]),
        );

        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(
            plan.nodes[0].label,
            "Index lookup on t using idx_cost (cost=a.cost)"
        );
        assert_eq!(
            plan.nodes[0].estimated,
            Some(Estimated {
                startup_cost: 0.35,
                total_cost: 0.35,
                rows: 1.0,
                width: 0,
            })
        );
        assert!(plan.nodes[0].actual.is_some());
        assert_eq!(plan.nodes[1].label, "Index lookup on u using idx (cost=5)");
        assert_eq!(
            plan.nodes[1].estimated,
            Some(Estimated {
                startup_cost: 2.50,
                total_cost: 2.50,
                rows: 12.0,
                width: 0,
            })
        );
    }

    #[test]
    fn sqlite_takes_its_depth_from_the_parent_column_not_the_page() {
        let columns = ["id", "parent", "notused", "detail"].map(str::to_string);
        let row = |id: &str, parent: &str, detail: &str| {
            vec![
                Some(id.to_string()),
                Some(parent.to_string()),
                Some("0".to_string()),
                Some(detail.to_string()),
            ]
        };
        let plan = parse(
            &columns,
            &rows(vec![
                row("2", "0", "SCAN accounts"),
                row("6", "2", "SEARCH orders USING INDEX orders_account"),
                row("4", "0", "USE TEMP B-TREE FOR ORDER BY"),
            ]),
        );

        let shape: Vec<(usize, &str)> = plan
            .nodes
            .iter()
            .map(|node| (node.depth, node.label.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (0, "SCAN accounts"),
                (1, "SEARCH orders USING INDEX orders_account"),
                (0, "USE TEMP B-TREE FOR ORDER BY"),
            ]
        );
        assert_eq!(Analysis::new(&plan, Engine::Sqlite).total, None);
        assert!(plan.summary.is_empty());
        assert!(plan.nodes.iter().all(|node| node.actual.is_none()));
        assert!(plan.nodes.iter().all(|node| node.self_ms.is_none()));

        // Some builds hand back the drawn tree in one column instead, where
        // depth is back to being indentation.
        let drawn = one_column(
            "|--SCAN accounts\n|  `--SEARCH orders USING INDEX\n`--USE TEMP B-TREE FOR ORDER BY",
        );
        let shape: Vec<(usize, &str)> = drawn
            .nodes
            .iter()
            .map(|node| (node.depth, node.label.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (0, "SCAN accounts"),
                (1, "SEARCH orders USING INDEX"),
                (0, "USE TEMP B-TREE FOR ORDER BY"),
            ]
        );
    }

    #[test]
    fn a_line_nobody_can_read_still_renders_as_itself() {
        let plan = one_column(
            r#"Limit  (cost=oops rows=x width=) (actual whatever)
  ->  ???  (cost=0.00..1.00 rows=1 width=1)
  not a node at all
Planning Time: broken"#,
        );

        // Nothing is dropped: metrics that did not parse stay readable in the
        // label they came from.
        assert_eq!(
            plan.nodes[0].label,
            "Limit (cost=oops rows=x width=) (actual whatever)"
        );
        assert_eq!(plan.nodes[0].estimated, None);
        assert_eq!(plan.nodes[0].actual, None);
        assert_eq!(plan.nodes[0].self_ms, None);

        assert_eq!(plan.nodes[1].label, "???");
        assert_eq!(plan.nodes[1].detail, vec!["not a node at all"]);
        assert_eq!(
            plan.summary,
            vec![("Planning Time".to_string(), "broken".to_string())]
        );
        // The unreadable `actual whatever` yields no time; the one readable
        // `cost=` may still give a cost basis.
        let analysis = Analysis::new(&plan, Engine::Postgres);
        assert_eq!(analysis.execution_ms, None);
        assert_ne!(analysis.basis, Basis::Measured);

        assert_eq!(parse(&[], &[]), Plan::default());
        assert_eq!(one_column("").nodes, Vec::new());
    }

    fn parts(label: &str) -> Vec<(&str, LabelPart)> {
        let parts = label_parts(label);
        // Whatever the label, the parts are all of it, in order.
        assert_eq!(
            parts
                .iter()
                .map(|(range, _)| &label[range.clone()])
                .collect::<String>(),
            label
        );
        parts
            .into_iter()
            .map(|(range, part)| (&label[range], part))
            .collect()
    }

    #[test]
    fn a_label_reads_as_operator_then_what_it_reads_and_through_what() {
        use LabelPart::*;
        assert_eq!(
            parts("Index Scan using accounts_pkey on accounts a"),
            [
                ("Index Scan", Operator),
                (" using ", Keyword),
                ("accounts_pkey", Index),
                (" on ", Keyword),
                ("accounts a", Relation),
            ]
        );
        assert_eq!(
            parts("Seq Scan on orders o (never executed)"),
            [
                ("Seq Scan", Operator),
                (" on ", Keyword),
                ("orders o", Relation),
                (" (never executed)", Aside),
            ]
        );
        // A bitmap index scan names the index after `on`, not a table.
        assert_eq!(
            parts("Bitmap Index Scan on orders_account_idx"),
            [
                ("Bitmap Index Scan", Operator),
                (" on ", Keyword),
                ("orders_account_idx", Index),
            ]
        );
        assert_eq!(parts("Hash Join"), [("Hash Join", Operator)]);
        assert_eq!(parts("Limit: 10 row(s)"), [("Limit: 10 row(s)", Operator)]);
        assert_eq!(parts(""), []);
        parts("on using ( ) é on  using");
    }

    #[test]
    fn only_conditions_and_keys_are_read_as_sql() {
        assert!(is_expression("Filter"));
        assert!(is_expression("Hash Cond"));
        assert!(is_expression("Sort Key"));
        assert!(!is_expression("Rows Removed by Filter"));
        assert!(!is_expression("Buffers"));
    }

    #[test]
    fn rows_removed_is_flagged_when_far_more_were_thrown_away_than_kept() {
        let kept = |rows| {
            Some(Actual {
                startup_ms: 0.0,
                total_ms: 1.0,
                rows,
                loops: 1.0,
            })
        };
        assert!(wasteful("Rows Removed by Filter: 98213", kept(10.0)));
        assert!(wasteful("Rows Removed by Index Recheck: 5000", None));
        assert!(!wasteful("Rows Removed by Filter: 98213", kept(100_000.0)));
        assert!(!wasteful("Rows Removed by Filter: 999", kept(0.0)));
        assert!(!wasteful("Filter: (balance > 100000)", kept(0.0)));
        assert!(!wasteful("Rows Removed by Filter: lots", kept(0.0)));
    }

    /// The nearest node above `index` that is shallower: the one it hangs from.
    fn parent_of(plan: &Plan, index: usize) -> Option<usize> {
        let depth = plan.nodes[index].depth;
        plan.nodes[..index]
            .iter()
            .rposition(|node| node.depth < depth)
    }

    /// Every node's own time adds up to the clock time of the nodes at the top.
    /// This is the property that makes a bar a share of something: a parent
    /// clamped to zero, or a producer counted twice, breaks it.
    #[track_caller]
    fn assert_self_sums_to_root(plan: &Plan) {
        assert!(
            plan.nodes
                .iter()
                .all(|node| node.actual.is_none() || node.self_ms.is_some()),
            "a timed node has no self time"
        );
        let wall: f64 = plan
            .nodes
            .iter()
            .filter(|node| node.depth == 0)
            .filter_map(|node| Some(node.actual?.inclusive_ms()))
            .sum();
        let own: f64 = plan.nodes.iter().filter_map(|node| node.self_ms).sum();
        let slack = (wall * 0.01).max(0.5);
        assert!(
            (own - wall).abs() <= slack,
            "self times add up to {own} ms of a {wall} ms root"
        );
    }

    /// Every line the server printed is somewhere in the plan: a node's label,
    /// one of its details or its heading, or the summary. Nothing is dropped.
    #[track_caller]
    fn assert_nothing_dropped(text: &str) {
        let plan = one_column(text);
        for line in text.lines().map(str::trim_end) {
            if line.trim().is_empty() || line.trim().eq_ignore_ascii_case("QUERY PLAN") {
                continue;
            }
            let (_, body, _) = strip_marker(line);
            let (label, _, _) = split_metrics(body);
            let (name, value) = body.split_once(':').unwrap_or((body, ""));
            let found = plan.nodes.iter().any(|node| {
                node.label == label
                    || node.detail.iter().any(|detail| detail == body)
                    || node.heading.as_deref() == Some(body)
            }) || plan.summary.iter().any(|(entry, text)| {
                (entry == name.trim() && text.starts_with(value.trim())) || text.contains(body)
            });
            assert!(found, "dropped: {line:?}");
        }
    }

    #[test]
    fn a_parallel_plan_attributes_its_workers_by_wall_time() {
        let plan = one_column(include_str!("testdata/plan-postgres-parallel.txt"));
        let at = plan.nodes.iter().position(gathers).unwrap();
        let gather = &plan.nodes[at];
        let below: Vec<&PlanNode> = plan.nodes[at + 1..]
            .iter()
            .take_while(|node| node.depth > gather.depth)
            .collect();
        let children: Vec<&&PlanNode> = below
            .iter()
            .filter(|node| node.depth == gather.depth + 1)
            .collect();

        // Two workers and the leader.
        let workers = children[0].actual.unwrap().loops;
        assert_eq!(workers, 3.0);
        let own = gather.actual.unwrap().inclusive_ms();
        let taken: f64 = children
            .iter()
            .map(|node| node.actual.unwrap().inclusive_ms() / workers)
            .sum();
        assert!(
            own > taken,
            "the fixture's Gather must outlast its children"
        );
        close(gather.self_ms, own - taken);

        let wall = plan.nodes[0].actual.unwrap().inclusive_ms();
        for node in below {
            assert!(node.self_ms.unwrap() <= wall, "{}", node.label);
        }
        assert_self_sums_to_root(&plan);
    }

    #[test]
    fn a_subplan_is_a_child_of_the_node_that_runs_it() {
        let plan = one_column(include_str!("testdata/plan-postgres-cte-subplan.txt"));
        let at = plan
            .nodes
            .iter()
            .position(|node| {
                node.heading
                    .as_deref()
                    .is_some_and(|heading| heading.starts_with("SubPlan"))
            })
            .unwrap();
        let owner = parent_of(&plan, at).unwrap();
        assert_eq!(plan.nodes[at].depth, plan.nodes[owner].depth + 1);
        // The owner's ordinary child sits beside it, not above it.
        assert!(plan.nodes.iter().enumerate().any(|(index, node)| {
            index != at
                && node.heading.is_none()
                && node.depth == plan.nodes[at].depth
                && parent_of(&plan, index) == Some(owner)
        }));
        assert!(plan.nodes[owner].self_ms.unwrap() > 0.0);
    }

    #[test]
    fn a_heading_arrow_is_read_at_the_headings_own_column() {
        let plan = one_column(
            r#"Result  (cost=0.00..1.00 rows=1 width=4) (actual time=0.000..10.000 rows=1 loops=1)
  ->  Seq Scan on a  (cost=0.00..1.00 rows=1 width=4) (actual time=0.000..2.000 rows=1 loops=1)
  SubPlan 1
    ->  Aggregate  (cost=0.00..1.00 rows=1 width=4) (actual time=0.000..3.000 rows=1 loops=1)
  SubPlan 2
    ->  Aggregate  (cost=0.00..1.00 rows=1 width=4) (actual time=0.000..4.000 rows=1 loops=1)"#,
        );
        let shape: Vec<(usize, Option<&str>)> = plan
            .nodes
            .iter()
            .map(|node| (node.depth, node.heading.as_deref()))
            .collect();
        assert_eq!(
            shape,
            [
                (0, None),
                (1, None),
                (1, Some("SubPlan 1")),
                (1, Some("SubPlan 2")),
            ]
        );
        assert!(plan.nodes.iter().all(|node| node.detail.is_empty()));
        close(plan.nodes[0].self_ms, 10.0 - 2.0 - 3.0 - 4.0);
        assert_self_sums_to_root(&plan);
    }

    #[test]
    fn a_cte_is_read_as_a_child_of_its_owner_and_timed_inside_its_first_consumer() {
        let plan = one_column(include_str!("testdata/plan-postgres-cte-subplan.txt"));
        let producer = plan
            .nodes
            .iter()
            .position(|node| {
                node.heading
                    .as_deref()
                    .is_some_and(|heading| heading.starts_with("CTE "))
            })
            .unwrap();
        assert_eq!(parent_of(&plan, producer), Some(0));
        assert_eq!(plan.nodes[producer].depth, 1);

        let consumer = plan
            .nodes
            .iter()
            .position(|node| node.label.starts_with("CTE Scan on big"))
            .unwrap();
        let producer_ms = plan.nodes[producer].actual.unwrap().inclusive_ms();
        close(
            plan.nodes[consumer].self_ms,
            plan.nodes[consumer].actual.unwrap().inclusive_ms() - producer_ms,
        );
        assert!(plan.nodes[consumer].self_ms.unwrap() < producer_ms);

        for node in &plan.nodes {
            assert!(
                node.detail
                    .iter()
                    .all(|line| !is_heading(line) && !line.starts_with("CTE")),
                "{:?}",
                node.detail
            );
        }
        assert_self_sums_to_root(&plan);
    }

    #[test]
    fn a_cte_read_twice_is_credited_to_the_first_consumer_in_document_order() {
        let text = |first: &str, between: &str| {
            format!(
                r#"Append  (actual time=0.000..30.000 rows=1 loops=1)
{between}  ->  CTE Scan on big {first}  (actual time=0.000..8.000 rows=1 loops=1)
  ->  CTE Scan on big other  (actual time=0.000..6.000 rows=1 loops=1)
  CTE big
    ->  Seq Scan on t  (actual time=0.000..5.000 rows=1 loops=1)"#
            )
        };
        let plan = one_column(&text("a", ""));
        let credited: Vec<(&str, Option<f64>)> = plan
            .nodes
            .iter()
            .map(|node| (node.label.as_str(), node.self_ms))
            .collect();
        assert_eq!(credited[1], ("CTE Scan on big a", Some(3.0)));
        assert_eq!(credited[2], ("CTE Scan on big other", Some(6.0)));
        close(plan.nodes[0].self_ms, 30.0 - 8.0 - 6.0);
        assert_self_sums_to_root(&plan);

        // An unrelated sibling before it moves nothing: the first `CTE Scan on
        // big` in the text is still the one credited.
        let plan = one_column(&text(
            "a",
            "  ->  Seq Scan on u  (actual time=0.000..1.000 rows=1 loops=1)\n",
        ));
        let credited = plan
            .nodes
            .iter()
            .find(|node| node.label == "CTE Scan on big a")
            .unwrap();
        close(credited.self_ms, 3.0);
        assert_self_sums_to_root(&plan);
    }

    #[test]
    fn planning_and_jit_lines_belong_to_the_summary_not_to_a_node() {
        let plan = one_column(include_str!("testdata/plan-postgres-cte-subplan.txt"));
        let entry = |label: &str| {
            plan.summary
                .iter()
                .find(|(name, _)| name == label)
                .map(|(_, value)| value.as_str())
                .unwrap()
        };
        let planning = entry("Planning");
        assert!(planning.contains("Buffers:"), "{planning}");
        let jit = entry("JIT");
        assert!(
            jit.contains("Functions:") && jit.contains("Timing:"),
            "{jit}"
        );

        let last = plan.nodes.last().unwrap();
        assert!(last.detail.iter().all(|line| {
            !["Functions:", "Options:", "Timing:"]
                .iter()
                .any(|key| line.starts_with(key))
                && !planning.contains(line.as_str())
        }));
    }

    #[test]
    fn every_plan_fixture_has_self_times_that_sum_to_its_root() {
        for text in [
            include_str!("testdata/plan-postgres-parallel.txt"),
            include_str!("testdata/plan-postgres-cte-subplan.txt"),
            include_str!("testdata/plan-postgres-spill.txt"),
            include_str!("testdata/plan-postgres-large.txt"),
        ] {
            assert_self_sums_to_root(&one_column(text));
            assert_nothing_dropped(text);
        }
    }

    #[test]
    fn a_fractional_estimate_still_parses() {
        let plan = one_column("-> Group aggregate: count(0)  (cost=3.8 rows=1.73)");
        assert_eq!(plan.nodes[0].label, "Group aggregate: count(0)");
        assert_eq!(
            plan.nodes[0].estimated,
            Some(Estimated {
                startup_cost: 3.8,
                total_cost: 3.8,
                rows: 1.73,
                width: 0,
            })
        );
    }

    #[test]
    fn a_two_decimal_actual_row_count_parses() {
        let plan = one_column(
            "Seq Scan on t  (cost=0.00..15.00 rows=1000 width=4) (actual time=0.010..0.200 rows=1000.00 loops=1)",
        );
        assert_eq!(plan.nodes[0].actual.map(|actual| actual.rows), Some(1000.0));
        assert_eq!(
            plan.nodes[0].estimated.map(|estimated| estimated.rows),
            Some(1000.0)
        );
    }

    #[test]
    fn a_heading_nobody_follows_loses_nothing_and_panics_nowhere() {
        let checked = |text: &str| {
            assert_nothing_dropped(text);
            one_column(text)
        };
        let root =
            "Result  (cost=0.00..1.00 rows=1 width=4) (actual time=0.000..1.000 rows=1 loops=1)";

        // Nothing after it: it stays a line of the node above.
        let plan = checked(&format!("{root}\n  SubPlan 1"));
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(plan.nodes[0].detail, ["SubPlan 1"]);

        // An arrow at the wrong depth is an ordinary node, and the heading
        // is still there.
        let plan = checked(&format!(
            "{root}\n  InitPlan 1 (returns $0)\n        ->  Seq Scan on t  (cost=0.00..1.00 rows=1 width=4)"
        ));
        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.nodes[0].detail, ["InitPlan 1 (returns $0)"]);
        assert_eq!(plan.nodes[1].heading, None);

        // Two headings with no arrow between them keep both.
        let plan = checked(&format!("{root}\n  SubPlan 1\n  CTE c"));
        assert_eq!(plan.nodes[0].detail, ["SubPlan 1", "CTE c"]);

        // A plan that is only a heading.
        let plan = checked("SubPlan 1");
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(plan.nodes[0].label, "SubPlan 1");

        // Text a summary line cannot hold in UTF-8 arrives as replacement
        // characters, and is kept.
        let plan = checked(&format!("{root}\nJIT:\n  \u{fffd}\u{fffd}"));
        assert_eq!(
            plan.summary,
            [("JIT".to_string(), "\u{fffd}\u{fffd}".to_string())]
        );

        // A count too large for a float to hold.
        let plan = checked(
            "Seq Scan on t  (cost=0.00..1.00 rows=1e999 width=4) (actual time=0.000..1.000 rows=1e999 loops=1)",
        );
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(
            Analysis::new(&plan, Engine::Postgres).nodes[0].misestimate,
            None
        );
    }

    #[test]
    fn numbers_are_found_but_not_inside_names() {
        let text = "shared hit=12 read=3.5 idx_1 9kB é7";
        let found: Vec<&str> = numbers(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(found, ["12", "3.5", "9", "7"]);
        assert!(numbers("").is_empty());
    }
}
