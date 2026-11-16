//! Seeded random operation sequences over the lease state machine, with the invariants of
//! ADR-0007 section 7 checked after every step. The store is in memory: the SQL is the
//! product's, and a file store would spend the test budget on `fsync`. The cases are split
//! into shards so each test stays within the 0.5 s budget of an unoptimised build.

use std::collections::{BTreeMap, BTreeSet};

use hmac::Mac;

use super::*;

const SHARDS: u64 = 50;
const CASES_PER_SHARD: u64 = 20;
const STEPS: usize = 64;
const BASE_SEED: u64 = 0x5eed_1ea5_e000_0000;
const T0: f64 = 1_763_290_000.0;
const IDS: [&str; 2] = ["case:a", "case:b"];
const CLOCK_STEPS: [f64; 10] = [
    0.0, 0.0, 0.0, 0.0, 1.0, 10.0, 130.0, 900.0, 3_600.0, 3_601.0,
];
const TTLS: [f64; 4] = [5.0, LEASE_TTL, 4_000.0, 4_000.0];
const DELAYS: [f64; 3] = [0.0, 5.0, RETRY_DELAY];
/// `deferred` is weighted up so sequences reach `deferred-exhausted`; `done` is not C1.
const OUTCOMES: [&str; 9] = [
    "success",
    "silent",
    "uncertain",
    "deferred",
    "deferred",
    "deferred",
    "deferred",
    "deferred",
    "done",
];
const FOREIGN_LEASE: &str = "0123456789abcdef0123456789abcdef";
const EDGES: [(&str, &str); 8] = [
    ("pending", "leased"),
    ("leased", "pending"),
    ("leased", "processing"),
    ("processing", "done"),
    ("processing", "uncertain"),
    ("processing", "deferred"),
    ("deferred", "pending"),
    ("deferred", "uncertain"),
];

/// `SplitMix64`: small, std-only and fully determined by its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).expect("index fits usize")
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

#[derive(Debug)]
enum Op {
    Publish(usize),
    Claim(f64),
    Start(String, String),
    Renew(String, String, f64),
    Retry(String, String, f64),
    Ack(String, String, &'static str),
    Recover,
}

type Snapshot = BTreeMap<String, EventRow>;

/// One sequence: its seed and clock, the leases it was handed, and the deferred re-offers
/// per id so far.
struct Run<'a> {
    seed: u64,
    inbox: &'a mut Inbox,
    packets: &'a [Value],
    now: f64,
    held: Vec<(String, String)>,
    reoffers: BTreeMap<String, i64>,
    covered: &'a mut BTreeSet<(String, String)>,
}

fn memory_inbox() -> Inbox {
    let conn = Connection::open_in_memory().expect("in-memory db");
    conn.execute_batch(SCHEMA).expect("schema");
    let mac = PacketMac::new_from_slice(&[7; KEY_LEN]).expect("key");
    Inbox { mac, conn }
}

fn signed_packets() -> Vec<Value> {
    let mac = PacketMac::new_from_slice(&[7; KEY_LEN]).expect("key");
    IDS.iter()
        .map(|id| {
            let mut packet = serde_json::json!({
                "version": 1, "kind": "task_result", "id": id, "payload": "Card finished.",
                "source_ts": 1_763_290_000, "trust": "untrusted-source-data",
            });
            let mut tag = mac.clone();
            tag.update(&canonical(&packet).expect("canonical"));
            packet["mac"] = Value::String(lower_hex(&tag.finalize().into_bytes()));
            packet
        })
        .collect()
}

fn snapshot(inbox: &Inbox) -> Snapshot {
    let mut stmt = inbox
        .conn
        .prepare(&format!("SELECT {ROW_COLUMNS} FROM events"))
        .expect("prepare");
    stmt.query_map([], event_row)
        .expect("query")
        .map(|row| row.map(|row| (row.id.clone(), row)))
        .collect::<Result<_, _>>()
        .expect("rows")
}

