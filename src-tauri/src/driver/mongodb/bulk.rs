//! A query tab's `bulkWrite`, the pure parts: its operations grouped into the server's write
//! commands, and the counts of their replies.
//!
//! As mongosh's legacy bulk API, each run of consecutive operations of one command is one
//! `insert`, `update` (replaces included) or `delete`; an unordered bulk keeps the groups in
//! order. The session side, which sends them, is `query.rs`'s.

use mongodb::bson::{doc, Bson, Document};

use super::query::with_id;
use crate::mongoquery::{BulkOp, UpdateSpec};

/// The write command a group of operations is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkKind {
    Insert,
    Update,
    Delete,
}

/// One write command's body without its collection: its kind and its items (the documents of
/// an `insert`, the `{ q, u, upsert, multi }` of an `update`, the `{ q, limit }` of a `delete`).
#[derive(Debug, Clone, PartialEq)]
pub struct BulkCommand {
    pub kind: BulkKind,
    pub items: Vec<Document>,
}

impl BulkCommand {
    /// The command to send: `{ insert | update | delete: collection, <items>, ordered, comment }`.
    pub fn command(&self, collection: &str, ordered: bool, comment: &Bson) -> Document {
        let (name, field) = match self.kind {
            BulkKind::Insert => ("insert", "documents"),
            BulkKind::Update => ("update", "updates"),
            BulkKind::Delete => ("delete", "deletes"),
        };
        doc! { name: collection, field: self.items.clone(), "ordered": ordered, "comment": comment.clone() }
    }
}

/// The groups of `ops`: each run of consecutive operations of one command, with the index of
/// its first operation. Inserts get an `ObjectId` `_id` first when they have none (`with_id`).
pub fn bulk_batches(ops: &[BulkOp]) -> Vec<(usize, BulkCommand)> {
    let mut batches: Vec<(usize, BulkCommand)> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let (kind, item) = match op {
            BulkOp::Insert(d) => (BulkKind::Insert, with_id(d.clone())),
            BulkOp::Update { filter, update, upsert, many } => {
                let u: Bson = match update {
                    UpdateSpec::Modifiers(m) => m.clone().into(),
                    UpdateSpec::Pipeline(p) => p.clone().into(),
                };
                (BulkKind::Update, doc! { "q": filter.clone(), "u": u, "upsert": *upsert, "multi": *many })
            }
            BulkOp::Replace { filter, replacement, upsert } => (
                BulkKind::Update,
                doc! { "q": filter.clone(), "u": replacement.clone(), "upsert": *upsert, "multi": false },
            ),
            // `limit: 0` deletes every match.
            BulkOp::Delete { filter, many } => {
                (BulkKind::Delete, doc! { "q": filter.clone(), "limit": if *many { 0 } else { 1 } })
            }
        };
        match batches.last_mut() {
            Some((_, batch)) if batch.kind == kind => batch.items.push(item),
            _ => batches.push((i, BulkCommand { kind, items: vec![item] })),
        }
    }
    batches
}

/// What the replies of a bulk's commands counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BulkTally {
    pub inserted: u64,
    pub matched: u64,
    pub modified: u64,
    pub upserted: u64,
    pub deleted: u64,
}

/// A command's reply the app couldn't read: a count missing or not one, or write errors it can't
/// make out. The caller logs the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnreadableReply;

impl BulkTally {
    /// Adds the counts of a `kind` command's reply. An update's `n` counts its upserts too
    ///, so matched is `n` minus them. A reply without `n` (or an update's without
    /// `nModified`, or with an `upserted` that isn't an array) is unreadable, and adds nothing.
    pub fn add(&mut self, kind: BulkKind, reply: &Document) -> Result<(), UnreadableReply> {
        let n = number(reply, "n").ok_or(UnreadableReply)?;
        match kind {
            BulkKind::Insert => self.inserted += n,
            BulkKind::Update => {
                let modified = number(reply, "nModified").ok_or(UnreadableReply)?;
                let upserted = match reply.get("upserted") {
                    None => 0,
                    Some(Bson::Array(u)) => u.len() as u64,
                    Some(_) => return Err(UnreadableReply),
                };
                self.upserted += upserted;
                self.matched += n.saturating_sub(upserted);
                self.modified += modified;
            }
            BulkKind::Delete => self.deleted += n,
        }
        Ok(())
    }

    /// The documents the bulk changed: inserted, modified, upserted and deleted.
    pub fn rows_affected(&self) -> u64 {
        self.inserted + self.modified + self.upserted + self.deleted
    }

    /// `Inserted <i>, matched <m>, modified <mod>, upserted <u>, deleted <d>.`, all five always.
    pub fn notice(&self) -> String {
        let BulkTally { inserted, matched, modified, upserted, deleted } = self;
        format!("Inserted {inserted}, matched {matched}, modified {modified}, upserted {upserted}, deleted {deleted}.")
    }
}

/// A write error of a command's reply, at its operation's index in the whole bulk.
#[derive(Debug, Clone, PartialEq)]
pub struct BulkWriteError {
    pub index: usize,
    pub code: i32,
    pub message: String,
    /// `errInfo`: a validator's details (code 121).
    pub details: Option<Document>,
}

