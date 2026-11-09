//! A query tab's `explain`, the pure parts: the command it sends, and its reply's winning plan
//! as rows. The session side, which sends it, is `query.rs`'s.

use mongodb::bson::{doc, Bson, Document};

use super::query::{plural_documents, user_limit};
use super::values;
use crate::mongoquery::{ExplainVerbosity, MongoCall, MongoQueryOp};

/// The keys a plan stage holds its children under, in the order they are shown; `inputStages`
/// (an array) comes right after `inputStage`.
const ONE_CHILD: [&str; 4] = ["outerStage", "innerStage", "thenStage", "elseStage"];
/// The fields a stage's `details` show, the first present one.
const DETAILS: [&str; 4] = ["filter", "sortPattern", "keyPattern", "transformBy"];

/// The `explain` command of `of`, it and its inner command carrying `comment` (`currentOp`
/// shows the outer one as `command.comment`, which `kill_by_comment` finds):
/// - a find: `{ find, filter, projection?, sort?, skip?, limit? }`, the limit the user's
///   (`user_limit`), with no cap;
/// - an aggregate: `{ aggregate, pipeline, cursor: {} }`.
///
/// `None` for anything else, which `parse` never puts under an explain.
pub(super) fn explain_command(
    call: &MongoCall,
    of: &MongoQueryOp,
    verbosity: ExplainVerbosity,
    comment: &Bson,
) -> Option<Document> {
    let mut inner = match of {
        MongoQueryOp::Find { filter, projection, sort, skip, limit, one } => {
            let mut find = doc! { "find": call.collection.as_str(), "filter": filter.clone() };
            if let Some(p) = projection {
                find.insert("projection", p.clone());
            }
            if let Some(s) = sort {
                find.insert("sort", s.clone());
            }
            if let Some(n) = skip {
                find.insert("skip", i64::try_from(*n).unwrap_or(i64::MAX));
            }
            if let Some(n) = user_limit(*one, *limit) {
                find.insert("limit", i64::try_from(n).unwrap_or(i64::MAX));
            }
            find
        }
        MongoQueryOp::Aggregate { pipeline } => {
            doc! { "aggregate": call.collection.as_str(), "pipeline": pipeline.clone(), "cursor": {} }
        }
        _ => return None,
    };
    inner.insert("comment", comment.clone());
    Some(doc! { "explain": inner, "verbosity": verbosity.as_str(), "comment": comment.clone() })
}

/// The error of a statement that failed while explain ran it (with `executionStats` or
/// `allPlansExecution`): the server replies `ok: 1`, with `executionSuccess: false`,
/// `errorCode` and `errorMessage` in `executionStats` (on 6.0's aggregate, `stages[0].$cursor`'s).
/// Its code, when there is one, and its message; `None` when the run didn't fail.
pub(crate) fn execution_failure(reply: &Document) -> Option<(Option<i32>, String)> {
    let stats = match reply.get_document("executionStats") {
        Ok(stats) => stats,
        Err(_) => reply
            .get_array("stages")
            .ok()?
            .first()?
            .as_document()?
            .get_document("$cursor")
            .ok()?
            .get_document("executionStats")
            .ok()?,
    };
    if stats.get_bool("executionSuccess").ok()? {
        return None;
    }
    let code = match stats.get("errorCode") {
        Some(Bson::Int32(c)) => Some(*c),
        Some(Bson::Int64(c)) => i32::try_from(*c).ok(),
        Some(Bson::Double(c)) if c.fract() == 0.0 => Some(*c as i32),
        _ => None,
    };
    let message = stats.get_str("errorMessage").unwrap_or(FAILED_WHILE_RUN).to_string();
    Some((code, message))
}

/// The message of a failed run whose stats have no `errorMessage`.
const FAILED_WHILE_RUN: &str = "The statement failed while explain ran it.";

