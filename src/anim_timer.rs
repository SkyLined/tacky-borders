use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};

use crate::border_registry::discard_panic;
use crate::post_message_w;
use crate::utils::WM_APP_ANIMATE;

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnimationTimerExit {
    Stopped,
    InvalidInterval,
    SpawnFailed(String),
    PostFailed(String),
    Panicked,
}

impl std::fmt::Display for AnimationTimerExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for AnimationTimerExit {}

#[derive(Debug)]
pub struct AnimationTimer {
    stop: Option<Sender<()>>,
    worker: Option<JoinHandle<AnimationTimerExit>>,
    outcome: Option<AnimationTimerExit>,
}

impl AnimationTimer {
    #[cfg(test)]
    pub(crate) fn fail_next_spawn_for_test() {
        FAIL_NEXT_SPAWN.set(true);
    }

    pub fn new(hwnd: HWND, interval: Duration) -> Result<Self, AnimationTimerExit> {
        let hwnd_isize = hwnd.0 as isize;
        Self::spawn(interval, move || {
            post_message_w(
                Some(HWND(hwnd_isize as _)),
                WM_APP_ANIMATE,
                WPARAM(0),
                LPARAM(0),
            )
            .map_err(|error| format!("{error:#}"))
        })
    }

    pub(crate) fn spawn(
        interval: Duration,
        post: impl FnMut() -> Result<(), String> + Send + 'static,
    ) -> Result<Self, AnimationTimerExit> {
        if interval.is_zero() {
            return Err(AnimationTimerExit::InvalidInterval);
        }
        #[cfg(test)]
        if FAIL_NEXT_SPAWN.replace(false) {
            return Err(AnimationTimerExit::SpawnFailed(
                "injected timer spawn failure".into(),
            ));
        }
        let (stop, receiver) = mpsc::channel();
        match thread::Builder::new()
            .name("border-animation".into())
            .spawn(move || Self::run(receiver, interval, post))
        {
            Ok(worker) => Ok(Self {
                stop: Some(stop),
                worker: Some(worker),
                outcome: None,
            }),
            Err(error) => Err(AnimationTimerExit::SpawnFailed(error.to_string())),
        }
    }

    fn run(
        stop: Receiver<()>,
        interval: Duration,
        mut post: impl FnMut() -> Result<(), String>,
    ) -> AnimationTimerExit {
        loop {
            match stop.recv_timeout(interval) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return AnimationTimerExit::Stopped,
                Err(RecvTimeoutError::Timeout) => {
                    if !matches!(stop.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                        return AnimationTimerExit::Stopped;
                    }
                    if let Err(error) = post() {
                        return AnimationTimerExit::PostFailed(error);
                    }
                }
            }
        }
    }

    pub fn is_finished(&self) -> bool {
        self.outcome.is_some() || self.worker.as_ref().is_some_and(JoinHandle::is_finished)
    }

    pub fn poll_completion(&mut self) -> Option<&AnimationTimerExit> {
        if self.is_finished() {
            self.finish();
        }
        self.outcome.as_ref()
    }

    pub fn stop_and_join(&mut self) -> &AnimationTimerExit {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.finish();
        self.outcome.get_or_insert(AnimationTimerExit::Stopped)
    }

    fn finish(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.outcome = Some(match worker.join() {
                Ok(outcome) => outcome,
                Err(payload) => {
                    discard_panic(payload);
                    AnimationTimerExit::Panicked
                }
            });
        }
    }
}

impl Drop for AnimationTimer {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    fn await_completion(timer: &AnimationTimer) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !timer.is_finished() {
            assert!(Instant::now() < deadline, "timer did not complete");
            thread::yield_now();
        }
    }

    #[test]
    fn long_interval_is_interruptible_and_join_is_idempotent() {
        let mut timer =
            AnimationTimer::spawn(Duration::MAX, || panic!("unexpected frame")).unwrap();
        assert_eq!(timer.stop_and_join(), &AnimationTimerExit::Stopped);
        assert!(timer.worker.is_none());
        assert_eq!(timer.stop_and_join(), &AnimationTimerExit::Stopped);
    }

    #[test]
    fn zero_interval_is_rejected_without_spawning() {
        assert!(matches!(
            AnimationTimer::spawn(Duration::ZERO, || panic!("unexpected frame")),
            Err(AnimationTimerExit::InvalidInterval)
        ));
    }

    #[test]
    fn post_failure_is_observed_and_joined() {
        let mut timer = AnimationTimer::spawn(Duration::from_millis(1), || {
            Err("injected post failure".into())
        })
        .unwrap();
        await_completion(&timer);
        assert_eq!(
            timer.poll_completion(),
            Some(&AnimationTimerExit::PostFailed(
                "injected post failure".into()
            ))
        );
        assert!(timer.worker.is_none());
    }

    #[test]
    fn panic_is_contained_in_join_outcome() {
        let mut timer =
            AnimationTimer::spawn(Duration::from_millis(1), || panic!("injected timer panic"))
                .unwrap();
        await_completion(&timer);
        assert_eq!(timer.poll_completion(), Some(&AnimationTimerExit::Panicked));
    }

    #[test]
    fn no_post_after_join_and_replacement_retires_previous_timer() {
        let posts = Arc::new(AtomicUsize::new(0));
        let worker_posts = posts.clone();
        let (posted, observed) = mpsc::channel();
        let mut timer = AnimationTimer::spawn(Duration::from_millis(1), move || {
            worker_posts.fetch_add(1, Ordering::SeqCst);
            let _ = posted.send(());
            Ok(())
        })
        .unwrap();
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(timer.stop_and_join(), &AnimationTimerExit::Stopped);
        let count = posts.load(Ordering::SeqCst);
        timer = AnimationTimer::spawn(Duration::MAX, || panic!("unexpected replacement frame"))
            .unwrap();
        assert_eq!(timer.stop_and_join(), &AnimationTimerExit::Stopped);
        assert_eq!(posts.load(Ordering::SeqCst), count);
    }
}
