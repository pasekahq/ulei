//! The schedule of `serve` (ADR-0056 section 4) against a clock its caller reads: each
//! connector due at `max(last start + every, not_before)`, the C10 `sync` entry every
//! `sync_every`, one run per connector and at most [`MAX_RUNS`] at once. Times are Unix seconds.

/// Runs in flight at once.
pub(super) const MAX_RUNS: usize = 4;

/// What the daemon does now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Action {
    /// Start a run of this connector.
    Start(String),
    /// Record this connector `busy`: it is due while its last run still runs.
    Busy(String),
    /// Call C10 `sync`.
    Sync,
}

struct Entry {
    name: String,
    every: f64,
    last_start: Option<f64>,
    not_before: Option<f64>,
    running: bool,
    /// When a run due during the running one was recorded `busy`.
    busy_at: Option<f64>,
}

impl Entry {
    /// When this entry is next due; for a running one, when its next run is found busy.
    fn due(&self) -> f64 {
        let last = match (self.last_start, self.busy_at) {
            (Some(start), Some(busy)) => Some(start.max(busy)),
            (last, busy) => last.or(busy),
        };
        let period = last.map_or(0.0, |last| last + self.every);
        period.max(self.not_before.unwrap_or(0.0))
    }
}

/// The due times of the connected connectors and of `sync`.
pub(super) struct Schedule {
    entries: Vec<Entry>,
    sync_every: f64,
    sync_due: f64,
    syncing: bool,
}

impl Schedule {
    /// A schedule whose first `sync` is due at `now`.
    pub(super) fn new(sync_every: f64, now: f64) -> Self {
        Self {
            entries: Vec::new(),
            sync_every,
            sync_due: now,
            syncing: false,
        }
    }

    /// Adds connector `name` run every `every` seconds, last started at `last_start` and not
    /// to run before `not_before`, as its state file holds them.
    pub(super) fn add(
        &mut self,
        name: &str,
        every: f64,
        last_start: Option<f64>,
        not_before: Option<f64>,
    ) {
        self.entries.push(Entry {
            name: name.to_owned(),
            every,
            last_start,
            not_before,
            running: false,
            busy_at: None,
        });
    }

