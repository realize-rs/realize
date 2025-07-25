#![allow(dead_code)] // work in progress

use super::{download::download, progress::ByteCountProgress};
use crate::rpc::Household;
use futures::StreamExt;
use realize_storage::{Job, JobStatus, Storage};
use realize_types::Arena;
use std::sync::Arc;
use tarpc::tokio_util::sync::CancellationToken;
use tokio::{sync::broadcast, task::JoinHandle};

/// Notifications broadcast by [Churten].
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ChurtenNotification {
    /// Report the general job state.
    Update {
        arena: Arena,
        job: Arc<Job>,
        progress: JobProgress,
    },

    /// Report bytecount update, such as for a copy or a download job.
    ///
    /// Not all jobs emit such updates.
    UpdateByteCount {
        arena: Arena,
        job: Arc<Job>,

        /// Current number of bytes.
        ///
        /// The first such update normally, but not necessarily has
        /// current_bytes set to 0.
        ///
        /// This value normally but not necessarily increases.
        current_bytes: u64,

        /// Total (expected) number of bytes.
        ///
        /// This value is normally, but not necessarily stable.
        total_bytes: u64,
    },
}

impl ChurtenNotification {
    pub(crate) fn arena(&self) -> Arena {
        match self {
            ChurtenNotification::Update { arena, .. } => *arena,
            ChurtenNotification::UpdateByteCount { arena, .. } => *arena,
        }
    }
    pub(crate) fn job(&self) -> &Arc<Job> {
        match self {
            ChurtenNotification::Update { job, .. } => job,
            ChurtenNotification::UpdateByteCount { job, .. } => job,
        }
    }
}

/// Job progress reported by [ChurtenNotification]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JobProgress {
    /// A job exists, but isn't running yet.
    Pending,

    /// The job is running.
    Running,

    /// The job was completed successfully.
    Done,

    /// The job was abandoned, likely because it is outdated.
    Abandoned,

    /// The job was cancelled by a call to [Churten::shutdown].
    Cancelled,

    /// The job failed. It may be retried.
    ///
    /// The string is an error description.
    Failed(String),
}

