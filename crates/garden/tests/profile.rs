//! The pinned core profile through `ulei_garden::serve::Server` on a socketpair, the worker and
//! `ulei_garden::profile::Profile`, on an injected clock. All people, places and texts are
//! fictional.

#![allow(missing_docs, clippy::unwrap_used)]

use std::cell::Cell;
use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use serde_json::{Value, json};
use ulei_garden::api::ApiConfig;
use ulei_garden::edge::{Edge, EdgeConfig};
use ulei_garden::miss::{Inbox, ProducerKey};
use ulei_garden::model::TtlTable;
use ulei_garden::profile::{DELIMITER, Profile};
use ulei_garden::rerank;
use ulei_garden::second::Provider;
use ulei_garden::serve::{Server, Settings, TIMEOUT, open};
use ulei_garden::store::{Store, starting_domains};
use ulei_garden::wire::c7;
use ulei_garden::worker::{Clock, Schedule, Wake, Worker};

const NOW: i64 = 1_800_000_000;

/// A clock the test sets.
#[derive(Clone, Default)]
struct Fake(Rc<Cell<i64>>);

impl Fake {
    fn at(t: i64) -> Self {
        let clock = Self::default();
        clock.0.set(t);
        clock
    }
}

impl Clock for Fake {
    fn now(&self) -> i64 {
        self.0.get()
    }
}

