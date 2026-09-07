//! A small `std::thread` worker pool that drives a [`Scheduler`].
//!
//! The scheduler owns all the policy — budgets, priority order, result
//! validation. This type only adds execution: N worker threads pull dispatched
//! jobs, run the closures off the caller's thread, and hand results back. It is
//! deliberately thin; the deterministic [`Scheduler`] remains the thing tests
//! and the tick loop use directly.
//!
//! Shutdown is explicit and total: [`ThreadJobPool::shutdown`] drops every
//! still-queued job, lets every already-dispatched job run to completion, joins
//! the workers, and returns the inner scheduler so any validated results can
//! still be drained.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::scheduler::{
    Dispatch, InstallOutcome, JobHandle, JobRequest, Rejected, ReloadSummary, Scheduler,
};
use crate::token::WorldView;
use crate::{Generation, SchedulerConfig};

struct Shared<T> {
    scheduler: Mutex<Scheduler<T>>,
    ready: Mutex<VecDeque<Dispatch<T>>>,
    signal: Condvar,
    stop: AtomicBool,
}

/// A worker pool wrapping a [`Scheduler`]. See the module docs.
pub struct ThreadJobPool<T: Send + 'static> {
    shared: Arc<Shared<T>>,
    workers: Vec<JoinHandle<()>>,
}

impl<T: Send + 'static> ThreadJobPool<T> {
    /// Start `worker_count` worker threads (clamped to at least 1).
    pub fn new(config: SchedulerConfig, worker_count: usize) -> Self {
        Self::with_generation(config, Generation::START, worker_count)
    }

    pub fn with_generation(
        config: SchedulerConfig,
        generation: Generation,
        worker_count: usize,
    ) -> Self {
        let shared = Arc::new(Shared {
            scheduler: Mutex::new(Scheduler::with_generation(config, generation)),
            ready: Mutex::new(VecDeque::new()),
            signal: Condvar::new(),
            stop: AtomicBool::new(false),
        });

        let workers = (0..worker_count.max(1))
            .map(|_| {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || worker_loop(&shared))
            })
            .collect();

        Self { shared, workers }
    }

    /// Submit a job. On success the pool immediately makes any newly dispatchable
    /// work visible to the workers.
    pub fn submit(&self, request: JobRequest<T>) -> Result<JobHandle, Rejected<T>> {
        let handle = {
            let mut scheduler = self.lock_scheduler();
            scheduler.submit(request)?
        };
        self.pump();
        Ok(handle)
    }

    /// Cancel a job (see [`Scheduler::cancel`]).
    pub fn cancel(&self, handle: JobHandle) -> bool {
        self.lock_scheduler().cancel(handle)
    }

    /// Reload the world (see [`Scheduler::reload_world`]).
    pub fn reload_world(&self, new_generation: Generation) -> ReloadSummary {
        self.lock_scheduler().reload_world(new_generation)
    }

    /// Validate and take the results that have completed so far.
    pub fn install<W: WorldView + ?Sized>(&self, world: &W) -> InstallOutcome<T> {
        let outcome = self.lock_scheduler().install(world);
        // Installation releases completed-result capacity. Pump immediately so
        // work that was held behind that bound does not require an unrelated
        // later submission to start.
        self.pump();
        outcome
    }

    /// `true` while any job is queued or running.
    pub fn has_active_jobs(&self) -> bool {
        self.lock_scheduler().has_active_jobs()
    }

    pub fn generation(&self) -> Generation {
        self.lock_scheduler().generation()
    }

    /// Stop accepting work, run every dispatched job to completion, join the
    /// workers, and return the inner scheduler.
    pub fn shutdown(self) -> Scheduler<T> {
        self.lock_scheduler().begin_shutdown();
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.signal.notify_all();
        for worker in self.workers {
            let _ = worker.join();
        }
        // All workers have dropped their Arc clone; we hold the last one.
        let shared = Arc::into_inner(self.shared)
            .expect("all workers joined, so the pool holds the only Shared reference");
        shared
            .scheduler
            .into_inner()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn lock_scheduler(&self) -> std::sync::MutexGuard<'_, Scheduler<T>> {
        self.shared
            .scheduler
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Move newly dispatchable jobs from the scheduler into the ready list and
    /// wake workers. Lock order is always scheduler → ready.
    fn pump(&self) {
        let mut scheduler = self.lock_scheduler();
        let dispatched = scheduler.dispatch();
        drop(scheduler);
        if dispatched.is_empty() {
            return;
        }
        let mut ready = lock(&self.shared.ready);
        ready.extend(dispatched);
        drop(ready);
        self.shared.signal.notify_all();
    }
}

