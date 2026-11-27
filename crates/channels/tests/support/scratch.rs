//! A private scratch directory and Unix socket paths beside it, removed on drop.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct Scratch {
    dir: PathBuf,
    /// `tg-<pid>-<n>`, the start of each socket file name.
    stem: String,
    sockets: Mutex<Vec<PathBuf>>,
}

impl Scratch {
    /// A fresh directory named after the process, a counter and `tag`.
    pub(crate) fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("ulei-tg-{pid}-{n}-{tag}"));
        fs::create_dir_all(&dir).expect("scratch dir");
        Self {
            dir,
            stem: format!("tg-{pid}-{n}"),
            sockets: Mutex::new(Vec::new()),
        }
    }

    /// `name` inside the directory.
    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The socket path `name` of this scratch, the same on every call. It lies outside the
    /// directory: a Unix socket path has at most 107 bytes.
    pub(crate) fn socket(&self, name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{}-{name}.sock", self.stem));
        let mut sockets = self.sockets.lock().expect("scratch sockets");
        if !sockets.contains(&path) {
            sockets.push(path.clone());
        }
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
        for socket in self.sockets.get_mut().expect("scratch sockets").iter() {
            let _ = fs::remove_file(socket);
        }
    }
}