impl Run<'_> {
    /// A lease for a call that needs one of `statuses`: mostly a live token of a row in
    /// one of them so sequences progress, else any token handed out, else a foreign one.
    fn lease(&self, rng: &mut Rng, rows: &Snapshot, statuses: &[&str]) -> (String, String) {
        let live: Vec<&(String, String)> = self
            .held
            .iter()
            .filter(|(id, lease)| {
                rows.get(id).is_some_and(|row| {
                    row.lease.as_ref() == Some(lease) && statuses.contains(&row.status.as_str())
                })
            })
            .collect();
        match rng.below(10) {
            0..=7 if !live.is_empty() => live[rng.below(live.len())].clone(),
            0..=8 if !self.held.is_empty() => self.held[rng.below(self.held.len())].clone(),
            _ => (rng.pick(&IDS).to_owned(), FOREIGN_LEASE.to_owned()),
        }
    }

    fn next_op(&self, rng: &mut Rng, rows: &Snapshot) -> Op {
        match rng.below(20) {
            0..=1 => Op::Publish(rng.below(IDS.len())),
            2..=5 => Op::Claim(rng.pick(&TTLS)),
            6..=9 => {
                let (id, lease) = self.lease(rng, rows, &["leased"]);
                Op::Start(id, lease)
            }
            10 => {
                let (id, lease) = self.lease(rng, rows, &["leased", "processing"]);
                Op::Renew(id, lease, rng.pick(&TTLS))
            }
            11 => {
                let (id, lease) = self.lease(rng, rows, &["leased"]);
                Op::Retry(id, lease, rng.pick(&DELAYS))
            }
            12..=17 => {
                let (id, lease) = self.lease(rng, rows, &["processing"]);
                Op::Ack(id, lease, rng.pick(&OUTCOMES))
            }
            _ => Op::Recover,
        }
    }

    fn step(&mut self, step: usize, op: &Op, before: &Snapshot) -> Snapshot {
        let at = format!(
            "seed {:#018x} step {step} now {} {op:?}",
            self.seed, self.now
        );
        let now = self.now;
        let held = |id: &str, lease: &str, statuses: &[&str]| {
            before.get(id).is_some_and(|row| {
                row.lease.as_deref() == Some(lease) && statuses.contains(&row.status.as_str())
            })
        };
        let mut claimed = None;
        let applied = match op {
            Op::Publish(i) => {
                self.inbox.publish(&self.packets[*i], now).expect(&at);
                true
            }
            Op::Claim(ttl) => {
                claimed = self.inbox.claim(now, *ttl).expect(&at);
                if let Some(row) = &claimed {
                    let lease = row.lease.clone().expect(&at);
                    self.held.push((row.id.clone(), lease));
                }
                true
            }
            Op::Start(id, lease) => {
                let ok = self.inbox.start(id, lease, "session").expect(&at);
                assert_eq!(ok, held(id, lease, &["leased"]), "{at}");
                ok
            }
            Op::Renew(id, lease, ttl) => {
                let ok = self.inbox.renew(id, lease, now, *ttl).expect(&at);
                assert_eq!(ok, held(id, lease, &["leased", "processing"]), "{at}");
                ok
            }
            Op::Retry(id, lease, delay) => {
                let ok = self.inbox.retry(id, lease, now, *delay).expect(&at);
                assert_eq!(ok, held(id, lease, &["leased"]), "{at}");
                ok
            }
            Op::Ack(id, lease, outcome) => match self.inbox.ack(id, lease, outcome, now) {
                Err(Error::InvalidOutcome) => {
                    assert_eq!(*outcome, "done", "{at}");
                    false
                }
                got => {
                    let ok = got.expect(&at);
                    assert_eq!(ok, held(id, lease, &["processing"]), "{at}");
                    ok
                }
            },
            Op::Recover => {
                self.inbox.recover(now).expect(&at);
                true
            }
        };
        let after = snapshot(self.inbox);
        if !applied {
            assert_eq!(&after, before, "{at}: a refused call changed the store");
        }
        self.check(&at, op, before, &after, claimed.as_ref());
        after
    }

    fn check(
        &mut self,
        at: &str,
        op: &Op,
        before: &Snapshot,
        after: &Snapshot,
        claimed: Option<&EventRow>,
    ) {
        for (id, new) in after {
            let Some(old) = before.get(id) else {
                assert!(
                    matches!(op, Op::Publish(i) if IDS[*i] == id),
                    "{at}: {id} appeared"
                );
                assert_eq!((new.status.as_str(), new.attempts), ("pending", 0), "{at}");
                continue;
            };
            assert_eq!(
                (&new.packet, &new.digest, new.created.to_bits()),
                (&old.packet, &old.digest, old.created.to_bits()),
                "{at}: {id} immutable columns changed"
            );
            let is_claimed = claimed.is_some_and(|row| row.id == *id);
            assert_eq!(
                new.attempts,
                old.attempts + i64::from(is_claimed),
                "{at}: {id} attempts"
            );
            match new.status.as_str() {
                "leased" | "processing" => assert!(
                    new.lease.as_ref().is_some_and(|l| l.len() == 32
                        && l.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
                    "{at}: {id} {} without a lease token",
                    new.status
                ),
                "pending" => assert_eq!(new.lease, None, "{at}: pending {id} keeps a lease"),
                _ => {}
            }
            self.check_edges(at, id, old, new, is_claimed);
        }
        assert!(
            before.keys().all(|id| after.contains_key(id)),
            "{at}: a row vanished"
        );
        if let Op::Claim(_) = op {
            self.check_claim(at, before, after, claimed);
        }
    }

    /// Records each status change as edges and refuses any outside ADR-0007 section 7. A
    /// claim runs recovery first, so its row may pass through `pending` within the step.
    fn check_edges(&mut self, at: &str, id: &str, old: &EventRow, new: &EventRow, claimed: bool) {
        let (from, to) = (old.status.as_str(), new.status.as_str());
        let path: Vec<(&str, &str)> = if from == to {
            vec![]
        } else if claimed && from != "pending" {
            vec![(from, "pending"), ("pending", to)]
        } else {
            vec![(from, to)]
        };
        for edge in path {
            assert!(EDGES.contains(&edge), "{at}: {id} illegal edge {edge:?}");
            if edge == ("deferred", "pending") {
                assert!(
                    old.attempts < MAX_OFFERS,
                    "{at}: {id} re-offered after MAX_OFFERS"
                );
                let count = self.reoffers.entry(id.to_owned()).or_default();
                *count += 1;
                assert!(*count <= MAX_OFFERS, "{at}: {id} re-offered {count} times");
            }
            self.covered.insert((edge.0.to_owned(), edge.1.to_owned()));
        }
        if let Some(outcome) = new.outcome.as_deref().filter(|_| from != to) {
            self.covered
                .insert(("outcome".to_owned(), outcome.to_owned()));
        }
    }

    /// The claimed row had no valid lease of another holder, got a fresh token and was the
    /// oldest claimable row; no claim means nothing was claimable.
    fn check_claim(
        &self,
        at: &str,
        before: &Snapshot,
        after: &Snapshot,
        claimed: Option<&EventRow>,
    ) {
        let claimable = |row: &EventRow| row.status == "pending" && row.available <= self.now;
        let order = |row: &EventRow| (row.created.to_bits(), row.id.clone());
        let Some(row) = claimed else {
            assert!(
                !after.values().any(claimable),
                "{at}: claimable row left unclaimed"
            );
            return;
        };
        let id = &row.id;
        assert_eq!(Some(row), after.get(id), "{at}: claim returned a stale row");
        assert_eq!(row.status, "leased", "{at}");
        let old = &before[id];
        if matches!(old.status.as_str(), "leased" | "processing") {
            assert!(
                old.lease_until.is_some_and(|until| until < self.now),
                "{at}: {id} claimed under a valid lease"
            );
        }
        assert_ne!(row.lease, old.lease, "{at}: {id} lease token reused");
        assert!(
            !after
                .values()
                .filter(|r| r.id != row.id)
                .any(|r| claimable(r) && order(r) < order(row)),
            "{at}: an older claimable row was skipped"
        );
    }
}

