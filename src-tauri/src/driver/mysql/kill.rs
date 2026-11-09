//! Time limits and Stop: a running statement is stopped with `KILL QUERY` from a second
//! connection, which fails it with errno 1317 and leaves its connection usable. A statement is
//! never stopped by abandoning it on purpose: it would go on running on the server. When the
//! KILL itself can't be done, `bounded` says so (`Bounded::Abandoned`).

use std::time::Duration;

use futures_util::future::BoxFuture;
use mysql_async::prelude::Queryable;
use mysql_async::Conn;

use super::{error_message, MysqlConfig};
use crate::error::{AppError, AppResult};

/// How long a killed statement may take to report its failure before it is abandoned.
pub const KILL_WAIT: Duration = Duration::from_secs(2);

/// How long `kill_query` may take in all (opening its connection, setting it up, running the
/// KILL), whatever the profile's connect timeout. So a bounded statement gives up at most
/// `limit + KILL_CONNECT_WAIT + KILL_WAIT` after it started.
pub const KILL_CONNECT_WAIT: Duration = Duration::from_secs(3);

/// The server's errno for a statement stopped by `KILL QUERY`.
pub const ER_QUERY_INTERRUPTED: u16 = 1317;

/// Why a bounded statement has no result.
#[derive(Debug)]
pub enum Bounded {
    /// The limit passed and the statement was stopped by the KILL (errno 1317). The connection
    /// is idle and may be used again.
    TimedOut,
    /// The limit passed and the statement could not be stopped: the KILL failed, or the
    /// statement didn't return within `KILL_WAIT` of it. It was abandoned and may still be
    /// running, with unread results on the connection. The connection must be dropped or
    /// disconnected: never used again, never returned to a pool.
    Abandoned,
    /// The statement failed by itself, before the limit or while it was being killed. The
    /// connection is as the error left it (`mysql_async::Error::is_fatal` tells).
    Failed(mysql_async::Error),
}

/// Stops the statement running on connection `id`, from a new connection opened with the same
/// options. Fails if that takes longer than `KILL_CONNECT_WAIT`.
pub async fn kill_query(cfg: &MysqlConfig, id: u32) -> AppResult<()> {
    let kill = async {
        let mut conn = cfg.open().await?;
        let killed = conn.query_drop(format!("KILL QUERY {id}")).await;
        // Closing the connection doesn't hold up the caller.
        tokio::spawn(async move { conn.disconnect().await.ok() });
        killed.map_err(|e| AppError::Query(error_message(&e)))
    };
    match tokio::time::timeout(KILL_CONNECT_WAIT, kill).await {
        Ok(done) => done,
        Err(_) => Err(AppError::Connection(format!(
            "Timed out after {} s stopping the statement of connection {id}",
            KILL_CONNECT_WAIT.as_secs()
        ))),
    }
}

/// Runs `f(conn)` for at most `limit`. At the limit the statement is killed, and `f` is given
/// up to `KILL_WAIT` more to return:
/// - with errno 1317 → `TimedOut` (the connection is usable);
/// - with another error → `Failed`;
/// - with its result (it finished just as it was killed) → that result.
///
/// If the KILL fails, or `f` hasn't returned after `KILL_WAIT`, `f` is dropped mid-statement and
/// the result is `Abandoned`: the caller must not use `conn` again (see `Bounded::Abandoned`).
pub async fn bounded<T, F>(cfg: &MysqlConfig, conn: &mut Conn, limit: Duration, f: F) -> Result<T, Bounded>
where
    F: for<'c> FnOnce(&'c mut Conn) -> BoxFuture<'c, mysql_async::Result<T>>,
{
    let id = conn.id();
    let mut run = f(conn);
    if let Ok(done) = tokio::time::timeout(limit, &mut run).await {
        return done.map_err(Bounded::Failed);
    }
    if let Err(e) = kill_query(cfg, id).await {
        eprintln!("couldn't stop MySQL connection {id}'s statement at its time limit: {}", e.message());
        return Err(Bounded::Abandoned);
    }
    let outcome = tokio::time::timeout(KILL_WAIT, run).await.ok();
    if outcome.is_none() {
        eprintln!("MySQL connection {id}'s statement didn't stop within {KILL_WAIT:?} of KILL QUERY");
    }
    after_kill(outcome)
}

/// What `f` returned after the KILL, or `None` when it didn't return within `KILL_WAIT`.
fn after_kill<T>(outcome: Option<mysql_async::Result<T>>) -> Result<T, Bounded> {
    match outcome {
        None => Err(Bounded::Abandoned),
        Some(Err(mysql_async::Error::Server(e))) if e.code == ER_QUERY_INTERRUPTED => Err(Bounded::TimedOut),
        Some(Err(e)) => Err(Bounded::Failed(e)),
        Some(Ok(v)) => Ok(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::{IoError, ServerError};

    fn server_error(code: u16) -> mysql_async::Error {
        mysql_async::Error::Server(ServerError { code, message: "m".into(), state: "HY000".into() })
    }

    #[test]
    fn mysql_after_kill_tells_the_outcomes_apart() {
        assert!(matches!(after_kill::<i64>(Some(Err(server_error(1317)))), Err(Bounded::TimedOut)));
        assert!(matches!(after_kill::<i64>(None), Err(Bounded::Abandoned)));
        assert!(matches!(after_kill::<i64>(Some(Err(server_error(1146)))), Err(Bounded::Failed(_))));
        let io = mysql_async::Error::Io(IoError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset)));
        match after_kill::<i64>(Some(Err(io))) {
            Err(Bounded::Failed(e)) => assert!(e.is_fatal()),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(after_kill(Some(Ok(7))).unwrap(), 7);
    }
}
