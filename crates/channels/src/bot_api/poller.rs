// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Blocking long-poll loop: offset bookkeeping and the reference's recovery ladders.

use std::sync::Arc;
use std::time::Duration;

use super::client::{ALL_UPDATE_TYPES, Client};
use super::clock::{Clock, Stop};
use super::error::{Error, ErrorKind};
use super::types::Update;

/// Conflict retries before the poller gives up.
const MAX_CONFLICT_RETRIES: u32 = 5;
/// Network-error retries before the poller gives up.
const MAX_NETWORK_RETRIES: u32 = 10;
const NETWORK_BASE_DELAY: Duration = Duration::from_secs(5);
const NETWORK_MAX_DELAY: Duration = Duration::from_secs(60);
/// Added to a flood-control wait on `getUpdates`.
const RETRY_AFTER_SLACK: Duration = Duration::from_millis(500);
/// Ceiling of the back-off after other polling errors.
const OTHER_ERROR_MAX_DELAY: Duration = Duration::from_secs(30);

/// Pulls updates with `getUpdates`, confirming each batch with `last update_id + 1`.
///
/// Idle is one blocked long poll; there are no timers. A stop request ends back-off waits
/// at once and is seen between polls, so it takes effect within one poll timeout plus the
/// read timeout of the client.
pub struct Poller {
    client: Client,
    clock: Arc<dyn Clock>,
    stop: Stop,
    offset: Option<i64>,
    other_delay: Duration,
    network_errors: u32,
    conflicts: u32,
    conflict_recovering: bool,
}

#[derive(Clone, Copy)]
enum Ladder {
    Conflict,
    Network,
}

impl Poller {
    /// A poller on `client` that ends when `stop` is requested.
    pub fn new(client: Client, clock: Arc<dyn Clock>, stop: Stop) -> Self {
        Self {
            client,
            clock,
            stop,
            offset: None,
            other_delay: Duration::ZERO,
            network_errors: 0,
            conflicts: 0,
            conflict_recovering: false,
        }
    }

    /// The offset the next `getUpdates` confirms with, once a batch was handed out.
    pub fn offset(&self) -> Option<i64> {
        self.offset
    }

    /// Clears any webhook so polling receives updates; `drop_pending` also discards the
    /// queued ones (the reference drops them on a cold start and keeps them on reconnect).
    ///
    /// # Errors
    ///
    /// The classified [`Error`] of `deleteWebhook`.
    pub fn start(&mut self, drop_pending: bool) -> Result<(), Error> {
        self.client.delete_webhook(drop_pending)
    }

    /// Blocks until the next non-empty batch (`Some`), a stop (`None`) or a fatal error:
    /// an invalid token, or a conflict or network failure that outlived its retries.
    /// A batch counts as handed out: the next poll confirms it and it never comes back.
    ///
    /// # Errors
    ///
    /// `Unauthorized`, or the last error of an exhausted conflict or network ladder.
    pub fn next_batch(&mut self) -> Result<Option<Vec<Update>>, Error> {
        loop {
            if self.stop.is_requested() {
                return Ok(None);
            }
            let timeout = self.client.config().poll_timeout;
            match self
                .client
                .get_updates(self.offset, timeout, ALL_UPDATE_TYPES)
            {
                Ok(updates) => {
                    self.made_progress();
                    if self.stop.is_requested() {
                        // Unconfirmed, so a later poller fetches this batch again.
                        return Ok(None);
                    }
                    if let Some(last) = updates.last() {
                        self.offset = Some(last.update_id + 1);
                        return Ok(Some(updates));
                    }
                }
                Err(err) => self.recover(err)?,
            }
        }
    }

    /// Confirms the batches handed out so far with one non-blocking `getUpdates`, so a
    /// restarted process does not receive them again.
    ///
    /// # Errors
    ///
    /// The classified [`Error`] of the confirming call.
    pub fn shutdown(&self) -> Result<(), Error> {
        match self.offset {
            Some(offset) => self
                .client
                .get_updates(Some(offset), Duration::ZERO, ALL_UPDATE_TYPES)
                .map(drop),
            None => Ok(()),
        }
    }

    fn made_progress(&mut self) {
        self.other_delay = Duration::ZERO;
        self.network_errors = 0;
        // The first success after a conflict restart does not prove the other poller gone.
        if self.conflict_recovering {
            self.conflict_recovering = false;
        } else {
            self.conflicts = 0;
        }
    }

    /// Waits out one polling error; `Err` when polling cannot continue.
    fn recover(&mut self, err: Error) -> Result<(), Error> {
        let ladder = match err.kind {
            ErrorKind::Unauthorized => return Err(err),
            ErrorKind::Timeout | ErrorKind::ConnectTimeout => return Ok(()),
            ErrorKind::RetryAfter(wait) => {
                self.clock
                    .sleep_or_stop(wait + RETRY_AFTER_SLACK, &self.stop);
                return Ok(());
            }
            _ if err.is_polling_conflict() => Ladder::Conflict,
            _ if err.is_network_error() => Ladder::Network,
            _ => {
                self.other_delay = if self.other_delay.is_zero() {
                    Duration::from_secs(1)
                } else {
                    self.other_delay.mul_f64(1.5).min(OTHER_ERROR_MAX_DELAY)
                };
                self.clock.sleep_or_stop(self.other_delay, &self.stop);
                return Ok(());
            }
        };
        self.restart(ladder, err)
    }

    /// Back-off then re-bootstrap; a failed re-bootstrap climbs the same ladder.
    fn restart(&mut self, ladder: Ladder, mut err: Error) -> Result<(), Error> {
        loop {
            let (wait, drop_pending) = match ladder {
                Ladder::Conflict => {
                    self.conflicts += 1;
                    if self.conflicts > MAX_CONFLICT_RETRIES {
                        return Err(err);
                    }
                    // Telegram holds a dead poller's session ~30 s: wait 20 s, 30 s, ... 60 s.
                    (
                        Duration::from_secs(10 + 10 * u64::from(self.conflicts)),
                        true,
                    )
                }
                Ladder::Network => {
                    self.network_errors += 1;
                    if self.network_errors > MAX_NETWORK_RETRIES {
                        return Err(err);
                    }
                    let factor = 1u32 << (self.network_errors - 1).min(16);
                    ((NETWORK_BASE_DELAY * factor).min(NETWORK_MAX_DELAY), false)
                }
            };
            if self.clock.sleep_or_stop(wait, &self.stop) {
                return Ok(());
            }
            // Dropping pending updates is how the reference ends the other getUpdates session.
            if drop_pending {
                self.conflict_recovering = true;
            }
            match self.client.delete_webhook(drop_pending) {
                Ok(()) => return Ok(()),
                Err(next) => err = next,
            }
        }
    }
}