/// A type that processes jobs and returns the result for [Churten].
///
/// Outside of tests, this is normally [JobHandlerImpl].
pub(crate) trait JobHandler: Sync + Send + Clone {
    fn run(
        &self,
        arena: Arena,
        job: &Arc<Job>,
        tx: broadcast::Sender<ChurtenNotification>,
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
        let (tx, _) = broadcast::channel(16);

        Self {
            storage,
            handler,
            task: None,
            tx,
        }
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
    let mut result_stream = storage
        .job_stream()
        .map(|(arena, job)| {
            let job = Arc::new(job);
            let _ = tx.send(ChurtenNotification::Update {
                arena,
                job: Arc::clone(&job),
                progress: JobProgress::Pending,
            });

            (arena, job)
        })
        .map(|(arena, job)| run_job(handler, arena, job, &tx, shutdown.clone()))
        .buffer_unordered(PARALLEL_JOB_COUNT);
    while let Some((arena, job, status)) = tokio::select!(
        result = result_stream.next() => {
            result
        }
        _ = shutdown.cancelled() => { None })
    {
        let _ = tx.send(ChurtenNotification::Update {
            arena,
            job: Arc::clone(&job),
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
        if let Err(err) = storage.job_finished(arena, &job, status) {
            // We don't want to interrupt job processing, even in this case.
            log::warn!("[{arena}] failed to report status of {job:?}: {err}");
        }
    }
}

async fn run_job<H: JobHandler>(
    handler: &H,
    arena: Arena,
    job: Arc<Job>,
    tx: &broadcast::Sender<ChurtenNotification>,
    shutdown: CancellationToken,
) -> (Arena, Arc<Job>, anyhow::Result<JobStatus>) {
    log::debug!("[{arena}] STARTING: {job:?}");
    let _ = tx.send(ChurtenNotification::Update {
        arena,
        job: job.clone(),
        progress: JobProgress::Running,
    });
    let result = handler.run(arena, &job, tx.clone(), shutdown).await;

    (arena, job, result)
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
        tx: broadcast::Sender<ChurtenNotification>,
        shutdown: CancellationToken,
    ) -> anyhow::Result<JobStatus> {
        let mut progress = TxByteCountProgress {
            tx,
            arena,
            job: Arc::clone(job),
        };
        match &**job {
            Job::Download(path, _, hash) => {
                download(
                    &self.storage,
                    &self.household,
                    arena,
                    path,
                    hash,
                    &mut progress,
                    shutdown,
                )
                .await
            }
        }
    }
}

struct TxByteCountProgress {
    tx: broadcast::Sender<ChurtenNotification>,
    arena: Arena,
    job: Arc<Job>,
}

impl ByteCountProgress for TxByteCountProgress {
    fn update(&mut self, current_bytes: u64, total_bytes: u64) {
        let _ = self.tx.send(ChurtenNotification::UpdateByteCount {
            arena: self.arena,
            job: Arc::clone(&self.job),
            current_bytes,
            total_bytes,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::testing::{self, HouseholdFixture};
    use realize_storage::utils::hash::digest;
    use realize_storage::{JobUpdate, Mark};
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
                let job = Arc::new(Job::Download(foo, 1, hash));
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Pending,
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Running,
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    ChurtenNotification::UpdateByteCount {
                        arena,
                        job: job.clone(),
                        current_bytes: 0,
                        total_bytes: 11,
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    ChurtenNotification::UpdateByteCount {
                        arena,
                        job: job.clone(),
                        current_bytes: 11,
                        total_bytes: 11,
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Done,
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
            arena: Arena,
            job: &Arc<Job>,
            tx: broadcast::Sender<ChurtenNotification>,
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
                let _ = tx.send(ChurtenNotification::UpdateByteCount {
                    arena,
                    job: Arc::clone(job),
                    current_bytes: 50,
                    total_bytes: 100,
                });
                tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                let _ = tx.send(ChurtenNotification::UpdateByteCount {
                    arena,
                    job: Arc::clone(job),
                    current_bytes: 100,
                    total_bytes: 100,
                });
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
                let job = Arc::new(Job::Download(foo, 1, hash));

                // Check Pending notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Pending,
                    },
                    rx.recv().await?
                );

                // Check Running notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Running,
                    },
                    rx.recv().await?
                );

                // Check progress updates
                assert_eq!(
                    ChurtenNotification::UpdateByteCount {
                        arena,
                        job: job.clone(),
                        current_bytes: 50,
                        total_bytes: 100,
                    },
                    rx.recv().await?
                );

                assert_eq!(
                    ChurtenNotification::UpdateByteCount {
                        arena,
                        job: job.clone(),
                        current_bytes: 100,
                        total_bytes: 100,
                    },
                    rx.recv().await?
                );

                // Check Done notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Done,
                    },
                    rx.recv().await?
                );

                assert_eq!(JobUpdate::Outdated, storage.check_job(arena, &*job).await?);
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
                let job = Arc::new(Job::Download(foo, 1, hash));

                // Check Pending notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Pending,
                    },
                    rx.recv().await?
                );

                // Check Running notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Running,
                    },
                    rx.recv().await?
                );

                // Check Abandoned notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Abandoned,
                    },
                    rx.recv().await?
                );

                assert_eq!(JobUpdate::Outdated, storage.check_job(arena, &*job).await?);
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
                let job = Arc::new(Job::Download(foo, 1, hash));

                // Check Pending notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Pending,
                    },
                    rx.recv().await?
                );

                // Check Running notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Running,
                    },
                    rx.recv().await?
                );

                // Check Failed notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Failed(error_msg.to_string()),
                    },
                    rx.recv().await?
                );

                assert_eq!(JobUpdate::Same, storage.check_job(arena, &*job).await?);
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
                let job = Arc::new(Job::Download(foo, 1, hash));

                // Check Pending notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Pending,
                    },
                    rx.recv().await?
                );

                // Check Running notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Running,
                    },
                    rx.recv().await?
                );

                // Check Cancelled notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Cancelled,
                    },
                    rx.recv().await?
                );

                assert_eq!(JobUpdate::Same, storage.check_job(arena, &*job).await?);
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
                let job = Arc::new(Job::Download(foo, 1, hash));

                // Check Pending notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Pending,
                    },
                    rx.recv().await?
                );

                // Check Running notification
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
                        progress: JobProgress::Running,
                    },
                    rx.recv().await?
                );

                // Check Done notification (no progress updates)
                assert_eq!(
                    ChurtenNotification::Update {
                        arena,
                        job: job.clone(),
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
                let foo1 = fixture.inner.write_file(b, "foo1", "content1").await?;
                let foo2 = fixture.inner.write_file(b, "foo2", "content2").await?;
                let foo3 = fixture.inner.write_file(b, "foo3", "content3").await?;

                let hash1 = digest("content1");
                let hash2 = digest("content2");
                let hash3 = digest("content3");

                let job1 = Arc::new(Job::Download(foo1, 1, hash1));
                let job2 = Arc::new(Job::Download(foo2, 2, hash2));
                let job3 = Arc::new(Job::Download(foo3, 3, hash3));

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
                    .filter(|n| n.job().counter() == job1.counter())
                    .collect();
                let job2_notifications: Vec<_> = notifications
                    .iter()
                    .filter(|n| n.job().counter() == job2.counter())
                    .collect();
                let job3_notifications: Vec<_> = notifications
                    .iter()
                    .filter(|n| n.job().counter() == job3.counter())
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
                        ChurtenNotification::Update {
                            progress: JobProgress::Pending,
                            ..
                        }
                    ));
                    assert!(matches!(
                        job_notifications[1],
                        ChurtenNotification::Update {
                            progress: JobProgress::Running,
                            ..
                        }
                    ));
                    assert!(matches!(
                        job_notifications[2],
                        ChurtenNotification::Update {
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
}
