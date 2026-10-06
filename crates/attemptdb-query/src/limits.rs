//! Bounded execution for the surfaces an agent or a browser drives.
//!
//! The CLI is the owner at a prompt; MCP and the local web UI are a language
//! model and a page. For those, one statement must not be able to take the
//! machine down or flood a context window, whatever it asks for. A
//! [`QueryLimits`] bounds every way a statement can grow:
//!
//! - **rows**: the cap is pushed into the plan (`LIMIT cap + 1`, the extra
//!   row is how truncation is detected) before anything is collected, so
//!   `SELECT * FROM generate_series(1, 30000000)` produces `cap + 1` rows,
//!   not thirty million;
//! - **bytes**: collection stops once the rows held are far past the byte
//!   budget, and [`QueryResult::capped`](crate::QueryResult::capped)
//!   converts only what fits into JSON, CSV or a table;
//! - **time**: the statement runs as a task on a private runtime and is
//!   aborted, which drops the DataFusion stream, when the deadline passes or
//!   the caller cancels;
//! - **memory**: the statement runs under a memory pool of its own with
//!   spilling disabled, so a runaway join or sort fails instead of
//!   exhausting RAM;
//! - **retraction**: content columns of retracted rows are returned as NULL
//!   (see [`mask`](crate::mask)); retraction is not redaction on the owner's
//!   CLI, but it is on these surfaces.

use crate::error::{QueryError, Result};
use crate::result::QueryResult;
use crate::{ResultKind, read_only_sql};
use datafusion::arrow::array::{Array, AsArray, RecordBatch};
use datafusion::arrow::datatypes::DataType;
use datafusion::execution::disk_manager::{DiskManagerBuilder, DiskManagerMode};
use datafusion::execution::memory_pool::GreedyMemoryPool;
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use datafusion::logical_expr::LogicalPlan;
use datafusion::physical_plan::{SendableRecordBatchStream, execute_stream};
use datafusion::prelude::SessionContext;
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Notify;

/// Default wall-clock limit per statement on the agent and UI surfaces.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
/// Default memory pool per statement.
pub const DEFAULT_MEMORY_BYTES: usize = 1 << 30;
/// Default serialised-size budget of one result for an agent (MCP).
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;
/// Default cap on one cell's text (cut, with a marker, beyond it).
pub const DEFAULT_MAX_CELL_BYTES: usize = 8 * 1024;

/// What one statement may cost. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct QueryLimits {
    /// Rows kept; the plan is cut at one more than this to detect that
    /// there were more.
    pub max_rows: usize,
    /// Serialised-size budget of the kept rows (compact JSON bytes).
    pub max_bytes: usize,
    /// One cell's text is cut at this many bytes.
    pub max_cell_bytes: usize,
    /// Wall-clock limit; `None` for none.
    pub timeout: Option<Duration>,
    /// Memory pool for the statement; `None` for none.
    pub memory_bytes: Option<usize>,
    /// Return content columns of retracted rows as NULL.
    pub mask_retracted: bool,
    /// Caller-side cancellation (an MCP `notifications/cancelled`).
    pub cancel: Option<CancelToken>,
}

impl QueryLimits {
    /// The agent/UI defaults with `max_rows` rows and `max_bytes` bytes.
    pub fn new(max_rows: usize, max_bytes: usize) -> Self {
        Self {
            max_rows: max_rows.max(1),
            max_bytes,
            max_cell_bytes: DEFAULT_MAX_CELL_BYTES,
            timeout: Some(DEFAULT_TIMEOUT),
            memory_bytes: Some(DEFAULT_MEMORY_BYTES),
            mask_retracted: true,
            cancel: None,
        }
    }

    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Rows held while collecting may exceed the byte budget by this much
    /// before collection stops: generous, because only the budgeted prefix
    /// is ever converted, and the point is to bound memory, not to decide
    /// the cut.
    fn collect_budget(&self) -> usize {
        self.max_bytes.saturating_mul(8).clamp(8 << 20, 256 << 20)
    }
}

/// A cancellation flag a statement in flight can be waited on and aborted
/// by. Cloning shares the flag.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

#[derive(Debug, Default)]
struct CancelInner {
    flag: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.flag.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::SeqCst)
    }

    /// Resolves once [`Self::cancel`] has been called.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Stack of every thread of the statement runtime. DataFusion plans a
/// statement by recursing through its expression and plan trees; the default
/// 2 MiB worker stack overflowed (and aborted the process) on a chain of a
/// few hundred operators in an optimised build, far fewer in a debug one. The
/// memory is address space, committed only as a statement uses it. The
/// statement-size limits in [`crate::guard`] keep real statements far below it.
pub(crate) const STATEMENT_STACK_BYTES: usize = 128 << 20;

