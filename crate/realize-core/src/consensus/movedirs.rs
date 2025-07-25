//! Move and sync algorithms for Realize - Symmetric File Syncer
//!
//! This module implements the core file move and synchronization algorithms, including
//! rsync-based partial transfer, progress reporting, and error handling. It operates
//! over the RealStoreService trait and is designed to be robust and restartable.

use crate::rpc::realstore::{
    RangedHash, RealStoreServiceClient, RealStoreServiceRequest, RealStoreServiceResponse,
};
use futures::stream::StreamExt as _;
use futures::{FutureExt, future};
use prometheus::{IntCounter, IntCounterVec, register_int_counter, register_int_counter_vec};
use realize_storage::{RealStoreError, RealStoreOptions, SyncedFile};
use realize_types;
use realize_types::{Arena, ByteRange, ByteRanges};
use std::collections::HashMap;
use std::sync::Arc;
use tarpc::client::stub::Stub;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::Sender;

const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
const PARALLEL_FILE_COUNT: usize = 4;
const HASH_FILE_CHUNK: u64 = 256 * 1024 * 1024; // 256M
const PARALLEL_FILE_HASH: usize = 4;

lazy_static::lazy_static! {
    pub static ref METRIC_START_COUNT: IntCounter =
        register_int_counter!("realize_move_start_count", "Number of times move_files() was called").unwrap();
    pub static ref METRIC_END_COUNT: IntCounter =
        register_int_counter!("realize_move_end_count", "Number of times move_files() finished").unwrap();
    pub static ref METRIC_FILE_START_COUNT: IntCounter =
        register_int_counter!("realize_move_file_start_count", "Number of files started by move_files").unwrap();
    pub static ref METRIC_FILE_END_COUNT: IntCounterVec =
        register_int_counter_vec!(
            "realize_move_file_end_count",
            "Number of files synced (status is Ok or Inconsistent)",
            &["status"]
        ).unwrap();
    pub static ref METRIC_READ_BYTES: IntCounterVec =
        register_int_counter_vec!(
            "realize_move_read_bytes",
            "Number of bytes read, with method label (read or diff)",
            &["method"]
        ).unwrap();
    pub static ref METRIC_WRITE_BYTES: IntCounterVec =
        register_int_counter_vec!(
            "realize_move_write_bytes",
            "Number of bytes written, with method label (send or apply_patch)",
            &["method"]
        ).unwrap();
    pub static ref METRIC_RANGE_READ_BYTES: IntCounterVec =
        register_int_counter_vec!(
            "realize_move_range_read_bytes",
            "Number of bytes read (range), with method label (read or diff)",
            &["method"]
        ).unwrap();
    pub static ref METRIC_RANGE_WRITE_BYTES: IntCounterVec =
        register_int_counter_vec!(
            "realize_move_range_write_bytes",
            "Number of bytes written (range), with method label (send or apply_patch)",
            &["method"]
        ).unwrap();
    pub static ref METRIC_APPLY_DELTA_FALLBACK_COUNT: IntCounter =
        register_int_counter!(
            "realize_apply_delta_fallback_count",
            "Number of times copy had to be used as fallback to apply_delta",
        ).unwrap();
    pub static ref METRIC_APPLY_DELTA_FALLBACK_BYTES: IntCounter =
        register_int_counter!(
            "realize_apply_delta_fallback_bytes",
            "Bytes for which copy had to be used as fallback to apply_delta",
        ).unwrap();
}

/// Options used for RPC calls on the source.
fn src_options() -> RealStoreOptions {
    RealStoreOptions {
        ignore_partial: true,
    }
}

/// Options used for RPC calls on the destination.
fn dst_options() -> RealStoreOptions {
    RealStoreOptions {
        ignore_partial: false,
    }
}

/// Event enum for channel-based progress reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    /// Indicates the start of moving a directory, with total files and bytes.
    MovingDir {
        arena: Arena,
        total_files: usize,
        total_bytes: u64,
        available_bytes: u64,
    },
    /// Indicates a file is being processed (start), with its path and size.
    MovingFile {
        arena: Arena,
        path: realize_types::Path,
        bytes: u64,

        /// Bytes already available, to be r-synced.
        available: u64,
    },
    /// File is being verified (hash check).
    VerifyingFile {
        arena: Arena,
        path: realize_types::Path,
    },
    /// File is being rsynced (diff/patch).
    RsyncingFile {
        arena: Arena,
        path: realize_types::Path,
    },
    /// File is being copied (data transfer).
    CopyingFile {
        arena: Arena,
        path: realize_types::Path,
    },
    /// File is waiting its turn to be copied.
    PendingFile {
        arena: Arena,
        path: realize_types::Path,
    },
    /// Increment byte count for a file and overall progress.
    IncrementByteCount {
        arena: Arena,
        path: realize_types::Path,
        bytecount: u64,
    },
    /// Decrement byte count for a file and overall progress, when
    /// retrying a range previously assumed to be correct.
    DecrementByteCount {
        arena: Arena,
        path: realize_types::Path,
        bytecount: u64,
    },
    /// File was moved successfully.
    FileSuccess {
        arena: Arena,
        path: realize_types::Path,
    },
    /// Moving the file failed.
    FileError {
        arena: Arena,
        path: realize_types::Path,
        error: String,
    },
}

