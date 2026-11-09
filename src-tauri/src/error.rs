use serde::ser::{SerializeMap, Serializer};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AppError {
    #[error("connection error: {0}")]
    Connection(String),
    #[error("query error: {0}")]
    Query(String),
    #[error("filter error at {position}: {message}")]
    Filter { message: String, position: usize },
    #[error("not found: {0}")]
    NotFound(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("secret error: {0}")]
    Secret(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The outcome is unknown (the connection failed during COMMIT): the change may or may not
    /// have been applied, so the UI must not claim that nothing was saved.
    #[error("uncertain outcome: {0}")]
    Uncertain(String),
    /// A save on a server without transactions stopped at a failing change; the changes before
    /// it stay saved (MongoDB only). `applied`: the index of the failing change in `set.changes`
    /// (skipped empty updates count as settled). `uncertain`: that change's write got no
    /// answer, so it may or may not have been applied; the UI asks before saving again.
    #[error("partly saved: {message}")]
    PartlySaved { message: String, applied: usize, uncertain: bool },
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    fn kind(&self) -> &'static str {
        match self {
            AppError::Connection(_) => "connection",
            AppError::Query(_) => "query",
            AppError::Filter { .. } => "filter",
            AppError::NotFound(_) => "notFound",
            AppError::Storage(_) => "storage",
            AppError::Secret(_) => "secret",
            AppError::Unsupported(_) => "unsupported",
            AppError::Uncertain(_) => "uncertain",
            AppError::PartlySaved { .. } => "partlySaved",
        }
    }

    /// The message without the kind prefix that `Display` adds.
    pub fn message(&self) -> &str {
        match self {
            AppError::Connection(m)
            | AppError::Query(m)
            | AppError::NotFound(m)
            | AppError::Storage(m)
            | AppError::Secret(m)
            | AppError::Unsupported(m)
            | AppError::Uncertain(m) => m,
            AppError::Filter { message, .. } | AppError::PartlySaved { message, .. } => message,
        }
    }
}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let len = match self {
            AppError::Filter { .. } | AppError::PartlySaved { uncertain: false, .. } => 3,
            AppError::PartlySaved { uncertain: true, .. } => 4,
            _ => 2,
        };
        let mut m = s.serialize_map(Some(len))?;
        m.serialize_entry("kind", self.kind())?;
        m.serialize_entry("message", self.message())?;
        match self {
            AppError::Filter { position, .. } => m.serialize_entry("position", position)?,
            AppError::PartlySaved { applied, uncertain, .. } => {
                m.serialize_entry("applied", applied)?;
                // Only when set, so a known-failure partial save keeps its original shape.
                if *uncertain {
                    m.serialize_entry("uncertain", &true)?;
                }
            }
            _ => {}
        }
        m.end()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::WorkerCrashed
            | sqlx::Error::Protocol(_) => AppError::Connection(e.to_string()),
            sqlx::Error::Database(db) => AppError::Query(db.message().to_string()),
            other => AppError::Query(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_failures_are_connection_errors() {
        for e in [sqlx::Error::PoolClosed, sqlx::Error::WorkerCrashed, sqlx::Error::PoolTimedOut] {
            assert!(matches!(AppError::from(e), AppError::Connection(_)));
        }
        assert!(matches!(AppError::from(sqlx::Error::RowNotFound), AppError::Query(_)));
    }

    /// The wire form of every kind: `kind` and `message`, plus `position` for a filter error and
    /// `applied` for a partial save (and `uncertain` only when its next write's outcome is unknown).
    #[test]
    fn errors_serialize_by_kind() {
        let json = |e: AppError| serde_json::to_string(&e).unwrap();
        assert_eq!(json(AppError::Query("q".into())), r#"{"kind":"query","message":"q"}"#);
        assert_eq!(json(AppError::Uncertain("u".into())), r#"{"kind":"uncertain","message":"u"}"#);
        assert_eq!(
            json(AppError::Filter { message: "f".into(), position: 4 }),
            r#"{"kind":"filter","message":"f","position":4}"#
        );
        let partly = AppError::PartlySaved { message: "Saved 1 of 3 changes".into(), applied: 1, uncertain: false };
        assert_eq!(partly.message(), "Saved 1 of 3 changes");
        assert_eq!(json(partly), r#"{"kind":"partlySaved","message":"Saved 1 of 3 changes","applied":1}"#);
        let unknown = AppError::PartlySaved { message: "Saved 2 of 3 changes".into(), applied: 2, uncertain: true };
        assert_eq!(json(unknown), r#"{"kind":"partlySaved","message":"Saved 2 of 3 changes","applied":2,"uncertain":true}"#);
    }

    #[test]
    fn message_has_no_kind_prefix() {
        let e = AppError::Query("relation does not exist".into());
        assert_eq!(e.message(), "relation does not exist");
        assert_eq!(e.to_string(), "query error: relation does not exist");
    }
}