/// A fresh 0700 directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ulei-garden-profile-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self(dir)
    }

    fn store(&self) -> PathBuf {
        self.0.join("garden.sqlite3")
    }

    fn core(&self) -> PathBuf {
        self.0.join("brain/CORE.md")
    }

    fn key(&self) -> ProducerKey {
        let path = self.0.join("producer.key");
        if !path.exists() {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap()
                .write_all(&[0x5a; 32])
                .unwrap();
        }
        ProducerKey::read(&path, rustix::process::geteuid().as_raw()).unwrap()
    }

    fn settings(&self) -> Settings {
        Settings {
            store: self.store(),
            socket: self.0.join("garden.sock"),
            producer_key: self.key(),
            inbox_socket: self.0.join("inbox.sock"),
            domains: starting_domains().unwrap(),
            ttl: TtlTable::default(),
            api: ApiConfig::default(),
            schedule: Schedule::default(),
            second_level: Provider::None,
            reranker: rerank::Choice::None,
            embedder: None,
            core: None,
        }
    }

    /// The core file read back; it must exist.
    fn read_core(&self) -> String {
        fs::read_to_string(self.core()).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The server of `scratch` on `clock` rendering `profile`.
fn server(scratch: &Scratch, clock: &Fake, profile: &Arc<Profile>) -> Server<Fake> {
    let garden = open(&scratch.settings(), None).unwrap();
    let uid = rustix::process::geteuid().as_raw();
    Server::new(
        garden,
        uid,
        TIMEOUT,
        Arc::new(Wake::default()),
        clock.clone(),
    )
    .with_profile(Arc::clone(profile))
}

fn profile(scratch: &Scratch, char_limit: usize) -> Arc<Profile> {
    Arc::new(Profile::new(scratch.core(), char_limit))
}

/// The `200` body of the answer of `server` to `method` with `body`.
fn call(server: &mut Server<Fake>, method: &str, body: &Value) -> Vec<u8> {
    let body = body.to_string();
    let (mut client, end) = UnixStream::pair().unwrap();
    write!(
        client,
        "POST /v1/{method} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    server.serve(end);
    let mut raw = Vec::new();
    client.read_to_end(&mut raw).unwrap();
    let start = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(raw.starts_with(b"HTTP/1.1 200 "));
    raw.split_off(start)
}

fn propose(server: &mut Server<Fake>, candidate: &Value) -> c7::ProposeResponse {
    serde_json::from_slice(&call(server, "propose", candidate)).unwrap()
}

/// The `pinned` flag `search` gives the fact with `text`, which it must find.
fn pinned_in_search(server: &mut Server<Fake>, q: &str, text: &str) -> Option<bool> {
    let found: c7::SearchResponse =
        serde_json::from_slice(&call(server, "search", &json!({ "q": q, "private": true })))
            .unwrap();
    found
        .facts
        .into_iter()
        .find(|f| f.fact == text)
        .unwrap()
        .pinned
}

/// An owner fact about the person `entity`.
fn owner(entity: &str, fact: &str) -> Value {
    json!({
        "fact": fact,
        "entity": entity,
        "kind": "person",
        "source_ref": "msg:12",
        "source_kind": "owner",
    })
}

fn pinned(entity: &str, fact: &str) -> Value {
    let mut c = owner(entity, fact);
    c["pin"] = json!(true);
    c
}

fn status(answer: &c7::ProposeResponse) -> (c7::ProposalStatus, Option<c7::ProposalReason>) {
    (answer.status, answer.reason)
}

fn entries(lines: &[&str]) -> String {
    lines.join(DELIMITER)
}

#[test]
fn pinned_current_facts_render_by_entity_name_then_first_pin_with_private_ones_and_the_delimiter() {
    let scratch = Scratch::new();
    let clock = Fake::at(NOW);
    let core = profile(&scratch, 1375);
    let mut server = server(&scratch, &clock, &core);
    let accepted = (c7::ProposalStatus::Accepted, None);

    let mut address = pinned("Ada Example", "Ada lives at 3 Old Road.");
    address["attribute"] = json!("address");
    assert_eq!(status(&propose(&mut server, &address)), accepted);
    assert_eq!(scratch.read_core(), "Ada Example: Ada lives at 3 Old Road.");
    let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&scratch.core()), 0o600);
    assert_eq!(mode(scratch.core().parent().unwrap()), 0o700);

    clock.0.set(NOW + 10);
    let mut bram = pinned("Bram Sample", "Bram keeps bees.");
    bram["private"] = json!(true);
    assert_eq!(status(&propose(&mut server, &bram)), accepted);
    clock.0.set(NOW + 20);
    let diet = pinned("Ada Example", "Ada eats no fish.");
    assert_eq!(status(&propose(&mut server, &diet)), accepted);
    clock.0.set(NOW + 30);
    assert_eq!(
        status(&propose(
            &mut server,
            &owner("Cleo Placeholder", "Cleo rows.")
        )),
        accepted,
        "an unpinned fact stays out"
    );
    assert_eq!(
        scratch.read_core(),
        entries(&[
            "Ada Example: Ada lives at 3 Old Road.",
            "Ada Example: Ada eats no fish.",
            "Bram Sample: Bram keeps bees.",
        ])
    );

    // A new address inherits the pin and the place of the first pin, before the later diet.
    clock.0.set(NOW + 40);
    let mut moved = owner("Ada Example", "Ada lives at 9 New Street.");
    moved["attribute"] = json!("address");
    assert_eq!(status(&propose(&mut server, &moved)), accepted);
    assert_eq!(
        scratch.read_core(),
        entries(&[
            "Ada Example: Ada lives at 9 New Street.",
            "Ada Example: Ada eats no fish.",
            "Bram Sample: Bram keeps bees.",
        ])
    );
    assert_eq!(
        pinned_in_search(&mut server, "New Street", "Ada lives at 9 New Street."),
        Some(true)
    );
    assert_eq!(
        pinned_in_search(&mut server, "rows", "Cleo rows."),
        None,
        "an unpinned fact has no flag"
    );
}

#[test]
fn pin_false_unpins_and_a_pin_change_is_an_update_never_a_duplicate() {
    let scratch = Scratch::new();
    let clock = Fake::at(NOW);
    let core = profile(&scratch, 1375);
    let mut server = server(&scratch, &clock, &core);
    let text = "Ada sings in a choir.";
    let first = propose(&mut server, &pinned("Ada Example", text));
    assert_eq!(first.status, c7::ProposalStatus::Accepted);
    assert_eq!(scratch.read_core(), format!("Ada Example: {text}"));

    clock.0.set(NOW + 10);
    let again = propose(&mut server, &pinned("Ada Example", text));
    assert_eq!(again.status, c7::ProposalStatus::Duplicate);
    assert_eq!(again.fact_id, first.fact_id);

    clock.0.set(NOW + 20);
    let mut unpin = owner("Ada Example", text);
    unpin["pin"] = json!(false);
    let unpinned = propose(&mut server, &unpin);
    assert_eq!(unpinned.status, c7::ProposalStatus::Accepted);
    assert_ne!(unpinned.fact_id, first.fact_id);
    assert_eq!(scratch.read_core(), "");
    assert_eq!(pinned_in_search(&mut server, "choir", text), None);

    clock.0.set(NOW + 30);
    assert_eq!(
        propose(&mut server, &unpin).status,
        c7::ProposalStatus::Duplicate
    );
    assert_eq!(
        propose(&mut server, &owner("Ada Example", text)).status,
        c7::ProposalStatus::Duplicate,
        "without pin a restatement is a duplicate"
    );
    let repinned = propose(&mut server, &pinned("Ada Example", text));
    assert_eq!(repinned.status, c7::ProposalStatus::Accepted);
    assert_eq!(scratch.read_core(), format!("Ada Example: {text}"));
    assert_eq!(pinned_in_search(&mut server, "choir", text), Some(true));
}

