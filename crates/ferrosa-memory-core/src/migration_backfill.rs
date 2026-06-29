//! Streaming SELECT-INTO for non-destructive type/primary-key migrations.
//!
//! CQL cannot change a column's type in place (and `INSERT INTO new SELECT FROM
//! old` cannot change types either). This module models the engine's eventual
//! in-place `ALTER ... TYPE` (a UCS-style background relayout) at the
//! application layer: a streaming **SELECT-INTO via fmem**. ferrosa pages the
//! source table → each row flows **through fmem**, which applies a per-row
//! `transform` (e.g. `uuid → text`) the database can't express → fmem streams it
//! **back into ferrosa**'s new-shape table.
//!
//! ## Memory discipline (do not regress)
//!
//! - **Never materialize** the table — or even a page — in fmem. The source is a
//!   **row-at-a-time** stream ([`RowStream::next_row`]); exactly **one row is
//!   resident** at any moment. There is no `Vec<Row>` page buffer.
//! - **Move, don't copy.** A row is **moved** out of the stream, **moved**
//!   through `transform`, and **moved** into the sink. Row data is never
//!   `Clone`d. Only the resume cursor (a few bytes) is copied.
//! - At the CQL layer the sink/transform operate on `CqlValue`s **moved out of
//!   the source row** (`Vec::take`/`std::mem::take`), so an unchanged column is
//!   handed straight to the dest INSERT; only the column whose *type* changes is
//!   re-encoded (uuid→text unavoidably allocates the text form).
//! - **Resumable** — the source paging cursor is checkpointed every
//!   `checkpoint_every` rows and at completion, so a crash resumes mid-stream.
//! - **Non-destructive / fail-loud** — the source is never dropped; a transform
//!   error aborts the run with the last checkpoint intact for a retry.
//!
//! The I/O is behind three small traits so the pipe logic is unit-testable with
//! in-memory fakes; production impls wrap a ferrosa paged streaming SELECT, a
//! per-row (or bounded-batch) INSERT, and a `migration_backfill_progress` table.

use std::future::Future;

use anyhow::Result;

/// Opaque resume cursor — in production, a token bound (`token(pk) > cursor`) or
/// CQL paging state. Only this (a handful of bytes) is ever copied.
pub type Cursor = Vec<u8>;

/// A resumable, **row-at-a-time** stream of source rows. No page buffering.
pub trait RowStream {
    type Row;
    /// Reposition the stream to resume *after* `cursor` (a prior checkpoint).
    fn seek(&mut self, cursor: Cursor) -> impl Future<Output = Result<()>>;
    /// Move the next row out of the stream, or `None` at end. Never clones.
    fn next_row(&mut self) -> impl Future<Output = Result<Option<Self::Row>>>;
    /// The current resume position (cursor to checkpoint), if any rows have been
    /// produced.
    fn cursor(&self) -> Option<Cursor>;
}

/// A sink that consumes transformed rows **by move**. Implementations may buffer
/// a small, bounded batch internally for throughput; `flush` drains it. Row data
/// is never cloned.
pub trait RowSink {
    type Row;
    fn put(&mut self, row: Self::Row) -> impl Future<Output = Result<()>>;
    fn flush(&mut self) -> impl Future<Output = Result<()>>;
}

/// Resume-checkpoint store. Production impl is a `migration_backfill_progress`
/// table keyed by job name.
pub trait Checkpoints {
    fn load(&self, job: &str) -> impl Future<Output = Result<Option<Cursor>>>;
    fn save(&self, job: &str, cursor: &[u8]) -> impl Future<Output = Result<()>>;
}

/// Outcome of a backfill run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillReport {
    /// Rows copied during *this* run (excludes rows copied before a resume).
    pub rows_copied: u64,
    /// Whether the run resumed from a pre-existing checkpoint.
    pub resumed: bool,
}

