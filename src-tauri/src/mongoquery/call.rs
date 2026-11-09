//! One statement of a MongoDB query tab: `db.<collection>.<method>(args)`, parsed into a call,
//! and classified as a read or a write from its tokens alone.

use mongodb::bson::{Bson, Document};

use super::ParseError;
use super::lex::{self, Tok, Token};
use super::literal::Parser;
use crate::model::ParamValues;
use crate::sqltext::StatementKind;

/// A parsed statement: the collection and what to do with it.
#[derive(Debug, Clone, PartialEq)]
pub struct MongoCall {
    pub collection: String,
    pub op: MongoQueryOp,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MongoQueryOp {
    Find {
        filter: Document,
        projection: Option<Document>,
        sort: Option<Document>,
        skip: Option<u64>,
        limit: Option<i64>,
        one: bool,
    },
    Aggregate {
        pipeline: Vec<Document>,
    },
    CountDocuments {
        filter: Document,
    },
    EstimatedDocumentCount,
    Distinct {
        field: String,
        filter: Document,
    },
    InsertOne {
        doc: Document,
    },
    InsertMany {
        docs: Vec<Document>,
        ordered: bool,
    },
    Update {
        filter: Document,
        update: UpdateSpec,
        upsert: bool,
        many: bool,
    },
    ReplaceOne {
        filter: Document,
        replacement: Document,
        upsert: bool,
    },
    Delete {
        filter: Document,
        many: bool,
    },
    /// `findOneAndUpdate`, `findOneAndReplace` and `findOneAndDelete`. `upsert` and
    /// `return_after` are always false for a delete.
    FindOneAnd {
        filter: Document,
        action: FindAndAction,
        projection: Option<Document>,
        sort: Option<Document>,
        upsert: bool,
        /// The document after the change (`returnDocument: "after"`), not the one before it.
        return_after: bool,
    },
    BulkWrite {
        ops: Vec<BulkOp>,
        ordered: bool,
    },
    Drop,
    /// `db.createCollection(name, options)`: the call's collection is `name`; `options` go to
    /// `create` as written (`parse` refuses a `create` among them).
    CreateCollection {
        options: Document,
    },
    /// `name` is the `name` option, or the generated one (`index_name`); `options` are the rest,
    /// as written (`parse` refuses a `key` among them).
    CreateIndex {
        keys: Document,
        name: String,
        options: Document,
    },
    /// An index name (a string other than `"*"`) or a keys document.
    DropIndex {
        index: Bson,
    },
    GetIndexes,
    RenameCollection {
        to: String,
        drop_target: bool,
    },
    /// `.explain(verbosity)` after `find(…)` (and its chain) or `aggregate(…)` without an
    /// output stage: always a read.
    Explain {
        of: Box<MongoQueryOp>,
        verbosity: ExplainVerbosity,
    },
}

/// What a `findOneAnd*` does to the document it finds.
#[derive(Debug, Clone, PartialEq)]
pub enum FindAndAction {
    Update(UpdateSpec),
    Replace(Document),
    Delete,
}

/// One operation of a `bulkWrite`.
#[derive(Debug, Clone, PartialEq)]
pub enum BulkOp {
    Insert(Document),
    Update {
        filter: Document,
        update: UpdateSpec,
        upsert: bool,
        many: bool,
    },
    Replace {
        filter: Document,
        replacement: Document,
        upsert: bool,
    },
    Delete {
        filter: Document,
        many: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainVerbosity {
    QueryPlanner,
    ExecutionStats,
    AllPlansExecution,
}

impl ExplainVerbosity {
    const ALL: [Self; 3] = [
        Self::QueryPlanner,
        Self::ExecutionStats,
        Self::AllPlansExecution,
    ];

    /// The server's name of the verbosity.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::QueryPlanner => "queryPlanner",
            Self::ExecutionStats => "executionStats",
            Self::AllPlansExecution => "allPlansExecution",
        }
    }
}

/// mongosh's name for an index on `keys`: `<field>_<value>` pairs joined by `_`, as in
/// `a_1_b_-1` or `t_text`.
pub fn index_name(keys: &Document) -> String {
    keys.iter()
        .map(|(field, value)| {
            let value = match value {
                Bson::String(s) => s.clone(),
                Bson::Int32(n) => n.to_string(),
                Bson::Int64(n) => n.to_string(),
                // As JavaScript prints a number: `1`, not `1.0`.
                Bson::Double(f) if f.fract() == 0.0 && f.abs() < i64::MAX as f64 => {
                    (*f as i64).to_string()
                }
                Bson::Double(f) => f.to_string(),
                other => other.to_string(),
            };
            format!("{field}_{value}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

/// An update's second argument: a document of `$` operators, or an aggregation pipeline.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateSpec {
    Modifiers(Document),
    Pipeline(Vec<Document>),
}

/// The methods a query tab runs, in the order messages list them.
const METHODS: [&str; 22] = [
    "find",
    "findOne",
    "aggregate",
    "countDocuments",
    "estimatedDocumentCount",
    "distinct",
    "insertOne",
    "insertMany",
    "updateOne",
    "updateMany",
    "replaceOne",
    "deleteOne",
    "deleteMany",
    "findOneAndUpdate",
    "findOneAndReplace",
    "findOneAndDelete",
    "bulkWrite",
    "drop",
    "createIndex",
    "dropIndex",
    "getIndexes",
    "renameCollection",
];

const WRITES: [&str; 15] = [
    "insertOne",
    "insertMany",
    "updateOne",
    "updateMany",
    "replaceOne",
    "deleteOne",
    "deleteMany",
    "findOneAndUpdate",
    "findOneAndReplace",
    "findOneAndDelete",
    "bulkWrite",
    "drop",
    "createIndex",
    "dropIndex",
    "renameCollection",
];

/// Refused by `parse` on a collection, but they change data, so the editor treats them as
/// writes: the database methods, and mongosh's legacy write methods.
const OTHER_WRITES: [&str; 7] = [
    "createCollection",
    "dropDatabase",
    "remove",
    "insert",
    "update",
    "save",
    "findAndModify",
];

/// The database-level calls (`db.<name>(…)`) that change data; any other is a read.
const DATABASE_WRITES: [&str; 3] = ["createCollection", "dropDatabase", "runCommand"];

/// What may follow `find(…)`.
const CHAIN: [&str; 6] = ["sort", "skip", "limit", "pretty", "toArray", "explain"];

const FIND_AND_MODIFY_OPTIONS: [&str; 5] = [
    "projection",
    "sort",
    "upsert",
    "returnDocument",
    "returnNewDocument",
];

const OUTPUT_STAGES: [&str; 2] = ["$out", "$merge"];

const ORDINALS: [&str; 3] = ["first", "second", "third"];

impl MongoCall {
    /// The method's name, as written in the statement.
    pub fn verb(&self) -> &'static str {
        match &self.op {
            MongoQueryOp::Find { one: false, .. } => "find",
            MongoQueryOp::Find { one: true, .. } => "findOne",
            MongoQueryOp::Aggregate { .. } => "aggregate",
            MongoQueryOp::CountDocuments { .. } => "countDocuments",
            MongoQueryOp::EstimatedDocumentCount => "estimatedDocumentCount",
            MongoQueryOp::Distinct { .. } => "distinct",
            MongoQueryOp::InsertOne { .. } => "insertOne",
            MongoQueryOp::InsertMany { .. } => "insertMany",
            MongoQueryOp::Update { many: false, .. } => "updateOne",
            MongoQueryOp::Update { many: true, .. } => "updateMany",
            MongoQueryOp::ReplaceOne { .. } => "replaceOne",
            MongoQueryOp::Delete { many: false, .. } => "deleteOne",
            MongoQueryOp::Delete { many: true, .. } => "deleteMany",
            MongoQueryOp::FindOneAnd { action, .. } => match action {
                FindAndAction::Update(_) => "findOneAndUpdate",
                FindAndAction::Replace(_) => "findOneAndReplace",
                FindAndAction::Delete => "findOneAndDelete",
            },
            MongoQueryOp::BulkWrite { .. } => "bulkWrite",
            MongoQueryOp::Drop => "drop",
            MongoQueryOp::CreateCollection { .. } => "createCollection",
            MongoQueryOp::CreateIndex { .. } => "createIndex",
            MongoQueryOp::DropIndex { .. } => "dropIndex",
            MongoQueryOp::GetIndexes => "getIndexes",
            MongoQueryOp::RenameCollection { .. } => "renameCollection",
            MongoQueryOp::Explain { .. } => "explain",
        }
    }

    /// Write for the write methods and for a pipeline with a `$out` or `$merge` stage anywhere.
    /// An explain is a read: `parse` refuses one of an output pipeline.
    pub fn kind(&self) -> StatementKind {
        match &self.op {
            MongoQueryOp::Aggregate { pipeline } if pipeline.iter().any(is_output_stage) => {
                StatementKind::Write
            }
            MongoQueryOp::Find { .. }
            | MongoQueryOp::Aggregate { .. }
            | MongoQueryOp::CountDocuments { .. }
            | MongoQueryOp::EstimatedDocumentCount
            | MongoQueryOp::Distinct { .. }
            | MongoQueryOp::GetIndexes
            | MongoQueryOp::Explain { .. } => StatementKind::Read,
            MongoQueryOp::InsertOne { .. }
            | MongoQueryOp::InsertMany { .. }
            | MongoQueryOp::Update { .. }
            | MongoQueryOp::ReplaceOne { .. }
            | MongoQueryOp::Delete { .. }
            | MongoQueryOp::FindOneAnd { .. }
            | MongoQueryOp::BulkWrite { .. }
            | MongoQueryOp::Drop
            | MongoQueryOp::CreateCollection { .. }
            | MongoQueryOp::CreateIndex { .. }
            | MongoQueryOp::DropIndex { .. }
            | MongoQueryOp::RenameCollection { .. } => StatementKind::Write,
        }
    }

    /// Whether a successful run counts changed documents (`Outcome::Affected`): the write
    /// methods and `bulkWrite` do; a pipeline with an output stage, a `findOneAnd*` (which
    /// returns the document) and the collection and index helpers report no count.
    pub fn reports_affected(&self) -> bool {
        match &self.op {
            MongoQueryOp::InsertOne { .. }
            | MongoQueryOp::InsertMany { .. }
            | MongoQueryOp::Update { .. }
            | MongoQueryOp::ReplaceOne { .. }
            | MongoQueryOp::Delete { .. }
            | MongoQueryOp::BulkWrite { .. } => true,
            MongoQueryOp::Find { .. }
            | MongoQueryOp::Aggregate { .. }
            | MongoQueryOp::CountDocuments { .. }
            | MongoQueryOp::EstimatedDocumentCount
            | MongoQueryOp::Distinct { .. }
            | MongoQueryOp::FindOneAnd { .. }
            | MongoQueryOp::Drop
            | MongoQueryOp::CreateCollection { .. }
            | MongoQueryOp::CreateIndex { .. }
            | MongoQueryOp::DropIndex { .. }
            | MongoQueryOp::GetIndexes
            | MongoQueryOp::RenameCollection { .. }
            | MongoQueryOp::Explain { .. } => false,
        }
    }

    /// Whether an export may run the call again for every row: `find`, `distinct` and a
    /// pipeline without an output stage (running anything else again would repeat a write).
    pub fn is_exportable(&self) -> bool {
        match &self.op {
            MongoQueryOp::Find { .. } | MongoQueryOp::Distinct { .. } => true,
            MongoQueryOp::Aggregate { .. } => self.kind() == StatementKind::Read,
            MongoQueryOp::CountDocuments { .. }
            | MongoQueryOp::EstimatedDocumentCount
            | MongoQueryOp::InsertOne { .. }
            | MongoQueryOp::InsertMany { .. }
            | MongoQueryOp::Update { .. }
            | MongoQueryOp::ReplaceOne { .. }
            | MongoQueryOp::Delete { .. }
            | MongoQueryOp::FindOneAnd { .. }
            | MongoQueryOp::BulkWrite { .. }
            | MongoQueryOp::Drop
            | MongoQueryOp::CreateCollection { .. }
            | MongoQueryOp::CreateIndex { .. }
            | MongoQueryOp::DropIndex { .. }
            | MongoQueryOp::GetIndexes
            | MongoQueryOp::RenameCollection { .. }
            | MongoQueryOp::Explain { .. } => false,
        }
    }

    /// The pipeline's last stage when it is `$out` or `$merge`.
    pub fn output_stage(&self) -> Option<&Document> {
        match &self.op {
            MongoQueryOp::Aggregate { pipeline } => pipeline.last().filter(|s| is_output_stage(s)),
            _ => None,
        }
    }
}

fn is_output_stage(stage: &Document) -> bool {
    OUTPUT_STAGES.iter().any(|k| stage.contains_key(k))
}

/// The kind and the method name of a statement, from its tokens, so text that doesn't parse
/// still classifies. The verb is `""` when no method follows the collection.
pub fn classify(stmt: &str) -> (StatementKind, String) {
    classify_tokens(&lex::tokenize_lenient(stmt).0)
}

pub(crate) fn classify_tokens(tokens: &[Token]) -> (StatementKind, String) {
    let toks: Vec<&Tok> = tokens
        .iter()
        .map(|t| &t.kind)
        .filter(|k| **k != Tok::Newline)
        .collect();
    if let Some(name) = database_method(&toks) {
        let kind = if DATABASE_WRITES.contains(&name) {
            StatementKind::Write
        } else {
            StatementKind::Read
        };
        return (kind, name.to_string());
    }
    let Some(verb) = method_name(&toks) else {
        return (StatementKind::Read, String::new());
    };
    // As `parse` gives it: whatever it explains, an explain is a read.
    if last_call(&toks) == Some("explain") {
        return (StatementKind::Read, "explain".into());
    }
    let write = WRITES.contains(&verb.as_str())
        || OTHER_WRITES.contains(&verb.as_str())
        || (verb == "aggregate" && has_output_key(&toks));
    let kind = if write {
        StatementKind::Write
    } else {
        StatementKind::Read
    };
    (kind, verb)
}

/// The name after `db.<ident>.`, `db.getCollection(…).` or `db[…].`.
fn method_name(t: &[&Tok]) -> Option<String> {
    if !matches!(t.first(), Some(Tok::Ident(db)) if db == "db") {
        return None;
    }
    let after = match t.get(1)? {
        Tok::Dot => match t.get(2)? {
            Tok::Ident(n) if n == "getCollection" && t.get(3) == Some(&&Tok::LParen) => {
                past_close(t, 3)?
            }
            Tok::Ident(_) => 3,
            _ => return None,
        },
        Tok::LBracket => past_close(t, 1)?,
        _ => return None,
    };
    match (t.get(after)?, t.get(after + 1)?) {
        (Tok::Dot, Tok::Ident(m)) => Some(m.clone()),
        _ => None,
    }
}

/// The name of a database-level call, `db.<name>(`: any name but `getCollection`.
fn database_method<'t>(t: &[&'t Tok]) -> Option<&'t str> {
    match t {
        [Tok::Ident(db), Tok::Dot, Tok::Ident(name), Tok::LParen, ..]
            if db == "db" && name != "getCollection" =>
        {
            Some(name.as_str())
        }
        _ => None,
    }
}