    /// The actions due at `now`, in order: `sync`, the busy connectors, then the runs that
    /// start, earliest due first, as long as a slot is free. A due run without a slot waits for
    /// [`Schedule::finished`].
    pub(super) fn due(&mut self, now: f64) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.sync_due <= now {
            if !self.syncing {
                self.syncing = true;
                actions.push(Action::Sync);
            }
            self.sync_due = now + self.sync_every;
        }
        for entry in &mut self.entries {
            if entry.running && entry.due() <= now {
                entry.busy_at = Some(now);
                actions.push(Action::Busy(entry.name.clone()));
            }
        }
        let mut waiting: Vec<(f64, usize)> = (self.entries.iter().enumerate())
            .filter(|(_, e)| !e.running && e.due() <= now)
            .map(|(i, e)| (e.due(), i))
            .collect();
        waiting.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let free = MAX_RUNS.saturating_sub(self.running());
        for (_, i) in waiting.into_iter().take(free) {
            let entry = &mut self.entries[i];
            entry.running = true;
            entry.last_start = Some(now);
            entry.busy_at = None;
            actions.push(Action::Start(entry.name.clone()));
        }
        actions
    }

    /// The nearest time something falls due; a run waiting for a slot is not counted, since
    /// only [`Schedule::finished`] frees one.
    pub(super) fn next(&self) -> f64 {
        let free = self.running() < MAX_RUNS;
        (self.entries.iter())
            .filter(|e| e.running || free)
            .map(Entry::due)
            .fold(self.sync_due, f64::min)
    }

    /// The run of `name` ended; `not_before` is what it left for the next one.
    pub(super) fn finished(&mut self, name: &str, not_before: Option<f64>) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.name == name) {
            entry.running = false;
            entry.not_before = not_before;
        }
    }

    /// The `sync` call returned.
    pub(super) fn synced(&mut self) {
        self.syncing = false;
    }

    /// Runs in flight.
    pub(super) fn running(&self) -> usize {
        self.entries.iter().filter(|e| e.running).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: f64 = 1_700_000_000.0;

    fn start(name: &str) -> Action {
        Action::Start(name.to_owned())
    }

    fn busy(name: &str) -> Action {
        Action::Busy(name.to_owned())
    }

    #[test]
    fn a_connector_runs_once_per_period_and_the_daemon_sleeps_until_the_nearest_due_time() {
        let mut schedule = Schedule::new(900.0, T0);
        schedule.add("a", 60.0, None, None);
        schedule.add("b", 90.0, Some(T0 - 30.0), None);
        assert_eq!(schedule.due(T0), [Action::Sync, start("a")]);
        assert_eq!(schedule.next(), T0 + 60.0);
        schedule.finished("a", None);
        assert_eq!(schedule.due(T0 + 59.9), []);
        assert_eq!(schedule.due(T0 + 60.0), [start("a"), start("b")]);
        schedule.finished("a", None);
        schedule.finished("b", None);
        assert_eq!(schedule.next(), T0 + 120.0);
        assert_eq!(schedule.due(T0 + 120.0), [start("a")]);
        schedule.finished("a", None);
        assert_eq!(schedule.next(), T0 + 150.0);
    }

    #[test]
    fn a_connector_overdue_after_downtime_runs_once_at_start() {
        let mut schedule = Schedule::new(900.0, T0);
        schedule.add("late", 60.0, Some(T0 - 3600.0), None);
        assert_eq!(schedule.due(T0), [Action::Sync, start("late")]);
        schedule.finished("late", None);
        schedule.synced();
        assert_eq!(schedule.due(T0 + 1.0), []);
        assert_eq!(schedule.next(), T0 + 60.0);
    }

    #[test]
    fn not_before_from_the_state_file_holds_after_a_restart_and_a_run_leaves_a_new_one() {
        let mut schedule = Schedule::new(900.0, T0);
        schedule.add("limited", 60.0, Some(T0 - 3600.0), Some(T0 + 500.0));
        assert_eq!(schedule.due(T0), [Action::Sync]);
        assert_eq!(schedule.next(), T0 + 500.0);
        assert_eq!(schedule.due(T0 + 500.0), [start("limited")]);
        schedule.finished("limited", Some(T0 + 1000.0));
        assert_eq!(schedule.next(), T0 + 900.0);
        schedule.synced();
        assert_eq!(schedule.due(T0 + 900.0), [Action::Sync]);
        assert_eq!(schedule.next(), T0 + 1000.0);
        assert_eq!(schedule.due(T0 + 1000.0), [start("limited")]);
        schedule.finished("limited", None);
        assert_eq!(schedule.next(), T0 + 1060.0);
    }

    #[test]
    fn sync_is_due_every_sync_every_and_one_in_flight_is_not_doubled() {
        let mut schedule = Schedule::new(300.0, T0);
        assert_eq!(schedule.due(T0), [Action::Sync]);
        assert_eq!(schedule.next(), T0 + 300.0);
        assert_eq!(schedule.due(T0 + 300.0), []);
        assert_eq!(schedule.next(), T0 + 600.0);
        schedule.synced();
        assert_eq!(schedule.due(T0 + 600.0), [Action::Sync]);
        schedule.synced();
        assert_eq!(schedule.next(), T0 + 900.0);
    }

    #[test]
    fn a_run_due_while_the_last_one_runs_is_busy_once_per_period() {
        let mut schedule = Schedule::new(900.0, T0);
        schedule.add("slow", 60.0, None, None);
        assert_eq!(schedule.due(T0), [Action::Sync, start("slow")]);
        assert_eq!(schedule.next(), T0 + 60.0);
        assert_eq!(schedule.due(T0 + 60.0), [busy("slow")]);
        assert_eq!(schedule.next(), T0 + 120.0);
        assert_eq!(schedule.due(T0 + 100.0), []);
        schedule.finished("slow", None);
        assert_eq!(schedule.due(T0 + 119.0), []);
        assert_eq!(schedule.due(T0 + 120.0), [start("slow")]);
    }

    #[test]
    fn four_run_at_once_and_a_fifth_starts_when_a_slot_frees() {
        let mut schedule = Schedule::new(900.0, T0);
        for name in ["a", "b", "c", "d", "e"] {
            schedule.add(name, 60.0, None, None);
        }
        assert_eq!(
            schedule.due(T0),
            [Action::Sync, start("a"), start("b"), start("c"), start("d")]
        );
        assert_eq!(schedule.running(), MAX_RUNS);
        assert_eq!(schedule.next(), T0 + 60.0);
        assert_eq!(schedule.due(T0 + 10.0), []);
        schedule.finished("c", None);
        assert_eq!(schedule.next(), 0.0);
        assert_eq!(schedule.due(T0 + 12.0), [start("e")]);
        assert_eq!(schedule.running(), MAX_RUNS);
        schedule.finished("a", None);
        schedule.finished("b", None);
        schedule.finished("d", None);
        schedule.finished("e", None);
        assert_eq!(schedule.next(), T0 + 60.0);
        assert_eq!(
            schedule.due(T0 + 60.0),
            [start("a"), start("b"), start("c"), start("d")]
        );
    }
}