/// The rows of `reply`'s winning plan, each `{ stage, index, details }`, children before their
/// parent; and the stats notice, when the reply has `executionStats`. `None` when there is no
/// plan. The plan is:
/// - `queryPlanner.winningPlan`, unwrapping 8.0's `queryPlan`;
/// - or, on 6.0's aggregate, `stages[0].$cursor`'s, followed by a row per later stage: its `$`
///   key, no index, and that key's value as the details.
pub(crate) fn explain_rows(reply: &Document) -> Option<(Vec<Document>, Option<String>)> {
    let mut rows = Vec::new();
    if let Ok(planner) = reply.get_document("queryPlanner") {
        plan_rows(winning_plan(planner)?, &mut rows);
        return Some((rows, stats_notice(reply)));
    }
    let (first, later) = reply.get_array("stages").ok()?.split_first()?;
    let cursor = first.as_document()?.get_document("$cursor").ok()?;
    plan_rows(winning_plan(cursor.get_document("queryPlanner").ok()?)?, &mut rows);
    for stage in later {
        let Some((name, value)) = stage.as_document().and_then(|d| d.iter().find(|(k, _)| k.starts_with('$'))) else {
            eprintln!("explain: a pipeline stage without a $ key: {stage}");
            continue;
        };
        rows.push(row(Bson::String(name.clone()), Bson::Null, Bson::String(values::ext_json(value))));
    }
    Some((rows, stats_notice(cursor)))
}

fn winning_plan(planner: &Document) -> Option<&Document> {
    let plan = planner.get_document("winningPlan").ok()?;
    Some(plan.get_document("queryPlan").unwrap_or(plan))
}

/// `stage`'s children's rows, then its own.
fn plan_rows(stage: &Document, rows: &mut Vec<Document>) {
    let input = stage.get_document("inputStage").into_iter();
    let inputs = stage.get_array("inputStages").into_iter().flatten().filter_map(Bson::as_document);
    let others = ONE_CHILD.iter().filter_map(|k| stage.get_document(k).ok());
    for child in input.chain(inputs).chain(others) {
        plan_rows(child, rows);
    }
    let text = |k: &str| stage.get_str(k).map_or(Bson::Null, |s| Bson::String(s.into()));
    let details = DETAILS.iter().find_map(|k| stage.get(k)).map_or(Bson::Null, |v| match v {
        Bson::String(s) => Bson::String(s.clone()),
        other => Bson::String(values::ext_json(other)),
    });
    rows.push(row(text("stage"), text("indexName"), details));
}

fn row(stage: Bson, index: Bson, details: Bson) -> Document {
    doc! { "stage": stage, "index": index, "details": details }
}

/// `Returned <n doc(s)>; examined <k index key(s)> and <d doc(s)> in <ms> ms.`, from
/// `parent.executionStats`; `None` without them.
fn stats_notice(parent: &Document) -> Option<String> {
    let stats = parent.get_document("executionStats").ok()?;
    let number = |k: &str| match stats.get(k)? {
        Bson::Int32(n) => usize::try_from(*n).ok(),
        Bson::Int64(n) => usize::try_from(*n).ok(),
        Bson::Double(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as usize),
        _ => None,
    };
    let (returned, keys, docs, ms) = (
        number("nReturned")?,
        number("totalKeysExamined")?,
        number("totalDocsExamined")?,
        number("executionTimeMillis")?,
    );
    let keys = if keys == 1 { "1 index key".to_string() } else { format!("{keys} index keys") };
    Some(format!(
        "Returned {}; examined {keys} and {} in {ms} ms.",
        plural_documents(returned),
        plural_documents(docs)
    ))
}

#[cfg(test)]
mod tests {
    use mongodb::bson::doc;

    use super::*;

    /// Each row's `stage`, `index` and `details`, `None` for null; and that every row has
    /// exactly those keys, in that order.
    fn cells(rows: &[Document]) -> Vec<(Option<&str>, Option<&str>, Option<&str>)> {
        rows.iter()
            .map(|r| {
                let keys: Vec<&str> = r.keys().map(String::as_str).collect();
                assert_eq!(keys, ["stage", "index", "details"]);
                let text = |k: &str| match r.get(k) {
                    Some(Bson::String(s)) => Some(s.as_str()),
                    Some(Bson::Null) => None,
                    other => panic!("{k}: {other:?}"),
                };
                (text("stage"), text("index"), text("details"))
            })
            .collect()
    }

    fn rows_of(reply: &Document) -> (Vec<Document>, Option<String>) {
        explain_rows(reply).expect("a plan")
    }