/// The name of the last `.<name>(` outside any bracket: the statement's last call.
fn last_call<'t>(t: &[&'t Tok]) -> Option<&'t str> {
    let mut depth = 0usize;
    let mut last = None;
    for (i, k) in t.iter().enumerate() {
        match k {
            Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
            Tok::RParen | Tok::RBracket | Tok::RBrace => depth = depth.saturating_sub(1),
            Tok::Dot if depth == 0 => {
                if let (Some(Tok::Ident(name)), Some(Tok::LParen)) =
                    (t.get(i + 1).copied(), t.get(i + 2).copied())
                {
                    last = Some(name.as_str());
                }
            }
            _ => {}
        }
    }
    last
}

/// The index after the bracket that closes the one at `open`.
fn past_close(t: &[&Tok], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, k) in t.iter().enumerate().skip(open) {
        match k {
            Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
            Tok::RParen | Tok::RBracket | Tok::RBrace => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// A `$out`/`$merge` key anywhere: a bare name (only ever a key), or a quoted one before `:`.
/// A quoted `"$out"` elsewhere is a value (a field path).
fn has_output_key(t: &[&Tok]) -> bool {
    t.iter().enumerate().any(|(i, k)| match k {
        Tok::Ident(n) => OUTPUT_STAGES.contains(&n.as_str()),
        Tok::Str(s) => OUTPUT_STAGES.contains(&s.as_str()) && t.get(i + 1) == Some(&&Tok::Colon),
        _ => false,
    })
}

/// Parses one statement, checking its arguments. Offsets in errors are chars into `stmt`.
pub fn parse(stmt: &str, params: &ParamValues) -> Result<MongoCall, ParseError> {
    let tokens = lex::tokenize(stmt)?;
    let mut p = Parser::new(&tokens, params);
    let call = match target(&mut p)? {
        Target::Database { name, at } => database_call(&mut p, name, at, &tokens)?,
        Target::Collection(collection) => MongoCall {
            collection,
            op: collection_call(&mut p, &tokens)?,
        },
    };
    while p.eat(&Tok::Semi) {}
    if let Some(t) = p.peek() {
        return Err(ParseError::new(
            "Unexpected text after the statement.",
            t.start,
        ));
    }
    Ok(call)
}

/// What a statement calls a method on.
enum Target<'t> {
    Collection(String),
    /// `db.<name>(`, the name written at `at`.
    Database { name: &'t str, at: usize },
}

/// `db.<ident>`, `db.getCollection(<string>)` or `db[<string>]`; or `db.<name>` followed by
/// `(`, a database-level call.
fn target<'t>(p: &mut Parser<'t>) -> Result<Target<'t>, ParseError> {
    let start = p.here();
    if !matches!(p.bump(), Some(Token { kind: Tok::Ident(db), .. }) if db == "db") {
        return Err(ParseError::new(
            "A statement starts with db, as in db.users.find().",
            start,
        ));
    }
    if p.eat(&Tok::LBracket) {
        let name = collection_name(p)?;
        if !p.eat(&Tok::RBracket) {
            return Err(ParseError::new(
                "Expected ] after the collection name.",
                p.here(),
            ));
        }
        return Ok(Target::Collection(name));
    }
    let dot = p.here();
    if !p.eat(&Tok::Dot) {
        return Err(ParseError::new("Expected . or [ after db.", dot));
    }
    let name_at = p.here();
    match p.bump() {
        Some(Token {
            kind: Tok::Ident(n),
            ..
        }) if n == "getCollection" && p.eat(&Tok::LParen) => {
            let name = collection_name(p)?;
            if !p.eat(&Tok::RParen) {
                return Err(ParseError::new(
                    "Expected ) after the collection name.",
                    p.here(),
                ));
            }
            Ok(Target::Collection(name))
        }
        Some(Token {
            kind: Tok::Ident(n),
            start,
            ..
        }) if matches!(p.peek(), Some(t) if t.kind == Tok::LParen) => Ok(Target::Database {
            name: n.as_str(),
            at: *start,
        }),
        Some(Token {
            kind: Tok::Ident(n),
            ..
        }) => Ok(Target::Collection(n.clone())),
        _ => Err(ParseError::new(
            "Expected a collection name after db.",
            name_at,
        )),
    }
}

/// `db.<name>(…)`: only `db.createCollection(name, options?)`.
fn database_call<'t>(
    p: &mut Parser,
    name: &'t str,
    at: usize,
    tokens: &'t [Token],
) -> Result<MongoCall, ParseError> {
    if name != "createCollection" {
        return Err(ParseError::new(
            format!(
                "db.{name}() isn't supported in query tabs. Supported: db.createCollection()."
            ),
            at,
        ));
    }
    let args = Args::read(p, name, tokens)?;
    args.at_most(2)?;
    let collection = match args.get(0) {
        Some(Bson::String(n)) => n.clone(),
        _ => return Err(args.expects(0, "a collection name string")),
    };
    let options = args.doc_or_empty(1)?;
    args.refuse_option(1, &options, "create")?;
    Ok(MongoCall {
        collection,
        op: MongoQueryOp::CreateCollection { options },
    })
}

/// `.<method>(…)` and its chain, after the collection.
fn collection_call(p: &mut Parser, tokens: &[Token]) -> Result<MongoQueryOp, ParseError> {
    let method_at = p.here();
    if !p.eat(&Tok::Dot) {
        return Err(ParseError::new(
            "Expected a method after the collection, as in db.users.find().",
            method_at,
        ));
    }
    let (method, at) = name_after_dot(p)?;
    match method {
        "createCollection" => {
            return Err(ParseError::new(
                "createCollection is called on db, as in db.createCollection(\"name\").",
                at,
            ));
        }
        // mongosh's `db.c.explain().find()`.
        "explain" => {
            return Err(ParseError::new(
                "Write explain after the call, as in db.users.find().explain().",
                at,
            ));
        }
        _ if !METHODS.contains(&method) => {
            return Err(ParseError::new(
                format!(
                    "{method} isn't supported in query tabs. Supported: {}.",
                    METHODS.join(", ")
                ),
                at,
            ));
        }
        _ => {}
    }
    let op = Args::read(p, method, tokens)?.method()?;
    chain(p, op, method, tokens)
}

fn collection_name(p: &mut Parser) -> Result<String, ParseError> {
    let at = p.here();
    match p.value()? {
        Bson::String(name) => Ok(name),
        _ => Err(ParseError::new("The collection name must be a string.", at)),
    }
}

/// The name after a `.` that was just read.
fn name_after_dot<'t>(p: &mut Parser<'t>) -> Result<(&'t str, usize), ParseError> {
    let at = p.here();
    match p.bump() {
        Some(Token {
            kind: Tok::Ident(n),
            start,
            ..
        }) => Ok((n.as_str(), *start)),
        _ => Err(ParseError::new("Expected a method name after the dot.", at)),
    }
}

