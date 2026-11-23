//! Landlock in the launcher (ADR-0056 sections 5.2 and 5.5): the kernel's ABI or the refusal it
//! causes, the rules of a run's paths and of its connect ports under `network = "direct"`, and
//! the restriction of the launcher before it executes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, LandlockStatus, NetPort,
    PathBeneath, PathFd, RestrictSelf, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
    Scope,
};

use super::unit::Launch;

/// Below ABI 3 truncation is never checked, so a connector could truncate the archive.
const MIN_ABI: ABI = ABI::V3;
/// The first ABI with the signal and abstract Unix socket scopes.
const SCOPES_ABI: ABI = ABI::V6;
/// The first ABI with TCP port rules.
const NET_ABI: ABI = ABI::V4;
/// System trees with read and execute.
const SYSTEM: [&str; 2] = ["/usr", "/etc"];
/// System trees with read and execute where the host has them.
const SYSTEM_IF_PRESENT: [&str; 4] = ["/lib", "/lib64", "/bin", "/sbin"];
/// Read only.
const READ: [&str; 4] = ["/proc", "/dev/zero", "/dev/urandom", "/dev/random"];
/// Read and write.
const DEV_NULL: &str = "/dev/null";

/// What a path is, which bounds the rights a rule on it may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Dir,
    File,
}

