use std::sync::atomic::{AtomicBool, Ordering};

type MultiTcpPanic = Box<dyn std::any::Any + Send + 'static>;

#[derive(Clone, Copy, Eq, PartialEq)]
enum MultiTcpWorkerState {
    Setup,
    Running,
    Released,
    Cancelled,
}

type MultiTcpWorkerGate =
    std::sync::Arc<(std::sync::Mutex<MultiTcpWorkerState>, std::sync::Condvar)>;

fn set_multi_tcp_worker_state(gate: &MultiTcpWorkerGate, state: MultiTcpWorkerState) {
    let (current, changed) = &**gate;
    *current.lock().unwrap_or_else(|error| error.into_inner()) = state;
    changed.notify_all();
}

enum MultiTcpWorkerEvent<R> {
    Ready(usize, Result<(), String>),
    Finished(usize, R),
    Failed(MultiTcpPanic),
}

pub(super) struct MultiTcpWorkerPhases<R> {
    worker_id: usize,
    gate: MultiTcpWorkerGate,
    cancelled: std::sync::Arc<AtomicBool>,
    events: std::sync::mpsc::SyncSender<MultiTcpWorkerEvent<R>>,
}

impl<R> MultiTcpWorkerPhases<R> {
    pub(super) fn ready(&mut self, setup: Result<(), String>) -> bool {
        if self
            .events
            .send(MultiTcpWorkerEvent::Ready(self.worker_id, setup))
            .is_err()
        {
            return false;
        }
        self.wait_while(MultiTcpWorkerState::Setup, MultiTcpWorkerState::Running)
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub(super) fn finished(&mut self, result: R) -> bool {
        if self
            .events
            .send(MultiTcpWorkerEvent::Finished(self.worker_id, result))
            .is_err()
        {
            return false;
        }
        self.wait_while(MultiTcpWorkerState::Running, MultiTcpWorkerState::Released)
    }

    fn wait_while(&self, waiting: MultiTcpWorkerState, proceed: MultiTcpWorkerState) -> bool {
        let (state, changed) = &*self.gate;
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        while *state == waiting {
            state = changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        *state == proceed
    }
}

pub(super) struct MultiTcpWorkers<R> {
    worker_count: usize,
    gate: MultiTcpWorkerGate,
    cancelled: std::sync::Arc<AtomicBool>,
    events: std::sync::mpsc::Receiver<MultiTcpWorkerEvent<R>>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl<R> MultiTcpWorkers<R> {
    pub(super) fn spawn<F>(worker_count: usize, worker: F) -> Result<Self, String>
    where
        R: Send + 'static,
        F: Fn(usize, MultiTcpWorkerPhases<R>) + Send + Sync + 'static,
    {
        let gate = std::sync::Arc::new((
            std::sync::Mutex::new(MultiTcpWorkerState::Setup),
            std::sync::Condvar::new(),
        ));
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let (event_tx, events) = std::sync::mpsc::sync_channel(worker_count);
        let worker = std::sync::Arc::new(worker);
        let mut handles = Vec::with_capacity(worker_count);

        for worker_id in 0..worker_count {
            let worker_gate = gate.clone();
            let worker_cancelled = cancelled.clone();
            let events = event_tx.clone();
            let worker = worker.clone();
            let spawn = std::thread::Builder::new()
                .name(format!("multi-tcp-{worker_id}"))
                .spawn(move || {
                    let panic_gate = worker_gate.clone();
                    let panic_cancelled = worker_cancelled.clone();
                    let phases = MultiTcpWorkerPhases {
                        worker_id,
                        gate: worker_gate,
                        cancelled: worker_cancelled,
                        events: events.clone(),
                    };
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        worker(worker_id, phases);
                    }));
                    if let Err(panic) = outcome {
                        panic_cancelled.store(true, Ordering::Relaxed);
                        set_multi_tcp_worker_state(&panic_gate, MultiTcpWorkerState::Cancelled);
                        let _ = events.send(MultiTcpWorkerEvent::Failed(panic));
                    }
                });

            match spawn {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    cancelled.store(true, Ordering::Relaxed);
                    set_multi_tcp_worker_state(&gate, MultiTcpWorkerState::Cancelled);
                    for handle in handles {
                        let _ = handle.join();
                    }
                    return Err(format!("failed to spawn worker {worker_id}: {error}"));
                }
            }
        }
        drop(event_tx);