pub async fn move_dir<T, U>(
    ctx: tarpc::context::Context,
    src: &RealStoreServiceClient<T>,
    dst: &RealStoreServiceClient<U>,
    arena: Arena,
    progress_tx: Option<Sender<ProgressEvent>>,
) -> Result<(usize, usize, usize), MoveFileError>
where
    T: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
    U: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
{
    move_dirs(ctx, src, dst, vec![arena], progress_tx).await
}

/// Moves files from source to destination using the RealStoreService interface, sending progress events to a channel.
pub async fn move_dirs<T, U>(
    ctx: tarpc::context::Context,
    src: &RealStoreServiceClient<T>,
    dst: &RealStoreServiceClient<U>,
    arenas: impl IntoIterator<Item = Arena>,
    progress_tx: Option<Sender<ProgressEvent>>,
) -> Result<(usize, usize, usize), MoveFileError>
where
    T: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
    U: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
{
    METRIC_START_COUNT.inc();
    let mut files_to_sync = vec![];
    for collected in futures::stream::iter(arenas.into_iter())
        .map(|arena| collect_files_to_sync(ctx, src, dst, arena, &progress_tx))
        .buffered(8)
        .collect::<Vec<_>>()
        .await
    {
        files_to_sync.append(&mut (collected?));
    }

    let copy_sem = Arc::new(Semaphore::new(1));
    let results = futures::stream::iter(files_to_sync.into_iter())
        .map(|(arena, src_file, dst_file)| {
            let copy_sem = copy_sem.clone();
            let tx = progress_tx.clone();
            let file_path = src_file.path.clone();

            async move {
                let tx = tx.clone();
                if let Some(tx) = &tx {
                    let _ = tx
                        .send(ProgressEvent::MovingFile {
                            arena: arena,
                            path: file_path.clone(),
                            bytes: src_file.size,
                            available: dst_file.as_ref().map_or(0, |f| f.size),
                        })
                        .await;
                }

                let result = move_file(
                    ctx,
                    src,
                    src_file,
                    dst,
                    dst_file,
                    arena,
                    copy_sem.clone(),
                    tx.clone(),
                )
                .await;
                match result {
                    Ok(_) => {
                        if let Some(tx) = &tx {
                            let _ = tx
                                .send(ProgressEvent::FileSuccess {
                                    arena: arena,
                                    path: file_path.clone(),
                                })
                                .await;
                        }

                        (1, 0, 0)
                    }
                    Err(MoveFileError::Rpc(tarpc::client::RpcError::DeadlineExceeded)) => {
                        log::debug!("{arena}/{file_path}: Deadline exceeded");
                        if let Some(tx) = &tx {
                            let _ = tx
                                .send(ProgressEvent::FileError {
                                    arena: arena,
                                    path: file_path.clone(),
                                    error: "Deadline exceeded".to_string(),
                                })
                                .await;
                        }

                        (0, 0, 1)
                    }
                    Err(ref err) => {
                        log::debug!("{arena}/{file_path}: {err}");
                        if let Some(tx) = &tx {
                            let _ = tx
                                .send(ProgressEvent::FileError {
                                    arena: arena,
                                    path: file_path.clone(),
                                    error: format!("{err}"),
                                })
                                .await;
                        }
                        (0, 1, 0)
                    }
                }
            }
        })
        .buffer_unordered(PARALLEL_FILE_COUNT)
        .collect::<Vec<(usize, usize, usize)>>()
        .await;
    let (success_count, error_count, interrupted_count) = results
        .into_iter()
        .fold((0, 0, 0), |(s, e, i), (s1, e1, i1)| {
            (s + s1, e + e1, i + i1)
        });
    METRIC_END_COUNT.inc();
    Ok((success_count, error_count, interrupted_count))
}

/// Collect pair of files to sync from a directory.
///
/// Also issues the [ProgressEvent::MovingDir] progress calls
async fn collect_files_to_sync<T, U>(
    ctx: tarpc::context::Context,
    src: &RealStoreServiceClient<T>,
    dst: &RealStoreServiceClient<U>,
    arena: Arena,
    progress_tx: &Option<Sender<ProgressEvent>>,
) -> Result<Vec<(Arena, SyncedFile, Option<SyncedFile>)>, MoveFileError>
where
    T: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
    U: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
{
    // 1. List files on src and dst in parallel
    let (src_files, dst_files) = future::join(
        src.list(ctx, arena, src_options()),
        dst.list(ctx, arena, dst_options()),
    )
    .await;
    let src_files = src_files??;
    let dst_files = dst_files??;

    let mut dst_map: HashMap<_, _> = dst_files.into_iter().map(|f| (f.path.clone(), f)).collect();

    let files_to_sync = src_files
        .into_iter()
        .map(|src_file| {
            let dst_file = dst_map.remove(&src_file.path);

            (arena, src_file, dst_file)
        })
        .collect::<Vec<_>>();

    if let Some(tx) = &progress_tx {
        let _ = tx
            .send(ProgressEvent::MovingDir {
                arena: arena,
                total_files: files_to_sync.len(),
                total_bytes: files_to_sync.iter().fold(0, |acc, (_, f, _)| acc + f.size),
                available_bytes: files_to_sync
                    .iter()
                    .fold(0, |acc, (_, _, f)| acc + f.as_ref().map_or(0, |f| f.size)),
            })
            .await;
    }

    Ok(files_to_sync)
}

