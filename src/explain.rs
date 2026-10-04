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

/// A parsed EXPLAIN, ready to render.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    /// Flattened depth-first, which is the order the server printed them and
    /// the order they render in.
    pub nodes: Vec<PlanNode>,
    /// The trailing `Planning Time: … ms` / `Execution Time: … ms` style lines,
    /// as label and value.
    pub summary: Vec<(String, String)>,
    /// What the bars are a share of. `None` when the plan carried no timings.
    pub total_ms: Option<f64>,
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
    /// `cost=0.00..35.50 rows=2550 width=4`, when the plan carried estimates.
    pub estimated: Option<Estimated>,
    /// `actual time=0.01..0.02 rows=10 loops=1`, when it was an ANALYZE.
    pub actual: Option<Actual>,
    /// Time in this node alone — its own total less its children's — in
    /// milliseconds, and what the bar is drawn from. `None` without timings.
    pub self_ms: Option<f64>,
}

/// What the planner guessed. MySQL prints one cost and no width, so
/// `startup_cost` and `total_cost` are the same number and `width` is 0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimated {
    pub startup_cost: f64,
    pub total_cost: f64,
    pub rows: u64,
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
    fn inclusive_ms(&self) -> f64 {
        self.total_ms * self.loops
    }
}

/// Parse the rows an EXPLAIN returned into a plan.
pub fn parse(columns: &[String], rows: &[Vec<Option<String>>]) -> Plan {
    if let Some(plan) = parse_linked(columns, rows).or_else(|| parse_document(columns, rows)) {
        return plan;
    }

    let text = rows
        .iter()
        .flatten()
        .flatten()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    parse_indented(text)
}

/// SQLite's `EXPLAIN QUERY PLAN`, which is a table and not a drawing: the tree
/// is in the `id`/`parent` linkage, and indentation would be a guess.
fn parse_linked(columns: &[String], rows: &[Vec<Option<String>>]) -> Option<Plan> {
    if columns.len() != 4 {
        return None;
    }
    let column = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let (id, parent, detail) = (column("id")?, column("parent")?, column("detail")?);
    column("notused")?;

    let mut depths: Vec<(i64, usize)> = Vec::new();
    let mut nodes = Vec::new();
    for row in rows {
        let cell = |index: usize| {
            row.get(index)
                .and_then(Option::as_deref)
                .unwrap_or_default()
                .trim()
        };
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
fn parse_document(columns: &[String], rows: &[Vec<Option<String>>]) -> Option<Plan> {
    use mongodb::bson::{Bson, Document};

    let [row] = rows else {
        return None;
    };
    let reply: Document = columns
        .iter()
        .zip(row)
        .filter_map(|(name, cell)| {
            let cell = cell.as_deref()?;
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
    plan.total_ms = plan
        .nodes
        .first()
        .and_then(|node| Some(node.actual?.inclusive_ms()))
        .or_else(|| {
            millis(
                reply
                    .get_document("executionStats")
                    .ok()?
                    .get("executionTimeMillis")?,
            )
        });
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
        let (indent, body, opens_node) = strip_marker(line);

        if opens_node || nodes.is_empty() {
            while ancestors.last().is_some_and(|last| *last >= indent) {
                ancestors.pop();
            }
            let (label, estimated, actual) = split_metrics(body);
            nodes.push(PlanNode {
                depth: ancestors.len(),
                label,
                detail: Vec::new(),
                estimated,
                actual,
                self_ms: None,
            });
            ancestors.push(indent);
        } else if indent == 0
            && let Some((label, value)) = body.split_once(':')
        {
            summary.push((label.trim().to_string(), value.trim().to_string()));
        } else if let Some(node) = nodes.last_mut() {
            node.detail.push(body.to_string());
        }
    }

    fill_self_ms(&mut nodes);
    let total_ms = summary
        .iter()
        .find(|(label, _)| label.eq_ignore_ascii_case("Execution Time"))
        .and_then(|(_, value)| value.split_whitespace().next()?.parse().ok())
        .or_else(|| Some(nodes.first()?.actual?.inclusive_ms()));

    Plan {
        nodes,
        summary,
        total_ms,
        text,
    }
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

fn cut<T>(label: &mut String, open: &str, parse: fn(&str) -> Option<T>) -> Option<T> {
    let start = label.find(open)?;
    let close = start + label[start..].find(')')?;
    let parsed = parse(&label[start + open.len()..close])?;
    label.replace_range(start..=close, "");
    Some(parsed)
}

/// `0.00..35.50 rows=2550 width=8`, and MySQL's `1.25 rows=10`.
fn parse_estimated(inner: &str) -> Option<Estimated> {
    let mut fields = inner.split_whitespace();
    let (startup_cost, total_cost) = pair(fields.next()?)?;
    let (mut rows, mut width) = (0, 0);
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

fn fill_self_ms(nodes: &mut [PlanNode]) {
    let inclusive: Vec<Option<f64>> = nodes
        .iter()
        .map(|node| node.actual.map(|actual| actual.inclusive_ms()))
        .collect();

    // ponytail: rescanning forward for each node is quadratic; plans are tens
    // of lines, and a child-index pass is the upgrade if one ever is not.
    for index in 0..nodes.len() {
        let Some(own) = inclusive[index] else {
            continue;
        };
        let depth = nodes[index].depth;
        let mut children = 0.0;
        for (sibling, node) in nodes.iter().enumerate().skip(index + 1) {
            if node.depth <= depth {
                break;
            }
            if node.depth == depth + 1 {
                children += inclusive[sibling].unwrap_or_default();
            }
        }
        // Estimates and loop averaging make small negatives normal, and a
        // negative bar is nonsense.
        nodes[index].self_ms = Some((own - children).max(0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_column(text: &str) -> Plan {
        parse(&["QUERY PLAN".to_string()], &[vec![Some(text.to_string())]])
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
        let row = reply
            .values()
            .map(|value| match value {
                mongodb::bson::Bson::String(text) => Some(text.clone()),
                other => serde_json::to_string(other).ok(),
            })
            .collect();
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
        close(plan.total_ms, 4.0);
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
        assert_eq!(plan.total_ms, None);
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
        close(plan.total_ms, 6.0);
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
        let plan = parse(&columns, &[vec![Some("1".into())]]);
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
                rows: 2550,
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
        // The reported execution time wins over the root's own, because it is
        // the number the user is shown.
        close(plan.total_ms, 0.041);
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
        close(plan.total_ms, 1.0);

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
            &[vec![Some(
                "-> Limit: 10 row(s)  (cost=1.25 rows=10) (actual time=0.021..0.030 rows=10 loops=1)\n    -> Table scan on accounts  (cost=2.50 rows=12) (actual time=0.019..0.025 rows=12 loops=1)\n"
                    .to_string(),
            )]],
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
                rows: 12,
                width: 0,
            })
        );
        close(plan.total_ms, 0.030);
        close(plan.nodes[0].self_ms, 0.030 - 0.025);
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
            &[
                row("2", "0", "SCAN accounts"),
                row("6", "2", "SEARCH orders USING INDEX orders_account"),
                row("4", "0", "USE TEMP B-TREE FOR ORDER BY"),
            ],
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
        assert_eq!(plan.total_ms, None);
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
        assert_eq!(plan.total_ms, None);

        assert_eq!(parse(&[], &[]), Plan::default());
        assert_eq!(one_column("").nodes, Vec::new());
    }
}