/// The kind of `path` after symlinks, none when it does not exist.
///
/// # Errors
/// The path exists but cannot be inspected.
fn kind(path: &Path) -> io::Result<Option<Kind>> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(Some(Kind::Dir)),
        Ok(_) => Ok(Some(Kind::File)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The ABI the launcher restricts itself with: the kernel's, capped at the newest the crate
/// knows. The error is the reason of the run's refusal for `name`.
pub(super) fn abi(status: LandlockStatus, name: &str) -> Result<ABI, String> {
    let fix = format!("or set connectors.{name}.landlock = \"off\"");
    let unavailable = |errno: &str| {
        format!(
            "landlock unavailable ({errno}); Linux 6.2 or later with landlock in the LSM list is required, {fix}"
        )
    };
    match status {
        LandlockStatus::NotImplemented => Err(unavailable("ENOSYS")),
        LandlockStatus::NotEnabled => Err(unavailable("EOPNOTSUPP")),
        LandlockStatus::Available { effective_abi, .. } if effective_abi < MIN_ABI => Err(format!(
            "landlock ABI {effective_abi}; ABI {MIN_ABI} (Linux 6.2) or later is required, {fix}"
        )),
        LandlockStatus::Available { effective_abi, .. } => Ok(effective_abi),
    }
}

/// The kernel's Landlock status, read without restricting anything.
///
/// # Errors
/// The crate's probe failed.
fn status() -> Result<LandlockStatus, String> {
    RestrictSelf::default()
        .no_new_privs(false)
        .apply()
        .map(|status| status.landlock)
        .map_err(|e| format!("landlock probe: {e}"))
}

/// The rules of a run under `abi`: each path with its rights, a file's cut to the rights a file
/// can have. `kind` tells what a path is; a missing path is an error unless it is one of the
/// system trees taken where present.
pub(super) fn rules(
    launch: &Launch,
    abi: ABI,
    kind: impl Fn(&Path) -> io::Result<Option<Kind>>,
) -> Result<Vec<(PathBuf, BitFlags<AccessFs>)>, String> {
    let read_exec = AccessFs::from_read(abi);
    let read = AccessFs::ReadFile | AccessFs::ReadDir;
    let write = AccessFs::from_all(abi) & !BitFlags::from(AccessFs::Execute);
    let system = SYSTEM.iter().map(|p| (PathBuf::from(p), read_exec, true));
    let present = SYSTEM_IF_PRESENT
        .iter()
        .map(|p| (PathBuf::from(p), read_exec, false));
    let read_only = READ.iter().map(|p| (PathBuf::from(p), read, true));
    let own = [
        (
            PathBuf::from(DEV_NULL),
            AccessFs::ReadFile | AccessFs::WriteFile,
        ),
        (launch.dir.clone(), read_exec),
        (launch.data_dir.clone(), write),
    ]
    .into_iter()
    .map(|(path, rights)| (path, rights, true));
    let key = launch
        .key
        .iter()
        .map(|path| (path.clone(), BitFlags::from(AccessFs::ReadFile), true));
    let grants = (launch.read.iter().map(|path| (path.clone(), read, true)))
        .chain(launch.write.iter().map(|path| (path.clone(), write, true)));
    let wanted: Vec<_> = system
        .chain(present)
        .chain(read_only)
        .chain(own)
        .chain(key)
        .chain(grants)
        .collect();
    let mut rules = Vec::with_capacity(wanted.len());
    for (path, rights, required) in wanted {
        match kind(&path).map_err(|e| format!("{}: {e}", path.display()))? {
            Some(Kind::Dir) => rules.push((path, rights)),
            Some(Kind::File) => rules.push((path, rights & AccessFs::from_file(abi))),
            None if required => {
                return Err(format!("{}: no such file or directory", path.display()));
            }
            None => {}
        }
    }
    Ok(rules)
}

/// The ports TCP connect is limited to under `abi`: the launch's `direct` ports from ABI 4,
/// none when it is not `direct` or the ABI has no port rules.
pub(super) fn connect_ports(launch: &Launch, abi: ABI) -> Option<&[u16]> {
    launch.direct.as_deref().filter(|_| abi >= NET_ABI)
}

/// Restricts this thread and what it executes to the rules of `launch`, handling every
/// filesystem right of the running ABI as a hard requirement, with the scopes from ABI 6 and
/// the connect ports of [`connect_ports`]. Returns the ABI and those ports; the error is the
/// reason of the run's refusal.
pub(super) fn confine(launch: &Launch) -> Result<(ABI, Option<Vec<u16>>), String> {
    let abi = abi(status()?, &launch.name)?;
    let rules = rules(launch, abi, kind)?;
    let ports = connect_ports(launch, abi);
    let failed = |e: landlock::RulesetError| format!("landlock ABI {abi}: {e}");
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(failed)?;
    if ports.is_some() {
        ruleset = ruleset
            .handle_access(AccessNet::ConnectTcp)
            .map_err(failed)?;
    }
    if abi >= SCOPES_ABI {
        ruleset = ruleset
            .scope(Scope::Signal | Scope::AbstractUnixSocket)
            .map_err(failed)?;
    }
    let mut created = ruleset.create().map_err(failed)?;
    for (path, rights) in rules {
        let fd = PathFd::new(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        created = created
            .add_rule(PathBeneath::new(fd, rights))
            .map_err(failed)?;
    }
    for &port in ports.unwrap_or_default() {
        created = created
            .add_rule(NetPort::new(port, AccessNet::ConnectTcp))
            .map_err(failed)?;
    }
    let status = created.restrict_self().map_err(failed)?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(format!(
            "landlock ABI {abi}: ruleset {:?}, not fully enforced",
            status.ruleset
        ));
    }
    Ok((abi, ports.map(<[u16]>::to_vec)))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::io::ErrorKind;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    use super::*;
    use crate::scratch::Scratch;

    fn launch(root: &Path) -> Launch {
        Launch {
            name: "notes".to_owned(),
            data_dir: root.join("state/notes/data"),
            dir: root.join("connectors/notes"),
            key: Some(root.join("secrets/notes.env")),
            read: vec![root.join("shared")],
            write: vec![root.join("outbox")],
            landlock: true,
            direct: None,
            program: OsString::from("/usr/bin/true"),
            args: Vec::new(),
        }
    }

    /// Every path exists but `missing`; device nodes and `.env` files are files.
    fn kinds_without(missing: Vec<&'static str>) -> impl Fn(&Path) -> io::Result<Option<Kind>> {
        move |path: &Path| {
            let text = path.to_str().expect("UTF-8");
            Ok(if missing.contains(&text) {
                None
            } else if text.starts_with("/dev/") || path.extension() == Some("env".as_ref()) {
                Some(Kind::File)
            } else {
                Some(Kind::Dir)
            })
        }
    }

    #[test]
    fn the_kernel_status_refuses_without_landlock_or_below_abi_3() {
        for (status, seen) in [
            (
                LandlockStatus::NotImplemented,
                "landlock unavailable (ENOSYS); ",
            ),
            (
                LandlockStatus::NotEnabled,
                "landlock unavailable (EOPNOTSUPP); ",
            ),
            (
                LandlockStatus::Available {
                    effective_abi: ABI::V2,
                    kernel_abi: None,
                },
                "landlock ABI 2; ABI 3 (Linux 6.2) or later is required, ",
            ),
        ] {
            let refused = abi(status, "notes").expect_err("refused");
            assert!(refused.starts_with(seen), "{refused}");
            assert!(
                refused.ends_with("or set connectors.notes.landlock = \"off\""),
                "{refused}"
            );
        }
        for known in [ABI::V3, ABI::V6, ABI::V8] {
            let status = LandlockStatus::Available {
                effective_abi: known,
                kernel_abi: None,
            };
            assert_eq!(abi(status, "notes"), Ok(known));
        }
    }

    #[test]
    fn each_path_gets_the_rights_of_its_row_and_files_only_file_rights() {
        let root = Path::new("/srv/run");
        let rules = rules(
            &launch(root),
            ABI::V5,
            kinds_without(vec!["/lib64", "/sbin"]),
        )
        .expect("rules");
        let rights = |path: &str| {
            rules
                .iter()
                .find(|(p, _)| p == Path::new(path))
                .map(|(_, r)| *r)
        };
        let read_exec = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;
        for path in ["/usr", "/etc", "/lib", "/bin", "/srv/run/connectors/notes"] {
            assert_eq!(rights(path), Some(read_exec), "{path}");
        }
        assert_eq!(rights("/lib64"), None);
        assert_eq!(rights("/sbin"), None);
        let read = AccessFs::ReadFile | AccessFs::ReadDir;
        assert_eq!(rights("/proc"), Some(read));
        assert_eq!(rights("/srv/run/shared"), Some(read));
        for device in ["/dev/zero", "/dev/urandom", "/dev/random"] {
            assert_eq!(rights(device), Some(AccessFs::ReadFile.into()), "{device}");
        }
        assert_eq!(
            rights("/dev/null"),
            Some(AccessFs::ReadFile | AccessFs::WriteFile)
        );
        assert_eq!(
            rights("/srv/run/secrets/notes.env"),
            Some(AccessFs::ReadFile.into())
        );
        let data = rights("/srv/run/state/notes/data").expect("data");
        assert_eq!(
            data,
            AccessFs::from_all(ABI::V5) & !BitFlags::from(AccessFs::Execute)
        );
        assert!(data.contains(AccessFs::Truncate | AccessFs::Refer | AccessFs::MakeReg));
        assert_eq!(rights("/srv/run/outbox"), Some(data));
        assert_eq!(rules.len(), 14);
    }

    #[test]
    fn a_missing_required_path_is_an_error() {
        let root = Path::new("/srv/run");
        for missing in [
            "/usr",
            "/dev/null",
            "/srv/run/secrets/notes.env",
            "/srv/run/shared",
        ] {
            let error =
                rules(&launch(root), ABI::V3, kinds_without(vec![missing])).expect_err(missing);
            assert_eq!(error, format!("{missing}: no such file or directory"));
        }
    }

    #[test]
    fn a_confined_thread_reaches_only_its_paths_and_grants() {
        let scratch = Scratch::new("landlock");
        let root = scratch.0.clone();
        let run = launch(&root);
        for dir in [&run.data_dir, &run.dir, &run.read[0], &run.write[0]] {
            fs::create_dir_all(dir).expect("dir");
        }
        fs::create_dir_all(root.join("secrets")).expect("secrets");
        let key = run.key.clone().expect("key");
        fs::write(&key, "TOKEN=x\n").expect("key");
        fs::write(root.join("secrets/other.env"), "TOKEN=y\n").expect("other key");
        fs::write(root.join("ulei.toml"), "[connectors]\n").expect("config");
        fs::write(root.join("archive.bin"), b"archive bytes").expect("archive");
        let shared = run.read[0].join("shared.txt");
        fs::write(&shared, "shared").expect("shared");
        let (data, outbox, at) = (
            run.data_dir.clone(),
            run.write[0].join("out.txt"),
            root.clone(),
        );
        // Landlock and no_new_privs bind only the calling thread, so the test runner stays free.
        let (abi, seen) = thread::spawn(move || {
            let (abi, ports) = confine(&run)?;
            assert_eq!(ports, None);
            let kind = |r: io::Result<()>| r.map_err(|e| e.kind());
            let read = |path: &Path| kind(fs::read(path).map(drop));
            let truncate = |path: &Path| {
                kind(
                    OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(path)
                        .map(drop),
                )
            };
            Ok::<_, String>((
                abi,
                [
                    ("read key", read(&key)),
                    ("read granted", read(&shared)),
                    ("write data", kind(fs::write(data.join("x"), "x"))),
                    ("write granted", kind(fs::write(&outbox, "x"))),
                    ("read other key", read(&at.join("secrets/other.env"))),
                    ("read ulei.toml", read(&at.join("ulei.toml"))),
                    ("create outside", kind(fs::write(at.join("new.txt"), "x"))),
                    ("truncate archive", truncate(&at.join("archive.bin"))),
                    ("write read grant", kind(fs::write(&shared, "x"))),
                ],
            ))
        })
        .join()
        .expect("confined thread")
        .expect("this host needs Landlock ABI 3 or later");
        assert!(abi >= ABI::V3);
        let (allowed, refused) = seen.split_at(4);
        for (what, result) in allowed {
            assert_eq!(*result, Ok(()), "{what}");
        }
        for (what, result) in refused {
            assert_eq!(*result, Err(ErrorKind::PermissionDenied), "{what}");
        }
        assert_eq!(
            fs::read(root.join("archive.bin")).expect("archive"),
            b"archive bytes"
        );
        assert!(!root.join("new.txt").exists());
    }

    #[test]
    fn direct_ports_are_limited_only_from_abi_4() {
        let root = Path::new("/srv/run");
        let direct = Launch {
            direct: Some(vec![443, 8443]),
            ..launch(root)
        };
        assert_eq!(connect_ports(&direct, ABI::V3), None);
        assert_eq!(connect_ports(&direct, ABI::V4), Some(&[443, 8443][..]));
        assert_eq!(connect_ports(&launch(root), ABI::V6), None);
    }

    #[test]
    fn a_direct_thread_connects_only_to_its_ports_from_abi_4() {
        let scratch = Scratch::new("landlock-net");
        let root = scratch.0.clone();
        let listed = TcpListener::bind("127.0.0.1:0").expect("listed");
        let other = TcpListener::bind("127.0.0.1:0").expect("other");
        let port = |l: &TcpListener| l.local_addr().expect("addr").port();
        let (listed_port, other_port) = (port(&listed), port(&other));
        let run = Launch {
            direct: Some(vec![listed_port]),
            ..launch(&root)
        };
        for dir in [&run.data_dir, &run.dir, &run.read[0], &run.write[0]] {
            fs::create_dir_all(dir).expect("dir");
        }
        fs::create_dir_all(root.join("secrets")).expect("secrets");
        fs::write(run.key.as_ref().expect("key"), "TOKEN=x\n").expect("key");
        let (abi, ports, seen) = thread::spawn(move || {
            let (abi, ports) = confine(&run)?;
            let connect = |p: u16| {
                TcpStream::connect(("127.0.0.1", p))
                    .map(drop)
                    .map_err(|e| e.kind())
            };
            Ok::<_, String>((abi, ports, [connect(listed_port), connect(other_port)]))
        })
        .join()
        .expect("confined thread")
        .expect("this host needs Landlock ABI 3 or later");
        if abi >= ABI::V4 {
            assert_eq!(ports, Some(vec![listed_port]));
            assert_eq!(seen, [Ok(()), Err(ErrorKind::PermissionDenied)]);
        } else {
            assert_eq!(ports, None);
            assert_eq!(seen, [Ok(()), Ok(())]);
        }
    }
}