fn run_case(
    seed: u64,
    inbox: &mut Inbox,
    packets: &[Value],
    covered: &mut BTreeSet<(String, String)>,
) {
    inbox.conn.execute("DELETE FROM events", []).expect("reset");
    let mut rng = Rng(seed);
    let mut run = Run {
        seed,
        inbox,
        packets,
        now: T0,
        held: Vec::new(),
        reoffers: BTreeMap::new(),
        covered,
    };
    let mut rows = Snapshot::new();
    for step in 0..STEPS {
        run.now += rng.pick(&CLOCK_STEPS);
        let op = run.next_op(&mut rng, &rows);
        rows = run.step(step, &op, &rows);
    }
}

/// Runs cases `shard * CASES_PER_SHARD ..` and requires every legal edge and both recovery
/// outcomes among them, so a generator change cannot silently stop exercising one.
fn run_shard(shard: u64) {
    assert!(shard < SHARDS, "shard {shard} of {SHARDS}");
    let packets = signed_packets();
    let mut inbox = memory_inbox();
    let mut covered = BTreeSet::new();
    for case in shard * CASES_PER_SHARD..(shard + 1) * CASES_PER_SHARD {
        run_case(BASE_SEED ^ case, &mut inbox, &packets, &mut covered);
    }
    for (from, to) in EDGES {
        assert!(
            covered.contains(&(from.to_owned(), to.to_owned())),
            "shard {shard}: edge {from} -> {to} never exercised"
        );
    }
    for outcome in ["expired-processing", "deferred-exhausted"] {
        assert!(
            covered.contains(&("outcome".to_owned(), outcome.to_owned())),
            "shard {shard}: outcome {outcome} never reached"
        );
    }
}

