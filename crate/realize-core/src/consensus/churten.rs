#![allow(dead_code)] // work in progress

use super::jobs;
use super::progress::TxByteCountProgress;
use super::tracker::{JobInfo, JobInfoTracker};
use super::types::{ChurtenNotification, JobProgress};
use crate::rpc::Household;
use futures::StreamExt;
use realize_storage::{Job, JobId, JobStatus, Storage};
use realize_types::Arena;
use std::sync::Arc;
use tarpc::tokio_util::sync::CancellationToken;
use tokio::sync::RwLock;
use tokio::{sync::broadcast, task::JoinHandle};

/// Capacity of the broadcast channel.
///
/// Used by [TxByteCountProgress] to scale down the number of bytes
/// notifications.
const BROADCAST_CHANNEL_CAPACITY: usize = 128;

/// Update byte count when the difference with the last update is at
/// least that many.
const BROADCAST_CHANNEL_RESOLUTION_BYTES: u64 = 64 * 1024;

/// A type that processes jobs and returns the result for [Churten].
///
/// Outside of tests, this is normally [JobHandlerImpl].
pub(crate) trait JobHandler: Sync + Send + Clone {
    fn run(
        &self,
        arena: Arena,
        job: &Arc<Job>,
        progress: &mut TxByteCountProgress,
        shutdown: CancellationToken,
    ) -> impl Future<Output = anyhow::Result<JobStatus>> + Sync + Send;
}

/// Bring the local store and peers closer together.
///
/// Maintains a background job that checks whatever needs to be done
/// and does it. Call [Churten::subscribe] to be notified of what
/// happens on that job.
pub(crate) struct Churten<H: JobHandler> {
    storage: Arc<Storage>,
    handler: H,
    task: Option<(JoinHandle<()>, CancellationToken)>,
    tx: broadcast::Sender<ChurtenNotification>,
    recent_jobs: Arc<RwLock<JobInfoTracker>>,
}

impl Churten<JobHandlerImpl> {
    pub(crate) fn new(storage: Arc<Storage>, household: Household) -> Self {
        Self::with_handler(
            Arc::clone(&storage),
            JobHandlerImpl::new(storage, household),
        )
    }
}

impl<H: JobHandler + 'static> Churten<H> {
    pub(crate) fn with_handler(storage: Arc<Storage>, handler: H) -> Self {
        let (tx, mut rx) = broadcast::channel(BROADCAST_CHANNEL_CAPACITY);

        let tracker = Arc::new(RwLock::new(JobInfoTracker::new(16)));
        tokio::spawn({
            let tracker = Arc::clone(&tracker);

            async move {
                while let Ok(n) = rx.recv().await {
                    tracker.write().await.update(&n);
                }
            }
        });
        Self {
            storage,
            handler,
            task: None,
            tx,
            recent_jobs: tracker,
        }
    }

    /// Return a list of recent jobs.
    ///
    /// The number of finished jobs reported by this method is limited.
    ///
    /// This is a snapshot; for up-to-date information, call [Churten::subscribe].
    pub(crate) async fn recent_jobs(&self) -> Vec<JobInfo> {
        self.recent_jobs.read().await.iter().cloned().collect()
    }

    /// Return a list of active jobs.
    ///
    /// This is a snapshot; for up-to-date information, call [Churten::subscribe].
    pub(crate) async fn active_jobs(&self) -> Vec<JobInfo> {
        self.recent_jobs.read().await.active().cloned().collect()
    }

    /// Subscribe to [ChurtenNotification]s.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ChurtenNotification> {
        self.tx.subscribe()
    }

    /// Check whether the background is running.
    pub(crate) fn is_running(&self) -> bool {
        self.task
            .as_ref()
            .map(|(handle, _)| !handle.is_finished())
            .unwrap_or(false)
    }

    /// Start the background jobs, unless they're already running.
    pub(crate) fn start(&mut self) {
        if self.task.is_some() {
            return;
        }
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            let storage = Arc::clone(&self.storage);
            let handler = self.handler.clone();
            let tx = self.tx.clone();

            async move { background_job(&storage, &handler, tx, shutdown).await }
        });
        self.task = Some((handle, shutdown));
    }

    /// Shutdown the background jobs, but don't wait for them to finish.
    ///
    /// Does nothing if the jobs aren't running.
    pub(crate) fn shutdown(&mut self) {
        log::debug!("shutting down");
        if let Some((_, shutdown)) = self.task.take() {
            shutdown.cancel();
        }
    }
}

