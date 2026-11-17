// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Batching and turn coalescing as a state machine over caller-supplied instants.
//!
//! The reference runs one sleeping task per pending batch; here the owner of the state asks
//! [`Coalescer::next_deadline`] and sleeps until it, so an idle channel never wakes up.
//!
//! Stages: the text batcher joins client-side splits of one long message, the photo and album
//! batchers join pictures, and the turn coalescer joins whatever arrives within the quiet
//! window (capped from the first message) into one turn.

use std::time::{Duration, Instant};

use super::event::{Event, can_coalesce, merge_coalesced};

/// Delays of the batchers and the turn coalescer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timing {
    /// Quiet window of the turn coalescer; zero disables coalescing.
    pub turn_coalesce: Duration,
    /// Cap of a coalesced turn, counted from its first message.
    pub turn_coalesce_max: Duration,
    /// Upper bound of the text batch delay.
    pub text_batch: Duration,
    /// Text batch delay when the last chunk is near Telegram's split length.
    pub text_batch_split: Duration,
    /// Delay of a burst of single photos.
    pub media_batch: Duration,
    /// Delay of an album (`media_group_id`).
    pub media_group: Duration,
}

impl Default for Timing {
    /// The reference defaults with the owner's coalescing window of 2 s, capped at 7 s.
    fn default() -> Self {
        Self {
            turn_coalesce: Duration::from_secs(2),
            turn_coalesce_max: Duration::from_secs(7),
            text_batch: Duration::from_millis(300),
            text_batch_split: Duration::from_secs(1),
            media_batch: Duration::from_millis(800),
            media_group: Duration::from_millis(800),
        }
    }
}

/// A chunk this long (in characters) is probably followed by a client-side continuation.
const SPLIT_THRESHOLD: usize = 4000;
const FAST_LEN: usize = 320;
const FAST_DELAY: Duration = Duration::from_millis(180);
const SHORT_LEN: usize = 1024;
const SHORT_DELAY: Duration = Duration::from_millis(240);

struct Pending<K> {
    key: K,
    event: Event,
    deadline: Instant,
}

struct Turn {
    chat_id: i64,
    parts: Vec<Event>,
    first: Instant,
    last: Instant,
}

/// Pending batches and turns of all chats.
pub(crate) struct Coalescer {
    timing: Timing,
    text: Vec<Pending<i64>>,
    photos: Vec<Pending<i64>>,
    groups: Vec<Pending<String>>,
    turns: Vec<Turn>,
}

enum Due {
    Text(usize),
    Photo(usize),
    Group(usize),
    Turn(usize),
}

impl Coalescer {
    pub(crate) fn new(mut timing: Timing) -> Self {
        timing.turn_coalesce_max = timing.turn_coalesce_max.max(timing.turn_coalesce);
        timing.text_batch_split = timing.text_batch_split.max(timing.text_batch);
        Self {
            timing,
            text: Vec::new(),
            photos: Vec::new(),
            groups: Vec::new(),
            turns: Vec::new(),
        }
    }

    fn text_delay(&self, event: &Event) -> Duration {
        let total = event.text.chars().count();
        if event.last_chunk_len >= SPLIT_THRESHOLD {
            self.timing.text_batch_split
        } else if total <= FAST_LEN {
            self.timing.text_batch.min(FAST_DELAY)
        } else if total <= SHORT_LEN {
            self.timing.text_batch.min(SHORT_DELAY)
        } else {
            self.timing.text_batch
        }
    }

    /// Text batcher: joins the event into the chat's pending text and restarts its timer.
    pub(crate) fn push_text(&mut self, mut event: Event, now: Instant) {
        let chat = event.chat_id;
        if let Some(i) = self.text.iter().position(|p| p.key == chat) {
            self.text[i].event.append_text_chunk(event);
            self.text[i].deadline = now + self.text_delay(&self.text[i].event);
        } else {
            event.last_chunk_len = event.text.chars().count();
            event.chunks = Some(vec![(event.forwarded, event.text.clone())]);
            let deadline = now + self.text_delay(&event);
            self.text.push(Pending {
                key: chat,
                event,
                deadline,
            });
        }
    }