        Ok(Self {
            worker_count,
            gate,
            cancelled,
            events,
            handles,
        })
    }

    pub(super) fn wait_ready(&mut self) -> Result<(), String> {
        let mut ready = vec![false; self.worker_count];
        let mut ready_count = 0;
        while ready_count < self.worker_count {
            let event = match self.events.recv() {
                Ok(event) => event,
                Err(_) => self.abort_with_message("worker event channel closed before ready"),
            };
            match event {
                MultiTcpWorkerEvent::Ready(worker_id, setup)
                    if worker_id < self.worker_count && !ready[worker_id] =>
                {
                    ready[worker_id] = true;
                    ready_count += 1;
                    if let Err(error) = setup {
                        self.cancel();
                        let join_panic = self.join_all();
                        if let Some(panic) = self.take_worker_panic() {
                            std::panic::resume_unwind(panic);
                        }
                        if let Some(panic) = join_panic {
                            std::panic::resume_unwind(panic);
                        }
                        return Err(format!("worker {worker_id}: {error}"));
                    }
                }
                MultiTcpWorkerEvent::Ready(worker_id, _) => self.abort_with_message(&format!(
                    "invalid or duplicate ready event from worker {worker_id}"
                )),
                MultiTcpWorkerEvent::Finished(worker_id, _) => self.abort_with_message(&format!(
                    "worker {worker_id} finished before the steady phase"
                )),
                MultiTcpWorkerEvent::Failed(panic) => self.abort_worker_panic(panic),
            }
        }
        Ok(())
    }

    pub(super) fn start(&self) {
        set_multi_tcp_worker_state(&self.gate, MultiTcpWorkerState::Running);
    }

    pub(super) fn wait_finished(&mut self, results: &mut [Option<R>]) {
        if results.len() != self.worker_count {
            self.abort_with_message("worker result slot count did not match worker count");
        }
        let mut finished_count = 0;
        while finished_count < self.worker_count {
            let event = match self.events.recv() {
                Ok(event) => event,
                Err(_) => self.abort_with_message("worker event channel closed before finish"),
            };
            match event {
                MultiTcpWorkerEvent::Finished(worker_id, result)
                    if worker_id < self.worker_count && results[worker_id].is_none() =>
                {
                    results[worker_id] = Some(result);
                    finished_count += 1;
                }
                MultiTcpWorkerEvent::Finished(worker_id, _) => self.abort_with_message(&format!(
                    "invalid or duplicate finish event from worker {worker_id}"
                )),
                MultiTcpWorkerEvent::Ready(worker_id, _) => self
                    .abort_with_message(&format!("duplicate ready event from worker {worker_id}")),
                MultiTcpWorkerEvent::Failed(panic) => self.abort_worker_panic(panic),
            }
        }
    }

    pub(super) fn release_and_join(mut self) {
        set_multi_tcp_worker_state(&self.gate, MultiTcpWorkerState::Released);
        let join_panic = self.join_all();
        if let Some(panic) = self.take_worker_panic() {
            std::panic::resume_unwind(panic);
        }
        if let Some(panic) = join_panic {
            std::panic::resume_unwind(panic);
        }
    }

    fn abort_worker_panic(&mut self, panic: MultiTcpPanic) -> ! {
        self.cancel();
        let _ = self.join_all();
        std::panic::resume_unwind(panic);
    }

    fn abort_with_message(&mut self, message: &str) -> ! {
        self.cancel();
        let join_panic = self.join_all();
        if let Some(panic) = self.take_worker_panic() {
            std::panic::resume_unwind(panic);
        }
        if let Some(panic) = join_panic {
            std::panic::resume_unwind(panic);
        }
        panic!("{message}");
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        set_multi_tcp_worker_state(&self.gate, MultiTcpWorkerState::Cancelled);
    }

    fn join_all(&mut self) -> Option<MultiTcpPanic> {
        let mut first_panic = None;
        for handle in self.handles.drain(..) {
            if let Err(panic) = handle.join()
                && first_panic.is_none()
            {
                first_panic = Some(panic);
            }
        }
        first_panic
    }

    fn take_worker_panic(&self) -> Option<MultiTcpPanic> {
        self.events.try_iter().find_map(|event| match event {
            MultiTcpWorkerEvent::Failed(panic) => Some(panic),
            _ => None,
        })
    }
}