impl<T: Send + 'static> std::fmt::Debug for ThreadJobPool<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadJobPool")
            .field("workers", &self.workers.len())
            .field("stopping", &self.shared.stop.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn worker_loop<T: Send + 'static>(shared: &Shared<T>) {
    loop {
        let dispatch = {
            let mut ready = lock(&shared.ready);
            loop {
                if let Some(job) = ready.pop_front() {
                    break Some(job);
                }
                if shared.stop.load(Ordering::SeqCst) {
                    break None;
                }
                ready = shared
                    .signal
                    .wait(ready)
                    .unwrap_or_else(|poison| poison.into_inner());
            }
        };

        let Some(dispatch) = dispatch else {
            return;
        };

        let completion = dispatch.run();

        // Apply the result, then surface any work a freed in-flight slot unlocks.
        let follow_on = {
            let mut scheduler = lock(&shared.scheduler);
            scheduler.apply(completion);
            scheduler.dispatch()
        };
        if !follow_on.is_empty() {
            let mut ready = lock(&shared.ready);
            ready.extend(follow_on);
            drop(ready);
            shared.signal.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TopologyEpoch;
    use crate::budget::{Lane, LaneBudget, Priority};
    use crate::testkit::MapWorld;
    use crate::token::JobToken;
    use spall_core::{BrickCoord, Revision, VolumeId};
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    fn vol() -> VolumeId {
        VolumeId::new(1).unwrap()
    }

    fn token(brick: i64) -> JobToken {
        JobToken::new(Generation(1), TopologyEpoch::START).reading(
            vol(),
            BrickCoord::new(brick, 0, 0),
            Revision(1),
        )
    }

    fn wait_until(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "pool did not settle in time");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn pool_runs_jobs_across_threads_and_installs_fresh_results() {
        let config = SchedulerConfig::uniform(LaneBudget::new(128, 1 << 24, 4));
        let pool = ThreadJobPool::<u64>::with_generation(config, Generation(1), 4);

        let mut world = MapWorld::new(Generation(1));
        let ran = Arc::new(AtomicUsize::new(0));
        const N: i64 = 64;
        for brick in 0..N {
            world.set_brick(vol(), BrickCoord::new(brick, 0, 0), Revision(1));
            let ran = Arc::clone(&ran);
            pool.submit(JobRequest::new(
                Lane::Visual,
                Priority::NORMAL,
                token(brick),
                move || {
                    ran.fetch_add(1, Ordering::SeqCst);
                    brick as u64
                },
            ))
            .unwrap();
        }

        wait_until(|| ran.load(Ordering::SeqCst) == N as usize && !pool.has_active_jobs());

        let outcome = pool.install(&world);
        assert_eq!(outcome.installed.len(), N as usize);
        assert!(outcome.discarded.is_empty());
        let mut values: Vec<u64> = outcome.installed.iter().map(|i| i.output).collect();
        values.sort_unstable();
        assert_eq!(values, (0..N as u64).collect::<Vec<_>>());

        let scheduler = pool.shutdown();
        assert!(scheduler.is_drained());
    }

    #[test]
    fn pool_shutdown_cancels_queued_work_and_drains_dispatched_work() {
        // One worker, one in-flight slot: most jobs will still be queued when we
        // pull the plug.
        let config = SchedulerConfig::uniform(LaneBudget::new(256, 1 << 24, 1));
        let pool = ThreadJobPool::<u64>::with_generation(config, Generation(1), 1);

        let ran = Arc::new(AtomicUsize::new(0));
        const N: usize = 200;
        for brick in 0..N as i64 {
            let ran = Arc::clone(&ran);
            pool.submit(JobRequest::new(
                Lane::Edit,
                Priority::NORMAL,
                token(brick),
                move || {
                    ran.fetch_add(1, Ordering::SeqCst);
                    0
                },
            ))
            .unwrap();
        }

        let scheduler = pool.shutdown();
        // Every job either ran or was cancelled while queued; none is left behind.
        assert!(!scheduler.has_active_jobs());
        let executed = ran.load(Ordering::SeqCst);
        assert!(executed <= N);
        // Results of executed-and-fresh jobs are still retrievable.
        let world = MapWorld::new(Generation(1)); // no bricks resident -> all stale
        let outcome = scheduler_install(scheduler, &world);
        assert_eq!(outcome.installed.len() + outcome.discarded.len(), executed);
    }

    #[test]
    fn install_pumps_work_released_by_completed_result_backpressure() {
        let config = SchedulerConfig::uniform(LaneBudget {
            max_queued_jobs: 2,
            max_queued_bytes: 1 << 20,
            max_in_flight: 1,
            max_completed_jobs: 1,
        });
        let pool = ThreadJobPool::<u64>::with_generation(config, Generation(1), 1);
        let ran = Arc::new(AtomicUsize::new(0));
        for brick in 0..2 {
            let ran_for_job = Arc::clone(&ran);
            pool.submit(JobRequest::new(
                Lane::Topology,
                Priority::NORMAL,
                token(brick),
                move || {
                    ran_for_job.fetch_add(1, Ordering::SeqCst);
                    brick as u64
                },
            ))
            .unwrap();
            if brick == 0 {
                wait_until(|| ran.load(Ordering::SeqCst) == 1 && !pool.has_active_jobs());
            }
        }

        // The second job is queued while the first completion fills the one
        // result slot. Draining that result must wake the worker itself.
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        pool.install(&MapWorld::new(Generation(1)));
        wait_until(|| ran.load(Ordering::SeqCst) == 2);
        let _ = pool.shutdown();
    }

    fn scheduler_install(mut scheduler: Scheduler<u64>, world: &MapWorld) -> InstallOutcome<u64> {
        scheduler.install(world)
    }
}