async fn move_file<T, U>(
    ctx: tarpc::context::Context,
    src: &RealStoreServiceClient<T>,
    src_file: SyncedFile,
    dst: &RealStoreServiceClient<U>,
    dst_file: Option<SyncedFile>,
    arena: Arena,
    copy_sem: Arc<Semaphore>,
    progress_tx: Option<Sender<ProgressEvent>>,
) -> Result<(), MoveFileError>
where
    T: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
    U: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
{
    METRIC_FILE_START_COUNT.inc();
    let path = src_file.path;
    let src_size = src_file.size;
    let dst_size = dst_file.as_ref().map(|f| f.size).unwrap_or(0);

    let ranges = ByteRanges::single(0, src_size);

    // Assume existing to be correct for now and report it as such in
    // the progress.
    let existing = ByteRanges::single(0, dst_size);

    let (copy, src_hash) = tokio::join!(
        async {
            // 1. Copy missing data
            let copy_ranges = ranges.subtraction(&existing);
            log::debug!("{arena}/{path:?} {ranges} copy {copy_ranges}");
            copy_file_range(
                ctx,
                arena,
                &path,
                src,
                dst,
                &progress_tx,
                copy_sem.clone(),
                &copy_ranges,
            )
            .await?;

            // 2. Truncate if necessary
            if dst_file.as_ref().map_or(0, |f| f.size) > src_file.size {
                dst.truncate(ctx, arena, path.clone(), src_file.size, dst_options())
                    .await??;
            }

            Ok::<(), MoveFileError>(())
        },
        // Compute the source file hash while doing the copy
        hash_file(
            ctx,
            src,
            arena,
            &path,
            src_size,
            HASH_FILE_CHUNK,
            src_options(),
        ),
    );
    copy?;
    let src_hash = src_hash?;

    // 3. Check hash, return if succeeds
    report_verifying(&progress_tx, arena, &path).await;
    let correct =
        match check_hashes_and_delete(ctx, &src_hash, src_size, src, dst, arena, &path).await? {
            HashCheck::Match => {
                return Ok(());
            }
            HashCheck::Mismatch {
                partial_match: matches,
                ..
            } => matches,
        };

    // 4. Use rsync to fix any mismatch
    let mut fallback_ranges = ByteRanges::new();
    let rsync_ranges = ranges.subtraction(&correct);
    log::debug!(
        "{}/{:?} {} rsync {} DEC:{}",
        arena,
        path,
        ranges,
        rsync_ranges,
        rsync_ranges.bytecount()
    );
    report_decrement_bytecount(&progress_tx, arena, &path, rsync_ranges.bytecount()).await;
    rsync_file_range(
        ctx,
        arena,
        &path,
        src,
        dst,
        &progress_tx,
        &rsync_ranges,
        &mut fallback_ranges,
    )
    .await?;

    // 5. Fallback to copy if necessary
    copy_file_range(
        ctx,
        arena,
        &path,
        src,
        dst,
        &progress_tx,
        copy_sem.clone(),
        &fallback_ranges,
    )
    .await?;

    // 6. Check again against hash
    report_verifying(&progress_tx, arena, &path).await;
    match check_hashes_and_delete(ctx, &src_hash, src_size, src, dst, arena, &path).await? {
        HashCheck::Mismatch { .. } => Err(MoveFileError::FailedToSync),
        HashCheck::Match => Ok(()),
    }
}