/// `.sort(doc)`, `.skip(n)`, `.limit(n)`, `.pretty()` and `.toArray()` after `find(…)`, and
/// `.explain(verbosity?)`, last, after `find(…)` or `aggregate(…)`.
fn chain(
    p: &mut Parser,
    mut op: MongoQueryOp,
    method: &str,
    tokens: &[Token],
) -> Result<MongoQueryOp, ParseError> {
    while p.eat(&Tok::Dot) {
        let (name, at) = name_after_dot(p)?;
        let supported: &[&str] = match &op {
            MongoQueryOp::Explain { .. } => {
                return Err(ParseError::new("explain must be the last call.", at));
            }
            MongoQueryOp::Find { one: false, .. } => &CHAIN,
            MongoQueryOp::Aggregate { .. } => &["explain"],
            _ => {
                return Err(ParseError::new(
                    format!(
                        "{name} isn't supported after {method}. Only find and aggregate can be \
                         followed by another call."
                    ),
                    at,
                ));
            }
        };
        if !supported.contains(&name) {
            return Err(ParseError::new(
                format!(
                    "{name} isn't supported after {method}. Supported: {}.",
                    supported.join(", ")
                ),
                at,
            ));
        }
        let args = Args::read(p, name, tokens)?;
        if name == "explain" {
            op = explain(op, &args, at)?;
            continue;
        }
        // Only find is followed by anything but explain.
        let MongoQueryOp::Find {
            sort, skip, limit, ..
        } = &mut op
        else {
            continue;
        };
        match name {
            "sort" => {
                args.at_most(1)?;
                *sort = Some(args.doc(0)?);
            }
            "skip" => {
                args.at_most(1)?;
                let n = args.whole(0).and_then(|n| u64::try_from(n).ok());
                *skip =
                    Some(n.ok_or_else(|| {
                        args.error(0, "skip expects a whole number of 0 or more.")
                    })?);
            }
            "limit" => {
                args.at_most(1)?;
                let n = args.whole(0);
                *limit = Some(n.ok_or_else(|| args.error(0, "limit expects a whole number."))?);
            }
            _ => args.at_most(0)?,
        }
    }
    Ok(op)
}

/// `.explain(verbosity?)` of `of`, the `explain` written at `at`.
fn explain(of: MongoQueryOp, args: &Args, at: usize) -> Result<MongoQueryOp, ParseError> {
    args.at_most(1)?;
    // The server may write under explain (it varies by version and verbosity), and an explain
    // is a read, which read-only profiles and Production allow.
    if let MongoQueryOp::Aggregate { pipeline } = &of
        && pipeline.iter().any(is_output_stage)
    {
        return Err(ParseError::new(
            "explain isn't supported on a pipeline with $out or $merge.",
            at,
        ));
    }
    let verbosity = match args.get(0) {
        None => Some(ExplainVerbosity::QueryPlanner),
        Some(Bson::String(v)) => ExplainVerbosity::ALL.into_iter().find(|k| k.as_str() == v),
        Some(_) => None,
    };
    let verbosity = verbosity.ok_or_else(|| {
        args.error(
            0,
            "explain expects \"queryPlanner\", \"executionStats\" or \"allPlansExecution\".",
        )
    })?;
    Ok(MongoQueryOp::Explain {
        of: Box::new(of),
        verbosity,
    })
}

struct Arg {
    value: Bson,
    at: usize,
}

/// A method's arguments, checked one by one with messages that name the method.
struct Args<'t> {
    method: &'t str,
    list: Vec<Arg>,
    /// Where `)` is: the place a missing argument is reported.
    close: usize,
    tokens: &'t [Token],
}

impl<'t> Args<'t> {
    /// `(` arguments `)`, a trailing comma allowed.
    fn read(p: &mut Parser, method: &'t str, tokens: &'t [Token]) -> Result<Self, ParseError> {
        if !p.eat(&Tok::LParen) {
            return Err(ParseError::new(
                format!("Expected ( after {method}."),
                p.here(),
            ));
        }
        let mut list = Vec::new();
        loop {
            let at = p.here();
            if p.eat(&Tok::RParen) {
                return Ok(Self {
                    method,
                    list,
                    close: at,
                    tokens,
                });
            }
            list.push(Arg {
                value: p.value()?,
                at,
            });
            if !p.eat(&Tok::Comma) && !matches!(p.peek(), Some(t) if t.kind == Tok::RParen) {
                return Err(ParseError::new("Expected , or ).", p.here()));
            }
        }
    }

    /// The call this method's arguments make.
    fn method(self) -> Result<MongoQueryOp, ParseError> {
        let m = self.method;
        Ok(match m {
            "find" | "findOne" => {
                self.at_most(2)?;
                MongoQueryOp::Find {
                    filter: self.doc_or_empty(0)?,
                    projection: self.optional_doc(1)?,
                    sort: None,
                    skip: None,
                    limit: None,
                    one: m == "findOne",
                }
            }
            "aggregate" => {
                self.at_most(1)?;
                MongoQueryOp::Aggregate {
                    pipeline: self.docs(0)?,
                }
            }
            "countDocuments" => {
                self.at_most(1)?;
                MongoQueryOp::CountDocuments {
                    filter: self.doc_or_empty(0)?,
                }
            }
            "estimatedDocumentCount" => {
                self.at_most(0)?;
                MongoQueryOp::EstimatedDocumentCount
            }
            "distinct" => {
                self.at_most(2)?;
                let field = match self.get(0) {
                    Some(Bson::String(f)) => f.clone(),
                    _ => return Err(self.expects(0, "a field name string")),
                };
                MongoQueryOp::Distinct {
                    field,
                    filter: self.doc_or_empty(1)?,
                }
            }
            "insertOne" => {
                self.at_most(1)?;
                MongoQueryOp::InsertOne { doc: self.doc(0)? }
            }
            "insertMany" => {
                self.at_most(2)?;
                let docs = self.docs(0)?;
                if docs.is_empty() {
                    return Err(self.error(0, "insertMany needs at least one document."));
                }
                MongoQueryOp::InsertMany {
                    docs,
                    ordered: self.flag(1, "ordered", true)?,
                }
            }
            "updateOne" | "updateMany" => {
                self.at_most(3)?;
                MongoQueryOp::Update {
                    filter: self.doc(0)?,
                    update: self.update(1)?,
                    upsert: self.flag(2, "upsert", false)?,
                    many: m == "updateMany",
                }
            }
            "replaceOne" => {
                self.at_most(3)?;
                MongoQueryOp::ReplaceOne {
                    filter: self.doc(0)?,
                    replacement: self.replacement(1)?,
                    upsert: self.flag(2, "upsert", false)?,
                }
            }
            "findOneAndUpdate" | "findOneAndReplace" => {
                self.at_most(3)?;
                let filter = self.doc(0)?;
                let action = if m == "findOneAndUpdate" {
                    FindAndAction::Update(self.update(1)?)
                } else {
                    FindAndAction::Replace(self.replacement(1)?)
                };
                let options = self.options(2, &FIND_AND_MODIFY_OPTIONS)?;
                MongoQueryOp::FindOneAnd {
                    filter,
                    action,
                    projection: self.option_doc(2, &options, "projection")?,
                    sort: self.option_doc(2, &options, "sort")?,
                    upsert: self.option_flag(2, &options, "upsert", false)?,
                    return_after: self.return_after(2, &options)?,
                }
            }
            "findOneAndDelete" => {
                self.at_most(2)?;
                let filter = self.doc(0)?;
                let options = self.options(1, &FIND_AND_MODIFY_OPTIONS[..2])?;
                MongoQueryOp::FindOneAnd {
                    filter,
                    action: FindAndAction::Delete,
                    projection: self.option_doc(1, &options, "projection")?,
                    sort: self.option_doc(1, &options, "sort")?,
                    upsert: false,
                    return_after: false,
                }
            }
            "bulkWrite" => {
                self.at_most(2)?;
                let Some(Bson::Array(items)) = self.get(0) else {
                    return Err(self.expects(0, "an array of operations"));
                };
                if items.is_empty() {
                    return Err(self.error(0, "bulkWrite needs at least one operation."));
                }
                let ops = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| bulk_op(i + 1, item).map_err(|m| self.error(0, m)))
                    .collect::<Result<_, _>>()?;
                MongoQueryOp::BulkWrite {
                    ops,
                    ordered: self.flag(1, "ordered", true)?,
                }
            }
            "drop" => {
                self.at_most(0)?;
                MongoQueryOp::Drop
            }
            "createIndex" => {
                self.at_most(2)?;
                let keys = self.doc(0)?;
                if keys.is_empty() {
                    return Err(self.error(
                        0,
                        "createIndex needs at least one key, as in { email: 1 }.",
                    ));
                }
                let not_a_key = keys.iter().find(|(_, v)| {
                    !matches!(
                        v,
                        Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::String(_)
                    )
                });
                if let Some((key, _)) = not_a_key {
                    return Err(ParseError::new(
                        format!(
                            "The value of {key} must be a number or a string, such as 1, -1 or \
                             \"text\"."
                        ),
                        self.key_at(0, key),
                    ));
                }
                let mut options = self.doc_or_empty(1)?;
                self.refuse_option(1, &options, "key")?;
                let name = match options.remove("name") {
                    None => index_name(&keys),
                    Some(Bson::String(name)) => name,
                    Some(_) => {
                        return Err(ParseError::new(
                            "name must be a string.",
                            self.key_at(1, "name"),
                        ));
                    }
                };
                MongoQueryOp::CreateIndex {
                    keys,
                    name,
                    options,
                }
            }
            "dropIndex" => {
                self.at_most(1)?;
                let index = match self.get(0) {
                    Some(Bson::String(name)) if name == "*" => {
                        return Err(
                            self.error(0, "dropIndex(\"*\") isn't supported; name one index.")
                        );
                    }
                    Some(index @ (Bson::String(_) | Bson::Document(_))) => index.clone(),
                    _ => return Err(self.expects(0, "an index name or a keys document")),
                };
                MongoQueryOp::DropIndex { index }
            }
            "getIndexes" => {
                self.at_most(0)?;
                MongoQueryOp::GetIndexes
            }
            "renameCollection" => {
                self.at_most(2)?;
                let to = match self.get(0) {
                    Some(Bson::String(to)) => to.clone(),
                    _ => return Err(self.expects(0, "a collection name string")),
                };
                let drop_target = match self.get(1) {
                    None => false,
                    Some(Bson::Boolean(b)) => *b,
                    Some(_) => return Err(self.expects(1, "true or false")),
                };
                MongoQueryOp::RenameCollection { to, drop_target }
            }
            _ => {
                self.at_most(1)?;
                MongoQueryOp::Delete {
                    filter: self.doc(0)?,
                    many: m == "deleteMany",
                }
            }
        })
    }

    fn get(&self, i: usize) -> Option<&Bson> {
        self.list.get(i).map(|a| &a.value)
    }

    /// An error at argument `i`, or at `)` when it is missing.
    fn error(&self, i: usize, message: impl Into<String>) -> ParseError {
        ParseError::new(message, self.list.get(i).map_or(self.close, |a| a.at))
    }

    fn expects(&self, i: usize, what: &str) -> ParseError {
        self.error(
            i,
            format!(
                "{} expects {what} as its {} argument.",
                self.method, ORDINALS[i]
            ),
        )
    }

    fn at_most(&self, n: usize) -> Result<(), ParseError> {
        if self.list.len() <= n {
            return Ok(());
        }
        let m = self.method;
        let message = match n {
            0 => format!("{m} takes no arguments."),
            1 => format!("{m} takes at most 1 argument."),
            n => format!("{m} takes at most {n} arguments."),
        };
        Err(self.error(n, message))
    }

    fn doc(&self, i: usize) -> Result<Document, ParseError> {
        match self.get(i) {
            Some(Bson::Document(d)) => Ok(d.clone()),
            _ => Err(self.expects(i, "a document")),
        }
    }

    fn optional_doc(&self, i: usize) -> Result<Option<Document>, ParseError> {
        match self.get(i) {
            None => Ok(None),
            Some(_) => self.doc(i).map(Some),
        }
    }

    fn doc_or_empty(&self, i: usize) -> Result<Document, ParseError> {
        Ok(self.optional_doc(i)?.unwrap_or_default())
    }

    /// An array of documents.
    fn docs(&self, i: usize) -> Result<Vec<Document>, ParseError> {
        let items = match self.get(i) {
            Some(Bson::Array(items)) => items.iter().map(|v| v.as_document().cloned()).collect(),
            _ => None,
        };
        items.ok_or_else(|| self.expects(i, "an array of documents"))
    }

    /// `$` operators, or a pipeline.
    fn update(&self, i: usize) -> Result<UpdateSpec, ParseError> {
        let not_an_update = || self.expects(i, "an update document or pipeline");
        let value = self.get(i).ok_or_else(not_an_update)?;
        update_spec(value, self.method)
            .map_err(|m| m.map_or_else(not_an_update, |m| self.error(i, m)))
    }

    /// A replacement document: no update operators.
    fn replacement(&self, i: usize) -> Result<Document, ParseError> {
        let replacement = self.doc(i)?;
        if has_operators(&replacement) {
            return Err(self.error(i, REPLACEMENT_OPERATORS));
        }
        Ok(replacement)
    }

    /// The boolean option `key` of the options document at `i`, the only option it may have.
    fn flag(&self, i: usize, key: &str, default: bool) -> Result<bool, ParseError> {
        let options = self.options(i, &[key])?;
        self.option_flag(i, &options, key, default)
    }

    /// The options document at `i` (empty when there is none), each of its keys one of
    /// `supported`.
    fn options(&self, i: usize, supported: &[&str]) -> Result<Document, ParseError> {
        let options = self.doc_or_empty(i)?;
        if let Some(other) = options.keys().find(|k| !supported.contains(&k.as_str())) {
            return Err(ParseError::new(
                format!(
                    "{other} isn't a supported option of {}. Supported: {}.",
                    self.method,
                    supported.join(", ")
                ),
                self.key_at(i, other),
            ));
        }
        Ok(options)
    }

    /// An error at `key` when the options document at `i` (passed through as written) has it:
    /// a key the app builds itself, which the option would replace.
    fn refuse_option(&self, i: usize, options: &Document, key: &str) -> Result<(), ParseError> {
        if !options.contains_key(key) {
            return Ok(());
        }
        Err(ParseError::new(
            format!("{key} isn't a supported option of {}.", self.method),
            self.key_at(i, key),
        ))
    }

    /// The boolean `key` of `options`, the options document at `i`.
    fn option_flag(
        &self,
        i: usize,
        options: &Document,
        key: &str,
        default: bool,
    ) -> Result<bool, ParseError> {
        match options.get(key) {
            None => Ok(default),
            Some(Bson::Boolean(b)) => Ok(*b),
            Some(_) => Err(ParseError::new(
                format!("{key} must be true or false."),
                self.key_at(i, key),
            )),
        }
    }

    /// The document `key` of `options`, the options document at `i`.
    fn option_doc(
        &self,
        i: usize,
        options: &Document,
        key: &str,
    ) -> Result<Option<Document>, ParseError> {
        match options.get(key) {
            None => Ok(None),
            Some(Bson::Document(d)) => Ok(Some(d.clone())),
            Some(_) => Err(ParseError::new(
                format!("{key} must be a document."),
                self.key_at(i, key),
            )),
        }
    }

    /// Whether a `findOneAnd*` returns the document after the change: `returnDocument: "after"`,
    /// or mongosh's legacy `returnNewDocument: true`, but not both.
    fn return_after(&self, i: usize, options: &Document) -> Result<bool, ParseError> {
        const KEYS: [&str; 2] = ["returnDocument", "returnNewDocument"];
        if KEYS.iter().all(|k| options.contains_key(k)) {
            let later = options.keys().filter(|k| KEYS.contains(&k.as_str())).last();
            return Err(ParseError::new(
                "Use returnDocument or returnNewDocument, not both.",
                later.map_or(self.close, |k| self.key_at(i, k)),
            ));
        }
        match options.get("returnDocument") {
            None => self.option_flag(i, options, "returnNewDocument", false),
            Some(Bson::String(s)) if s == "before" => Ok(false),
            Some(Bson::String(s)) if s == "after" => Ok(true),
            Some(_) => Err(ParseError::new(
                "returnDocument must be \"before\" or \"after\".",
                self.key_at(i, "returnDocument"),
            )),
        }
    }

    /// Where `key` is written in argument `i` (the argument itself when it came from a
    /// parameter).
    fn key_at(&self, i: usize, key: &str) -> usize {
        let Some(arg) = self.list.get(i) else {
            return self.close;
        };
        let written: Vec<&Token> = self
            .tokens
            .iter()
            .filter(|t| t.kind != Tok::Newline && t.start >= arg.at && t.start < self.close)
            .collect();
        written
            .windows(2)
            .find(|w| {
                matches!(&w[0].kind, Tok::Ident(k) | Tok::Str(k) if k == key)
                    && w[1].kind == Tok::Colon
            })
            .map_or(arg.at, |w| w[0].start)
    }

    /// A whole number, written as one or passed as a parameter.
    fn whole(&self, i: usize) -> Option<i64> {
        match self.get(i)? {
            Bson::Int32(n) => Some((*n).into()),
            Bson::Int64(n) => Some(*n),
            Bson::Double(f) if f.fract() == 0.0 && f.abs() < i64::MAX as f64 => Some(*f as i64),
            _ => None,
        }
    }
}