#[test]
fn pin_from_a_source_other_than_the_owner_or_on_a_secret_pointer_is_invalid() {
    let scratch = Scratch::new();
    let clock = Fake::at(NOW);
    let core = profile(&scratch, 1375);
    let mut server = server(&scratch, &clock, &core);
    let invalid = (
        c7::ProposalStatus::Rejected,
        Some(c7::ProposalReason::Invalid),
    );
    for (source, pin) in [
        (Some("agent"), true),
        (Some("internet"), true),
        (Some("raw"), false),
        (None, true),
    ] {
        let mut c = pinned("Ada Example", "Ada collects stamps.");
        c["pin"] = json!(pin);
        match source {
            Some(kind) => c["source_kind"] = json!(kind),
            None => drop(c.as_object_mut().unwrap().remove("source_kind")),
        }
        assert_eq!(status(&propose(&mut server, &c)), invalid, "{source:?}");
    }
    let mut secret = pinned("Ada Example", "Ada keeps a spare key in the vault.");
    secret["secret_ref"] = json!("vault:home/spare");
    assert_eq!(status(&propose(&mut server, &secret)), invalid);
    assert_eq!(scratch.read_core(), "");

    // A secret pointer that closes a pinned version does not take its pin.
    let mut door = pinned("Ada Example", "Ada's spare key is with the neighbour.");
    door["attribute"] = json!("spare_key");
    assert_eq!(
        propose(&mut server, &door).status,
        c7::ProposalStatus::Accepted
    );
    clock.0.set(NOW + 10);
    let mut vaulted = owner("Ada Example", "Ada's spare key is in the vault.");
    vaulted["attribute"] = json!("spare_key");
    vaulted["secret_ref"] = json!("vault:home/spare");
    assert_eq!(
        propose(&mut server, &vaulted).status,
        c7::ProposalStatus::Accepted
    );
    assert_eq!(scratch.read_core(), "");
}

#[test]
fn entries_over_the_budget_are_left_out_whole_and_counted_in_characters() {
    let scratch = Scratch::new();
    let clock = Fake::at(NOW);
    let first = "Ada Example: Ада пьёт чай без сахара.";
    let second = "Bram Sample: Bram keeps bees.";
    let joined = entries(&[first, second]);
    // Characters, not bytes: the Cyrillic text and the delimiter take more bytes than this.
    let limit = joined.chars().count();
    assert!(joined.len() > limit);
    let core = profile(&scratch, limit);
    let mut server = server(&scratch, &clock, &core);
    for (entity, text) in [
        ("Ada Example", "Ада пьёт чай без сахара."),
        ("Bram Sample", "Bram keeps bees."),
        ("Cleo Placeholder", "Cleo rows."),
        ("Dara Example", "Dara knits."),
    ] {
        clock.0.set(clock.now() + 10);
        assert_eq!(
            propose(&mut server, &pinned(entity, text)).status,
            c7::ProposalStatus::Accepted
        );
    }
    assert_eq!(scratch.read_core(), joined);

    // An entry that sorts first and does not fit leaves out every one after it as well.
    let narrow = Profile::new(scratch.0.join("narrow.md"), first.chars().count() - 1);
    narrow
        .render(&Store::open(&scratch.store()).unwrap(), clock.now())
        .unwrap();
    assert_eq!(fs::read_to_string(scratch.0.join("narrow.md")).unwrap(), "");
}