/// The `writeErrors` of the reply of the group that starts at `start`, by operation order; none
/// when the reply has none. A `writeErrors` that isn't an array, or an entry that isn't a
/// document or lacks its `index` or `code`, is unreadable. A missing `errmsg` is empty.
pub fn write_errors(reply: &Document, start: usize) -> Result<Vec<BulkWriteError>, UnreadableReply> {
    let errors = match reply.get("writeErrors") {
        None => return Ok(vec![]),
        Some(Bson::Array(errors)) => errors,
        Some(_) => return Err(UnreadableReply),
    };
    let mut errors = errors
        .iter()
        .map(|e| {
            let e = e.as_document().ok_or(UnreadableReply)?;
            let index = number(e, "index").and_then(|i| usize::try_from(i).ok()).ok_or(UnreadableReply)?;
            let code = number(e, "code").and_then(|c| i32::try_from(c).ok()).ok_or(UnreadableReply)?;
            Ok(BulkWriteError {
                index: start + index,
                code,
                message: e.get_str("errmsg").unwrap_or_default().to_string(),
                details: e.get_document("errInfo").ok().cloned(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    errors.sort_by_key(|e| e.index);
    Ok(errors)
}

/// `key` of a reply as a non-negative integer; `None` when it is missing or isn't one.
fn number(doc: &Document, key: &str) -> Option<u64> {
    match doc.get(key)? {
        Bson::Int32(n) => u64::try_from(*n).ok(),
        Bson::Int64(n) => u64::try_from(*n).ok(),
        Bson::Double(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use mongodb::bson::oid::ObjectId;

    use super::*;

    /// Consecutive operations of one command are one group; a replace is an update.
    #[test]
    fn bulk_batches_groups_consecutive_commands() {
        let ops = [
            BulkOp::Insert(doc! { "a": 1 }),
            BulkOp::Insert(doc! { "_id": 7, "b": 2 }),
            BulkOp::Update {
                filter: doc! { "a": 1 },
                update: UpdateSpec::Modifiers(doc! { "$set": { "a": 2 } }),
                upsert: true,
                many: true,
            },
            BulkOp::Replace { filter: doc! { "_id": 7 }, replacement: doc! { "b": 3 }, upsert: false },
            BulkOp::Delete { filter: doc! { "a": 2 }, many: false },
            BulkOp::Insert(doc! { "_id": 8 }),
        ];
        let batches = bulk_batches(&ops);
        let starts: Vec<usize> = batches.iter().map(|(start, _)| *start).collect();
        assert_eq!(starts, [0, 2, 4, 5]);
        let kinds: Vec<BulkKind> = batches.iter().map(|(_, b)| b.kind).collect();
        assert_eq!(kinds, [BulkKind::Insert, BulkKind::Update, BulkKind::Delete, BulkKind::Insert]);

        // `with_id`: an ObjectId first when there is none, the document's own kept.
        let inserts = &batches[0].1.items;
        assert_eq!(inserts.len(), 2);
        let first: Vec<&str> = inserts[0].keys().map(String::as_str).collect();
        assert_eq!(first, ["_id", "a"]);
        assert!(matches!(inserts[0].get("_id"), Some(Bson::ObjectId(_))), "{}", inserts[0]);
        assert_eq!(inserts[1], doc! { "_id": 7, "b": 2 });

        assert_eq!(
            batches[1].1.items,
            [
                doc! { "q": { "a": 1 }, "u": { "$set": { "a": 2 } }, "upsert": true, "multi": true },
                doc! { "q": { "_id": 7 }, "u": { "b": 3 }, "upsert": false, "multi": false },
            ]
        );
        assert_eq!(batches[2].1.items, [doc! { "q": { "a": 2 }, "limit": 1 }]);
        assert_eq!(batches[3].1.items, [doc! { "_id": 8 }]);

        // updateOne with a pipeline, deleteMany.
        let more = [
            BulkOp::Update {
                filter: doc! {},
                update: UpdateSpec::Pipeline(vec![doc! { "$set": { "x": 1 } }]),
                upsert: false,
                many: false,
            },
            BulkOp::Delete { filter: doc! {}, many: true },
        ];
        let batches = bulk_batches(&more);
        assert_eq!(
            batches,
            [
                (
                    0,
                    BulkCommand {
                        kind: BulkKind::Update,
                        items: vec![doc! { "q": {}, "u": [{ "$set": { "x": 1 } }], "upsert": false, "multi": false }],
                    }
                ),
                (1, BulkCommand { kind: BulkKind::Delete, items: vec![doc! { "q": {}, "limit": 0 }] }),
            ]
        );
        assert!(bulk_batches(&[]).is_empty());
    }

    #[test]
    fn bulk_command_names_the_collection() {
        let comment = Bson::String("ts-run-x".into());
        let items = vec![doc! { "_id": ObjectId::parse_str("650000000000000000000001").unwrap() }];
        let insert = BulkCommand { kind: BulkKind::Insert, items: items.clone() };
        assert_eq!(
            insert.command("c", true, &comment),
            doc! { "insert": "c", "documents": items, "ordered": true, "comment": "ts-run-x" }
        );
        let update = BulkCommand { kind: BulkKind::Update, items: vec![doc! { "q": {} }] };
        assert_eq!(
            update.command("c", false, &comment),
            doc! { "update": "c", "updates": [{ "q": {} }], "ordered": false, "comment": "ts-run-x" }
        );
        let delete = BulkCommand { kind: BulkKind::Delete, items: vec![doc! { "q": {} }] };
        assert_eq!(
            delete.command("c", true, &comment),
            doc! { "delete": "c", "deletes": [{ "q": {} }], "ordered": true, "comment": "ts-run-x" }
        );
    }

    /// Fact 94's replies: an update's `n` counts its upserts.
    #[test]
    fn bulk_tally_reads_the_replies() {
        let mut tally = BulkTally::default();
        let id = ObjectId::new();
        tally.add(BulkKind::Update, &doc! { "n": 5, "nModified": 4, "upserted": [{ "index": 1, "_id": id }], "ok": 1.0 }).unwrap();
        assert_eq!(tally, BulkTally { matched: 4, modified: 4, upserted: 1, ..BulkTally::default() });
        tally.add(BulkKind::Insert, &doc! { "n": 2, "ok": 1.0 }).unwrap();
        tally.add(BulkKind::Delete, &doc! { "n": 3_i64, "ok": 1.0 }).unwrap();
        // An update that matched nothing has no `upserted`.
        tally.add(BulkKind::Update, &doc! { "n": 0, "nModified": 0, "ok": 1.0 }).unwrap();
        assert_eq!(tally, BulkTally { inserted: 2, matched: 4, modified: 4, upserted: 1, deleted: 3 });
        assert_eq!(tally.rows_affected(), 2 + 4 + 1 + 3);
        assert_eq!(tally.notice(), "Inserted 2, matched 4, modified 4, upserted 1, deleted 3.");
        assert_eq!(BulkTally::default().notice(), "Inserted 0, matched 0, modified 0, upserted 0, deleted 0.");
    }

    #[test]
    fn write_errors_are_at_their_bulk_index() {
        let reply = doc! {
            "n": 1,
            "writeErrors": [
                { "index": 2, "code": 11000, "errmsg": "E11000 b" },
                { "index": 0, "code": 121, "errmsg": "Document failed validation", "errInfo": { "x": 1 } },
            ],
            "ok": 1.0,
        };
        assert_eq!(
            write_errors(&reply, 3).unwrap(),
            [
                BulkWriteError { index: 3, code: 121, message: "Document failed validation".into(), details: Some(doc! { "x": 1 }) },
                BulkWriteError { index: 5, code: 11000, message: "E11000 b".into(), details: None },
            ]
        );
        assert!(write_errors(&doc! { "n": 1, "ok": 1.0 }, 0).unwrap().is_empty());
    }

    /// A reply without its counts, or with write errors the app can't read, is unreadable, not
    /// zeros or no errors; the tally is left as it was. A missing `errmsg` is an empty message.
    #[test]
    fn an_unreadable_reply_is_an_error() {
        let mut tally = BulkTally { inserted: 1, ..BulkTally::default() };
        for (kind, reply) in [
            (BulkKind::Insert, doc! { "ok": 1.0 }),
            (BulkKind::Insert, doc! { "n": "2", "ok": 1.0 }),
            (BulkKind::Delete, doc! { "n": -1, "ok": 1.0 }),
            (BulkKind::Update, doc! { "n": 2, "ok": 1.0 }),
            (BulkKind::Update, doc! { "nModified": 2, "ok": 1.0 }),
            (BulkKind::Update, doc! { "n": 2, "nModified": 1, "upserted": 1, "ok": 1.0 }),
        ] {
            assert_eq!(tally.add(kind, &reply), Err(UnreadableReply), "{kind:?} {reply}");
        }
        assert_eq!(tally, BulkTally { inserted: 1, ..BulkTally::default() });

        for reply in [
            doc! { "n": 0, "writeErrors": { "index": 0, "code": 11000 } },
            doc! { "n": 0, "writeErrors": [5] },
            doc! { "n": 0, "writeErrors": [{ "code": 11000, "errmsg": "E11000" }] },
            doc! { "n": 0, "writeErrors": [{ "index": 0, "errmsg": "E11000" }] },
            doc! { "n": 0, "writeErrors": [{ "index": "0", "code": 11000 }] },
            doc! { "n": 0, "writeErrors": [{ "index": 0, "code": 11000 }, { "index": 1 }] },
        ] {
            assert_eq!(write_errors(&reply, 0), Err(UnreadableReply), "{reply}");
        }
        assert_eq!(
            write_errors(&doc! { "n": 0, "writeErrors": [{ "index": 1, "code": 11000 }] }, 2),
            Ok(vec![BulkWriteError { index: 3, code: 11000, message: String::new(), details: None }])
        );
    }
}
