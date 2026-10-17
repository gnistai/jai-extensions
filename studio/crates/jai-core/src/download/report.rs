//! Progress events for one component. State changes are reported at once;
//! byte counts at most once per interval (100 ms: about ten a second), with a
//! speed measured over the last few seconds.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{DlState, Progress};
use crate::catalog::Component;

/// Where progress events go.
pub(crate) type Sink = Arc<dyn Fn(Progress) + Send + Sync>;

/// The speed is measured over at most this much recent time...
const WINDOW: Duration = Duration::from_secs(3);
/// ...and only once at least this much has passed, to avoid wild first values.
const MIN_SPAN: Duration = Duration::from_millis(500);
/// Least time between two speed samples.
const SAMPLE_EVERY: Duration = Duration::from_millis(50);

/// Reports one component's progress. Clones share the same state, so a
/// blocking hashing thread can report too.
#[derive(Clone)]
pub(super) struct Reporter {
    sink: Sink,
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    id: String,
    total: u64,
    interval: Duration,
    state: DlState,
    bytes: u64,
    speed: u64,
    last_emit: Option<Instant>,
    samples: VecDeque<(Instant, u64)>,
}

impl Reporter {
    pub(super) fn new(c: &Component, sink: &Sink, interval: Duration) -> Reporter {
        Reporter {
            sink: sink.clone(),
            inner: Arc::new(Mutex::new(Inner {
                id: c.id.clone(),
                total: c.size,
                interval,
                state: DlState::Queued,
                bytes: 0,
                speed: 0,
                last_emit: None,
                samples: VecDeque::new(),
            })),
        }
    }

    /// Enters `state` at `bytes` and reports it.
    pub(super) fn set(&self, state: DlState, bytes: u64) {
        let event = self.with(|i| {
            i.state = state;
            i.bytes = bytes;
            i.speed = 0;
            i.samples.clear();
            i.samples.push_back((Instant::now(), bytes));
            i.event(None)
        });
        (self.sink)(event);
    }

    /// The byte count within the current state; reported when the last event
    /// is at least one interval old.
    pub(super) fn progress(&self, bytes: u64) {
        let event = self.with(|i| {
            let now = Instant::now();
            i.bytes = bytes;
            if i.samples
                .back()
                .is_none_or(|(t, _)| now.duration_since(*t) >= SAMPLE_EVERY)
            {
                i.samples.push_back((now, bytes));
            }
            while i.samples.len() > 2
                && i.samples
                    .front()
                    .is_some_and(|(t, _)| now.duration_since(*t) > WINDOW)
            {
                i.samples.pop_front();
            }
            if let Some(&(t0, b0)) = i.samples.front() {
                let span = now.duration_since(t0);
                if span >= MIN_SPAN {
                    i.speed = (bytes.saturating_sub(b0) as f64 / span.as_secs_f64()) as u64;
                }
            }
            let due = i
                .last_emit
                .is_none_or(|t| now.duration_since(t) >= i.interval);
            due.then(|| i.event(None))
        });
        if let Some(event) = event {
            (self.sink)(event);
        }
    }

    /// Reports a final state: Done (all bytes), Failed (with the error) or
    /// Canceled.
    pub(super) fn finish(&self, state: DlState, error: Option<String>) {
        let event = self.with(|i| {
            i.state = state;
            if state == DlState::Done {
                i.bytes = i.total;
            }
            i.speed = 0;
            i.event(error)
        });
        (self.sink)(event);
    }

    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut inner)
    }
}

impl Inner {
    fn event(&mut self, error: Option<String>) -> Progress {
        self.last_emit = Some(Instant::now());
        Progress {
            id: self.id.clone(),
            state: self.state,
            bytes: self.bytes,
            total: self.total,
            speed_bps: self.speed,
            error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Role;

    #[test]
    fn byte_counts_are_throttled_and_states_are_not() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        let sink: Sink = Arc::new(move |p| seen.lock().unwrap().push(p));
        let c = Component {
            id: "m".into(),
            role: Role::LlmModel,
            label: "m".into(),
            file: "m".into(),
            url: String::new(),
            size: 1000,
            sha256: String::new(),
            license: String::new(),
            source: String::new(),
            os: None,
            backend: None,
            unpack: None,
            strip: None,
            dir: None,
            exe: None,
        };
        let rep = Reporter::new(&c, &sink, Duration::from_secs(3600));
        rep.set(DlState::Downloading, 0);
        for b in 1..=100 {
            rep.progress(b);
        }
        rep.set(DlState::Verifying, 1000);
        rep.finish(DlState::Done, None);
        let states: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .map(|p| (p.state, p.bytes))
            .collect();
        assert_eq!(
            states,
            [
                (DlState::Downloading, 0),
                (DlState::Verifying, 1000),
                (DlState::Done, 1000)
            ]
        );
    }
}
