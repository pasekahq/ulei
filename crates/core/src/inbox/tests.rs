use std::path::PathBuf;

use super::*;

/// A fresh directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("ulei-inbox-unit-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .expect("scratch dir");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn connection_uses_adr_pragmas_and_python_table() {
    let scratch = Scratch::new("pragmas");
    let inbox = Inbox::open(scratch.0.join("inbox")).expect("open");
    let pragma = |name: &str| -> String {
        inbox
            .conn
            .query_row(&format!("PRAGMA {name}"), [], |r| {
                r.get::<_, rusqlite::types::Value>(0)
            })
            .map(|v| match v {
                rusqlite::types::Value::Integer(i) => i.to_string(),
                rusqlite::types::Value::Text(t) => t,
                other => format!("{other:?}"),
            })
            .expect("pragma")
    };
    assert_eq!(pragma("journal_mode"), "delete");
    assert_eq!(pragma("synchronous"), "2");
    assert_eq!(pragma("cache_size"), "-512");
    assert_eq!(pragma("busy_timeout"), "10000");

    // Python inbox.py `CREATE TABLE events` as `cid|name|type|notnull|dflt_value|pk`.
    let python = [
        "0|id|TEXT|0||1",
        "1|packet|BLOB|1||0",
        "2|digest|TEXT|1||0",
        "3|status|TEXT|1|'pending'|0",
        "4|attempts|INTEGER|1|0|0",
        "5|lease|TEXT|0||0",
        "6|lease_until|REAL|0||0",
        "7|available|REAL|1|0|0",
        "8|session_id|TEXT|0||0",
        "9|outcome|TEXT|0||0",
        "10|created|REAL|1||0",
    ];
    let mut stmt = inbox
        .conn
        .prepare("PRAGMA table_info(events)")
        .expect("table_info");
    let columns: Vec<String> = stmt
        .query_map([], |r| {
            Ok(format!(
                "{}|{}|{}|{}|{}|{}",
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                r.get::<_, i64>(5)?,
            ))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(columns, python);
}

#[test]
fn root_and_key_of_another_uid_are_refused() {
    let scratch = Scratch::new("owner");
    let key = scratch.0.join(KEY_FILE);
    fs::write(&key, [0u8; KEY_LEN]).expect("key");
    let other = own_uid().expect("uid").wrapping_add(1);
    assert!(matches!(
        guard(&scratch.0, Guarded::Root, other),
        Err(Error::ForeignOwner(Guarded::Root))
    ));
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("chmod");
    assert!(matches!(
        guard(&key, Guarded::Key, other),
        Err(Error::ForeignOwner(Guarded::Key))
    ));
}