async fn rsync_file_range<T, U>(
    ctx: tarpc::context::Context,
    arena: Arena,
    path: &realize_types::Path,
    src: &RealStoreServiceClient<T>,
    dst: &RealStoreServiceClient<U>,
    progress_tx: &Option<Sender<ProgressEvent>>,
    rsync_ranges: &ByteRanges,
    copy_ranges: &mut ByteRanges,
) -> Result<(), MoveFileError>
where
    T: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
    U: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
{
    if rsync_ranges.is_empty() {
        return Ok(());
    }

    report_rsyncing(progress_tx, arena, path).await;

    for range in rsync_ranges.chunked(CHUNK_SIZE) {
        let sig = dst
            .calculate_signature(ctx, arena, path.clone(), range.clone(), dst_options())
            .await??;
        let (delta, hash) = src
            .diff(ctx, arena, path.clone(), range.clone(), sig, src_options())
            .await??;
        METRIC_READ_BYTES
            .with_label_values(&["diff"])
            .inc_by(delta.0.len() as u64);
        METRIC_RANGE_READ_BYTES
            .with_label_values(&["diff"])
            .inc_by(range.bytecount());
        let delta_len = delta.0.len();
        match dst
            .apply_delta(
                ctx,
                arena,
                path.clone(),
                range.clone(),
                delta,
                hash,
                dst_options(),
            )
            .await?
        {
            Ok(_) => {
                METRIC_WRITE_BYTES
                    .with_label_values(&["apply_delta"])
                    .inc_by(delta_len as u64);
                METRIC_RANGE_WRITE_BYTES
                    .with_label_values(&["apply_delta"])
                    .inc_by(range.bytecount());
                log::debug!("{}/{:?} INC {}", arena, path, range.bytecount());
                report_increment_bytecount(progress_tx, arena, path, range.bytecount()).await;
            }
            Err(RealStoreError::HashMismatch) => {
                copy_ranges.add(&range);
                log::error!("{arena}/{path}:{range} hash mismatch after apply_delta, will copy",);
                METRIC_APPLY_DELTA_FALLBACK_COUNT.inc();
                METRIC_APPLY_DELTA_FALLBACK_BYTES.inc_by(range.bytecount());
            }
            Err(err) => {
                return Err(MoveFileError::from(err));
            }
        };
    }

    Ok(())
}

async fn copy_file_range<T, U>(
    ctx: tarpc::context::Context,
    arena: Arena,
    path: &realize_types::Path,
    src: &RealStoreServiceClient<T>,
    dst: &RealStoreServiceClient<U>,
    progress_tx: &Option<Sender<ProgressEvent>>,
    copy_sem: Arc<Semaphore>,
    copy_ranges: &ByteRanges,
) -> Result<(), MoveFileError>
where
    T: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
    U: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
{
    if copy_ranges.is_empty() {
        return Ok(());
    }
    // It can take some time for a permit to become available, so
    // go back to showing the file as Pending until then.
    report_pending(progress_tx, arena, path).await;
    let _lock = copy_sem.acquire().await;

    report_copying(progress_tx, arena, path).await;

    for range in copy_ranges.chunked(CHUNK_SIZE) {
        let data = src
            .read(ctx, arena, path.clone(), range.clone(), src_options())
            .await??;
        METRIC_READ_BYTES
            .with_label_values(&["read"])
            .inc_by(data.len() as u64);
        METRIC_RANGE_READ_BYTES
            .with_label_values(&["read"])
            .inc_by(range.bytecount());
        let data_len = data.len();
        dst.send(ctx, arena, path.clone(), range.clone(), data, dst_options())
            .await??;
        METRIC_WRITE_BYTES
            .with_label_values(&["send"])
            .inc_by(data_len as u64);
        METRIC_RANGE_WRITE_BYTES
            .with_label_values(&["send"])
            .inc_by(range.bytecount());
        report_increment_bytecount(progress_tx, arena, path, range.bytecount()).await;
    }

    Ok(())
}

async fn report_pending(
    progress_tx: &Option<Sender<ProgressEvent>>,
    arena: Arena,
    path: &realize_types::Path,
) {
    if let Some(tx) = progress_tx {
        let _ = tx
            .send(ProgressEvent::PendingFile {
                arena: arena,
                path: path.clone(),
            })
            .await;
    }
}

async fn report_copying(
    progress_tx: &Option<Sender<ProgressEvent>>,
    arena: Arena,
    path: &realize_types::Path,
) {
    if let Some(tx) = progress_tx {
        let _ = tx
            .send(ProgressEvent::CopyingFile {
                arena: arena,
                path: path.clone(),
            })
            .await;
    }
}

async fn report_rsyncing(
    progress_tx: &Option<Sender<ProgressEvent>>,
    arena: Arena,
    path: &realize_types::Path,
) {
    if let Some(tx) = progress_tx {
        let _ = tx
            .send(ProgressEvent::RsyncingFile {
                arena: arena,
                path: path.clone(),
            })
            .await;
    }
}

async fn report_verifying(
    progress_tx: &Option<Sender<ProgressEvent>>,
    arena: Arena,
    path: &realize_types::Path,
) {
    if let Some(tx) = progress_tx {
        let _ = tx
            .send(ProgressEvent::VerifyingFile {
                arena: arena,
                path: path.clone(),
            })
            .await;
    }
}

async fn report_increment_bytecount(
    progress_tx: &Option<Sender<ProgressEvent>>,
    arena: Arena,
    path: &realize_types::Path,
    bytecount: u64,
) {
    if bytecount == 0 {
        return;
    }
    if let Some(tx) = progress_tx {
        let _ = tx
            .send(ProgressEvent::IncrementByteCount {
                arena: arena,
                path: path.clone(),
                bytecount,
            })
            .await;
    }
}

async fn report_decrement_bytecount(
    progress_tx: &Option<Sender<ProgressEvent>>,
    arena: Arena,
    path: &realize_types::Path,
    bytecount: u64,
) {
    if bytecount == 0 {
        return;
    }
    if let Some(tx) = progress_tx {
        let _ = tx
            .send(ProgressEvent::DecrementByteCount {
                arena: arena,
                path: path.clone(),
                bytecount,
            })
            .await;
    }
}

