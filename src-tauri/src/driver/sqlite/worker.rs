//! One SQLite connection on its own thread.
//!
//! rusqlite is synchronous, so every connection lives on a dedicated thread and takes jobs
//! (closures over `&mut Connection`) from a channel, one at a time, in order. `call` awaits a
//! job's result without blocking the async runtime. `interrupt_current` stops the statement
//! running on the thread right now (from any thread, while a `call` is in flight).
//!
//! Every connection:
//! - is opened without `SQLITE_OPEN_CREATE`, read-only for read-only profiles;
//! - never checkpoints a WAL file when it closes (`SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE`), so closing
//!   doesn't rewrite the user's main file;
//! - has foreign keys on (the constraints the schema declares are honoured, as in DB Browser for
//!   SQLite) before the startup SQL runs, which may turn them off again.
//!
//! Lifetimes:
//! - The thread ends when its job channel closes (`shutdown`, or drop) after the job it is
//!   running; right after a job that asks for it (`call_then` with `After::End`); or right away
//!   when a job panics. Its connection is closed on the thread (which rolls back any transaction
//!   left open), and then `shutdown` returns. A panic is logged with its message and the job's
//!   closure (which names the operation). In release builds a panic aborts the app anyway
//!   (`panic = "abort"` in `Cargo.toml`).
//! - A job whose caller has gone away (its `call` future was dropped) before the job's turn is
//!   skipped. Dropping a `call` future while its job runs interrupts that job, and only that job:
//!   an interrupt can't land on a later one. (SQLite's interrupt stops running statements only,
//!   so a job caught between two statements runs its next one.)
//! - `shutdown(grace)` takes no new jobs, lets those queued or running finish, and waits up to
//!   `grace` for the thread to end. Dropping the worker is the abrupt path: it interrupts the
//!   running statement (nobody can be waiting for it then) and never waits for the thread, so it
//!   is safe anywhere, including in async code and in drop guards.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::config::DbConfig;
use rusqlite::{Connection, InterruptHandle, OpenFlags};
use tokio::sync::{oneshot, watch};

use super::{SqliteConfig, SQLITE_BUSY_TIMEOUT};
use crate::error::{AppError, AppResult};

type Job = Box<dyn FnOnce(&mut Connection, &Shared) + Send>;

/// State the worker and its thread share.
struct Shared {
    /// The id of the job running on the thread; 0 when none. Changed and read under the lock, so
    /// an interrupt aimed at a job can't land after that job has ended.
    running: Mutex<u64>,
    /// Set when the thread ends or is about to (a job panicked).
    ended: AtomicBool,
}

impl Shared {
    fn running(&self) -> MutexGuard<'_, u64> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct SqliteWorker {
    /// `None` once stopped: no new jobs are taken.
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
    interrupt: InterruptHandle,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    /// Its sender lives on the thread and is dropped when the thread ends.
    exited: watch::Receiver<()>,
}

/// What a worker answers once it takes no more work.
pub(super) fn closed() -> AppError {
    AppError::Connection("The database connection is closed.".into())
}

/// What a job asks of its worker when it is done (`call_then`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum After {
    /// Take the next job.
    Continue,
    /// End the thread, closing the connection (SQLite rolls back a transaction left open).
    End,
}

/// A panic's message, when it has one.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(no message)")
}

/// Interrupts the job of a `call` whose future is dropped before the job's result arrived.
struct CallGuard<'a, T> {
    worker: &'a SqliteWorker,
    id: u64,
    reply: oneshot::Receiver<T>,
    done: bool,
}

impl<T> Drop for CallGuard<'_, T> {
    fn drop(&mut self) {
        if !self.done {
            // Closed first: a job that hasn't started sees it and is skipped; one that has
            // started is marked running, and is interrupted below.
            self.reply.close();
            self.worker.interrupt_if_running(self.id);
        }
    }
}