/// The private runtime statements run on, so the caller's own thread (an
/// MCP stdio loop, an HTTP handler) stays free to enforce the deadline even
/// when the statement does not yield.
fn runtime() -> Result<&'static tokio::runtime::Runtime> {
    static RT: OnceLock<std::result::Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RT.get_or_init(|| {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .clamp(2, 4);
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .max_blocking_threads(4)
            .thread_stack_size(STATEMENT_STACK_BYTES)
            .thread_name("attemptdb-query")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
    })
    .as_ref()
    .map_err(|e| QueryError::Exec(format!("cannot start the query runtime: {e}")))
}

/// A task on the statement runtime that is aborted when its handle is
/// dropped: a caller that stops waiting (a timeout around the future, a
/// closed connection) must not leave the statement running.
struct StatementTask<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for StatementTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Run `fut` on the statement runtime (big stacks, off the caller's thread)
/// and wait for it. Dropping the returned future aborts the task.
pub(crate) async fn on_statement_runtime<T, F>(fut: F) -> Result<T>
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let mut task = StatementTask(runtime()?.spawn(fut));
    match (&mut task.0).await {
        Ok(v) => Ok(v),
        Err(e) if e.is_panic() => Err(QueryError::Exec("the statement panicked".into())),
        Err(_) => Err(QueryError::Exec("the statement was aborted".into())),
    }
}

/// Run `sql` with no row, byte, time or memory bound (the owner's path: the
/// CLI, the daemon, tests). It still runs on the statement runtime: planning
/// recurses, and the caller's thread may be a worker with a small stack.
pub(crate) async fn run_sql_unbounded(ctx: SessionContext, sql: String) -> Result<QueryResult> {
    on_statement_runtime(async move {
        let df = ctx.sql_with_options(&sql, read_only_sql()).await?;
        let schema = Arc::clone(df.schema().inner());
        let batches = df.collect().await?;
        let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
        let is_explain = sql
            .trim_start()
            .get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("EXPLAIN"));
        let kind = if is_explain {
            ResultKind::Explanation
        } else if rows == 0 {
            ResultKind::Empty
        } else {
            ResultKind::Rows
        };
        Ok(QueryResult::new(schema, batches, kind, Vec::new()))
    })
    .await?
}