macro_rules! shards {
    ($($name:ident = $shard:literal),* $(,)?) => {$(
        #[test]
        fn $name() {
            run_shard($shard);
        }
    )*};
}

shards! {
    random_sequences_keep_the_lease_invariants_00 = 0,
    random_sequences_keep_the_lease_invariants_01 = 1,
    random_sequences_keep_the_lease_invariants_02 = 2,
    random_sequences_keep_the_lease_invariants_03 = 3,
    random_sequences_keep_the_lease_invariants_04 = 4,
    random_sequences_keep_the_lease_invariants_05 = 5,
    random_sequences_keep_the_lease_invariants_06 = 6,
    random_sequences_keep_the_lease_invariants_07 = 7,
    random_sequences_keep_the_lease_invariants_08 = 8,
    random_sequences_keep_the_lease_invariants_09 = 9,
    random_sequences_keep_the_lease_invariants_10 = 10,
    random_sequences_keep_the_lease_invariants_11 = 11,
    random_sequences_keep_the_lease_invariants_12 = 12,
    random_sequences_keep_the_lease_invariants_13 = 13,
    random_sequences_keep_the_lease_invariants_14 = 14,
    random_sequences_keep_the_lease_invariants_15 = 15,
    random_sequences_keep_the_lease_invariants_16 = 16,
    random_sequences_keep_the_lease_invariants_17 = 17,
    random_sequences_keep_the_lease_invariants_18 = 18,
    random_sequences_keep_the_lease_invariants_19 = 19,
    random_sequences_keep_the_lease_invariants_20 = 20,
    random_sequences_keep_the_lease_invariants_21 = 21,
    random_sequences_keep_the_lease_invariants_22 = 22,
    random_sequences_keep_the_lease_invariants_23 = 23,
    random_sequences_keep_the_lease_invariants_24 = 24,
    random_sequences_keep_the_lease_invariants_25 = 25,
    random_sequences_keep_the_lease_invariants_26 = 26,
    random_sequences_keep_the_lease_invariants_27 = 27,
    random_sequences_keep_the_lease_invariants_28 = 28,
    random_sequences_keep_the_lease_invariants_29 = 29,
    random_sequences_keep_the_lease_invariants_30 = 30,
    random_sequences_keep_the_lease_invariants_31 = 31,
    random_sequences_keep_the_lease_invariants_32 = 32,
    random_sequences_keep_the_lease_invariants_33 = 33,
    random_sequences_keep_the_lease_invariants_34 = 34,
    random_sequences_keep_the_lease_invariants_35 = 35,
    random_sequences_keep_the_lease_invariants_36 = 36,
    random_sequences_keep_the_lease_invariants_37 = 37,
    random_sequences_keep_the_lease_invariants_38 = 38,
    random_sequences_keep_the_lease_invariants_39 = 39,
    random_sequences_keep_the_lease_invariants_40 = 40,
    random_sequences_keep_the_lease_invariants_41 = 41,
    random_sequences_keep_the_lease_invariants_42 = 42,
    random_sequences_keep_the_lease_invariants_43 = 43,
    random_sequences_keep_the_lease_invariants_44 = 44,
    random_sequences_keep_the_lease_invariants_45 = 45,
    random_sequences_keep_the_lease_invariants_46 = 46,
    random_sequences_keep_the_lease_invariants_47 = 47,
    random_sequences_keep_the_lease_invariants_48 = 48,
    random_sequences_keep_the_lease_invariants_49 = 49,
}