    /// Photo burst batcher of a chat.
    pub(crate) fn push_photo(&mut self, event: Event, now: Instant) {
        let deadline = now + self.timing.media_batch;
        push_media(&mut self.photos, event.chat_id, event, deadline);
    }

    /// Album batcher keyed by `media_group_id`.
    pub(crate) fn push_group(&mut self, group: String, event: Event, now: Instant) {
        let deadline = now + self.timing.media_group;
        push_media(&mut self.groups, group, event, deadline);
    }

    /// Turn coalescer; returns events that are due at once (coalescing off, or a pending turn
    /// the new event cannot join).
    pub(crate) fn dispatch(&mut self, event: Event, now: Instant) -> Vec<Event> {
        if self.timing.turn_coalesce.is_zero() {
            return vec![event];
        }
        let mut out = Vec::new();
        let chat = event.chat_id;
        if let Some(i) = self.turns.iter().position(|t| t.chat_id == chat) {
            if can_coalesce(&self.turns[i].parts, &event) {
                self.turns[i].parts.push(event);
                self.turns[i].last = now;
                return out;
            }
            out.push(merge_coalesced(self.turns.remove(i).parts));
        }
        self.turns.push(Turn {
            chat_id: chat,
            parts: vec![event],
            first: now,
            last: now,
        });
        out
    }

    /// Sends the chat's pending turn out now (a command keeps its place after it).
    pub(crate) fn flush_turn_now(&mut self, chat_id: i64) -> Option<Event> {
        let i = self.turns.iter().position(|t| t.chat_id == chat_id)?;
        Some(merge_coalesced(self.turns.remove(i).parts))
    }

    fn turn_deadline(&self, turn: &Turn) -> Instant {
        (turn.last + self.timing.turn_coalesce).min(turn.first + self.timing.turn_coalesce_max)
    }

    fn earliest(&self) -> Option<(Instant, Due)> {
        let text = self
            .text
            .iter()
            .enumerate()
            .map(|(i, p)| (p.deadline, Due::Text(i)));
        let photos = self
            .photos
            .iter()
            .enumerate()
            .map(|(i, p)| (p.deadline, Due::Photo(i)));
        let groups = self
            .groups
            .iter()
            .enumerate()
            .map(|(i, p)| (p.deadline, Due::Group(i)));
        let turns = self
            .turns
            .iter()
            .enumerate()
            .map(|(i, t)| (self.turn_deadline(t), Due::Turn(i)));
        text.chain(photos)
            .chain(groups)
            .chain(turns)
            .min_by_key(|(at, _)| *at)
    }

    /// When the state machine next has work; `None` when nothing is pending.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.earliest().map(|(at, _)| at)
    }

    /// Runs every timer due by `now` in deadline order; returns the turns to send, in order.
    pub(crate) fn poll(&mut self, now: Instant) -> Vec<Event> {
        let mut out = Vec::new();
        while let Some((at, due)) = self.earliest() {
            if at > now {
                break;
            }
            match due {
                Due::Text(i) => {
                    let event = self.text.remove(i).event;
                    out.extend(self.dispatch(event, at));
                }
                Due::Photo(i) => {
                    let event = self.photos.remove(i).event;
                    out.extend(self.dispatch(event, at));
                }
                Due::Group(i) => {
                    let event = self.groups.remove(i).event;
                    out.extend(self.dispatch(event, at));
                }
                Due::Turn(i) => out.push(merge_coalesced(self.turns.remove(i).parts)),
            }
        }
        out
    }
}

fn push_media<K: PartialEq>(
    batches: &mut Vec<Pending<K>>,
    key: K,
    event: Event,
    deadline: Instant,
) {
    if let Some(p) = batches.iter_mut().find(|p| p.key == key) {
        p.event.append_media_item(event);
        p.deadline = deadline;
    } else {
        batches.push(Pending {
            key,
            event,
            deadline,
        });
    }
}
