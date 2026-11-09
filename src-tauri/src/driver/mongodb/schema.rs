//! Columns of a collection, inferred from a deterministic sample of its documents.
//!
//! The sample is the first `SAMPLE_EACH_END` documents by `_id` ascending plus the last
//! `SAMPLE_EACH_END` by `_id` descending, without repeats. Never `$sample`: it is random, so
//! the columns would reorder between loads.

use futures_util::TryStreamExt;
use mongodb::bson::{doc, Bson, Document};
use mongodb::Collection;

use super::values::type_name;
use crate::model::{ColumnInfo, Editing};

/// Documents read from each end of the collection.
pub const SAMPLE_EACH_END: i64 = 500;
/// The `data_type` of a column with no value but null in the sample.
pub const NO_TYPE: &str = "null";

/// A field seen in the sample.
struct Field {
    name: String,
    /// Type names in order of first appearance, with how often each was seen.
    types: Vec<(&'static str, usize)>,
    /// Documents where the field is present and not null.
    present: usize,
}

/// The sample of `coll`: the ascending documents, then the descending ones not already read.
pub async fn sample(coll: &Collection<Document>) -> mongodb::error::Result<Vec<Document>> {
    let first: Vec<Document> =
        coll.find(doc! {}).sort(doc! { "_id": 1 }).limit(SAMPLE_EACH_END).await?.try_collect().await?;
    let last: Vec<Document> =
        coll.find(doc! {}).sort(doc! { "_id": -1 }).limit(SAMPLE_EACH_END).await?.try_collect().await?;
    Ok(dedup(first, last))
}

/// The identity of a document in the sample: its `_id`'s BSON bytes (type and value), or the
/// whole document's for one without `_id` (a view may project it away).
fn identity(d: &Document) -> Vec<u8> {
    let key = match d.get("_id") {
        Some(id) => doc! { "_id": id.clone() },
        None => d.clone(),
    };
    key.to_vec().unwrap_or_default()
}

fn dedup(first: Vec<Document>, last: Vec<Document>) -> Vec<Document> {
    let seen: std::collections::HashSet<Vec<u8>> = first.iter().map(identity).collect();
    let mut out = first;
    out.extend(last.into_iter().filter(|d| !seen.contains(&identity(d))));
    out
}

/// The columns of `docs`: `_id` first, then every top-level field in order of first appearance.
///
/// - `data_type` is the most frequent type (ties go to the first seen), or `NO_TYPE`;
/// - `full_type` lists every type seen, in order of first appearance;
/// - `nullable` is true when the field is missing or null in any document;
/// - `read_only` for `_id`, and for a name with a `.` or a leading `$`, which `$set` would write
///   as another field.
///
/// Only `Bson::Null` (and a missing field) counts as no value: an `undefined` value is shown as
/// one (code `x`), so it counts as the type `other`.
pub fn infer(docs: &[Document]) -> Vec<ColumnInfo> {
    let mut fields: Vec<Field> = vec![Field { name: "_id".into(), types: Vec::new(), present: 0 }];
    for d in docs {
        for (name, value) in d {
            let at = match fields.iter().position(|f| &f.name == name) {
                Some(at) => at,
                None => {
                    fields.push(Field { name: name.clone(), types: Vec::new(), present: 0 });
                    fields.len() - 1
                }
            };
            if matches!(value, Bson::Null) {
                continue;
            }
            let f = &mut fields[at];
            f.present += 1;
            let ty = type_name(value);
            match f.types.iter_mut().find(|(t, _)| *t == ty) {
                Some((_, n)) => *n += 1,
                None => f.types.push((ty, 1)),
            }
        }
    }
    fields.into_iter().map(|f| column(f, docs.len())).collect()
}

fn column(f: Field, documents: usize) -> ColumnInfo {
    // `max_by_key` keeps the last of equal maxima; reversed, that is the first seen.
    let dominant = f.types.iter().rev().max_by_key(|(_, n)| *n).map(|(t, _)| *t);
    let data_type = match (dominant, f.name.as_str()) {
        (Some(t), _) => t,
        // An empty collection's documents get ObjectId keys from the driver.
        (None, "_id") => "objectId",
        (None, _) => NO_TYPE,
    };
    let full_type = if f.types.is_empty() {
        data_type.to_string()
    } else {
        f.types.iter().map(|(t, _)| *t).collect::<Vec<_>>().join(", ")
    };
    let is_id = f.name == "_id";
    ColumnInfo {
        read_only: is_id || f.name.contains('.') || f.name.starts_with('$'),
        data_type: data_type.to_string(),
        full_type,
        cast_type: data_type.to_string(),
        nullable: f.present < documents,
        default: None,
        is_primary_key: is_id,
        name: f.name,
    }
}

/// The columns of a page: the sample's, then any top-level field of `page` the sample lacks, in
/// order of first appearance on the page, typed by the page alone. Those are read-only: a save is
/// planned against the sampled columns, which don't have them.
pub fn with_page_fields(mut columns: Vec<ColumnInfo>, page: &[Document]) -> Vec<ColumnInfo> {
    let mut extra: Vec<String> = Vec::new();
    for d in page {
        for name in d.keys() {
            if !columns.iter().any(|c| &c.name == name) && !extra.contains(name) {
                extra.push(name.clone());
            }
        }
    }
    if extra.is_empty() {
        return columns;
    }
    let inferred = infer(page);
    columns.extend(
        inferred.into_iter().filter(|c| extra.contains(&c.name)).map(|c| ColumnInfo { read_only: true, ..c }),
    );
    columns
}

/// The most segments a field path has.
pub const FIELD_PATH_DEPTH: usize = 4;
/// The most field paths offered for a collection.
pub const MAX_FIELD_PATHS: usize = 500;

/// A field in the merged tree of the sample's documents.
struct PathNode {
    name: String,
    children: Vec<PathNode>,
}

/// The field paths of `docs`, for completion: every field, dot-joined with the documents it is in
/// (`address.city`), also through arrays of documents (`items.sku`), at most `FIELD_PATH_DEPTH`
/// segments deep.
///
/// The documents are merged into one tree, each field's children in order of first appearance,
/// and the paths are its pre-order: a field comes right after its parent. A name that is empty,
/// has a `.` or starts with `$` can't be written as a path, so it is left out with its subtree.
/// At most `MAX_FIELD_PATHS`: fields seen after that many are left out.
pub fn field_paths(docs: &[Document]) -> Vec<String> {
    let mut roots: Vec<PathNode> = Vec::new();
    let mut count = 0;
    for d in docs {
        merge(&mut roots, d, 1, &mut count);
    }
    let mut out = Vec::with_capacity(count);
    for n in &roots {
        flatten(n, "", &mut out);
    }
    out
}

fn merge(nodes: &mut Vec<PathNode>, d: &Document, depth: usize, count: &mut usize) {
    for (name, value) in d {
        if name.is_empty() || name.contains('.') || name.starts_with('$') {
            continue;
        }
        let at = match nodes.iter().position(|n| &n.name == name) {
            Some(at) => at,
            None if *count < MAX_FIELD_PATHS => {
                *count += 1;
                nodes.push(PathNode { name: name.clone(), children: Vec::new() });
                nodes.len() - 1
            }
            None => continue,
        };
        if depth == FIELD_PATH_DEPTH {
            continue;
        }
        let children = &mut nodes[at].children;
        match value {
            Bson::Document(inner) => merge(children, inner, depth + 1, count),
            Bson::Array(items) => {
                for item in items {
                    if let Bson::Document(inner) = item {
                        merge(children, inner, depth + 1, count);
                    }
                }
            }
            _ => {}
        }
    }
}

fn flatten(n: &PathNode, prefix: &str, out: &mut Vec<String>) {
    let path = if prefix.is_empty() { n.name.clone() } else { format!("{prefix}.{}", n.name) };
    out.push(path.clone());
    for c in &n.children {
        flatten(c, &path, out);
    }
}

/// Editing for a collection (a view or time-series collection is `Editing::View`, decided by the
/// caller): `FloatKey` when a sampled `_id` is a double, as on SQL.
pub fn editing(docs: &[Document]) -> Editing {
    if docs.iter().any(|d| matches!(d.get("_id"), Some(Bson::Double(_)))) {
        Editing::FloatKey
    } else {
        Editing::Allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(cols: &[ColumnInfo]) -> Vec<&str> {
        cols.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn infer_orders_id_first_then_first_appearance() {
        let docs = [doc! { "b": 1, "_id": 1, "a": 2 }, doc! { "_id": 2, "c": 3, "a": 1 }];
        assert_eq!(names(&infer(&docs)), ["_id", "b", "a", "c"]);
        assert!(infer(&docs)[0].is_primary_key && infer(&docs)[0].read_only);
        assert!(!infer(&docs)[1].is_primary_key);
    }

    #[test]
    fn infer_types_dominant_full_and_nullable() {
        let docs = [
            doc! { "_id": 1, "age": 30, "x": Bson::Null },
            doc! { "_id": 2, "age": 30_i64 },
            doc! { "_id": 3, "age": 30.0 },
            doc! { "_id": 4, "age": "30" },
            doc! { "_id": 5, "age": 41 },
            doc! { "_id": 6, "age": Bson::Null },
        ];
        let cols = infer(&docs);
        let age = &cols[1];
        assert_eq!(age.data_type, "int32");
        assert_eq!(age.cast_type, "int32");
        assert_eq!(age.full_type, "int32, int64, double, string");
        assert!(age.nullable);
        let id = &cols[0];
        assert_eq!((id.data_type.as_str(), id.full_type.as_str(), id.nullable), ("int32", "int32", false));
        // Null is no type (`type_name(Null)` would say "other").
        let x = &cols[2];
        assert_eq!((x.data_type.as_str(), x.full_type.as_str(), x.nullable), (NO_TYPE, NO_TYPE, true));
    }

    #[test]
    fn infer_ties_go_to_the_first_seen() {
        let docs = [doc! { "_id": 1, "s": 1.5 }, doc! { "_id": 2, "s": 2 }];
        assert_eq!(infer(&docs)[1].data_type, "double");
        assert_eq!(infer(&docs)[1].full_type, "double, int32");
        let docs = [doc! { "_id": 1, "s": 2 }, doc! { "_id": 2, "s": 1.5 }, doc! { "_id": 3, "s": 2.5 }];
        assert_eq!(infer(&docs)[1].data_type, "double");
    }

    #[test]
    fn infer_undefined_is_a_value_of_type_other() {
        let docs = [doc! { "_id": 1, "u": Bson::Undefined }];
        let u = &infer(&docs)[1];
        assert_eq!((u.data_type.as_str(), u.nullable), ("other", false));
    }

    #[test]
    fn infer_marks_dotted_and_dollar_names_read_only() {
        let mut d = doc! { "_id": 1, "a.b": 5, "plain": 1 };
        d.insert("$c", 7);
        let cols = infer(&[d]);
        let ro: Vec<&str> = cols.iter().filter(|c| c.read_only).map(|c| c.name.as_str()).collect();
        assert_eq!(ro, ["_id", "a.b", "$c"]);
    }

    #[test]
    fn infer_of_nothing_is_an_object_id_key() {
        let cols = infer(&[]);
        assert_eq!(names(&cols), ["_id"]);
        assert_eq!(cols[0].data_type, "objectId");
        assert!(!cols[0].nullable);
    }

    #[test]
    fn infer_without_id_has_a_nullable_id() {
        let cols = infer(&[doc! { "a": 1 }]);
        assert_eq!(names(&cols), ["_id", "a"]);
        assert!(cols[0].nullable);
    }

    #[test]
    fn dedup_keeps_the_ascending_order_then_new_descending_ones() {
        let first = vec![doc! { "_id": 1 }, doc! { "_id": 2 }];
        let last = vec![doc! { "_id": 3 }, doc! { "_id": 2 }, doc! { "_id": 2_i64 }];
        // Int64 2 is another key than Int32 2.
        assert_eq!(dedup(first, last), [doc! { "_id": 1 }, doc! { "_id": 2 }, doc! { "_id": 3 }, doc! { "_id": 2_i64 }]);
    }

    #[test]
    fn page_fields_missing_from_the_sample_are_appended() {
        let sample = infer(&[doc! { "_id": 1, "a": 1 }]);
        let page = [doc! { "_id": 7, "z": "x", "a": 2 }, doc! { "_id": 8, "y": true, "z": "w" }];
        let cols = with_page_fields(sample.clone(), &page);
        assert_eq!(names(&cols), ["_id", "a", "z", "y"]);
        assert_eq!(cols[2].data_type, "string");
        assert!(!cols[2].nullable);
        assert_eq!(cols[3].data_type, "bool");
        assert!(cols[3].nullable);
        // Fields outside the sample can't be edited: the grid blocks the edit.
        let read_only: Vec<bool> = cols.iter().map(|c| c.read_only).collect();
        assert_eq!(read_only, [true, false, true, true]);
        // A page with nothing new keeps the sample's columns as they are.
        assert_eq!(with_page_fields(sample.clone(), &[doc! { "_id": 2, "a": "s" }]), sample);
    }

    #[test]
    fn field_paths_walk_documents_and_arrays_of_documents() {
        let mut d = doc! {
            "_id": 1,
            "name": "Ann",
            "address": { "city": "Oslo", "zip": "0150" },
            "items": [{ "sku": "a" }, 3],
            "a.b": 1,
        };
        d.insert("$x", doc! { "y": 1 });
        assert_eq!(field_paths(&[d]), ["_id", "name", "address", "address.city", "address.zip", "items", "items.sku"]);
    }

    #[test]
    fn field_paths_are_pre_order_by_first_appearance() {
        let docs = [doc! { "a": { "x": 1 } }, doc! { "b": 1, "a": { "y": [{ "z": 1 }], "x": 2 } }];
        assert_eq!(field_paths(&docs), ["a", "a.x", "a.y", "a.y.z", "b"]);
    }

    #[test]
    fn field_paths_skip_empty_names_with_their_subtree() {
        let docs = [doc! { "": { "k": 1 }, "p": { "": 1, "q": 1 } }];
        assert_eq!(field_paths(&docs), ["p", "p.q"]);
    }

    #[test]
    fn field_paths_stop_at_four_segments() {
        let docs = [doc! { "a": { "b": { "c": { "d": { "e": 1 } } } } }];
        assert_eq!(field_paths(&docs), ["a", "a.b", "a.b.c", "a.b.c.d"]);
    }

    #[test]
    fn field_paths_stop_at_500() {
        let mut d = Document::new();
        for i in 0..600 {
            d.insert(format!("f{i}"), i);
        }
        let paths = field_paths(&[d]);
        assert_eq!(paths.len(), 500);
        assert_eq!((paths[0].as_str(), paths[499].as_str()), ("f0", "f499"));
        assert!(field_paths(&[]).is_empty());
    }

    #[test]
    fn a_double_key_is_a_float_key() {
        assert_eq!(editing(&[doc! { "_id": 1 }, doc! { "_id": 2.5 }]), Editing::FloatKey);
        assert_eq!(editing(&[doc! { "_id": 1 }]), Editing::Allowed);
        assert_eq!(editing(&[]), Editing::Allowed);
    }
}