#[test]
fn a_reader_sees_the_old_or_the_new_file_whole_and_the_file_stays_owner_only() {
    let scratch = Scratch::new();
    let mut store = Store::open(&scratch.store()).unwrap();
    store.seed_domains(&starting_domains().unwrap()).unwrap();
    let mut edge = Edge::new(&mut store, EdgeConfig::default());
    let mut lasting = pinned("Ada Example", &"Ada plants tulips. ".repeat(40));
    lasting["valid_from"] = json!(NOW - 100);
    let mut brief = pinned("Bram Sample", &"Bram paints fences. ".repeat(40));
    brief["valid_from"] = json!(NOW - 100);
    brief["valid_to"] = json!(NOW + 1);
    for c in [&lasting, &brief] {
        let c: c7::ProposeRequest = serde_json::from_value(c.clone()).unwrap();
        assert!(matches!(
            edge.propose(&c, NOW - 100).unwrap(),
            ulei_garden::edge::Outcome::Accepted { .. }
        ));
    }
    fs::create_dir_all(scratch.core().parent().unwrap()).unwrap();
    fs::write(scratch.core(), "an older file").unwrap();
    fs::set_permissions(scratch.core(), fs::Permissions::from_mode(0o644)).unwrap();

    let core = Profile::new(scratch.core(), 10_000);
    core.render(&store, NOW).unwrap();
    let both = scratch.read_core();
    core.render(&store, NOW + 1).unwrap();
    let one = scratch.read_core();
    assert_ne!(both, one);
    assert_eq!(
        fs::metadata(scratch.core()).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let (path, done) = (scratch.core(), Arc::clone(&done));
        let (both, one) = (both.clone(), one.clone());
        thread::spawn(move || {
            let mut reads = 0;
            while !done.load(Ordering::Relaxed) {
                let seen = fs::read_to_string(&path).unwrap();
                assert!(
                    seen == both || seen == one,
                    "a torn file: {} bytes",
                    seen.len()
                );
                reads += 1;
            }
            reads
        })
    };
    for n in 0..40 {
        core.render(&store, NOW + n % 2).unwrap();
    }
    done.store(true, Ordering::Relaxed);
    assert!(reader.join().unwrap() > 0);
    let names: Vec<_> = fs::read_dir(scratch.core().parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["CORE.md"], "no temporary file is left");
}

#[test]
fn an_expired_or_ended_pinned_fact_leaves_the_file_after_the_worker_pass_due_then() {
    let scratch = Scratch::new();
    let clock = Fake::at(NOW);
    let core = profile(&scratch, 1375);
    let mut server = server(&scratch, &clock, &core);
    let mut weather = pinned("Fogtown", "Fogtown expects drizzle.");
    weather["kind"] = json!("place");
    weather["ttl_kind"] = json!("weather");
    weather["observed_at"] = json!(NOW);
    let mut trip = pinned("Ada Example", "Ada is away in the hills.");
    trip["valid_to"] = json!(NOW + 600);
    let lasting = pinned("Bram Sample", "Bram keeps bees.");
    for c in [&weather, &trip, &lasting] {
        assert_eq!(propose(&mut server, c).status, c7::ProposalStatus::Accepted);
    }
    assert_eq!(
        scratch.read_core(),
        entries(&[
            "Ada Example: Ada is away in the hills.",
            "Bram Sample: Bram keeps bees.",
            "Fogtown: Fogtown expects drizzle.",
        ])
    );

    let mut worker = Worker::new(
        Store::open(&scratch.store()).unwrap(),
        Schedule::default(),
        clock.clone(),
        Inbox::new(scratch.0.join("inbox.sock"), scratch.key()),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap()
    .with_profile(Arc::clone(&core));
    assert_eq!(worker.tick(), NOW + 600, "the end of the trip");
    clock.0.set(NOW + 600);
    let expiry = NOW + 3 * 3_600;
    assert_eq!(worker.tick(), expiry);
    assert_eq!(
        scratch.read_core(),
        entries(&[
            "Bram Sample: Bram keeps bees.",
            "Fogtown: Fogtown expects drizzle.",
        ])
    );
    clock.0.set(expiry);
    worker.tick();
    assert_eq!(worker.passes().expiry, 1);
    assert_eq!(scratch.read_core(), "Bram Sample: Bram keeps bees.");
}