const REPLACEMENT_OPERATORS: &str = "A replacement document can't contain update operators.";

fn has_operators(d: &Document) -> bool {
    d.keys().any(|k| k.starts_with('$'))
}

/// `value` as `method`'s update: `$` operators, or a pipeline. The error is its message, or
/// `None` when `value` is neither a document nor an array of documents.
fn update_spec(value: &Bson, method: &str) -> Result<UpdateSpec, Option<String>> {
    match value {
        Bson::Document(d) if d.is_empty() => Err(Some("The update document is empty.".into())),
        Bson::Document(d) if !d.keys().all(|k| k.starts_with('$')) => Err(Some(format!(
            "{method} needs update operators such as {{ $set: … }}, or a pipeline."
        ))),
        Bson::Document(d) => Ok(UpdateSpec::Modifiers(d.clone())),
        Bson::Array(items) if items.is_empty() => {
            Err(Some("The update pipeline is empty.".into()))
        }
        Bson::Array(items) => items
            .iter()
            .map(|v| v.as_document().cloned())
            .collect::<Option<_>>()
            .map(UpdateSpec::Pipeline)
            .ok_or(None),
        _ => Err(None),
    }
}

/// Operation `n` (1-based) of a `bulkWrite`: a document with one key, the operation's name,
/// whose value holds its fields. The error is the message, which the caller places.
fn bulk_op(n: usize, item: &Bson) -> Result<BulkOp, String> {
    let shape = || {
        format!(
            "Operation {n} should be a document with one key: insertOne, updateOne, updateMany, \
             replaceOne, deleteOne or deleteMany."
        )
    };
    let mut entries = item.as_document().ok_or_else(shape)?.iter();
    let (Some((kind, body)), None) = (entries.next(), entries.next()) else {
        return Err(shape());
    };
    let (fields, required): (&[&str], &[&str]) = match kind.as_str() {
        "insertOne" => (&["document"], &["document"]),
        "updateOne" | "updateMany" => (&["filter", "update", "upsert"], &["filter", "update"]),
        "replaceOne" => (
            &["filter", "replacement", "upsert"],
            &["filter", "replacement"],
        ),
        "deleteOne" | "deleteMany" => (&["filter"], &["filter"]),
        _ => return Err(shape()),
    };
    let prefix = format!("Operation {n} ({kind})");
    let needs = || format!("{prefix} needs {}.", required.join(" and "));
    let body = body.as_document().ok_or_else(needs)?;
    if let Some(other) = body.keys().find(|k| !fields.contains(&k.as_str())) {
        return Err(format!(
            "{prefix}: {other} isn't a supported field. Supported: {}.",
            fields.join(", ")
        ));
    }
    if !required.iter().all(|k| body.contains_key(k)) {
        return Err(needs());
    }
    let doc = |key: &str| match body.get(key) {
        Some(Bson::Document(d)) => Ok(d.clone()),
        _ => Err(format!("{prefix}: {key} must be a document.")),
    };
    let upsert = || match body.get("upsert") {
        None => Ok(false),
        Some(Bson::Boolean(b)) => Ok(*b),
        Some(_) => Err(format!("{prefix}: upsert must be true or false.")),
    };
    Ok(match kind.as_str() {
        "insertOne" => BulkOp::Insert(doc("document")?),
        "updateOne" | "updateMany" => {
            let filter = doc("filter")?;
            let update = update_spec(body.get("update").unwrap_or(&Bson::Null), kind)
                .map_err(|m| match m {
                    Some(m) => format!("{prefix}: {m}"),
                    None => format!("{prefix}: update must be an update document or pipeline."),
                })?;
            BulkOp::Update {
                filter,
                update,
                upsert: upsert()?,
                many: kind == "updateMany",
            }
        }
        "replaceOne" => {
            let filter = doc("filter")?;
            let replacement = doc("replacement")?;
            if has_operators(&replacement) {
                return Err(format!("{prefix}: {REPLACEMENT_OPERATORS}"));
            }
            BulkOp::Replace {
                filter,
                replacement,
                upsert: upsert()?,
            }
        }
        _ => BulkOp::Delete {
            filter: doc("filter")?,
            many: kind == "deleteMany",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use StatementKind::{Read, Write};
    use mongodb::bson::{doc, oid::ObjectId};

    fn call(stmt: &str) -> MongoCall {
        parse(stmt, &ParamValues::new()).unwrap_or_else(|e| panic!("{stmt}: {e:?}"))
    }

    fn op(stmt: &str) -> MongoQueryOp {
        call(stmt).op
    }

    fn err(stmt: &str) -> ParseError {
        match parse(stmt, &ParamValues::new()) {
            Ok(c) => panic!("{stmt} parsed as {c:?}"),
            Err(e) => e,
        }
    }

    fn message(stmt: &str) -> String {
        err(stmt).message
    }

    fn at(stmt: &str, needle: &str) -> usize {
        let byte = stmt.find(needle).unwrap();
        stmt[..byte].chars().count()
    }

    const SUPPORTED: &str = "Supported: find, findOne, aggregate, countDocuments, \
        estimatedDocumentCount, distinct, insertOne, insertMany, updateOne, updateMany, \
        replaceOne, deleteOne, deleteMany, findOneAndUpdate, findOneAndReplace, \
        findOneAndDelete, bulkWrite, drop, createIndex, dropIndex, getIndexes, renameCollection.";

    /// One statement per row of the methods table, with its verb and kind.
    const TABLE: [(&str, &str, StatementKind); 22] = [
        ("db.users.find({ a: 1 })", "find", Read),
        ("db.users.findOne()", "findOne", Read),
        ("db.users.aggregate([{ $match: {} }])", "aggregate", Read),
        ("db.users.countDocuments()", "countDocuments", Read),
        (
            "db.users.estimatedDocumentCount()",
            "estimatedDocumentCount",
            Read,
        ),
        ("db.users.distinct(\"city\")", "distinct", Read),
        ("db.users.insertOne({ a: 1 })", "insertOne", Write),
        ("db.users.insertMany([{ a: 1 }])", "insertMany", Write),
        (
            "db.users.updateOne({}, { $set: { a: 1 } })",
            "updateOne",
            Write,
        ),
        (
            "db.users.updateMany({}, { $inc: { a: 1 } })",
            "updateMany",
            Write,
        ),
        ("db.users.replaceOne({}, { a: 1 })", "replaceOne", Write),
        ("db.users.deleteOne({ a: 1 })", "deleteOne", Write),
        ("db.users.deleteMany({})", "deleteMany", Write),
        (
            "db.users.findOneAndUpdate({ a: 1 }, { $set: { b: 2 } })",
            "findOneAndUpdate",
            Write,
        ),
        (
            "db.users.findOneAndReplace({ a: 1 }, { a: 2 })",
            "findOneAndReplace",
            Write,
        ),
        (
            "db.users.findOneAndDelete({ a: 1 })",
            "findOneAndDelete",
            Write,
        ),
        (
            "db.users.bulkWrite([{ insertOne: { document: { a: 1 } } }])",
            "bulkWrite",
            Write,
        ),
        ("db.users.drop()", "drop", Write),
        ("db.users.createIndex({ email: 1 })", "createIndex", Write),
        ("db.users.dropIndex(\"email_1\")", "dropIndex", Write),
        ("db.users.getIndexes()", "getIndexes", Read),
        (
            "db.users.renameCollection(\"people\")",
            "renameCollection",
            Write,
        ),
    ];

    #[test]
    fn parse_each_method() {
        assert_eq!(
            op("db.users.find()"),
            MongoQueryOp::Find {
                filter: doc! {},
                projection: None,
                sort: None,
                skip: None,
                limit: None,
                one: false
            }
        );
        assert_eq!(
            op(
                "db.users.find({ a: 1 }, { a: 1, _id: 0 })\n  .sort({ b: -1, a: 1 })\n  \
                .skip(5)\n  .limit(-10)\n  .pretty()\n  .toArray()"
            ),
            MongoQueryOp::Find {
                filter: doc! { "a": 1 },
                projection: Some(doc! { "a": 1, "_id": 0 }),
                sort: Some(doc! { "b": -1, "a": 1 }),
                skip: Some(5),
                limit: Some(-10),
                one: false
            }
        );
        // Sort keeps the order written.
        let MongoQueryOp::Find {
            sort: Some(sort), ..
        } = op("db.u.find().sort({ z: 1, a: 1 })")
        else {
            panic!()
        };
        assert_eq!(sort.keys().collect::<Vec<_>>(), ["z", "a"]);
        assert_eq!(
            op("db.users.findOne({ a: 1 })"),
            MongoQueryOp::Find {
                filter: doc! { "a": 1 },
                projection: None,
                sort: None,
                skip: None,
                limit: None,
                one: true
            }
        );
        assert_eq!(
            op("db.users.aggregate([{ $match: { a: 1 } }, { $group: { _id: \"$a\" } },])"),
            MongoQueryOp::Aggregate {
                pipeline: vec![
                    doc! { "$match": { "a": 1 } },
                    doc! { "$group": { "_id": "$a" } }
                ]
            }
        );
        assert_eq!(
            op("db.users.countDocuments({ a: 1 })"),
            MongoQueryOp::CountDocuments {
                filter: doc! { "a": 1 }
            }
        );
        assert_eq!(
            op("db.users.countDocuments()"),
            MongoQueryOp::CountDocuments { filter: doc! {} }
        );
        assert_eq!(
            op("db.users.estimatedDocumentCount()"),
            MongoQueryOp::EstimatedDocumentCount
        );
        assert_eq!(
            op("db.users.distinct(\"city\", { a: 1 })"),
            MongoQueryOp::Distinct {
                field: "city".into(),
                filter: doc! { "a": 1 }
            }
        );
        assert_eq!(
            op("db.users.distinct('city')"),
            MongoQueryOp::Distinct {
                field: "city".into(),
                filter: doc! {}
            }
        );
        assert_eq!(
            op("db.users.insertOne({ a: 1, b: \"x\" })"),
            MongoQueryOp::InsertOne {
                doc: doc! { "a": 1, "b": "x" }
            }
        );
        assert_eq!(
            op("db.users.insertMany([{ a: 1 }, { a: 2 }])"),
            MongoQueryOp::InsertMany {
                docs: vec![doc! { "a": 1 }, doc! { "a": 2 }],
                ordered: true
            }
        );
        assert_eq!(
            op("db.users.insertMany([{ a: 1 }], { ordered: false })"),
            MongoQueryOp::InsertMany {
                docs: vec![doc! { "a": 1 }],
                ordered: false
            }
        );
        assert_eq!(
            op("db.users.updateOne({ a: 1 }, { $set: { b: 2 } })"),
            MongoQueryOp::Update {
                filter: doc! { "a": 1 },
                update: UpdateSpec::Modifiers(doc! { "$set": { "b": 2 } }),
                upsert: false,
                many: false
            }
        );
        assert_eq!(
            op("db.users.updateMany({}, [{ $set: { b: \"$a\" } }], { upsert: true })"),
            MongoQueryOp::Update {
                filter: doc! {},
                update: UpdateSpec::Pipeline(vec![doc! { "$set": { "b": "$a" } }]),
                upsert: true,
                many: true
            }
        );
        assert_eq!(
            op("db.users.replaceOne({ a: 1 }, { a: 2 }, { upsert: true })"),
            MongoQueryOp::ReplaceOne {
                filter: doc! { "a": 1 },
                replacement: doc! { "a": 2 },
                upsert: true
            }
        );
        assert_eq!(
            op("db.users.replaceOne({ a: 1 }, { a: 2 })"),
            MongoQueryOp::ReplaceOne {
                filter: doc! { "a": 1 },
                replacement: doc! { "a": 2 },
                upsert: false
            }
        );
        assert_eq!(
            op("db.users.deleteOne({ a: 1 })"),
            MongoQueryOp::Delete {
                filter: doc! { "a": 1 },
                many: false
            }
        );
        assert_eq!(
            op("db.users.deleteMany({})"),
            MongoQueryOp::Delete {
                filter: doc! {},
                many: true
            }
        );

        assert_eq!(call("db.getCollection(\"a.b\").find()").collection, "a.b");
        assert_eq!(call("db[\"x y\"].countDocuments()").collection, "x y");
        assert_eq!(call("db.users.find();").collection, "users");

        // Verb and kind of the parsed call agree with the lexical classification.
        for (stmt, verb, kind) in TABLE {
            let c = call(stmt);
            assert_eq!((c.verb(), c.kind()), (verb, kind), "{stmt}");
            assert_eq!(classify(stmt), (kind, verb.to_string()), "{stmt}");
            assert_eq!(c.output_stage(), None, "{stmt}");
        }
    }

    /// The lexical classification (`classify`, from the tables of method names) and the parsed
    /// call's (`kind`, `verb`, from the op) agree for every supported method, an aggregation
    /// with and without an output stage included; and only the write methods count documents.
    #[test]
    fn classify_agrees_with_the_parsed_call_for_every_method() {
        let mut verbs: Vec<&str> = TABLE.iter().map(|(_, verb, _)| *verb).collect();
        verbs.sort_unstable();
        let mut methods = METHODS.to_vec();
        methods.sort_unstable();
        assert_eq!(verbs, methods, "TABLE has one statement per supported method");
        let pipelines = [
            ("db.users.aggregate([{ $match: {} }, { $out: \"x\" }])", Write),
            ("db.users.aggregate([{ $merge: { into: \"x\" } }])", Write),
            ("db.users.aggregate([{ $out: \"x\" }, { $match: {} }])", Write),
            ("db.users.aggregate([{ $project: { v: \"$out\" } }])", Read),
        ];
        let statements = TABLE
            .iter()
            .map(|(stmt, _, _)| *stmt)
            .chain(pipelines.iter().map(|(stmt, _)| *stmt));
        for stmt in statements {
            let c = call(stmt);
            assert_eq!(
                classify(stmt),
                (c.kind(), c.verb().to_string()),
                "{stmt}"
            );
            let counts = WRITES[..7].contains(&c.verb()) || c.verb() == "bulkWrite";
            assert_eq!(c.reports_affected(), counts, "{stmt}");
        }
        for (stmt, kind) in pipelines {
            assert_eq!(call(stmt).kind(), kind, "{stmt}");
        }
    }

    /// An export runs again only what reads rows: never a write or an output pipeline.
    #[test]
    fn exportable_calls() {
        for (stmt, _, _) in TABLE {
            let exportable = matches!(
                call(stmt).verb(),
                "find" | "findOne" | "aggregate" | "distinct"
            );
            assert_eq!(call(stmt).is_exportable(), exportable, "{stmt}");
        }
        let out = "db.users.aggregate([{ $match: {} }, { $out: \"x\" }])";
        assert!(!call(out).is_exportable());
        assert!(!call("db.users.aggregate([{ $merge: \"x\" }])").is_exportable());
    }

    #[test]
    fn unsupported_method_names_the_supported_set() {
        let stmt = "db.users.watch()";
        assert_eq!(
            err(stmt),
            ParseError::new(
                format!("watch isn't supported in query tabs. {SUPPORTED}"),
                9
            )
        );
        let stmt = "db.users.find().count()";
        assert_eq!(
            err(stmt),
            ParseError::new(
                "count isn't supported after find. Supported: sort, skip, limit, pretty, toArray, \
                 explain.",
                16
            )
        );
        let stmt = "db.users.aggregate([]).toArray()";
        assert_eq!(
            err(stmt),
            ParseError::new(
                "toArray isn't supported after aggregate. Supported: explain.",
                at(stmt, "toArray")
            )
        );
        let stmt = "db.users.updateOne({}, { $set: { a: 1 } }, { multi: true })";
        assert_eq!(
            err(stmt),
            ParseError::new(
                "multi isn't a supported option of updateOne. Supported: upsert.",
                at(stmt, "multi")
            )
        );
        let stmt = "db.users.insertMany([{}], { ordered: true, w: 1 })";
        assert_eq!(
            err(stmt),
            ParseError::new(
                "w isn't a supported option of insertMany. Supported: ordered.",
                at(stmt, "w:")
            )
        );

        // Refused names that change data still classify as writes; others as reads.
        for name in [
            "findOneAndUpdate",
            "findOneAndReplace",
            "findOneAndDelete",
            "bulkWrite",
            "drop",
            "createCollection",
            "createIndex",
            "dropIndex",
            "renameCollection",
            "dropDatabase",
        ] {
            assert_eq!(
                classify(&format!("db.users.{name}()")),
                (Write, name.to_string())
            );
        }
        assert_eq!(classify("db.users.count()"), (Read, "count".into()));
        assert_eq!(classify("db.users.watch()"), (Read, "watch".into()));
    }

    #[test]
    fn aggregate_with_out_or_merge_classifies_as_write() {
        let out_last = "db.a.aggregate([{ $match: {} }, { $out: \"x\" }])";
        let merge_last = "db.a.aggregate([{ $match: {} }, { $merge: { into: \"x\" } }])";
        let out_middle = "db.a.aggregate([{ $out: \"x\" }, { $match: {} }])";
        let quoted = "db.a.aggregate([{ \"$merge\": \"x\" }])";
        for stmt in [out_last, merge_last, out_middle, quoted] {
            assert_eq!(classify(stmt), (Write, "aggregate".into()), "{stmt}");
            assert_eq!(call(stmt).kind(), Write, "{stmt}");
        }
        assert_eq!(call(out_last).output_stage(), Some(&doc! { "$out": "x" }));
        assert_eq!(
            call(merge_last).output_stage(),
            Some(&doc! { "$merge": { "into": "x" } })
        );
        assert_eq!(call(out_middle).output_stage(), None);

        // `"$out"` as a value is a field path, not a stage.
        let read = "db.a.aggregate([{ $project: { v: \"$out\" } }])";
        assert_eq!(classify(read), (Read, "aggregate".into()));
        assert_eq!(call(read).kind(), Read);
        assert_eq!(call(read).output_stage(), None);

        // Text that doesn't parse yet.
        assert_eq!(
            classify("db.a.aggregate([{ $out"),
            (Write, "aggregate".into())
        );
    }

    #[test]
    fn classify_unparsable_text() {
        assert_eq!(
            classify("db.users.deleteMany("),
            (Write, "deleteMany".into())
        );
        assert_eq!(classify("db.users.find({ a: \"open"), (Read, "find".into()));
        assert_eq!(
            classify("db.getCollection(\"x\").insertOne("),
            (Write, "insertOne".into())
        );
        assert_eq!(classify("db[\"x\"].drop("), (Write, "drop".into()));
        assert_eq!(
            classify("db.users\n  .updateOne({ @"),
            (Write, "updateOne".into())
        );
        for text in ["", "show dbs", "db.users", "db.users.", "users.find()"] {
            assert_eq!(classify(text), (Read, String::new()), "{text:?}");
        }
    }

    #[test]
    fn params_in_statements() {
        let params: ParamValues = [
            (
                "id".to_string(),
                Some("650000000000000000000001".to_string()),
            ),
            ("n".to_string(), Some("5".to_string())),
        ]
        .into_iter()
        .collect();
        let c = parse(
            "db.a.find({ _id: ObjectId(:id), n: :n }).limit(:n)",
            &params,
        )
        .unwrap();
        let id = ObjectId::parse_str("650000000000000000000001").unwrap();
        assert_eq!(
            c.op,
            MongoQueryOp::Find {
                filter: doc! { "_id": id, "n": 5 },
                projection: None,
                sort: None,
                skip: None,
                limit: Some(5),
                one: false
            }
        );
        // `ObjectId(":id")` is the text `:id`, which isn't an ObjectId.
        assert!(
            message("db.a.find({ _id: ObjectId(\":id\") })").starts_with("ObjectId needs 24 hex")
        );
    }

    #[test]
    fn update_validation_messages() {
        assert_eq!(
            message("db.a.updateOne({}, { b: 1 })"),
            "updateOne needs update operators such as { $set: … }, or a pipeline."
        );
        assert_eq!(
            message("db.a.updateMany({}, { $set: { a: 1 }, b: 1 })"),
            "updateMany needs update operators such as { $set: … }, or a pipeline."
        );
        assert_eq!(
            message("db.a.updateOne({}, {})"),
            "The update document is empty."
        );
        assert_eq!(
            message("db.a.updateOne({}, [])"),
            "The update pipeline is empty."
        );
        assert_eq!(
            message("db.a.updateOne({})"),
            "updateOne expects an update document or pipeline as its second argument."
        );
        assert_eq!(
            message("db.a.updateOne({}, { $set: { a: 1 } }, { upsert: 1 })"),
            "upsert must be true or false."
        );
        assert_eq!(
            message("db.a.replaceOne({}, { a: 1, $set: { a: 1 } })"),
            "A replacement document can't contain update operators."
        );
        assert_eq!(
            message("db.a.insertMany([])"),
            "insertMany needs at least one document."
        );
        assert_eq!(
            message("db.a.insertMany([{}, 5])"),
            "insertMany expects an array of documents as its first argument."
        );
        let stmt = "db.a.find({ a: :x })";
        assert_eq!(err(stmt), ParseError::new("No value for :x", 15));
        let stmt = "db.a.insertOne(5)";
        assert_eq!(
            err(stmt),
            ParseError::new("insertOne expects a document as its first argument.", 15)
        );
        let stmt = "db.a.deleteMany()";
        assert_eq!(
            err(stmt),
            ParseError::new("deleteMany expects a document as its first argument.", 16)
        );
        assert_eq!(
            message("db.a.find({}, 5)"),
            "find expects a document as its second argument."
        );
        assert_eq!(
            message("db.a.aggregate({})"),
            "aggregate expects an array of documents as its first argument."
        );
        assert_eq!(
            message("db.a.distinct(5)"),
            "distinct expects a field name string as its first argument."
        );
        assert_eq!(
            message("db.a.find({}, {}, {})"),
            "find takes at most 2 arguments."
        );
        assert_eq!(
            message("db.a.deleteOne({}, {})"),
            "deleteOne takes at most 1 argument."
        );
        assert_eq!(
            message("db.a.estimatedDocumentCount({})"),
            "estimatedDocumentCount takes no arguments."
        );
        assert_eq!(
            message("db.a.find().skip(-1)"),
            "skip expects a whole number of 0 or more."
        );
        assert_eq!(
            message("db.a.find().limit(1.5)"),
            "limit expects a whole number."
        );
        assert_eq!(
            message("db.a.find().sort()"),
            "sort expects a document as its first argument."
        );
        let stmt = "db.a.find() db.b.find()";
        assert_eq!(
            err(stmt),
            ParseError::new("Unexpected text after the statement.", 12)
        );
        assert_eq!(
            err("use test"),
            ParseError::new("A statement starts with db, as in db.users.find().", 0)
        );
        assert_eq!(
            message("db.users"),
            "Expected a method after the collection, as in db.users.find()."
        );
        assert_eq!(message("db.users.find"), "Expected ( after find.");
        assert_eq!(
            err("db. 5"),
            ParseError::new("Expected a collection name after db.", 4)
        );
        assert_eq!(
            message("db.getCollection(5).find()"),
            "The collection name must be a string."
        );
        // Lexing errors keep their offset within the statement.
        assert_eq!(
            err("db.a.find({ n: \"Zoë 😀\", a: })"),
            ParseError::new("Expected a value.", 27)
        );
    }

    #[test]
    fn parse_new_methods() {
        use FindAndAction::{Delete, Replace, Update};
        assert_eq!(
            op("db.users.findOneAndUpdate({ a: 1 }, { $set: { b: 2 } }, \
                { returnDocument: \"after\", sort: { a: -1 }, upsert: true })"),
            MongoQueryOp::FindOneAnd {
                filter: doc! { "a": 1 },
                action: Update(UpdateSpec::Modifiers(doc! { "$set": { "b": 2 } })),
                projection: None,
                sort: Some(doc! { "a": -1 }),
                upsert: true,
                return_after: true
            }
        );
        assert_eq!(
            op("db.users.findOneAndUpdate({}, [{ $set: { b: 1 } }], { projection: { b: 1 } })"),
            MongoQueryOp::FindOneAnd {
                filter: doc! {},
                action: Update(UpdateSpec::Pipeline(vec![doc! { "$set": { "b": 1 } }])),
                projection: Some(doc! { "b": 1 }),
                sort: None,
                upsert: false,
                return_after: false
            }
        );
        let return_after = |stmt: &str| match op(stmt) {
            MongoQueryOp::FindOneAnd { return_after, .. } => return_after,
            other => panic!("{stmt}: {other:?}"),
        };
        assert!(return_after(
            "db.users.findOneAndUpdate({}, { $set: { b: 1 } }, { returnNewDocument: true })"
        ));
        assert!(!return_after(
            "db.users.findOneAndUpdate({}, { $set: { b: 1 } }, { returnNewDocument: false })"
        ));
        assert!(!return_after(
            "db.users.findOneAndUpdate({}, { $set: { b: 1 } }, { returnDocument: \"before\" })"
        ));
        assert_eq!(
            op("db.users.findOneAndReplace({ a: 1 }, { a: 2 }, \
                { upsert: true, returnNewDocument: true, projection: { a: 1 } })"),
            MongoQueryOp::FindOneAnd {
                filter: doc! { "a": 1 },
                action: Replace(doc! { "a": 2 }),
                projection: Some(doc! { "a": 1 }),
                sort: None,
                upsert: true,
                return_after: true
            }
        );
        assert_eq!(
            op("db.users.findOneAndReplace({ a: 1 }, { a: 2 })"),
            MongoQueryOp::FindOneAnd {
                filter: doc! { "a": 1 },
                action: Replace(doc! { "a": 2 }),
                projection: None,
                sort: None,
                upsert: false,
                return_after: false
            }
        );
        assert_eq!(
            op("db.users.findOneAndDelete({ a: 1 }, { sort: { a: 1 }, projection: { _id: 0 } })"),
            MongoQueryOp::FindOneAnd {
                filter: doc! { "a": 1 },
                action: Delete,
                projection: Some(doc! { "_id": 0 }),
                sort: Some(doc! { "a": 1 }),
                upsert: false,
                return_after: false
            }
        );
        assert_eq!(
            op("db.users.bulkWrite([{ deleteOne: { filter: { a: 1 } } }])"),
            MongoQueryOp::BulkWrite {
                ops: vec![BulkOp::Delete {
                    filter: doc! { "a": 1 },
                    many: false
                }],
                ordered: true
            }
        );
        assert_eq!(op("db.users.drop()"), MongoQueryOp::Drop);
        assert_eq!(
            op("db.users.createIndex({ email: 1 }, { unique: true })"),
            MongoQueryOp::CreateIndex {
                keys: doc! { "email": 1 },
                name: "email_1".into(),
                options: doc! { "unique": true }
            }
        );
        assert_eq!(
            op("db.users.createIndex({ a: 1, b: -1 }, { name: \"ab\", sparse: true })"),
            MongoQueryOp::CreateIndex {
                keys: doc! { "a": 1, "b": -1 },
                name: "ab".into(),
                options: doc! { "sparse": true }
            }
        );
        assert_eq!(
            op("db.users.createIndex({ t: \"text\" })"),
            MongoQueryOp::CreateIndex {
                keys: doc! { "t": "text" },
                name: "t_text".into(),
                options: doc! {}
            }
        );
        assert_eq!(
            op("db.users.dropIndex(\"email_1\")"),
            MongoQueryOp::DropIndex {
                index: Bson::String("email_1".into())
            }
        );
        assert_eq!(
            op("db.users.dropIndex({ email: 1 })"),
            MongoQueryOp::DropIndex {
                index: Bson::Document(doc! { "email": 1 })
            }
        );
        assert_eq!(op("db.users.getIndexes()"), MongoQueryOp::GetIndexes);
        assert_eq!(
            op("db.users.renameCollection(\"people\")"),
            MongoQueryOp::RenameCollection {
                to: "people".into(),
                drop_target: false
            }
        );
        assert_eq!(
            op("db.users.renameCollection(\"people\", true)"),
            MongoQueryOp::RenameCollection {
                to: "people".into(),
                drop_target: true
            }
        );
        assert_eq!(
            call("db.createCollection(\"logs\", { capped: true, size: 4096 })"),
            MongoCall {
                collection: "logs".into(),
                op: MongoQueryOp::CreateCollection {
                    options: doc! { "capped": true, "size": 4096 }
                }
            }
        );

        // Only bulkWrite counts documents; none of the new calls is exportable.
        for (stmt, verb, _) in &TABLE[13..] {
            let c = call(stmt);
            assert_eq!(c.reports_affected(), *verb == "bulkWrite", "{stmt}");
            assert!(!c.is_exportable(), "{stmt}");
        }
        let created = call("db.createCollection(\"x\")");
        assert!(!created.reports_affected());
        assert!(!created.is_exportable());
    }

    #[test]
    fn parse_bulk_write() {
        let stmt = "db.users.bulkWrite([\n\
            { insertOne: { document: { a: 1 } } },\n\
            { updateOne: { filter: { a: 1 }, update: { $set: { b: 1 } }, upsert: true } },\n\
            { updateMany: { filter: {}, update: [{ $set: { c: 1 } }] } },\n\
            { replaceOne: { filter: { a: 2 }, replacement: { a: 3 }, upsert: false } },\n\
            { deleteOne: { filter: { a: 3 } } },\n\
            { deleteMany: { filter: {} } },\n\
        ], { ordered: false })";
        assert_eq!(
            op(stmt),
            MongoQueryOp::BulkWrite {
                ops: vec![
                    BulkOp::Insert(doc! { "a": 1 }),
                    BulkOp::Update {
                        filter: doc! { "a": 1 },
                        update: UpdateSpec::Modifiers(doc! { "$set": { "b": 1 } }),
                        upsert: true,
                        many: false
                    },
                    BulkOp::Update {
                        filter: doc! {},
                        update: UpdateSpec::Pipeline(vec![doc! { "$set": { "c": 1 } }]),
                        upsert: false,
                        many: true
                    },
                    BulkOp::Replace {
                        filter: doc! { "a": 2 },
                        replacement: doc! { "a": 3 },
                        upsert: false
                    },
                    BulkOp::Delete {
                        filter: doc! { "a": 3 },
                        many: false
                    },
                    BulkOp::Delete {
                        filter: doc! {},
                        many: true
                    },
                ],
                ordered: false
            }
        );
        let c = call(stmt);
        assert_eq!((c.verb(), c.kind()), ("bulkWrite", Write));
        assert!(c.reports_affected());
        assert_eq!(
            op("db.users.bulkWrite([{ insertOne: { document: {} } }], { ordered: true })"),
            MongoQueryOp::BulkWrite {
                ops: vec![BulkOp::Insert(doc! {})],
                ordered: true
            }
        );
    }

    #[test]
    fn new_method_errors() {
        let check = |stmt: &str, message: &str, needle: &str| {
            assert_eq!(err(stmt), ParseError::new(message, at(stmt, needle)), "{stmt}");
        };
        let check_close = |stmt: &str, message: &str| {
            let close = stmt.chars().count() - 1;
            assert_eq!(err(stmt), ParseError::new(message, close), "{stmt}");
        };

        // findOneAnd* options.
        check(
            "db.a.findOneAndUpdate({}, { $set: { a: 1 } }, { returnOriginal: false })",
            "returnOriginal isn't a supported option of findOneAndUpdate. Supported: projection, \
             sort, upsert, returnDocument, returnNewDocument.",
            "returnOriginal",
        );
        check(
            "db.a.findOneAndReplace({}, { a: 1 }, { sort: {}, w: 1 })",
            "w isn't a supported option of findOneAndReplace. Supported: projection, sort, upsert, \
             returnDocument, returnNewDocument.",
            "w:",
        );
        check(
            "db.a.findOneAndDelete({}, { upsert: true })",
            "upsert isn't a supported option of findOneAndDelete. Supported: projection, sort.",
            "upsert",
        );
        check(
            "db.a.findOneAndUpdate({}, { $set: { a: 1 } }, { returnDocument: \"later\" })",
            "returnDocument must be \"before\" or \"after\".",
            "returnDocument",
        );
        check(
            "db.a.findOneAndUpdate({}, { $set: { a: 1 } }, { returnDocument: true })",
            "returnDocument must be \"before\" or \"after\".",
            "returnDocument",
        );
        check(
            "db.a.findOneAndUpdate({}, { $set: { a: 1 } }, \
             { returnDocument: \"after\", returnNewDocument: true })",
            "Use returnDocument or returnNewDocument, not both.",
            "returnNewDocument",
        );
        check(
            "db.a.findOneAndReplace({}, { a: 1 }, \
             { returnNewDocument: true, returnDocument: \"after\" })",
            "Use returnDocument or returnNewDocument, not both.",
            "returnDocument:",
        );
        check(
            "db.a.findOneAndUpdate({}, { $set: { a: 1 } }, { upsert: 1 })",
            "upsert must be true or false.",
            "upsert",
        );
        check(
            "db.a.findOneAndReplace({}, { a: 1 }, { returnNewDocument: \"yes\" })",
            "returnNewDocument must be true or false.",
            "returnNewDocument",
        );
        check(
            "db.a.findOneAndDelete({}, { projection: 1 })",
            "projection must be a document.",
            "projection",
        );
        check(
            "db.a.findOneAndDelete({}, { sort: [1] })",
            "sort must be a document.",
            "sort",
        );
        check(
            "db.a.findOneAndUpdate({}, { b: 1 })",
            "findOneAndUpdate needs update operators such as { $set: … }, or a pipeline.",
            "{ b",
        );
        check(
            "db.a.findOneAndUpdate({}, [])",
            "The update pipeline is empty.",
            "[",
        );
        check(
            "db.a.findOneAndReplace({}, { a: 1, $set: { a: 1 } })",
            "A replacement document can't contain update operators.",
            "{ a",
        );
        check(
            "db.a.findOneAndReplace({}, 5)",
            "findOneAndReplace expects a document as its second argument.",
            "5",
        );
        check_close(
            "db.a.findOneAndDelete()",
            "findOneAndDelete expects a document as its first argument.",
        );
        check(
            "db.a.findOneAndUpdate({}, { $set: { a: 1 } }, {}, {})",
            "findOneAndUpdate takes at most 3 arguments.",
            "{})",
        );
        check(
            "db.a.findOneAndDelete({}, 5)",
            "findOneAndDelete expects a document as its second argument.",
            "5",
        );

        // bulkWrite: every operation message sits at the first argument.
        check(
            "db.a.bulkWrite([])",
            "bulkWrite needs at least one operation.",
            "[",
        );
        check(
            "db.a.bulkWrite({ insertOne: {} })",
            "bulkWrite expects an array of operations as its first argument.",
            "{",
        );
        let op2 = "Operation 2 should be a document with one key: insertOne, updateOne, \
                   updateMany, replaceOne, deleteOne or deleteMany.";
        for second in [
            "{ insertTwo: {} }",
            "5",
            "{}",
            "{ deleteOne: { filter: {} }, deleteMany: { filter: {} } }",
        ] {
            check(
                &format!("db.a.bulkWrite([{{ insertOne: {{ document: {{}} }} }}, {second}])"),
                op2,
                "[",
            );
        }
        let first_two = "{ insertOne: { document: {} } }, { deleteOne: { filter: {} } }";
        let third = |op: &str| format!("db.a.bulkWrite([{first_two}, {op}], {{ ordered: true }})");
        check(
            &third("{ updateOne: { filter: {} } }"),
            "Operation 3 (updateOne) needs filter and update.",
            "[",
        );
        check(
            &third("{ updateOne: { update: { $set: { a: 1 } } } }"),
            "Operation 3 (updateOne) needs filter and update.",
            "[",
        );
        check(
            &third("{ updateOne: { filter: {}, update: { $set: { a: 1 } }, upserts: true } }"),
            "Operation 3 (updateOne): upserts isn't a supported field. Supported: filter, \
             update, upsert.",
            "[",
        );
        check(
            &third("{ updateOne: 5 }"),
            "Operation 3 (updateOne) needs filter and update.",
            "[",
        );
        let first = |op: &str| format!("db.a.bulkWrite([{op}])");
        check(
            &first("{ updateMany: { filter: {}, update: { b: 1 } } }"),
            "Operation 1 (updateMany): updateMany needs update operators such as { $set: … }, or \
             a pipeline.",
            "[",
        );
        check(
            &first("{ updateOne: { filter: {}, update: {} } }"),
            "Operation 1 (updateOne): The update document is empty.",
            "[",
        );
        check(
            &first("{ updateOne: { filter: {}, update: [] } }"),
            "Operation 1 (updateOne): The update pipeline is empty.",
            "[",
        );
        check(
            &first("{ updateOne: { filter: {}, update: 5 } }"),
            "Operation 1 (updateOne): update must be an update document or pipeline.",
            "[",
        );
        check(
            &first("{ updateOne: { filter: {}, update: { $set: { a: 1 } }, upsert: 1 } }"),
            "Operation 1 (updateOne): upsert must be true or false.",
            "[",
        );
        check(
            &first("{ replaceOne: { filter: {}, replacement: { $set: { a: 1 } } } }"),
            "Operation 1 (replaceOne): A replacement document can't contain update operators.",
            "[",
        );
        check(
            &first("{ replaceOne: { filter: {}, replacement: 5 } }"),
            "Operation 1 (replaceOne): replacement must be a document.",
            "[",
        );
        check(
            &first("{ replaceOne: { filter: {} } }"),
            "Operation 1 (replaceOne) needs filter and replacement.",
            "[",
        );
        check(
            &first("{ replaceOne: { filter: {}, replacement: {}, multi: true } }"),
            "Operation 1 (replaceOne): multi isn't a supported field. Supported: filter, \
             replacement, upsert.",
            "[",
        );
        check(
            &first("{ insertOne: { a: 1 } }"),
            "Operation 1 (insertOne): a isn't a supported field. Supported: document.",
            "[",
        );
        check(
            &first("{ insertOne: {} }"),
            "Operation 1 (insertOne) needs document.",
            "[",
        );
        check(
            &first("{ insertOne: { document: 5 } }"),
            "Operation 1 (insertOne): document must be a document.",
            "[",
        );
        check(
            &first("{ deleteMany: {} }"),
            "Operation 1 (deleteMany) needs filter.",
            "[",
        );
        check(
            &first("{ deleteOne: { filter: 5 } }"),
            "Operation 1 (deleteOne): filter must be a document.",
            "[",
        );
        check(
            &first("{ deleteOne: { filter: {}, upsert: true } }"),
            "Operation 1 (deleteOne): upsert isn't a supported field. Supported: filter.",
            "[",
        );
        check(
            "db.a.bulkWrite([{ insertOne: { document: {} } }], { w: 1 })",
            "w isn't a supported option of bulkWrite. Supported: ordered.",
            "w:",
        );
        check(
            "db.a.bulkWrite([{ insertOne: { document: {} } }], { ordered: 0 })",
            "ordered must be true or false.",
            "ordered",
        );

        // Collection and index helpers.
        check("db.a.drop(1)", "drop takes no arguments.", "1");
        check("db.a.getIndexes({})", "getIndexes takes no arguments.", "{");
        check_close(
            "db.a.createIndex()",
            "createIndex expects a document as its first argument.",
        );
        check(
            "db.a.createIndex({})",
            "createIndex needs at least one key, as in { email: 1 }.",
            "{",
        );
        check(
            "db.a.createIndex({ a: 1, b: true })",
            "The value of b must be a number or a string, such as 1, -1 or \"text\".",
            "b:",
        );
        check(
            "db.a.createIndex({ a: 1 }, { unique: true, name: 5 })",
            "name must be a string.",
            "name",
        );
        check(
            "db.a.createIndex({ a: 1 }, 5)",
            "createIndex expects a document as its second argument.",
            "5",
        );
        check(
            "db.a.createIndex({ a: 1 }, {}, {})",
            "createIndex takes at most 2 arguments.",
            "{})",
        );
        check(
            "db.a.dropIndex(\"*\")",
            "dropIndex(\"*\") isn't supported; name one index.",
            "\"*\"",
        );
        check(
            "db.a.dropIndex(5)",
            "dropIndex expects an index name or a keys document as its first argument.",
            "5",
        );
        check_close(
            "db.a.dropIndex()",
            "dropIndex expects an index name or a keys document as its first argument.",
        );
        check(
            "db.a.dropIndex(\"a_1\", \"b_1\")",
            "dropIndex takes at most 1 argument.",
            "\"b_1\"",
        );
        check_close(
            "db.a.renameCollection()",
            "renameCollection expects a collection name string as its first argument.",
        );
        check(
            "db.a.renameCollection(\"b\", 1)",
            "renameCollection expects true or false as its second argument.",
            "1",
        );
        check(
            "db.a.renameCollection(\"b\", true, {})",
            "renameCollection takes at most 2 arguments.",
            "{",
        );
        check_close(
            "db.createCollection()",
            "createCollection expects a collection name string as its first argument.",
        );
        check(
            "db.createCollection(\"x\", 5)",
            "createCollection expects a document as its second argument.",
            "5",
        );
        check(
            "db.createCollection(\"x\", {}, {})",
            "createCollection takes at most 2 arguments.",
            "{})",
        );
    }

    /// The options of createCollection and createIndex pass through as written, except the keys
    /// the app builds itself: `create` (the collection) and `key` (the index's keys).
    #[test]
    fn options_cant_override_the_built_keys() {
        let check = |stmt: &str, message: &str, needle: &str| {
            assert_eq!(err(stmt), ParseError::new(message, at(stmt, needle)), "{stmt}");
        };
        check(
            "db.createCollection(\"a\", { capped: true, create: \"b\" })",
            "create isn't a supported option of createCollection.",
            "create:",
        );
        check(
            "db.a.createIndex({ a: 1 }, { unique: true, key: { b: 1 } })",
            "key isn't a supported option of createIndex.",
            "key:",
        );
        check(
            "db.a.createIndex({ a: 1 }, { \"key\": { b: 1 } })",
            "key isn't a supported option of createIndex.",
            "\"key\"",
        );
        // Other options still pass through.
        assert_eq!(
            call("db.a.createIndex({ a: 1 }, { unique: true, keyed: 1 })").op,
            MongoQueryOp::CreateIndex {
                keys: doc! { "a": 1 },
                name: "a_1".into(),
                options: doc! { "unique": true, "keyed": 1 },
            }
        );
    }

    #[test]
    fn explain_chain() {
        let sorted = "db.users.find({ a: 1 }).sort({ a: 1 }).explain(\"executionStats\")";
        assert_eq!(
            op(sorted),
            MongoQueryOp::Explain {
                of: Box::new(MongoQueryOp::Find {
                    filter: doc! { "a": 1 },
                    projection: None,
                    sort: Some(doc! { "a": 1 }),
                    skip: None,
                    limit: None,
                    one: false
                }),
                verbosity: ExplainVerbosity::ExecutionStats
            }
        );
        let aggregate = "db.users.aggregate([]).explain()";
        assert_eq!(
            op(aggregate),
            MongoQueryOp::Explain {
                of: Box::new(MongoQueryOp::Aggregate { pipeline: vec![] }),
                verbosity: ExplainVerbosity::QueryPlanner
            }
        );
        let all = "db.users.find().limit(5).explain(\"allPlansExecution\")";
        assert_eq!(
            op(all),
            MongoQueryOp::Explain {
                of: Box::new(MongoQueryOp::Find {
                    filter: doc! {},
                    projection: None,
                    sort: None,
                    skip: None,
                    limit: Some(5),
                    one: false
                }),
                verbosity: ExplainVerbosity::AllPlansExecution
            }
        );
        let planner = "db.users.find().explain(\"queryPlanner\")";
        let MongoQueryOp::Explain { verbosity, .. } = op(planner) else {
            panic!()
        };
        assert_eq!(verbosity, ExplainVerbosity::QueryPlanner);
        assert_eq!(
            [
                ExplainVerbosity::QueryPlanner,
                ExplainVerbosity::ExecutionStats,
                ExplainVerbosity::AllPlansExecution
            ]
            .map(|v| v.as_str()),
            ["queryPlanner", "executionStats", "allPlansExecution"]
        );
        for stmt in [sorted, aggregate, all, planner] {
            let c = call(stmt);
            assert_eq!((c.verb(), c.kind()), ("explain", Read), "{stmt}");
            assert_eq!(classify(stmt), (Read, "explain".to_string()), "{stmt}");
            assert!(!c.is_exportable(), "{stmt}");
            assert!(!c.reports_affected(), "{stmt}");
            assert_eq!(c.output_stage(), None, "{stmt}");
        }

        let stmt = "db.users.find().explain().limit(1)";
        assert_eq!(
            err(stmt),
            ParseError::new("explain must be the last call.", at(stmt, "limit"))
        );
        let stmt = "db.users.find().explain(\"full\")";
        assert_eq!(
            err(stmt),
            ParseError::new(
                "explain expects \"queryPlanner\", \"executionStats\" or \"allPlansExecution\".",
                at(stmt, "\"full\"")
            )
        );
        assert_eq!(
            message("db.users.find().explain(true)"),
            "explain expects \"queryPlanner\", \"executionStats\" or \"allPlansExecution\"."
        );
        assert_eq!(
            message("db.users.find().explain(\"queryPlanner\", 1)"),
            "explain takes at most 1 argument."
        );
        let stmt = "db.users.explain().find()";
        assert_eq!(
            err(stmt),
            ParseError::new(
                "Write explain after the call, as in db.users.find().explain().",
                at(stmt, "explain")
            )
        );
        let stmt = "db.users.countDocuments().explain()";
        assert_eq!(
            err(stmt),
            ParseError::new(
                "explain isn't supported after countDocuments. Only find and aggregate can be \
                 followed by another call.",
                at(stmt, "explain")
            )
        );
        let stmt = "db.users.findOne().explain()";
        assert_eq!(
            err(stmt),
            ParseError::new(
                "explain isn't supported after findOne. Only find and aggregate can be followed \
                 by another call.",
                at(stmt, "explain")
            )
        );
        for stmt in [
            "db.users.aggregate([{ $out: \"x\" }]).explain(\"executionStats\")",
            "db.users.aggregate([{ $merge: { into: \"x\" } }, { $match: {} }]).explain()",
        ] {
            assert_eq!(
                err(stmt),
                ParseError::new(
                    "explain isn't supported on a pipeline with $out or $merge.",
                    at(stmt, "explain")
                ),
                "{stmt}"
            );
        }

        // Whatever parse says, a statement whose last call is explain is a read named explain.
        for stmt in [
            "db.users.find().explain(\"full\")",
            "db.users.countDocuments().explain()",
            "db.users.aggregate([{ $out: \"x\" }]).explain(\"executionStats\")",
            "db.users.deleteMany({}).explain()",
            "db.users.find().explain(",
            "db.users\n  .find({ a: [1, 2] })\n  .explain()",
        ] {
            assert_eq!(classify(stmt), (Read, "explain".to_string()), "{stmt}");
        }
        // An explain that isn't the last call, or is inside an argument, isn't.
        assert_eq!(
            classify("db.users.find().explain().limit(1)"),
            (Read, "find".into())
        );
        assert_eq!(
            classify("db.users.deleteMany({ a: x.explain() })"),
            (Write, "deleteMany".into())
        );
    }

    #[test]
    fn database_level_calls() {
        let created = call("db.createCollection(\"x\")");
        assert_eq!(
            created,
            MongoCall {
                collection: "x".into(),
                op: MongoQueryOp::CreateCollection { options: doc! {} }
            }
        );
        assert_eq!((created.verb(), created.kind()), ("createCollection", Write));
        assert_eq!(
            classify("db.createCollection(\"x\")"),
            (Write, "createCollection".into())
        );
        let db_only =
            "db.<name>() isn't supported in query tabs. Supported: db.createCollection().";
        for name in ["dropDatabase", "getCollectionNames", "runCommand"] {
            let stmt = format!("db.{name}({{}})");
            assert_eq!(
                err(&stmt),
                ParseError::new(db_only.replace("<name>", name), 3),
                "{stmt}"
            );
        }
        let stmt = "db.users.createCollection()";
        assert_eq!(
            err(stmt),
            ParseError::new(
                "createCollection is called on db, as in db.createCollection(\"name\").",
                9
            )
        );
        assert_eq!(
            message("db.createCollection(\"x\").find()"),
            "Unexpected text after the statement."
        );

        assert_eq!(classify("db.dropDatabase()"), (Write, "dropDatabase".into()));
        assert_eq!(classify("db.runCommand({})"), (Write, "runCommand".into()));
        assert_eq!(
            classify("db.getCollectionNames()"),
            (Read, "getCollectionNames".into())
        );
        assert_eq!(
            classify("db.createCollection.find()"),
            (Read, "find".into())
        );
        assert_eq!(classify("db.dropDatabase("), (Write, "dropDatabase".into()));
        assert_eq!(classify("db.getCollection(\"x\")"), (Read, String::new()));

        // A property named like a database method is a collection, as getCollection is.
        assert_eq!(
            call("db.createCollection.find()").collection,
            "createCollection"
        );
        assert_eq!(call("db.dropDatabase.drop()").collection, "dropDatabase");
    }

    #[test]
    fn legacy_write_names_classify_as_writes() {
        for name in ["remove", "insert", "update", "save", "findAndModify"] {
            let stmt = format!("db.users.{name}({{}})");
            assert_eq!(classify(&stmt), (Write, name.to_string()), "{stmt}");
            assert_eq!(
                err(&stmt),
                ParseError::new(
                    format!("{name} isn't supported in query tabs. {SUPPORTED}"),
                    9
                ),
                "{stmt}"
            );
        }
    }

    #[test]
    fn index_name_rule() {
        assert_eq!(index_name(&doc! { "a": 1, "b": -1 }), "a_1_b_-1");
        assert_eq!(index_name(&doc! { "t": "text" }), "t_text");
        assert_eq!(index_name(&doc! { "a.b": 1 }), "a.b_1");
        assert_eq!(index_name(&doc! { "loc": "2dsphere" }), "loc_2dsphere");
        assert_eq!(index_name(&doc! { "a": 1.0, "b": -1_i64 }), "a_1_b_-1");
        assert_eq!(index_name(&doc! { "a": 0.5 }), "a_0.5");
    }
}