/// Number of background jobs to run in parallel.
const PARALLEL_JOB_COUNT: usize = 4;

/// Process jobs from the job stream and report their result to [Storage].
///
/// Processing ends if the stream ends or if cancelled using the token.
async fn background_job<H: JobHandler>(
    storage: &Arc<Storage>,
    handler: &H,
    tx: broadcast::Sender<ChurtenNotification>,
    shutdown: CancellationToken,
) {
    log::debug!("Collecting jobs...");
    let mut result_stream = storage
        .job_stream()
        .map(|(arena, job_id, job)| {
            let job = Arc::new(job);
            log::debug!("[{arena}] PENDING: {job_id} {job:?}");
            let _ = tx.send(ChurtenNotification::New {
                arena,
                job_id,
                job: Arc::clone(&job),
            });

            (arena, job_id, job)
        })
        .map(|(arena, job_id, job)| run_job(handler, arena, job_id, job, &tx, shutdown.clone()))
        .buffer_unordered(PARALLEL_JOB_COUNT);
    while let Some((arena, job_id, status)) = tokio::select!(
        result = result_stream.next() => {
            result
        }
        _ = shutdown.cancelled() => { None })
    {
        let _ = tx.send(ChurtenNotification::Finish {
            arena,
            job_id,
            progress: match &status {
                Ok(JobStatus::Done) => JobProgress::Done,
                Ok(JobStatus::Abandoned) => JobProgress::Abandoned,
                Err(err) => {
                    if shutdown.is_cancelled() {
                        JobProgress::Cancelled
                    } else {
                        JobProgress::Failed(err.to_string())
                    }
                }
            },
        });
        if let Err(err) = storage.job_finished(arena, job_id, status) {
            // We don't want to interrupt job processing, even in this case.
            log::warn!("[{arena}] failed to report status of job {job_id}: {err}");
        }
    }
    log::debug!("Done collecting jobs...");
}

async fn run_job<H: JobHandler>(
    handler: &H,
    arena: Arena,
    job_id: JobId,
    job: Arc<Job>,
    tx: &broadcast::Sender<ChurtenNotification>,
    shutdown: CancellationToken,
) -> (Arena, JobId, anyhow::Result<JobStatus>) {
    log::debug!("[{arena}] STARTING: {job_id} {job:?}");
    let _ = tx.send(ChurtenNotification::Start { arena, job_id });
    let mut progress = TxByteCountProgress::new(arena, job_id, tx.clone())
        .adaptive(BROADCAST_CHANNEL_CAPACITY)
        .with_min_byte_delta(BROADCAST_CHANNEL_RESOLUTION_BYTES);
    let result = handler.run(arena, &job, &mut progress, shutdown).await;

    (arena, job_id, result)
}

/// Dispatch jobs to the relevant function for processing.
#[derive(Clone)]
pub(crate) struct JobHandlerImpl {
    storage: Arc<Storage>,
    household: Household,
}

impl JobHandlerImpl {
    pub(crate) fn new(storage: Arc<Storage>, household: Household) -> Self {
        Self { storage, household }
    }
}