impl<R> Drop for MultiTcpWorkers<R> {
    fn drop(&mut self) {
        if !self.handles.is_empty() {
            self.cancel();
            let _ = self.join_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_multi_tcp_worker_panic_propagates(fail_before_ready: bool) {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let (completed_tx, completed_rx) = mpsc::channel();
        let supervisor = thread::spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let mut workers = MultiTcpWorkers::<()>::spawn(2, move |worker_id, mut phases| {
                    if worker_id == 0 && fail_before_ready {
                        panic!("injected multi_tcp setup failure");
                    }
                    if !phases.ready(Ok(())) {
                        return;
                    }
                    if worker_id == 0 && !fail_before_ready {
                        panic!("injected multi_tcp work failure");
                    }
                    if worker_id != 0 && !fail_before_ready {
                        thread::sleep(Duration::from_millis(50));
                    }
                    let _ = phases.finished(());
                })
                .unwrap();
                workers.wait_ready().unwrap();
                let mut results = std::iter::repeat_with(|| None).take(2).collect::<Vec<_>>();
                workers.start();
                workers.wait_finished(&mut results);
                workers.release_and_join();
            }));
            let _ = completed_tx.send(outcome);
        });

        let outcome = completed_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("multi_tcp coordinator did not propagate worker failure");
        supervisor.join().unwrap();
        let panic = outcome.expect_err("injected worker panic was not resumed");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert!(
            message.is_some_and(|message| message.starts_with("injected multi_tcp")),
            "unexpected panic payload: {message:?}"
        );
    }

    #[test]
    fn multi_tcp_coordinator_propagates_setup_panic_without_deadlock() {
        assert_multi_tcp_worker_panic_propagates(true);
    }

    #[test]
    fn multi_tcp_coordinator_propagates_work_panic_without_deadlock() {
        assert_multi_tcp_worker_panic_propagates(false);
    }

    #[test]
    fn worker_panic_cancels_a_long_running_peer_promptly() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        let (completed_tx, completed_rx) = mpsc::channel();
        let supervisor = thread::spawn(move || {
            let started = Instant::now();
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let mut workers = MultiTcpWorkers::<()>::spawn(2, |worker_id, mut phases| {
                    if !phases.ready(Ok(())) {
                        return;
                    }
                    if worker_id == 0 {
                        panic!("injected multi_tcp work failure");
                    }
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let mut iterations = 0u64;
                    while Instant::now() < deadline {
                        if iterations & 0xff == 0 && phases.is_cancelled() {
                            break;
                        }
                        std::hint::spin_loop();
                        iterations = iterations.wrapping_add(1);
                    }
                    let _ = phases.finished(());
                })
                .unwrap();
                workers.wait_ready().unwrap();
                let mut results = std::iter::repeat_with(|| None).take(2).collect::<Vec<_>>();
                workers.start();
                workers.wait_finished(&mut results);
                workers.release_and_join();
            }));
            let _ = completed_tx.send((started.elapsed(), outcome));
        });

        let (elapsed, outcome) = completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker panic did not cancel its long-running peer promptly");
        supervisor.join().unwrap();
        assert!(outcome.is_err());
        assert!(elapsed < Duration::from_secs(1), "elapsed: {elapsed:?}");
    }

    #[test]
    fn setup_error_cancels_before_work_and_joins_every_worker() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct DropCounter(Arc<AtomicUsize>);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let dropped = Arc::new(AtomicUsize::new(0));
        let work_started = Arc::new(AtomicBool::new(false));
        let worker_dropped = dropped.clone();
        let worker_started = work_started.clone();
        let mut workers = MultiTcpWorkers::<()>::spawn(2, move |worker_id, mut phases| {
            let _drop_counter = DropCounter(worker_dropped.clone());
            let setup = if worker_id == 0 {
                Err("injected setup failure".to_owned())
            } else {
                Ok(())
            };
            if !phases.ready(setup) {
                return;
            }
            worker_started.store(true, Ordering::Relaxed);
            let _ = phases.finished(());
        })
        .unwrap();

        let error = workers.wait_ready().unwrap_err();
        assert!(error.contains("worker 0: injected setup failure"));
        assert!(!work_started.load(Ordering::Relaxed));
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
    }
}