    /// 6.0, an equality on the unique `email_1`: FETCH over IXSCAN, scan first.
    fn find_60() -> Document {
        doc! {
            "explainVersion": "1",
            "queryPlanner": {
                "namespace": "fx.users",
                "winningPlan": {
                    "stage": "FETCH",
                    "inputStage": {
                        "stage": "IXSCAN",
                        "keyPattern": { "email": 1 },
                        "indexName": "email_1",
                        "isMultiKey": false,
                        "multiKeyPaths": { "email": [] },
                        "isUnique": true,
                        "direction": "forward",
                        "indexBounds": { "email": ["[\"ann@example.com\", \"ann@example.com\"]"] },
                    },
                },
                "rejectedPlans": [],
            },
            "executionStats": {
                "executionSuccess": true,
                "nReturned": 1,
                "executionTimeMillis": 0,
                "totalKeysExamined": 1,
                "totalDocsExamined": 1,
            },
            "ok": 1.0,
        }
    }

    #[test]
    fn find_on_60_is_the_scan_then_the_fetch() {
        let (rows, _) = rows_of(&find_60());
        assert_eq!(cells(&rows), [(Some("IXSCAN"), Some("email_1"), Some(r#"{"email":1}"#)), (Some("FETCH"), None, None)]);
    }

    /// 8.0's one EXPRESS_IXSCAN, whose `keyPattern` is a string, kept as written.
    #[test]
    fn find_on_80_is_one_express_scan() {
        let reply = doc! {
            "explainVersion": "1",
            "queryPlanner": {
                "winningPlan": {
                    "isCached": false,
                    "stage": "EXPRESS_IXSCAN",
                    "keyPattern": "{ email: 1 }",
                    "indexName": "email_1",
                },
                "rejectedPlans": [],
            },
            "ok": 1.0,
        };
        let (rows, stats) = rows_of(&reply);
        assert_eq!(cells(&rows), [(Some("EXPRESS_IXSCAN"), Some("email_1"), Some("{ email: 1 }"))]);
        assert_eq!(stats, None);
    }

    /// 6.0's aggregate: the `$cursor`'s plan, then a row per later stage, its `$` key's
    /// value as the details; the stats are the `$cursor`'s.
    #[test]
    fn aggregate_on_60_is_the_cursor_plan_then_the_stages() {
        let reply = doc! {
            "explainVersion": "1",
            "stages": [
                {
                    "$cursor": {
                        "queryPlanner": {
                            "namespace": "fx.users",
                            "winningPlan": {
                                "stage": "PROJECTION_SIMPLE",
                                "transformBy": { "active": 1, "_id": 0 },
                                "inputStage": {
                                    "stage": "COLLSCAN",
                                    "filter": { "active": { "$eq": true } },
                                    "direction": "forward",
                                },
                            },
                            "rejectedPlans": [],
                        },
                        "executionStats": {
                            "executionSuccess": true,
                            "nReturned": 4,
                            "executionTimeMillis": 0,
                            "totalKeysExamined": 0,
                            "totalDocsExamined": 6,
                        },
                    },
                    "nReturned": 4_i64,
                    "executionTimeMillisEstimate": 0_i64,
                },
                {
                    "$group": { "_id": "$active", "n": { "$sum": { "$const": 1 } } },
                    "maxAccumulatorMemoryUsageBytes": { "n": 80_i64 },
                    "totalOutputDataSizeBytes": 245_i64,
                    "usedDisk": false,
                    "spills": 0_i64,
                    "nReturned": 1_i64,
                    "executionTimeMillisEstimate": 0_i64,
                },
            ],
            "ok": 1.0,
        };
        let (rows, stats) = rows_of(&reply);
        assert_eq!(
            cells(&rows),
            [
                (Some("COLLSCAN"), None, Some(r#"{"active":{"$eq":true}}"#)),
                (Some("PROJECTION_SIMPLE"), None, Some(r#"{"active":1,"_id":0}"#)),
                (Some("$group"), None, Some(r#"{"_id":"$active","n":{"$sum":{"$const":1}}}"#)),
            ]
        );
        assert_eq!(stats.as_deref(), Some("Returned 4 documents; examined 0 index keys and 6 documents in 0 ms."));
    }

    /// 8.0's aggregate, pushed down: the plan is wrapped in `queryPlan`.
    #[test]
    fn aggregate_on_80_unwraps_the_query_plan() {
        let reply = doc! {
            "explainVersion": "2",
            "queryPlanner": {
                "namespace": "fx.users",
                "optimizedPipeline": true,
                "winningPlan": {
                    "isCached": false,
                    "queryPlan": {
                        "stage": "GROUP",
                        "planNodeId": 3,
                        "inputStage": {
                            "stage": "COLLSCAN",
                            "planNodeId": 1,
                            "filter": { "active": { "$eq": true } },
                            "direction": "forward",
                        },
                    },
                    "slotBasedPlan": { "slots": "$$RESULT=s9", "stages": "[3] project ..." },
                },
                "rejectedPlans": [],
            },
            "executionStats": {
                "executionSuccess": true,
                "nReturned": 1,
                "executionTimeMillis": 3,
                "totalKeysExamined": 0,
                "totalDocsExamined": 6,
                "executionStages": { "stage": "project", "inputStage": { "stage": "group" } },
            },
            "ok": 1.0,
        };
        let (rows, stats) = rows_of(&reply);
        assert_eq!(
            cells(&rows),
            [(Some("COLLSCAN"), None, Some(r#"{"active":{"$eq":true}}"#)), (Some("GROUP"), None, None)]
        );
        assert_eq!(stats.as_deref(), Some("Returned 1 document; examined 0 index keys and 6 documents in 3 ms."));
    }

    /// An `$or` on two indexes: `inputStages` in order, each child before its parent.
    #[test]
    fn an_or_tree_is_post_order() {
        let reply = doc! {
            "queryPlanner": {
                "winningPlan": {
                    "stage": "SUBPLAN",
                    "inputStage": {
                        "stage": "FETCH",
                        "inputStage": {
                            "stage": "OR",
                            "inputStages": [
                                { "stage": "IXSCAN", "keyPattern": { "_id": 1 }, "indexName": "_id_" },
                                { "stage": "IXSCAN", "keyPattern": { "email": 1 }, "indexName": "email_1" },
                            ],
                        },
                    },
                },
            },
        };
        let (rows, _) = rows_of(&reply);
        assert_eq!(
            cells(&rows),
            [
                (Some("IXSCAN"), Some("_id_"), Some(r#"{"_id":1}"#)),
                (Some("IXSCAN"), Some("email_1"), Some(r#"{"email":1}"#)),
                (Some("OR"), None, None),
                (Some("FETCH"), None, None),
                (Some("SUBPLAN"), None, None),
            ]
        );
    }

    /// Joins and conditionals: `outerStage`, `innerStage`, `thenStage` and `elseStage`, in that
    /// order after `inputStage`; `sortPattern` comes before `keyPattern`.
    #[test]
    fn every_child_key_is_followed() {
        let reply = doc! {
            "queryPlanner": {
                "winningPlan": {
                    "stage": "TOP",
                    "inputStage": { "stage": "A" },
                    "outerStage": { "stage": "B" },
                    "innerStage": { "stage": "C" },
                    "thenStage": { "stage": "D" },
                    "elseStage": { "stage": "E", "sortPattern": { "n": -1 }, "keyPattern": { "n": 1 } },
                },
            },
        };
        let (rows, _) = rows_of(&reply);
        let stages: Vec<_> = cells(&rows).into_iter().map(|(s, _, _)| s.unwrap()).collect();
        assert_eq!(stages, ["A", "B", "C", "D", "E", "TOP"]);
        assert_eq!(cells(&rows)[4].2, Some(r#"{"n":-1}"#));
    }

    #[test]
    fn a_reply_without_a_plan_is_none() {
        assert_eq!(explain_rows(&doc! { "ok": 1.0 }), None);
        assert_eq!(explain_rows(&doc! { "queryPlanner": { "namespace": "fx.users" } }), None);
        assert_eq!(explain_rows(&doc! { "stages": [] }), None);
        assert_eq!(explain_rows(&doc! { "stages": [{ "$group": { "_id": 1 } }] }), None);
    }

    /// The top-level `executionStats` of a find, singular and plural; none without them.
    #[test]
    fn the_stats_notice_counts_documents_and_keys() {
        let (_, stats) = rows_of(&find_60());
        assert_eq!(stats.as_deref(), Some("Returned 1 document; examined 1 index key and 1 document in 0 ms."));
        let mut reply = find_60();
        reply.insert(
            "executionStats",
            doc! { "nReturned": 6_i64, "executionTimeMillis": 12, "totalKeysExamined": 0, "totalDocsExamined": 6 },
        );
        assert_eq!(
            rows_of(&reply).1.as_deref(),
            Some("Returned 6 documents; examined 0 index keys and 6 documents in 12 ms.")
        );
        reply.remove("executionStats");
        assert_eq!(rows_of(&reply).1, None);
    }

    /// A statement that failed while explain ran it: `ok: 1`, but `executionSuccess: false`
    /// with the error (as 6.0 and 8.0 reply to a `$where` that throws, or to a killOp: 11601);
    /// on 6.0's aggregate under `$cursor`. A reply whose run succeeded, or that has no stats or
    /// no `executionSuccess`, has none.
    #[test]
    fn a_failed_run_is_its_error() {
        let failed = doc! {
            "executionSuccess": false,
            "errorMessage": "Error: boom :\n@:1:21\n",
            "errorCode": 139,
            "nReturned": 0,
            "executionTimeMillis": 12,
            "totalKeysExamined": 0,
            "totalDocsExamined": 1,
            "failed": true,
        };
        let mut reply = find_60();
        reply.insert("executionStats", failed.clone());
        assert_eq!(execution_failure(&reply), Some((Some(139), "Error: boom :\n@:1:21\n".to_string())));
        let cursor = doc! {
            "stages": [
                { "$cursor": { "queryPlanner": { "winningPlan": { "stage": "COLLSCAN" } }, "executionStats": failed } },
                { "$group": { "_id": 1 } },
            ],
            "ok": 1.0,
        };
        assert_eq!(execution_failure(&cursor), Some((Some(139), "Error: boom :\n@:1:21\n".to_string())));
        let no_code = doc! { "executionStats": { "executionSuccess": false, "errorMessage": "x" }, "ok": 1.0 };
        assert_eq!(execution_failure(&no_code), Some((None, "x".to_string())));

        assert_eq!(execution_failure(&find_60()), None);
        let mut no_flag = find_60();
        no_flag.insert("executionStats", doc! { "nReturned": 1 });
        assert_eq!(execution_failure(&no_flag), None);
        let mut planned = find_60();
        planned.remove("executionStats");
        assert_eq!(execution_failure(&planned), None);
    }

    #[test]
    fn explain_command_wraps_the_find_or_the_aggregate() {
        let call = |op: MongoQueryOp| MongoCall { collection: "users".into(), op };
        let comment = Bson::String("ts-run-1".into());
        let find = MongoQueryOp::Find {
            filter: doc! { "a": 1 },
            projection: Some(doc! { "a": 1 }),
            sort: Some(doc! { "a": -1 }),
            skip: Some(2),
            limit: Some(-5),
            one: false,
        };
        let command = explain_command(&call(find.clone()), &find, ExplainVerbosity::ExecutionStats, &comment);
        assert_eq!(
            command,
            Some(doc! {
                "explain": {
                    "find": "users", "filter": { "a": 1 }, "projection": { "a": 1 }, "sort": { "a": -1 },
                    "skip": 2_i64, "limit": 5_i64, "comment": "ts-run-1",
                },
                "verbosity": "executionStats",
                "comment": "ts-run-1",
            })
        );
        let bare = MongoQueryOp::Find { filter: doc! {}, projection: None, sort: None, skip: None, limit: None, one: false };
        let command = explain_command(&call(bare.clone()), &bare, ExplainVerbosity::QueryPlanner, &comment);
        assert_eq!(
            command,
            Some(doc! {
                "explain": { "find": "users", "filter": {}, "comment": "ts-run-1" },
                "verbosity": "queryPlanner",
                "comment": "ts-run-1",
            })
        );
        let aggregate = MongoQueryOp::Aggregate { pipeline: vec![doc! { "$match": { "a": 1 } }] };
        let command = explain_command(&call(aggregate.clone()), &aggregate, ExplainVerbosity::AllPlansExecution, &comment);
        assert_eq!(
            command,
            Some(doc! {
                "explain": { "aggregate": "users", "pipeline": [{ "$match": { "a": 1 } }], "cursor": {}, "comment": "ts-run-1" },
                "verbosity": "allPlansExecution",
                "comment": "ts-run-1",
            })
        );
        let count = MongoQueryOp::CountDocuments { filter: doc! {} };
        assert_eq!(explain_command(&call(count.clone()), &count, ExplainVerbosity::QueryPlanner, &comment), None);
    }
}