impl JobHandler for JobHandlerImpl {
    async fn run(
        &self,
        arena: Arena,
        job: &Arc<Job>,
        progress: &mut TxByteCountProgress,
        shutdown: CancellationToken,
    ) -> anyhow::Result<JobStatus> {
        match &**job {
            Job::Download(path, hash) => {
                jobs::download(
                    &self.storage,
                    &self.household,
                    arena,
                    path,
                    hash,
                    progress,
                    shutdown,
                )
                .await
            }
            Job::Realize(path, hash, index_hash) => {
                jobs::realize(
                    &self.storage,
                    &self.household,
                    arena,
                    path,
                    hash,
                    index_hash.as_ref(),
                    progress,
                    shutdown,
                )
                .await
            }
            Job::Unrealize(path, hash) => {
                jobs::unrealize(&self.storage, arena, path, hash, progress).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::consensus::progress::ByteCountProgress;
    use crate::consensus::types::JobAction;
    use crate::rpc::testing::{self, HouseholdFixture};
    use realize_storage::utils::hash::{self, digest};
    use realize_storage::{JobId, Mark};
    use tokio::io::AsyncReadExt;

    struct Fixture {
        inner: HouseholdFixture,
    }

    impl Fixture {
        async fn setup() -> anyhow::Result<Self> {
            let _ = env_logger::try_init();
            let household_fixture = HouseholdFixture::setup().await?;

            Ok(Self {
                inner: household_fixture,
            })
        }
    }

    #[tokio::test]
    async fn churten_downloads_file() -> anyhow::Result<()> {
        let mut fixture = Fixture::setup().await?;
        fixture
            .inner
            .with_two_peers()
            .await?
            .run(async |household_a, _household_b| {
                let a = HouseholdFixture::a();
                let b = HouseholdFixture::b();
                let arena = HouseholdFixture::test_arena();
                let storage = fixture.inner.storage(a)?;
                testing::connect(&household_a, b).await?;

                let mut churten = Churten::with_handler(
                    Arc::clone(&storage),
                    JobHandlerImpl::new(Arc::clone(&storage), household_a.clone()),
                );
                let mut rx = churten.subscribe();
                churten.start();

                storage.set_arena_mark(arena, Mark::Keep).await?;
                let foo = fixture.inner.write_file(b, "foo", "this is foo").await?;
                let hash = digest("this is foo");
                let job_id = JobId(1);
                assert_eq!(
                    ChurtenNotification::New {
                        arena,
                        job_id,
                        job: Arc::new(Job::Download(foo, hash)),
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    ChurtenNotification::Start { arena, job_id },
                    rx.recv().await?
                );

                assert_eq!(
                    ChurtenNotification::UpdateAction {
                        arena,
                        job_id,
                        action: JobAction::Download,
                        index: 2,
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    ChurtenNotification::UpdateByteCount {
                        arena,
                        job_id,
                        current_bytes: 0,
                        total_bytes: 11,
                        index: 3,
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    ChurtenNotification::UpdateByteCount {
                        arena,
                        job_id,
                        current_bytes: 11,
                        total_bytes: 11,
                        index: 4,
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    ChurtenNotification::UpdateAction {
                        arena,
                        job_id,
                        action: JobAction::Verify,
                        index: 5,
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    ChurtenNotification::Finish {
                        arena,
                        job_id,
                        progress: JobProgress::Done
                    },
                    rx.recv().await?
                );

                let mut blob = fixture.inner.open_file(a, "foo").await?;
                let mut buf = String::new();
                blob.read_to_string(&mut buf).await?;
                assert_eq!("this is foo", buf);

                Ok::<(), anyhow::Error>(())
            })
            .await?;

        Ok(())
    }

    /// A fake JobHandler for testing different job outcomes
    #[derive(Clone)]
    struct FakeJobHandler {
        result_fn: Arc<dyn Fn() -> anyhow::Result<JobStatus> + Send + Sync>,
        should_send_progress: bool,
        should_cancel: bool,
    }

    impl FakeJobHandler {
        fn new<F>(result_fn: F) -> Self
        where
            F: Fn() -> anyhow::Result<JobStatus> + Send + Sync + 'static,
        {
            Self {
                result_fn: Arc::new(result_fn),
                should_send_progress: false,
                should_cancel: false,
            }
        }

        fn with_progress(mut self, should_send: bool) -> Self {
            self.should_send_progress = should_send;
            self
        }

        fn with_cancel(mut self, should_cancel: bool) -> Self {
            self.should_cancel = should_cancel;
            self
        }
    }

    impl JobHandler for FakeJobHandler {
        async fn run(
            &self,
            _arena: Arena,
            _job: &Arc<Job>,
            progress: &mut TxByteCountProgress,
            shutdown: CancellationToken,
        ) -> anyhow::Result<JobStatus> {
            // Simulate some work time
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

            if self.should_cancel {
                shutdown.cancel();
            }
            if shutdown.is_cancelled() {
                shutdown.cancel();
                anyhow::bail!("cancelled");
            }

            // Send progress updates if requested
            if self.should_send_progress {
                progress.update(50 * 1024, 100 * 1024);
                progress.update(100 * 1024, 100 * 1024);
            }

            // Return the configured result
            (self.result_fn)()
        }
    }

    #[tokio::test]
    async fn churten_job_succeeds() -> anyhow::Result<()> {
        let mut fixture = Fixture::setup().await?;
        fixture
            .inner
            .with_two_peers()
            .await?
            .run(async |household_a, _household_b| {
                let a = HouseholdFixture::a();
                let b = HouseholdFixture::b();
                let arena = HouseholdFixture::test_arena();
                let storage = fixture.inner.storage(a)?;
                testing::connect(&household_a, b).await?;

                let handler = FakeJobHandler::new(|| Ok(JobStatus::Done)).with_progress(true);
                let mut churten = Churten::with_handler(Arc::clone(&storage), handler);
                let mut rx = churten.subscribe();
                churten.start();

                storage.set_arena_mark(arena, Mark::Keep).await?;
                let foo = fixture.inner.write_file(b, "foo", "test content").await?;
                let hash = digest("test content");
                let job_id = JobId(1);
                let job = Arc::new(Job::Download(foo.clone(), hash));
                // Check Pending notification
                assert_eq!(
                    ChurtenNotification::New { arena, job_id, job },
                    rx.recv().await?
                );

                // Check Running notification
                assert_eq!(
                    ChurtenNotification::Start { arena, job_id },
                    rx.recv().await?
                );

                // Check progress updates
                assert_eq!(
                    ChurtenNotification::UpdateByteCount {
                        arena,
                        job_id,
                        current_bytes: 50 * 1024,
                        total_bytes: 100 * 1024,
                        index: 2,
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    ChurtenNotification::UpdateByteCount {
                        arena,
                        job_id,
                        current_bytes: 100 * 1024,
                        total_bytes: 100 * 1024,
                        index: 3,
                    },
                    rx.recv().await?
                );

                // Check Done notification
                assert_eq!(
                    ChurtenNotification::Finish {
                        arena,
                        job_id,
                        progress: JobProgress::Done,
                    },
                    rx.recv().await?
                );

                assert_eq!(None, storage.job_for_path(arena, &foo).await?);
                Ok::<(), anyhow::Error>(())
            })
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn churten_job_abandoned() -> anyhow::Result<()> {
        let mut fixture = Fixture::setup().await?;
        fixture
            .inner
            .with_two_peers()
            .await?
            .run(async |household_a, _household_b| {
                let a = HouseholdFixture::a();
                let b = HouseholdFixture::b();
                let arena = HouseholdFixture::test_arena();
                let storage = fixture.inner.storage(a)?;
                testing::connect(&household_a, b).await?;

                let handler = FakeJobHandler::new(|| Ok(JobStatus::Abandoned));
                let mut churten = Churten::with_handler(Arc::clone(&storage), handler);
                let mut rx = churten.subscribe();
                churten.start();

                storage.set_arena_mark(arena, Mark::Keep).await?;
                let foo = fixture.inner.write_file(b, "foo", "test content").await?;
                let hash = digest("test content");
                let job_id = JobId(1);

                // Check Pending notification
                assert_eq!(
                    ChurtenNotification::New {
                        arena,
                        job_id,
                        job: Arc::new(Job::Download(foo.clone(), hash)),
                    },
                    rx.recv().await?
                );

                // Check Running notification
                assert_eq!(
                    ChurtenNotification::Start { arena, job_id },
                    rx.recv().await?
                );

                // Check Abandoned notification
                assert_eq!(
                    ChurtenNotification::Finish {
                        arena,
                        job_id,
                        progress: JobProgress::Abandoned,
                    },
                    rx.recv().await?
                );

                assert_eq!(None, storage.job_for_path(arena, &foo).await?);
                Ok::<(), anyhow::Error>(())
            })
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn churten_job_fails() -> anyhow::Result<()> {
        let mut fixture = Fixture::setup().await?;
        fixture
            .inner
            .with_two_peers()
            .await?
            .run(async |household_a, _household_b| {
                let a = HouseholdFixture::a();
                let b = HouseholdFixture::b();
                let arena = HouseholdFixture::test_arena();
                let storage = fixture.inner.storage(a)?;
                testing::connect(&household_a, b).await?;

                let error_msg = "Simulated job failure";
                let handler = FakeJobHandler::new(move || Err(anyhow::anyhow!(error_msg)));
                let mut churten = Churten::with_handler(Arc::clone(&storage), handler);
                let mut rx = churten.subscribe();
                churten.start();

                storage.set_arena_mark(arena, Mark::Keep).await?;
                let foo = fixture.inner.write_file(b, "foo", "test content").await?;
                let hash = digest("test content");
                let job_id = JobId(1);
                let job = Arc::new(Job::Download(foo.clone(), hash.clone()));

                // Check Pending notification
                assert_eq!(
                    ChurtenNotification::New { arena, job_id, job },
                    rx.recv().await?
                );

                // Check Running notification
                assert_eq!(
                    ChurtenNotification::Start { arena, job_id },
                    rx.recv().await?
                );

                // Check Failed notification
                assert_eq!(
                    ChurtenNotification::Finish {
                        arena,
                        job_id,
                        progress: JobProgress::Failed(error_msg.to_string()),
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    Some((job_id, Job::Download(foo.clone(), hash))),
                    storage.job_for_path(arena, &foo).await?
                );
                Ok::<(), anyhow::Error>(())
            })
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn churten_job_cancelled() -> anyhow::Result<()> {
        let mut fixture = Fixture::setup().await?;
        fixture
            .inner
            .with_two_peers()
            .await?
            .run(async |household_a, _household_b| {
                let a = HouseholdFixture::a();
                let b = HouseholdFixture::b();
                let arena = HouseholdFixture::test_arena();
                let storage = fixture.inner.storage(a)?;
                testing::connect(&household_a, b).await?;

                // Create a handler that will be cancelled
                let handler = FakeJobHandler::new(|| Ok(JobStatus::Done)).with_cancel(true);
                let mut churten = Churten::with_handler(Arc::clone(&storage), handler);
                let mut rx = churten.subscribe();
                churten.start();

                storage.set_arena_mark(arena, Mark::Keep).await?;
                let foo = fixture.inner.write_file(b, "foo", "test content").await?;
                let hash = digest("test content");
                let job_id = JobId(1);

                // Check Pending notification
                assert_eq!(
                    ChurtenNotification::New {
                        arena,
                        job_id,
                        job: Arc::new(Job::Download(foo.clone(), hash.clone()))
                    },
                    rx.recv().await?
                );

                // Check Running notification
                assert_eq!(
                    ChurtenNotification::Start { arena, job_id },
                    rx.recv().await?
                );

                // Check Cancelled notification
                assert_eq!(
                    ChurtenNotification::Finish {
                        arena,
                        job_id,
                        progress: JobProgress::Cancelled
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    Some((job_id, Job::Download(foo.clone(), hash))),
                    storage.job_for_path(arena, &foo).await?
                );
                Ok::<(), anyhow::Error>(())
            })
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn churten_job_no_progress_updates() -> anyhow::Result<()> {
        let mut fixture = Fixture::setup().await?;
        fixture
            .inner
            .with_two_peers()
            .await?
            .run(async |household_a, _household_b| {
                let a = HouseholdFixture::a();
                let b = HouseholdFixture::b();
                let arena = HouseholdFixture::test_arena();
                let storage = fixture.inner.storage(a)?;
                testing::connect(&household_a, b).await?;

                let handler = FakeJobHandler::new(|| Ok(JobStatus::Done)).with_progress(false);
                let mut churten = Churten::with_handler(Arc::clone(&storage), handler);
                let mut rx = churten.subscribe();
                churten.start();

                storage.set_arena_mark(arena, Mark::Keep).await?;
                let foo = fixture.inner.write_file(b, "foo", "test content").await?;
                let hash = digest("test content");
                let job_id = JobId(1);
                let job = Arc::new(Job::Download(foo, hash));

                // Check Pending notification
                assert_eq!(
                    ChurtenNotification::New { arena, job_id, job },
                    rx.recv().await?
                );

                // Check Running notification
                assert_eq!(
                    ChurtenNotification::Start { arena, job_id },
                    rx.recv().await?
                );

                // Check Done notification (no progress updates)
                assert_eq!(
                    ChurtenNotification::Finish {
                        arena,
                        job_id,
                        progress: JobProgress::Done,
                    },
                    rx.recv().await?
                );

                Ok::<(), anyhow::Error>(())
            })
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn churten_multiple_jobs_parallel() -> anyhow::Result<()> {
        let mut fixture = Fixture::setup().await?;
        fixture
            .inner
            .with_two_peers()
            .await?
            .run(async |household_a, _household_b| {
                let a = HouseholdFixture::a();
                let b = HouseholdFixture::b();
                let arena = HouseholdFixture::test_arena();
                let storage = fixture.inner.storage(a)?;
                testing::connect(&household_a, b).await?;

                let handler = FakeJobHandler::new(|| Ok(JobStatus::Done));
                let mut churten = Churten::with_handler(Arc::clone(&storage), handler);
                let mut rx = churten.subscribe();
                churten.start();

                storage.set_arena_mark(arena, Mark::Keep).await?;

                // Create multiple files to trigger multiple jobs
                fixture.inner.write_file(b, "foo1", "content1").await?;
                fixture.inner.write_file(b, "foo2", "content2").await?;
                fixture.inner.write_file(b, "foo3", "content3").await?;

                // Collect all notifications
                let mut notifications = Vec::new();
                for _ in 0..9 {
                    // 3 jobs × 3 notifications each (Pending, Running, Done)
                    if let Ok(notification) = rx.recv().await {
                        notifications.push(notification);
                    }
                }

                // Verify we got the expected number of notifications
                assert_eq!(notifications.len(), 9);

                // Verify each job has the expected sequence
                let job1_notifications: Vec<_> = notifications
                    .iter()
                    .filter(|n| n.job_id() == JobId(1))
                    .collect();
                let job2_notifications: Vec<_> = notifications
                    .iter()
                    .filter(|n| n.job_id() == JobId(2))
                    .collect();
                let job3_notifications: Vec<_> = notifications
                    .iter()
                    .filter(|n| n.job_id() == JobId(3))
                    .collect();

                assert_eq!(job1_notifications.len(), 3);
                assert_eq!(job2_notifications.len(), 3);
                assert_eq!(job3_notifications.len(), 3);

                // Verify each job has the correct sequence
                for job_notifications in
                    [job1_notifications, job2_notifications, job3_notifications]
                {
                    assert!(matches!(
                        job_notifications[0],
                        ChurtenNotification::New { .. }
                    ));
                    assert!(matches!(
                        job_notifications[1],
                        ChurtenNotification::Start { .. }
                    ));
                    assert!(matches!(
                        job_notifications[2],
                        ChurtenNotification::Finish {
                            progress: JobProgress::Done,
                            ..
                        }
                    ));
                }

                Ok::<(), anyhow::Error>(())
            })
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn recent_jobs() -> anyhow::Result<()> {
        let mut fixture = Fixture::setup().await?;
        fixture
            .inner
            .with_two_peers()
            .await?
            .run(async |household_a, _household_b| {
                let a = HouseholdFixture::a();
                let b = HouseholdFixture::b();
                let arena = HouseholdFixture::test_arena();
                let storage = fixture.inner.storage(a)?;
                testing::connect(&household_a, b).await?;

                let handler = FakeJobHandler::new(|| Ok(JobStatus::Done));
                let mut churten = Churten::with_handler(Arc::clone(&storage), handler);
                let mut rx = churten.subscribe();
                churten.start();

                storage.set_arena_mark(arena, Mark::Keep).await?;

                // Create multiple files to trigger multiple jobs
                let foo1 = fixture.inner.write_file(b, "foo1", "content1").await?;
                let foo2 = fixture.inner.write_file(b, "foo2", "content2").await?;
                let foo3 = fixture.inner.write_file(b, "foo3", "content3").await?;

                // Collect all notifications
                let mut finished_count = 0;
                while let Ok(notification) = rx.recv().await {
                    match notification {
                        ChurtenNotification::Finish { .. } => {
                            finished_count += 1;
                            if finished_count == 3 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }

                let mut recent_jobs = churten
                    .recent_jobs()
                    .await
                    .into_iter()
                    .map(|job| (job.id, job))
                    .collect::<HashMap<JobId, JobInfo>>();

                assert_eq!(
                    Some(JobInfo {
                        arena,
                        id: JobId(1),
                        job: Arc::new(Job::Download(foo1, hash::digest("content1"))),
                        progress: JobProgress::Done,
                        action: None,
                        byte_progress: None,
                        notification_index: 0,
                    }),
                    recent_jobs.remove(&JobId(1))
                );
                assert_eq!(
                    Some(JobInfo {
                        arena,
                        id: JobId(2),
                        job: Arc::new(Job::Download(foo2, hash::digest("content2"))),
                        progress: JobProgress::Done,
                        action: None,
                        byte_progress: None,
                        notification_index: 0,
                    }),
                    recent_jobs.remove(&JobId(2))
                );
                assert_eq!(
                    Some(JobInfo {
                        arena,
                        id: JobId(3),
                        job: Arc::new(Job::Download(foo3, hash::digest("content3"))),
                        progress: JobProgress::Done,
                        action: None,
                        byte_progress: None,
                        notification_index: 0,
                    }),
                    recent_jobs.remove(&JobId(3))
                );

                Ok::<(), anyhow::Error>(())
            })
            .await?;

        Ok(())
    }
}