/// Hash file in chunks and return the result.
pub(crate) async fn hash_file<T>(
    ctx: tarpc::context::Context,
    client: &RealStoreServiceClient<T>,
    arena: Arena,
    relative_path: &realize_types::Path,
    file_size: u64,
    chunk_size: u64,
    options: RealStoreOptions,
) -> Result<RangedHash, MoveFileError>
where
    T: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
{
    let results = futures::stream::iter(ByteRange::new(0, file_size).chunked(chunk_size))
        .map(|range| {
            client
                .hash(ctx, arena, relative_path.clone(), range.clone(), options)
                .map(move |res| res.map(|h| (range.clone(), h)))
        })
        .buffer_unordered(PARALLEL_FILE_HASH)
        .collect::<Vec<_>>()
        .await;

    let mut ranged = RangedHash::new();
    for res in results.into_iter() {
        let (range, hash_res) = res?;
        ranged.add(range, hash_res?);
    }
    Ok(ranged)
}

/// Return value for [check_hashes_and_delete]
enum HashCheck {
    /// The hashes matched.
    Match,

    /// The hashes didn't match.
    ///
    /// If some hash range matched, it is reported as partial match.
    Mismatch { partial_match: ByteRanges },
}

/// Check hashes and, if they match, finish the dest file and delete the source.
///
/// Return true if the hashes matched, false otherwise.
async fn check_hashes_and_delete<T, U>(
    ctx: tarpc::context::Context,
    src_hash: &RangedHash,
    file_size: u64,
    src: &RealStoreServiceClient<T>,
    dst: &RealStoreServiceClient<U>,
    arena: Arena,
    path: &realize_types::Path,
) -> Result<HashCheck, MoveFileError>
where
    T: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
    U: Stub<Req = RealStoreServiceRequest, Resp = RealStoreServiceResponse>,
{
    let dst_hash = hash_file(
        ctx,
        dst,
        arena,
        path,
        file_size,
        HASH_FILE_CHUNK,
        dst_options(),
    )
    .await?;
    let is_complete_src = src_hash.is_complete(file_size);
    let is_complete_dst = dst_hash.is_complete(file_size);
    let (matches, mismatches) = src_hash.diff(&dst_hash);
    if !mismatches.is_empty() || !is_complete_src || !is_complete_dst {
        log::debug!(
            "{arena}:{path:?} inconsistent hashes\nsrc: {src_hash}\ndst: {dst_hash}\nmatches: {matches}\nmismatches: {mismatches}"
        );
        METRIC_FILE_END_COUNT
            .with_label_values(&["Inconsistent"])
            .inc();
        return Ok(HashCheck::Mismatch {
            partial_match: matches,
        });
    }
    log::debug!("{arena}/{path} MOVED");
    // Hashes match, finish and delete
    dst.finish(ctx, arena, path.clone(), dst_options())
        .await??;
    src.delete(ctx, arena, path.clone(), src_options())
        .await??;

    METRIC_FILE_END_COUNT.with_label_values(&["Ok"]).inc();
    Ok(HashCheck::Match)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::realstore::server::{self};
    use assert_fs::TempDir;
    use assert_fs::prelude::*;
    use assert_unordered::assert_eq_unordered;
    use realize_storage::RealStore;
    use realize_storage::utils::hash;
    use realize_types::{Arena, Hash};
    use std::path::PathBuf;
    use walkdir::WalkDir;

    #[tokio::test]
    async fn events_for_successful_copy() -> anyhow::Result<()> {
        let _ = env_logger::try_init();
        let src_temp = TempDir::new()?;
        let dst_temp = TempDir::new()?;
        src_temp.child("foo").write_str("abc")?;
        let arena = Arena::from("testdir");
        let src_dir = src_temp.path();
        let dst_dir = dst_temp.path();
        let src_server = server::create_inprocess_client(RealStore::single(arena, src_dir));
        let dst_server = server::create_inprocess_client(RealStore::single(arena, dst_dir));

        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let (success, error, _) = move_dir(
            tarpc::context::current(),
            &src_server,
            &dst_server,
            Arena::from("testdir"),
            Some(tx),
        )
        .await?;

        assert_eq!(success, 1);
        assert_eq!(error, 0);

        // Collect all events
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        use ProgressEvent::*;
        assert_eq!(
            vec![
                MovingDir {
                    arena: Arena::from("testdir"),
                    total_files: 1,
                    total_bytes: 3,
                    available_bytes: 0,
                },
                MovingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?,
                    bytes: 3,
                    available: 0
                },
                PendingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
                CopyingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
                IncrementByteCount {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?,
                    bytecount: 3
                },
                VerifyingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
                FileSuccess {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
            ],
            events
        );

        Ok(())
    }

    #[tokio::test]
    async fn events_for_continued_copy() -> anyhow::Result<()> {
        let _ = env_logger::try_init();
        let arena = Arena::from("testdir");
        let src_temp = TempDir::new()?;
        let dst_temp = TempDir::new()?;
        src_temp.child("foo").write_str("abcdefghi")?;
        dst_temp.child(".foo.part").write_str("abc")?;

        let src_server = server::create_inprocess_client(RealStore::single(arena, src_temp.path()));
        let dst_server = server::create_inprocess_client(RealStore::single(arena, dst_temp.path()));

        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let (success, error, _) = move_dir(
            tarpc::context::current(),
            &src_server,
            &dst_server,
            Arena::from("testdir"),
            Some(tx),
        )
        .await?;

        assert_eq!(success, 1);
        assert_eq!(error, 0);

        // Collect all events
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        use ProgressEvent::*;
        assert_eq!(
            vec![
                MovingDir {
                    arena: Arena::from("testdir"),
                    total_files: 1,
                    total_bytes: 9,
                    available_bytes: 3,
                },
                MovingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?,
                    bytes: 9,
                    available: 3
                },
                PendingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
                CopyingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
                IncrementByteCount {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?,
                    bytecount: 6
                },
                VerifyingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
                FileSuccess {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
            ],
            events
        );

        Ok(())
    }

    #[tokio::test]
    async fn events_for_failed_copy() -> anyhow::Result<()> {
        let _ = env_logger::try_init();
        let src_temp = TempDir::new()?;
        let dst_temp = TempDir::new()?;
        let arena = Arena::from("testdir");
        src_temp.child("foo").write_str("abcdefghi")?;
        dst_temp.child(".foo.part").write_str("xxx")?;

        let src_server = server::create_inprocess_client(RealStore::single(arena, src_temp.path()));
        let dst_server = server::create_inprocess_client(RealStore::single(arena, dst_temp.path()));

        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let (success, error, _) = move_dir(
            tarpc::context::current(),
            &src_server,
            &dst_server,
            Arena::from("testdir"),
            Some(tx),
        )
        .await?;

        assert_eq!(success, 1);
        assert_eq!(error, 0);

        // Collect all events
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        use ProgressEvent::*;
        assert_eq!(
            vec![
                MovingDir {
                    arena: Arena::from("testdir"),
                    total_files: 1,
                    total_bytes: 9,
                    available_bytes: 3,
                },
                MovingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?,
                    bytes: 9,
                    available: 3
                },
                PendingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
                CopyingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
                IncrementByteCount {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?,
                    bytecount: 6
                },
                VerifyingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
                DecrementByteCount {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?,
                    bytecount: 9
                },
                RsyncingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
                IncrementByteCount {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?,
                    bytecount: 9
                },
                VerifyingFile {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
                FileSuccess {
                    arena: Arena::from("testdir"),
                    path: realize_types::Path::parse("foo")?
                },
            ],
            events
        );

        Ok(())
    }

    #[tokio::test]
    async fn move_some_files() -> anyhow::Result<()> {
        let _ = env_logger::try_init();

        let arena = Arena::from("testdir");

        // Setup source directory with files
        let src_temp = TempDir::new()?;
        let src_server = server::create_inprocess_client(RealStore::single(arena, src_temp.path()));

        // Setup destination directory (empty)
        let dst_temp = TempDir::new()?;
        let dst_server = server::create_inprocess_client(RealStore::single(arena, dst_temp.path()));

        // Pre-populate destination with a file of the same length as source (should trigger rsync optimization)
        src_temp.child("same_length").write_str("hello")?;
        dst_temp.child("same_length").write_str("xxxxx")?; // same length as "hello"

        // Corrupted dst file in partial state
        src_temp.child("longer").write_str("world")?;
        dst_temp.child(".longer.part").write_str("corrupt")?;

        // Corrupted dst file in final state
        dst_temp.child("corrupt_final").write_str("corruptfinal")?;
        src_temp.child("corrupt_final").write_str("bazgood")?;

        // Partially copied dst file (shorter than src)
        dst_temp.child("partial").write_str("par")?;
        src_temp.child("partial").write_str("partialcopy")?;

        println!("test_move_files: all files set up");
        let (success, error, _interrupted) = move_dir(
            tarpc::context::current(),
            &src_server,
            &dst_server,
            Arena::from("testdir"),
            None,
        )
        .await?;
        assert_eq!((success, error), (4, 0));
        // Check that files are present in destination and not in source
        assert_eq_unordered!(snapshot_dir(src_temp.path())?, vec![]);
        assert_eq_unordered!(
            snapshot_dir(dst_temp.path())?,
            vec![
                (PathBuf::from("same_length"), "hello".to_string()),
                (PathBuf::from("longer"), "world".to_string()),
                (PathBuf::from("corrupt_final"), "bazgood".to_string()),
                (PathBuf::from("partial"), "partialcopy".to_string()),
            ]
        );

        Ok(())
    }

    #[tokio::test]
    #[test_tag::tag(slow)]
    async fn move_files_chunked() -> anyhow::Result<()> {
        let _ = env_logger::try_init();
        const FILE_SIZE: usize = (1.25 * CHUNK_SIZE as f32) as usize;
        let chunk = vec![0xAB; FILE_SIZE];
        let chunk2 = vec![0xCD; FILE_SIZE];
        let chunk3 = vec![0xEF; FILE_SIZE];
        let chunk4 = vec![0x12; FILE_SIZE];
        let chunk5 = vec![0x34; FILE_SIZE];

        let arena = Arena::from("testdir");
        let src_temp = TempDir::new()?;
        let src_server = server::create_inprocess_client(RealStore::single(arena, src_temp.path()));

        let dst_temp = TempDir::new()?;
        let dst_server = server::create_inprocess_client(RealStore::single(arena, dst_temp.path()));

        // Case 1: source > CHUNK_SIZE, destination empty
        src_temp.child("large_empty").write_binary(&chunk)?;
        // Case 2: source > CHUNK_SIZE, destination same size, but content is different
        src_temp.child("large_diff").write_binary(&chunk)?;
        dst_temp.child("large_diff").write_binary(&chunk2)?;
        // Case 3: source > CHUNK_SIZE, destination truncated, shorter than CHUNK_SIZE
        src_temp.child("large_trunc_short").write_binary(&chunk)?;
        dst_temp
            .child("large_trunc_short")
            .write_binary(&chunk3[..(0.5 * CHUNK_SIZE as f32) as usize])?;
        // Case 4: source > CHUNK_SIZE, destination truncated, longer than CHUNK_SIZE
        src_temp.child("large_trunc_long").write_binary(&chunk)?;
        dst_temp
            .child("large_trunc_long")
            .write_binary(&chunk4[..(1.25 * CHUNK_SIZE as f32) as usize])?;
        // Case 5: source > CHUNK_SIZE, destination same content, with garbage at the end
        src_temp.child("large_garbage").write_binary(&chunk)?;
        let mut garbage = chunk.clone();
        garbage.extend_from_slice(&chunk5[..1024 * 1024]); // 1MB garbage
        dst_temp.child("large_garbage").write_binary(&garbage)?;

        let mut ctx = tarpc::context::current();
        ctx.deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);

        let (success, error, _interrupted) =
            move_dir(ctx, &src_server, &dst_server, Arena::from("testdir"), None).await?;
        assert_eq!(
            (5, 0),
            (success, error),
            "should be: 5 files moved, 0 errors"
        );
        // Check that files are present in destination and not in source
        assert_eq_unordered!(snapshot_dir(src_temp.path())?, vec![]);
        let expected = vec![
            (PathBuf::from("large_empty"), chunk.clone()),
            (PathBuf::from("large_diff"), chunk.clone()),
            (PathBuf::from("large_trunc_short"), chunk.clone()),
            (PathBuf::from("large_trunc_long"), chunk.clone()),
            (PathBuf::from("large_garbage"), chunk.clone()),
        ];
        let actual = snapshot_dir_bin(dst_temp.path())?;
        for (path, data) in expected {
            let found = actual
                .iter()
                .find(|(p, _)| *p == path)
                .unwrap_or_else(|| panic!("missing {path:?}"));
            assert_eq!(&found.1, &data, "content mismatch for {path:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn move_files_partial_error() -> anyhow::Result<()> {
        let _ = env_logger::try_init();
        let arena = Arena::from("testdir");
        let src_temp = TempDir::new()?;
        let src_server = server::create_inprocess_client(RealStore::single(arena, src_temp.path()));
        let dst_temp = TempDir::new()?;
        let dst_server = server::create_inprocess_client(RealStore::single(arena, dst_temp.path()));
        // Good file
        src_temp.child("good").write_str("ok")?;
        // Unreadable file
        let unreadable = src_temp.child("bad");
        unreadable.write_str("fail")?;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(unreadable.path(), fs::Permissions::from_mode(0o000))?;
        let (success, error, _interrupted) = move_dir(
            tarpc::context::current(),
            &src_server,
            &dst_server,
            Arena::from("testdir"),
            None,
        )
        .await?;
        assert_eq!(success, 1, "One file should succeed");
        assert_eq!(error, 1, "One file should fail");
        // Restore permissions for cleanup
        fs::set_permissions(unreadable.path(), fs::Permissions::from_mode(0o644))?;
        Ok(())
    }

    #[tokio::test]
    async fn move_files_ignores_partial_in_src() -> anyhow::Result<()> {
        let _ = env_logger::try_init();
        let src_temp = TempDir::new()?;
        let dst_temp = TempDir::new()?;
        let arena = Arena::from("testdir");
        let src_dir = src_temp.path();
        let dst_dir = dst_temp.path();
        let src_server = server::create_inprocess_client(RealStore::single(arena, src_dir));
        let dst_server = server::create_inprocess_client(RealStore::single(arena, dst_dir));
        // Create a final file and a partial file in src
        src_temp.child("final.txt").write_str("finaldata")?;
        src_temp
            .child(".partial.txt.part")
            .write_str("partialdata")?;
        // Run move_files
        let (success, error, _interrupted) = move_dir(
            tarpc::context::current(),
            &src_server,
            &dst_server,
            Arena::from("testdir"),
            None,
        )
        .await?;
        // Only the final file should be moved
        assert_eq!(success, 1, "Only one file should be moved");
        assert_eq!(error, 0, "No errors expected");
        // Check that only final.txt is present in dst
        let files = WalkDir::new(dst_temp.path())
            .into_iter()
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(files, vec!["final.txt"]);
        Ok(())
    }

    #[tokio::test]
    async fn hash_file_non_chunked() -> anyhow::Result<()> {
        let _ = env_logger::try_init();

        let temp = TempDir::new()?;
        let file = temp.child("somefile");
        let content = b"baa, baa, black sheep";
        file.write_binary(content)?;

        let arena = Arena::from("dir");
        let server = server::create_inprocess_client(RealStore::single(arena, temp.path()));
        let ranged = hash_file(
            tarpc::context::current(),
            &server,
            arena,
            &realize_types::Path::parse("somefile")?,
            content.len() as u64,
            HASH_FILE_CHUNK,
            RealStoreOptions::default(),
        )
        .await?;
        assert_eq!(
            RangedHash::single(
                ByteRange {
                    start: 0,
                    end: content.len() as u64
                },
                hash::digest(content)
            ),
            ranged
        );
        Ok(())
    }

    #[tokio::test]
    async fn hash_file_chunked() -> anyhow::Result<()> {
        let _ = env_logger::try_init();

        let temp = TempDir::new()?;
        let file = temp.child("somefile");
        let content = b"baa, baa, black sheep";
        file.write_binary(content)?;

        let arena = Arena::from("dir");
        let server = server::create_inprocess_client(RealStore::single(arena, temp.path()));
        let ranged = hash_file(
            tarpc::context::current(),
            &server,
            arena,
            &realize_types::Path::parse("somefile")?,
            content.len() as u64,
            4,
            RealStoreOptions::default(),
        )
        .await?;
        let mut expected = RangedHash::new();
        expected.add(ByteRange { start: 0, end: 4 }, hash::digest(b"baa,"));
        expected.add(ByteRange { start: 4, end: 8 }, hash::digest(b" baa"));
        expected.add(ByteRange { start: 8, end: 12 }, hash::digest(b", bl"));
        expected.add(ByteRange { start: 12, end: 16 }, hash::digest(b"ack "));
        expected.add(ByteRange { start: 16, end: 20 }, hash::digest(b"shee"));
        expected.add(ByteRange { start: 20, end: 21 }, hash::digest(b"p"));

        assert_eq!(ranged, expected);

        Ok(())
    }

    #[tokio::test]
    async fn hash_file_wrong_size() -> anyhow::Result<()> {
        let _ = env_logger::try_init();

        let temp = TempDir::new()?;
        let file = temp.child("somefile");
        let content = b"foobar";
        file.write_binary(content)?;

        let arena = Arena::from("dir");
        let server = server::create_inprocess_client(RealStore::single(arena, temp.path()));
        let ranged = hash_file(
            tarpc::context::current(),
            &server,
            arena,
            &realize_types::Path::parse("somefile")?,
            8,
            4,
            RealStoreOptions::default(),
        )
        .await?;
        let mut expected = RangedHash::new();
        expected.add(ByteRange { start: 0, end: 4 }, hash::digest(b"foob"));
        expected.add(ByteRange { start: 4, end: 8 }, Hash::zero());
        assert_eq!(ranged, expected);

        Ok(())
    }

    /// Return the set of files in [dir] and their content.
    fn snapshot_dir(dir: &std::path::Path) -> anyhow::Result<Vec<(PathBuf, String)>> {
        let mut result = vec![];
        for entry in WalkDir::new(dir).into_iter().flatten() {
            if !entry.path().is_file() {
                continue;
            }
            let relpath = pathdiff::diff_paths(entry.path(), dir);
            if let Some(relpath) = relpath {
                let content = std::fs::read_to_string(entry.path())?;
                result.push((relpath, content));
            }
        }

        Ok(result)
    }

    /// Return the set of files in [dir] and their content (binary).
    fn snapshot_dir_bin(dir: &std::path::Path) -> anyhow::Result<Vec<(PathBuf, Vec<u8>)>> {
        let mut result = vec![];
        for entry in WalkDir::new(dir).into_iter().flatten() {
            if !entry.path().is_file() {
                continue;
            }
            let relpath = pathdiff::diff_paths(entry.path(), dir);
            if let Some(relpath) = relpath {
                let content = std::fs::read(entry.path())?;
                result.push((relpath, content));
            }
        }
        Ok(result)
    }
}

/// Errors returned by [move_dir]
///
/// Error messages are kept short, to avoid repetition when printed
/// with anyhow {:#}.
#[derive(Debug, Error)]
pub enum MoveFileError {
    #[error("RPC error: {0}")]
    Rpc(#[from] tarpc::client::RpcError),
    #[error("Remote Error: {0}")]
    Realize(#[from] RealStoreError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Data still inconsistent after sync")]
    FailedToSync,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
