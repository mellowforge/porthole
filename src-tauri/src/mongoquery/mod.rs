//! The mongosh subset that MongoDB query tabs accept: tokens, literal values, statements and
//! the editor's analysis of a tab's text.

pub mod call;
pub mod format;
pub mod lex;
pub mod literal;
pub mod split;

pub use call::{
    BulkOp, ExplainVerbosity, FindAndAction, MongoCall, MongoQueryOp, UpdateSpec, classify,
    index_name, parse,
};
pub use split::{MongoSpan, analyze, find_params, split};

/// A parse failure. `at` is a 0-based char offset into the text given to the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub at: usize,
}

impl ParseError {
    pub(crate) fn new(message: impl Into<String>, at: usize) -> Self {
        Self {
            message: message.into(),
            at,
        }
    }
}