impl SqliteWorker {
    /// Opens the file (never creating it) on a new thread, sets the busy timeout and runs the
    /// startup SQL. Blocks until that is done: call it off the async runtime.
    pub fn open(cfg: &SqliteConfig) -> AppResult<SqliteWorker> {
        if !cfg.path.exists() {
            return Err(AppError::Connection(format!("The file doesn't exist: {}", cfg.path.display())));
        }
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let (opened_tx, opened_rx) = mpsc::sync_channel::<AppResult<InterruptHandle>>(1);
        let (exit_tx, exited) = watch::channel(());
        let shared = Arc::new(Shared { running: Mutex::new(0), ended: AtomicBool::new(false) });
        let cfg = cfg.clone();
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("sqlite-worker".into())
            .spawn(move || {
                // Declared first, so dropped last: after the connection has closed.
                let _exit = OnExit { shared: thread_shared.clone(), _signal: exit_tx };
                let mut conn = match open_connection(&cfg) {
                    Ok(conn) => conn,
                    Err(e) => {
                        let _ = opened_tx.send(Err(e));
                        return;
                    }
                };
                if opened_tx.send(Ok(conn.get_interrupt_handle())).is_err() {
                    return;
                }
                drop(opened_tx);
                while let Ok(job) = jobs_rx.recv() {
                    job(&mut conn, &thread_shared);
                    if thread_shared.ended.load(Ordering::SeqCst) {
                        break;
                    }
                }
                // `conn` is closed here, on its own thread.
            })
            .map_err(|e| AppError::Connection(format!("Couldn't start the database thread: {e}")))?;
        let interrupt = opened_rx.recv().map_err(|_| closed())??;
        Ok(SqliteWorker { jobs: Mutex::new(Some(jobs_tx)), interrupt, shared, next_id: AtomicU64::new(1), exited })
    }

