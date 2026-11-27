# Runbook: IO and CPU classes of the ulei units and Hive heads

A manual procedure, never run in CI and never run by an agent on its own: the owner, or the
secretary on the owner's word, runs it on the deployment host as the account that runs the ulei
units. It puts the class slices of ADR-0103 in force: it delegates the io controller to the user
manager if it is missing, gives `ulei.toml` the disk's values, installs the slices, removes the
hand-made drop-ins that would override them, and checks the caps the kernel applies. Section 1 is
the reference for reading the classes at any time.

Placeholders. Fill them on the host only; never write a value into this repository.

| Placeholder | Meaning |
|---|---|
| `<ULEI_CHECKOUT>`, `<RELEASE_DIR>` | A checkout of the release's commit and the release directory, as in `docs/runbooks/mvp-pilot-cutover.md` 1.3. |
| `<CONFIG_HOME>` | `$XDG_CONFIG_HOME` of the account (default `$HOME/.config`). |
| `<UID>` | The account's uid (`id -u`). |
| `<DEVICE>` | The block device of the data, such as `/dev/vdz`: `findmnt -no SOURCE -T <data dir>`, its whole disk from `lsblk -no PKNAME <partition>`. |
| `<READ_IOPS>`, `<WRITE_IOPS>` | What `<DEVICE>` sustains in random 4 KiB reads and writes, from the provider's figure or a measurement with a random 4 KiB O_DIRECT reader. |
| `<HIVE_CONFIG>` | The Hive's config file (`HIVE_CONFIG` of the dispatcher). |
| `<MANAGER>` | `/sys/fs/cgroup/user.slice/user-<UID>.slice/user@<UID>.service`. |

## 1. The classes and how to read them

| Class | Slice | Members | Caps |
|---|---|---|---|
| interactive | `ulei.slice` | ulei-engine, ulei-inbox, ulei-tg, ulei-memory, ulei-hive-mcp, ulei-garden, ulei-uncertain, ulei-hive-watchdog | none |
| Hive heads | `hive-heads.slice` | every head and script of the hive, idle IO class, nice 10 | `CPUWeight=50`, `CPUQuota` half the CPUs, 33 % of the read and 20 % of the write IOPS |
| urgent owner heads | `hive-urgent.slice` | the at most two urgent runs, `CPUWeight=1000`, best-effort IO class at level 0 | `CPUWeight=1000`, 15 % and 10 % |
| background | `background.slice` | ulei-night, ulei-connectors and its runs, the host's backups, imports and other batch work, idle IO class | `CPUWeight=1`, `CPUQuota` a quarter of the CPUs, 25 % and 20 % |

`hive-heads.slice` and `hive-urgent.slice` lie under `hive.slice`; their cgroups are
`<MANAGER>/hive.slice/hive-heads.slice` and `<MANAGER>/hive.slice/hive-urgent.slice`, the
others `<MANAGER>/<slice>`. A slice's cgroup exists while a unit runs in it.

- `systemctl --user show <slice> -p IOReadIOPSMax -p IOWriteIOPSMax -p CPUQuotaPerSecUSec -p CPUWeight -p DropInPaths`:
  what systemd holds for the slice and every drop-in that changes it.
- `cat <cgroup>/io.max`: one line per capped device, `MAJ:MIN rbps=max wbps=max riops=N wiops=N`;
  empty when nothing caps it, missing when the io controller is not delegated.
- `cat <cgroup>/cpu.max`: `QUOTA PERIOD`, `max 100000` when there is no quota;
  `cat <cgroup>/cpu.weight` the weight.
- `cat <cgroup>/io.stat`: per device the cumulative `rbytes wbytes rios wios`. Two readings `T`
  seconds apart give the IOPS: `(rios2 - rios1) / T`. A capped slice under load stays at about
  its `riops`.
- `cat <cgroup>/io.pressure`: `some avg10` is the share of the last 10 s in which a task of the
  slice waited for IO; the interactive slice's is the owner's view. `full` counts the time all
  its tasks waited.