/// Run `sql` over `ctx` within `limits`. The statement executes on the
/// private runtime; this future only waits, and drops the work (aborting the
/// task, which drops the DataFusion stream) on timeout or cancellation.
pub(crate) async fn run_sql_limited(
    ctx: SessionContext,
    sql: String,
    limits: &QueryLimits,
) -> Result<QueryResult> {
    let rt = runtime()?;
    let task_limits = limits.clone();
    let mut task = StatementTask(
        rt.spawn(async move { collect_limited(ctx, &sql, &task_limits).await }),
    );
    let deadline = limits.timeout.map(|t| tokio::time::Instant::now() + t);
    let timed_out = async {
        match deadline {
            Some(d) => tokio::time::sleep_until(d).await,
            None => std::future::pending::<()>().await,
        }
    };
    let cancelled = async {
        match &limits.cancel {
            Some(c) => c.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        joined = &mut task.0 => match joined {
            Ok(result) => result,
            Err(e) if e.is_panic() => Err(QueryError::Exec("the statement panicked".into())),
            Err(_) => Err(QueryError::Exec("the statement was aborted".into())),
        },
        _ = timed_out => {
            task.0.abort();
            Err(QueryError::Exec(format!(
                "statement stopped after {}: it ran longer than the time limit; narrow it (add filters or a LIMIT, select fewer columns)",
                human_duration(limits.timeout.unwrap_or_default())
            )))
        }
        _ = cancelled => {
            task.0.abort();
            Err(QueryError::Exec("statement cancelled".into()))
        }
    }
}

fn human_duration(d: Duration) -> String {
    if d.as_secs() >= 1 && d.subsec_millis() == 0 {
        format!("{}s", d.as_secs())
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// The statement itself: plan, cut, stream, collect.
async fn collect_limited(
    ctx: SessionContext,
    sql: &str,
    limits: &QueryLimits,
) -> Result<QueryResult> {
    let df = ctx.sql_with_options(sql, read_only_sql()).await?;
    let schema = Arc::clone(df.schema().inner());
    let is_explain = sql
        .trim_start()
        .get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("EXPLAIN"));
    // The row cap goes into the plan, one past the cap so that a result
    // that has exactly `max_rows` rows is not reported as truncated. Plans
    // that are not row sources (EXPLAIN, DESCRIBE, statements) are small
    // and left alone.
    let cut = !matches!(
        df.logical_plan(),
        LogicalPlan::Explain(_)
            | LogicalPlan::Analyze(_)
            | LogicalPlan::DescribeTable(_)
            | LogicalPlan::Statement(_)
            | LogicalPlan::Ddl(_)
            | LogicalPlan::Dml(_)
            | LogicalPlan::Copy(_)
            | LogicalPlan::Extension(_)
    );
    let df = if cut {
        df.limit(0, Some(limits.max_rows.saturating_add(1)))?
    } else {
        df
    };
    let mut task_ctx = df.task_ctx();
    if let Some(bytes) = limits.memory_bytes {
        // A pool of its own, and no spilling to temporary files: a
        // statement that wants more than its share fails.
        let env = RuntimeEnvBuilder::new()
            .with_memory_pool(Arc::new(GreedyMemoryPool::new(bytes)))
            .with_disk_manager_builder(
                DiskManagerBuilder::default().with_mode(DiskManagerMode::Disabled),
            )
            .build_arc()?;
        task_ctx = task_ctx.with_runtime(env);
    }
    let plan = df.create_physical_plan().await?;
    let mut stream = execute_stream(plan, Arc::new(task_ctx))?;

    let guard = limits.collect_budget();
    let mut batches: Vec<RecordBatch> = Vec::new();
    let mut kept = 0usize;
    let mut held = 0usize;
    let mut truncated = false;
    while let Some(batch) = next_batch(&mut stream).await {
        let batch = batch?;
        let n = batch.num_rows();
        if n == 0 {
            continue;
        }
        let room = limits.max_rows - kept;
        if n > room {
            if room > 0 {
                batches.push(batch.slice(0, room));
            }
            truncated = true;
            break;
        }
        kept += n;
        held = held.saturating_add(logical_size(&batch));
        batches.push(batch);
        if held > guard {
            // Far past the byte budget: the rest is not worth holding.
            truncated = true;
            break;
        }
        // An abort point per batch, whatever the operators below do.
        tokio::task::yield_now().await;
    }
    let kind = if is_explain {
        ResultKind::Explanation
    } else if batches.iter().all(|b| b.num_rows() == 0) {
        ResultKind::Empty
    } else {
        ResultKind::Rows
    };
    let mut r = QueryResult::new(schema, batches, kind, Vec::new());
    r.truncated = truncated;
    Ok(r)
}

async fn next_batch(
    stream: &mut SendableRecordBatchStream,
) -> Option<datafusion::error::Result<RecordBatch>> {
    poll_fn(|cx| stream.as_mut().poll_next(cx)).await
}

/// Bytes the rows of `batch` hold, counted by what the rows contain (not by
/// the buffers they are sliced from, which a limit leaves whole).
pub(crate) fn logical_size(batch: &RecordBatch) -> usize {
    batch.columns().iter().map(|c| array_size(c.as_ref())).sum()
}

fn array_size(a: &dyn Array) -> usize {
    match a.data_type() {
        DataType::Utf8 => {
            let o = a.as_string::<i32>().value_offsets();
            (o[o.len() - 1] - o[0]) as usize + a.len() * 4
        }
        DataType::LargeUtf8 => {
            let o = a.as_string::<i64>().value_offsets();
            (o[o.len() - 1] - o[0]) as usize + a.len() * 8
        }
        DataType::Binary => {
            let o = a.as_binary::<i32>().value_offsets();
            (o[o.len() - 1] - o[0]) as usize + a.len() * 4
        }
        DataType::LargeBinary => {
            let o = a.as_binary::<i64>().value_offsets();
            (o[o.len() - 1] - o[0]) as usize + a.len() * 8
        }
        DataType::Utf8View => a
            .as_string_view()
            .views()
            .iter()
            .map(|v| (*v as u32) as usize + 16)
            .sum(),
        DataType::List(_) => {
            let l = a.as_list::<i32>();
            let o = l.value_offsets();
            let (start, end) = (o[0] as usize, o[o.len() - 1] as usize);
            array_size(l.values().slice(start, end - start).as_ref()) + a.len() * 4
        }
        DataType::LargeList(_) => {
            let l = a.as_list::<i64>();
            let o = l.value_offsets();
            let (start, end) = (o[0] as usize, o[o.len() - 1] as usize);
            array_size(l.values().slice(start, end - start).as_ref()) + a.len() * 8
        }
        DataType::Struct(_) => a
            .as_struct()
            .columns()
            .iter()
            .map(|c| array_size(c.as_ref()))
            .sum(),
        DataType::Dictionary(..) => a.len() * 8,
        other => a.len() * other.primitive_width().unwrap_or(8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_token_wakes_waiters() {
        let t = CancelToken::new();
        assert!(!t.is_cancelled());
        let waiter = {
            let t = t.clone();
            tokio::spawn(async move { t.cancelled().await })
        };
        tokio::task::yield_now().await;
        t.cancel();
        waiter.await.unwrap();
        assert!(t.is_cancelled());
        // Already cancelled: resolves at once.
        t.cancelled().await;
    }
}