    /// `open` off the async runtime: opening and the startup SQL block (a locked file waits out
    /// the busy timeout).
    pub async fn open_async(cfg: &SqliteConfig) -> AppResult<SqliteWorker> {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || SqliteWorker::open(&cfg))
            .await
            .map_err(|e| AppError::Connection(e.to_string()))?
    }

    /// Runs `f` on the worker's thread with its connection and returns what it returns. Jobs run
    /// one at a time, in the order of the calls. Fails with "The database connection is closed."
    /// once the worker is shut down or its thread has ended. Dropping the returned future skips
    /// the job if it hasn't started, and interrupts it if it runs.
    pub async fn call<T, F>(&self, f: F) -> AppResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> T + Send + 'static,
    {
        self.run_job(std::any::type_name::<F>(), move |c| (f(c), After::Continue)).await
    }

    /// `call`, for a job that may end its worker: with `After::End` the thread ends right after
    /// the job and closes the connection; the caller still gets the job's result, and from then
    /// on the worker `is_closed`.
    pub(super) async fn call_then<T, F>(&self, f: F) -> AppResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> (T, After) + Send + 'static,
    {
        self.run_job(std::any::type_name::<F>(), f).await
    }

    /// `call_then`; `operation` (the caller's closure type, which names the function it was
    /// written in) is logged if the job panics.
    async fn run_job<T, F>(&self, operation: &'static str, f: F) -> AppResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> (T, After) + Send + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (reply_tx, reply_rx) = oneshot::channel();
        let job: Job = Box::new(move |conn, shared| {
            *shared.running() = id;
            if reply_tx.is_closed() {
                *shared.running() = 0;
                return;
            }
            let out = catch_unwind(AssertUnwindSafe(|| f(conn)));
            *shared.running() = 0;
            match out {
                Ok((value, after)) => {
                    // Marked before the reply is sent, so the caller already sees `is_closed`.
                    if after == After::End {
                        shared.ended.store(true, Ordering::SeqCst);
                    }
                    let _ = reply_tx.send(value);
                }
                // The connection may be mid-way through anything: the thread ends and closes it.
                // Marked before the reply is dropped, so the caller already sees `is_closed`.
                Err(payload) => {
                    eprintln!("a SQLite job panicked ({operation}): {}; its connection closes", panic_text(payload.as_ref()));
                    shared.ended.store(true, Ordering::SeqCst);
                }
            }
        });
        {
            let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
            jobs.as_ref().ok_or_else(closed)?.send(job).map_err(|_| closed())?;
        }
        let mut guard = CallGuard { worker: self, id, reply: reply_rx, done: false };
        let result = (&mut guard.reply).await;
        guard.done = true;
        result.map_err(|_| closed())
    }

    /// Stops the statement running on the worker's thread now, whichever job it belongs to; it
    /// fails with `SQLITE_INTERRUPT`. The worker stays usable. (Public for the integration
    /// tests only.)
    #[doc(hidden)]
    pub fn interrupt_current(&self) {
        self.interrupt.interrupt();
    }

    /// Interrupts job `id` if it is the one running; otherwise does nothing.
    pub(super) fn interrupt_if_running(&self, id: u64) {
        let running = self.shared.running();
        // Held while interrupting: the job can't end, nor the next start, in between.
        if *running == id {
            self.interrupt.interrupt();
        }
    }

    /// The id of the most recent `call`.
    #[cfg(test)]
    fn last_job_id(&self) -> u64 {
        self.next_id.load(Ordering::SeqCst) - 1
    }

    /// Whether the worker takes no more work: shut down, or its thread has ended.
    pub fn is_closed(&self) -> bool {
        self.shared.ended.load(Ordering::SeqCst) || self.jobs.lock().unwrap_or_else(|e| e.into_inner()).is_none()
    }

    /// Takes no new jobs; those already queued or running finish, then the thread closes the
    /// connection. Waits up to `grace` for that and returns whether the thread has ended. (Public
    /// for the integration tests only.)
    #[doc(hidden)]
    pub async fn shutdown(&self, grace: Duration) -> bool {
        self.stop();
        let mut exited = self.exited.clone();
        // `changed` fails once the thread's sender is gone; nothing is ever sent.
        tokio::time::timeout(grace, async { while exited.changed().await.is_ok() {} }).await.is_ok()
    }

    /// Takes no new jobs.
    fn stop(&self) {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

impl Drop for SqliteWorker {
    fn drop(&mut self) {
        // No `call` can be awaiting a result here (it borrows the worker), so whatever runs is
        // abandoned work. Queued jobs are skipped (their callers are gone).
        self.interrupt.interrupt();
        self.stop();
    }
}

/// Marks the worker as ended when its thread finishes, however it finishes, then (as its
/// `_signal` field drops) lets `shutdown` return.
struct OnExit {
    shared: Arc<Shared>,
    _signal: watch::Sender<()>,
}

impl Drop for OnExit {
    fn drop(&mut self) {
        self.shared.ended.store(true, Ordering::SeqCst);
    }
}

fn open_connection(cfg: &SqliteConfig) -> AppResult<Connection> {
    let mode = if cfg.read_only { OpenFlags::SQLITE_OPEN_READ_ONLY } else { OpenFlags::SQLITE_OPEN_READ_WRITE };
    // Never SQLITE_OPEN_CREATE: a missing file is an error, not a new database.
    let conn = Connection::open_with_flags(&cfg.path, mode | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(connection_error)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).map_err(connection_error)?;
    conn.busy_timeout(SQLITE_BUSY_TIMEOUT).map_err(connection_error)?;
    conn.execute_batch("PRAGMA foreign_keys = ON").map_err(connection_error)?;
    if let Some(sql) = cfg.startup_sql.as_deref() {
        conn.execute_batch(sql).map_err(|e| AppError::Connection(format!("Startup SQL failed: {e}")))?;
    }
    Ok(conn)
}

fn connection_error(e: rusqlite::Error) -> AppError {
    AppError::Connection(e.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// Counts to `n` without touching a table: long enough to interrupt.
    fn count_to(c: &mut Connection, n: i64) -> rusqlite::Result<i64> {
        c.query_row(
            "with recursive g(x) as (select 1 union all select x + 1 from g where x < ?1) select count(*) from g",
            [n],
            |r| r.get(0),
        )
    }

    fn worker() -> (tempfile::TempDir, SqliteWorker) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.sqlite");
        Connection::open(&path).unwrap().execute_batch("create table t (x)").unwrap();
        let w = SqliteWorker::open(&SqliteConfig { path, read_only: false, startup_sql: None }).unwrap();
        (dir, w)
    }

    #[tokio::test]
    async fn an_interrupt_meant_for_a_finished_job_misses_the_next_one() {
        let (_dir, w) = worker();
        w.call(|_| ()).await.unwrap();
        let finished = w.last_job_id();
        let (long, ()) = tokio::join!(w.call(|c| count_to(c, 2_000_000)), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            w.interrupt_if_running(finished);
        });
        assert_eq!(long.unwrap().unwrap(), 2_000_000);

        // The same call, aimed at the job that runs, stops it.
        let (long, ()) = tokio::join!(w.call(|c| count_to(c, i64::MAX)), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            w.interrupt_if_running(w.last_job_id());
        });
        let err = long.unwrap().unwrap_err();
        assert_eq!(err.sqlite_error_code(), Some(rusqlite::ErrorCode::OperationInterrupted));
    }

    #[tokio::test]
    async fn closed_resolves_once_the_thread_has_ended() {
        let (_dir, w) = worker();
        let slow = w.call(|_| std::thread::sleep(Duration::from_millis(300)));
        let started = Instant::now();
        let ((), ()) = tokio::join!(async { slow.await.unwrap() }, async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(w.shutdown(Duration::from_secs(5)).await);
        });
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert!(w.shared.ended.load(Ordering::SeqCst));
        // Already ended: resolves at once.
        assert!(w.shutdown(Duration::ZERO).await);
    }

    #[tokio::test]
    async fn shutdown_gives_up_after_its_grace() {
        let (_dir, w) = worker();
        let slow = w.call(|_| std::thread::sleep(Duration::from_millis(500)));
        let ((), ended) = tokio::join!(async { slow.await.unwrap() }, async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let started = Instant::now();
            let ended = w.shutdown(Duration::from_millis(100)).await;
            assert!(started.elapsed() < Duration::from_millis(400), "{:?}", started.elapsed());
            ended
        });
        assert!(!ended, "the job was still running");
        assert!(w.is_closed());
        assert_eq!(w.call(|_| ()).await.unwrap_err(), closed());
        assert!(w.shutdown(Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn a_job_that_asks_to_end_its_worker_ends_it_after_replying() {
        let (dir, w) = worker();
        // The job leaves a write transaction open, then ends the worker.
        let value = w
            .call_then(|c| {
                c.execute_batch("begin immediate; insert into t values (1)").unwrap();
                (7, After::End)
            })
            .await
            .unwrap();
        assert_eq!(value, 7);
        assert!(w.is_closed());
        assert_eq!(w.call(|_| ()).await.unwrap_err(), closed());
        assert!(w.shutdown(Duration::from_secs(5)).await);
        // Closing the connection rolled the transaction back and let go of the write lock.
        let other = Connection::open(dir.path().join("w.sqlite")).unwrap();
        other.execute_batch("begin immediate; rollback").unwrap();
        assert_eq!(other.query_row("select count(*) from t", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn panic_text_reads_str_and_string_payloads() {
        assert_eq!(panic_text(&"boom"), "boom");
        assert_eq!(panic_text(&String::from("bang")), "bang");
        assert_eq!(panic_text(&42), "(no message)");
    }
}