- `ls <MANAGER>/hive.slice/hive-heads.slice/`: the `hive-run-<id>.service` units now running.

Place the host's own background work in `background.slice`: a drop-in `Slice=background.slice`
with `IOSchedulingClass=idle` for its user units, or
`systemd-run --user --slice=background.slice -p IOSchedulingClass=idle -- <command>` for a one-off.

## 2. Host step

### 2.1 The io controller

```sh
cat <MANAGER>/cgroup.controllers
```

It must name `io`. If not, as root:

```sh
mkdir -p /etc/systemd/system/user@.service.d
printf '[Service]\nDelegate=cpu cpuset io memory pids\n' > /etc/systemd/system/user@.service.d/90-ulei-io.conf
systemctl daemon-reload
systemctl restart user@<UID>.service
```

The restart stops every user unit of the account; plan it with the owner. Rollback: remove the
drop-in, `systemctl daemon-reload` and restart `user@<UID>.service` again.

### 2.2 The disk's values

Add to `<CONFIG_HOME>/ulei/ulei.toml`, in this table form only:

```toml
[host.io]
device = "<DEVICE>"
read_iops = <READ_IOPS>
write_iops = <WRITE_IOPS>
```

### 2.3 Install the slices

```sh
cd <ULEI_CHECKOUT> && deploy/install.sh <RELEASE_DIR>
```

It writes `ulei.slice`, `hive-heads.slice`, `hive-urgent.slice` and `background.slice` into
`<CONFIG_HOME>/systemd/user/` with the caps of section 1 and prints a line when `[host.io]` is
missing or the user manager has no io controller.

### 2.4 Remove the hand-made drop-ins

Any of these left in place overrides the installed caps: an empty assignment such as
`IOReadIOPSMax=` resets the list, drop-ins apply after the unit file, and `user.control` wins over
the same name in `<CONFIG_HOME>/systemd/user`. Remove each that exists:

- `<CONFIG_HOME>/systemd/user.control/background.slice.d/50-IOReadIOPSMax.conf`
- `<CONFIG_HOME>/systemd/user.control/background.slice.d/50-IOWriteIOPSMax.conf`
- `<CONFIG_HOME>/systemd/user.control/background.slice.d/50-IOReadBandwidthMax.conf`
- `<CONFIG_HOME>/systemd/user.control/background.slice.d/50-IOWriteBandwidthMax.conf`
- `<CONFIG_HOME>/systemd/user.control/hive-heads.slice.d/50-IOReadIOPSMax.conf`
- `<CONFIG_HOME>/systemd/user.control/hive-heads.slice.d/50-IOWriteIOPSMax.conf`
- `<CONFIG_HOME>/systemd/user/hive-heads.slice.d/20-iops-budget.conf`
- `<CONFIG_HOME>/systemd/user/hive-heads.slice.d/30-cpu.conf`
- a `Slice=` drop-in of `hive-run-.service`, if one exists: the dispatcher names each head's slice.

In `<HIVE_CONFIG>`, replace the `head_io_device`, `head_io_read_max` and `head_io_write_max`
lines, commented out or not, with `head_io_device` and the per-head `head_io_read_iops` and
`head_io_write_iops`, or delete them. A config that still names a bandwidth key loads without it
and with one stderr line naming it.

### 2.5 Reload and check

```sh
systemctl --user daemon-reload
systemctl --user show hive-heads.slice hive-urgent.slice background.slice -p DropInPaths
```

`DropInPaths` must be empty for each: `install.sh` writes whole slice files and no drop-in. With a
head and a background unit running:

```sh
cat <MANAGER>/hive.slice/hive-heads.slice/io.max <MANAGER>/hive.slice/hive-heads.slice/cpu.max
cat <MANAGER>/background.slice/io.max <MANAGER>/background.slice/cpu.max
```

`io.max` names the device of `<DEVICE>` with the `riops` and `wiops` of section 1 and `cpu.max`
the quota. Rollback: put the removed files back from the account's backup and
`systemctl --user daemon-reload`.
