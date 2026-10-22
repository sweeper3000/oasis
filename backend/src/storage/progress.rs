//! Byte-weighted transfer work. Stage tokens reject late subprocess updates.
use rocket::serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(crate = "rocket::serde", rename_all = "snake_case")]
pub enum Phase {
    Preparing,
    CreatingFolders,
    Downloading,
    Decrypting,
    Copying,
    Encrypting,
    Uploading,
    Verifying,
    RemovingSource,
    Finalizing,
    Completed,
}

type Observer = Arc<dyn Fn(f64, Phase, Option<String>) + Send + Sync>;
struct Active {
    id: u64,
    weight: u128,
    fraction: f64,
}
struct State {
    total: u128,
    completed: u128,
    next_id: u64,
    active: Option<Active>,
    phase: Phase,
    file: Option<String>,
    published: Instant,
}

#[derive(Clone)]
pub struct Progress {
    state: Arc<Mutex<State>>,
    observer: Observer,
}
#[derive(Clone)]
pub struct Stage {
    progress: Progress,
    id: u64,
}

impl Progress {
    pub fn new(observer: impl Fn(f64, Phase, Option<String>) + Send + Sync + 'static) -> Self {
        Self {
            observer: Arc::new(observer),
            state: Arc::new(Mutex::new(State {
                total: 0,
                completed: 0,
                next_id: 0,
                active: None,
                phase: Phase::Preparing,
                file: None,
                published: Instant::now(),
            })),
        }
    }

    pub fn plan(&self, total: u128) {
        self.state.lock().unwrap().total = total.max(1);
    }

    pub fn stage(&self, phase: Phase, file: Option<&str>, weight: u128) -> Stage {
        let mut state = self.state.lock().unwrap();
        state.next_id += 1;
        let id = state.next_id;
        state.active = Some(Active {
            id,
            weight,
            fraction: 0.0,
        });
        state.phase = phase;
        state.file = file.map(String::from);
        self.publish(&mut state);
        Stage {
            progress: self.clone(),
            id,
        }
    }

    fn publish(&self, state: &mut State) {
        let active = state
            .active
            .as_ref()
            .map(|s| s.weight as f64 * s.fraction)
            .unwrap_or(0.0);
        let fraction =
            ((state.completed as f64 + active) / state.total.max(1) as f64).clamp(0.0, 1.0) * 0.99;
        (self.observer)(fraction, state.phase, state.file.clone());
        state.published = Instant::now();
    }
}

impl Stage {
    pub fn bytes(&self, done: u64, total: u64) {
        let mut state = self.progress.state.lock().unwrap();
        let active = match state.active.as_mut().filter(|s| s.id == self.id) {
            Some(s) => s,
            None => return,
        };
        // Empty stages receive their unit only on successful completion.
        let fraction = if total == 0 {
            0.0
        } else {
            (done as f64 / total as f64).min(1.0)
        };
        active.fraction = active.fraction.max(fraction);
        if state.published.elapsed() >= Duration::from_millis(250) {
            self.progress.publish(&mut state);
        }
    }

    pub fn finish(&self) {
        let mut state = self.progress.state.lock().unwrap();
        if state.active.as_ref().map(|s| s.id) != Some(self.id) {
            return;
        }
        let active = state.active.take().unwrap();
        state.completed += active.weight;
        self.progress.publish(&mut state);
    }

    pub fn phase(&self, phase: Phase) {
        let mut state = self.progress.state.lock().unwrap();
        if state.active.as_ref().map(|s| s.id) != Some(self.id) {
            return;
        }
        state.phase = phase;
        self.progress.publish(&mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_weights_retries_and_late_updates_never_regress_or_report_success() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let progress =
            Progress::new(move |value, phase, _| sink.lock().unwrap().push((value, phase)));
        progress.plan(400);
        let first = progress.stage(Phase::Downloading, Some("folder/file"), 100);
        first.bytes(50, 100);
        first.bytes(10, 100); // A retry counter reset is not backwards progress.
        first.phase(Phase::Verifying);
        first.finish();
        let second = progress.stage(Phase::Uploading, Some("folder/file"), 300);
        first.bytes(u64::MAX, 1); // Old subprocesses cannot change the active stage.
        first.finish();
        second.bytes(u64::MAX, 300);
        second.finish();
        let values = events.lock().unwrap();
        assert!(values.windows(2).all(|v| v[1].0 >= v[0].0));
        assert_eq!(values.last().unwrap().0, 0.99);
        assert_eq!(values[0].1, Phase::Downloading);
        assert!(values.iter().all(|v| v.0.is_finite() && v.0 < 1.0));
    }
}