/// Stream `source` → `transform` → `sink` one row at a time, checkpointing the
/// cursor every `checkpoint_every` rows (and at completion) so a crash resumes
/// mid-stream. This is `SELECT ... INTO` with an fmem-side per-row move
/// `transform` — the part CQL cannot express. It never touches the source.
///
/// Exactly one row is resident at a time: each row is **moved** out of `source`,
/// **moved** through `transform`, and **moved** into `sink`. `transform` is
/// fallible and must not silently drop rows — an error aborts the run
/// (fail-loud), leaving the last checkpoint intact so a retry resumes after the
/// cause is fixed.
pub async fn streaming_rewrite<Src, Dst, S, K, C, F>(
    source: &mut S,
    sink: &mut K,
    checkpoints: &C,
    job: &str,
    checkpoint_every: u64,
    transform: F,
) -> Result<BackfillReport>
where
    S: RowStream<Row = Src>,
    K: RowSink<Row = Dst>,
    C: Checkpoints,
    F: Fn(Src) -> Result<Dst>,
{
    anyhow::ensure!(
        checkpoint_every > 0,
        "streaming_rewrite: checkpoint_every must be > 0"
    );

    // Resume: the source seeks to the last checkpointed cursor, if any.
    let resumed = match checkpoints.load(job).await? {
        Some(cursor) => {
            source.seek(cursor).await?;
            true
        }
        None => false,
    };

    let mut rows_copied = 0u64;
    while let Some(row) = source.next_row().await? {
        let out = transform(row)?; // move Src -> Dst; fail-loud, checkpoint intact
        sink.put(out).await?; // move Dst into the sink
        rows_copied += 1;

        if rows_copied.is_multiple_of(checkpoint_every) {
            sink.flush().await?;
            if let Some(cursor) = source.cursor() {
                checkpoints.save(job, &cursor).await?;
            }
        }
    }

    sink.flush().await?;
    if let Some(cursor) = source.cursor() {
        checkpoints.save(job, &cursor).await?;
    }

    Ok(BackfillReport {
        rows_copied,
        resumed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Row-at-a-time source over `(u32 key, String val)`. Rows are *moved* out of
    /// an owning iterator (no clone); the cursor is the count consumed so far.
    struct VecRowStream {
        iter: std::vec::IntoIter<(u32, String)>,
        consumed: u32,
    }
    impl VecRowStream {
        fn new(rows: Vec<(u32, String)>) -> Self {
            Self {
                iter: rows.into_iter(),
                consumed: 0,
            }
        }
    }
    impl RowStream for VecRowStream {
        type Row = (u32, String);
        async fn seek(&mut self, cursor: Cursor) -> Result<()> {
            let skip = u32::from_le_bytes(cursor.as_slice().try_into().unwrap());
            for _ in 0..skip {
                if self.iter.next().is_some() {
                    self.consumed += 1;
                }
            }
            Ok(())
        }
        async fn next_row(&mut self) -> Result<Option<(u32, String)>> {
            match self.iter.next() {
                Some(row) => {
                    self.consumed += 1;
                    Ok(Some(row))
                }
                None => Ok(None),
            }
        }
        fn cursor(&self) -> Option<Cursor> {
            (self.consumed > 0).then(|| self.consumed.to_le_bytes().to_vec())
        }
    }

    /// Sink that takes transformed `(String, String)` rows by move.
    struct VecSink {
        got: Vec<(String, String)>,
        flushes: usize,
    }
    impl RowSink for VecSink {
        type Row = (String, String);
        async fn put(&mut self, row: (String, String)) -> Result<()> {
            self.got.push(row); // moved in, not cloned
            Ok(())
        }
        async fn flush(&mut self) -> Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    struct MemCkpt {
        store: RefCell<HashMap<String, Cursor>>,
    }
    impl Checkpoints for MemCkpt {
        async fn load(&self, job: &str) -> Result<Option<Cursor>> {
            Ok(self.store.borrow().get(job).cloned())
        }
        async fn save(&self, job: &str, cursor: &[u8]) -> Result<()> {
            self.store
                .borrow_mut()
                .insert(job.to_string(), cursor.to_vec());
            Ok(())
        }
    }

    /// uuid→text-style move transform: consumes the source row, returns the dest
    /// row. The unchanged `val` is *moved* through; only the key is re-encoded.
    fn key_to_text((k, v): (u32, String)) -> Result<(String, String)> {
        Ok((format!("k{k}"), v))
    }

    #[tokio::test]
    async fn streams_every_row_with_transform_one_at_a_time() {
        let mut src = VecRowStream::new((0..7).map(|i| (i, format!("v{i}"))).collect());
        let mut sink = VecSink {
            got: Vec::new(),
            flushes: 0,
        };
        let ckpt = MemCkpt {
            store: RefCell::new(HashMap::new()),
        };

        let report = streaming_rewrite(&mut src, &mut sink, &ckpt, "job", 3, key_to_text)
            .await
            .unwrap();

        assert_eq!(report.rows_copied, 7);
        assert!(!report.resumed);
        assert_eq!(sink.got.len(), 7);
        assert_eq!(sink.got[0], ("k0".to_string(), "v0".to_string()));
        assert_eq!(sink.got[6], ("k6".to_string(), "v6".to_string()));
        // checkpoint advanced to the end
        assert_eq!(
            ckpt.store.borrow().get("job").cloned(),
            Some(7u32.to_le_bytes().to_vec())
        );
    }

    #[tokio::test]
    async fn resumes_from_checkpoint_and_skips_already_copied() {
        let mut src = VecRowStream::new((0..7).map(|i| (i, format!("v{i}"))).collect());
        let mut sink = VecSink {
            got: Vec::new(),
            flushes: 0,
        };
        // Pre-seed the checkpoint at 4 (rows 0..4 already copied) — source seeks past them.
        let mut store = HashMap::new();
        store.insert("job".to_string(), 4u32.to_le_bytes().to_vec());
        let ckpt = MemCkpt {
            store: RefCell::new(store),
        };

        let report = streaming_rewrite(&mut src, &mut sink, &ckpt, "job", 3, key_to_text)
            .await
            .unwrap();

        assert!(report.resumed);
        assert_eq!(report.rows_copied, 3, "only rows 4,5,6 remain");
        assert_eq!(sink.got.len(), 3);
        assert_eq!(sink.got[0], ("k4".to_string(), "v4".to_string()));
        assert_eq!(sink.got[2], ("k6".to_string(), "v6".to_string()));
    }

    #[tokio::test]
    async fn transform_error_aborts_fail_loud_preserving_checkpoint() {
        let mut src = VecRowStream::new((0..6).map(|i| (i, format!("v{i}"))).collect());
        let mut sink = VecSink {
            got: Vec::new(),
            flushes: 0,
        };
        let ckpt = MemCkpt {
            store: RefCell::new(HashMap::new()),
        };

        // checkpoint_every=2: after rows 0,1 a checkpoint at 2 is saved; row 3 fails.
        let result = streaming_rewrite(&mut src, &mut sink, &ckpt, "job", 2, |(k, v)| {
            if k == 3 {
                anyhow::bail!("bad row {k}");
            }
            Ok((format!("k{k}"), v))
        })
        .await;

        assert!(result.is_err(), "transform error must abort the run");
        assert_eq!(
            ckpt.store.borrow().get("job").cloned(),
            Some(2u32.to_le_bytes().to_vec()),
            "the checkpoint from the last completed window survives for a retry"
        );
    }

    #[tokio::test]
    async fn rejects_zero_checkpoint_interval() {
        let mut src = VecRowStream::new(Vec::new());
        let mut sink = VecSink {
            got: Vec::new(),
            flushes: 0,
        };
        let ckpt = MemCkpt {
            store: RefCell::new(HashMap::new()),
        };
        let result = streaming_rewrite(&mut src, &mut sink, &ckpt, "job", 0, key_to_text).await;
        assert!(result.is_err());
    }
}
