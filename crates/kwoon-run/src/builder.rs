//! The one bubblewrap command-line builder (plan D5). Runs, the doctor probes, and
//! confined packing all use it, so the options below are decided in this file only.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A mount the probe must find in `/proc/self/mountinfo`, and only these.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredMount {
    pub mount_point: PathBuf,
    pub access: Access,
    /// bubblewrap creates some mounts only when the kernel offers the path (the
    /// read-only covers inside `/proc`). Those may be absent; all others must exist.
    pub optional: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Access {
    ReadOnly,
    Writable,
    /// Mounts whose flags bubblewrap chooses itself (`/proc`, `/dev`, `/tmp`, the root).
    Any,
}

/// The paths bubblewrap covers read-only inside `--proc` when the kernel has them.
const PROC_COVERS: [&str; 4] = ["sys", "sysrq-trigger", "irq", "bus"];
/// The device nodes bubblewrap binds into `--dev`, plus its devpts instance.
const DEV_NODES: [&str; 7] = ["null", "zero", "full", "random", "urandom", "tty", "pts"];

/// Root-level names that are usually links into `/usr` on a merged-usr system.
const USR_LINKS: [&str; 4] = ["/bin", "/lib", "/lib64", "/sbin"];

#[derive(Clone, Debug)]
enum Op {
    Proc,
    Dev,
    Tmpfs(PathBuf),
    Bind {
        source: PathBuf,
        dest: PathBuf,
        writable: bool,
    },
    Symlink {
        target: PathBuf,
        link: PathBuf,
    },
}

/// A bubblewrap invocation under construction.
#[derive(Clone, Debug)]
pub struct Sandbox {
    bwrap: PathBuf,
    share_net: bool,
    env: Vec<(String, String)>,
    ops: Vec<Op>,
    chdir: Option<PathBuf>,
}

impl Sandbox {
    /// The namespace and process options every sandbox gets. With nothing else added,
    /// the filesystem is bubblewrap's empty root.
    pub fn new(bwrap: impl Into<PathBuf>) -> Self {
        Sandbox {
            bwrap: bwrap.into(),
            share_net: false,
            env: Vec::new(),
            ops: Vec::new(),
            chdir: None,
        }
    }

    /// Runs need their provider's API, so they keep the host network (plan D5,
    /// "Network"). The packing sandbox does not call this.
    pub fn share_net(mut self) -> Self {
        self.share_net = true;
        self
    }

    pub fn setenv(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// `/proc`, `/dev`, an empty `/tmp`, read-only `/usr` and `/etc`, and the root-level
    /// links into `/usr`, copied from the host's own layout.
    pub fn system_mounts(mut self) -> Self {
        self.ops.push(Op::Proc);
        self.ops.push(Op::Dev);
        self.ops.push(Op::Tmpfs("/tmp".into()));
        for dir in ["/usr", "/etc"] {
            self.ops.push(Op::Bind {
                source: dir.into(),
                dest: dir.into(),
                writable: false,
            });
        }
        for name in USR_LINKS {
            let path = Path::new(name);
            match fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    if let Ok(target) = fs::read_link(path) {
                        self.ops.push(Op::Symlink {
                            target,
                            link: path.into(),
                        });
                    }
                }
                Ok(meta) if meta.is_dir() => self.ops.push(Op::Bind {
                    source: path.into(),
                    dest: path.into(),
                    writable: false,
                }),
                _ => {}
            }
        }
        // On hosts using systemd-resolved, /etc/resolv.conf links to a file under /run,
        // which is not mounted; without that one file the harness cannot resolve its
        // provider's API. Only the resolver file itself is bound, read-only.
        if let Some(target) = resolver_outside_bound_trees() {
            self.ops.push(Op::Bind {
                source: target.clone(),
                dest: target,
                writable: false,
            });
        }
        self
    }

    pub fn ro_bind(mut self, source: impl Into<PathBuf>, dest: impl Into<PathBuf>) -> Self {
        self.ops.push(Op::Bind {
            source: source.into(),
            dest: dest.into(),
            writable: false,
        });
        self
    }

    pub fn bind(mut self, source: impl Into<PathBuf>, dest: impl Into<PathBuf>) -> Self {
        self.ops.push(Op::Bind {
            source: source.into(),
            dest: dest.into(),
            writable: true,
        });
        self
    }

    pub fn chdir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.chdir = Some(dir.into());
        self
    }

    /// The full argv: bubblewrap, its options, then `command`.
    pub fn argv(&self, command: &[OsString]) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec![self.bwrap.clone().into()];
        let mut push = |items: &[&str]| argv.extend(items.iter().map(OsString::from));
        // --unshare-all only tries the user and cgroup namespaces; the user and PID
        // namespaces are required, so they are named again.
        push(&["--unshare-all", "--unshare-user", "--unshare-pid"]);
        if self.share_net {
            push(&["--share-net"]);
        }
        push(&["--die-with-parent", "--new-session", "--clearenv"]);
        for (key, value) in &self.env {
            argv.extend(["--setenv".into(), key.into(), value.into()]);
        }
        for op in &self.ops {
            match op {
                Op::Proc => argv.extend(["--proc".into(), "/proc".into()]),
                Op::Dev => argv.extend(["--dev".into(), "/dev".into()]),
                Op::Tmpfs(dest) => argv.extend(["--tmpfs".into(), dest.into()]),
                Op::Bind {
                    source,
                    dest,
                    writable,
                } => {
                    let flag = if *writable { "--bind" } else { "--ro-bind" };
                    argv.extend([flag.into(), source.into(), dest.into()]);
                }
                Op::Symlink { target, link } => {
                    argv.extend(["--symlink".into(), target.into(), link.into()]);
                }
            }
        }
        if let Some(dir) = &self.chdir {
            argv.extend(["--chdir".into(), dir.into()]);
        }
        argv.extend(command.iter().cloned());
        argv
    }

    /// The mounts this sandbox creates, for the probe's mount check.
    pub fn declared_mounts(&self) -> Vec<DeclaredMount> {
        let any = |path: PathBuf, optional| DeclaredMount {
            mount_point: path,
            access: Access::Any,
            optional,
        };
        let mut mounts = vec![any("/".into(), false)];
        for op in &self.ops {
            match op {
                Op::Proc => {
                    mounts.push(any("/proc".into(), false));
                    mounts.extend(
                        PROC_COVERS
                            .iter()
                            .map(|name| any(Path::new("/proc").join(name), true)),
                    );
                }
                Op::Dev => {
                    mounts.push(any("/dev".into(), false));
                    mounts.extend(
                        DEV_NODES
                            .iter()
                            .map(|name| any(Path::new("/dev").join(name), false)),
                    );
                }
                Op::Tmpfs(dest) => mounts.push(any(dest.clone(), false)),
                Op::Bind { dest, writable, .. } => mounts.push(DeclaredMount {
                    mount_point: dest.clone(),
                    access: if *writable {
                        Access::Writable
                    } else {
                        Access::ReadOnly
                    },
                    optional: false,
                }),
                Op::Symlink { .. } => {}
            }
        }
        mounts
    }

    /// The names of the variables the sandboxed process starts with. bubblewrap adds
    /// `PWD` when it changes directory.
    pub fn declared_env(&self) -> Vec<String> {
        let mut names: Vec<String> = self.env.iter().map(|(key, _)| key.clone()).collect();
        if self.chdir.is_some() {
            names.push("PWD".into());
        }
        names.sort();
        names.dedup();
        names
    }
}

fn resolver_outside_bound_trees() -> Option<PathBuf> {
    let link = Path::new("/etc/resolv.conf");
    if !fs::symlink_metadata(link).ok()?.file_type().is_symlink() {
        return None;
    }
    let target = fs::canonicalize(link).ok()?;
    if target.starts_with("/usr") || target.starts_with("/etc") || !target.is_file() {
        return None;
    }
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn requires_user_and_pid_namespaces_and_clears_the_environment() {
        let argv = strings(&Sandbox::new("/usr/bin/bwrap").argv(&["/probe".into()]));
        assert_eq!(
            argv,
            [
                "/usr/bin/bwrap",
                "--unshare-all",
                "--unshare-user",
                "--unshare-pid",
                "--die-with-parent",
                "--new-session",
                "--clearenv",
                "/probe"
            ]
        );
    }

    #[test]
    fn shares_the_network_only_when_asked() {
        let argv = strings(&Sandbox::new("bwrap").share_net().argv(&[]));
        assert!(argv.contains(&"--share-net".to_string()));
    }

    #[test]
    fn declares_every_bind_with_its_access() {
        let sandbox = Sandbox::new("bwrap")
            .ro_bind("/opt/h", "/opt/h")
            .bind("/runs/1/clone", "/runs/1/clone")
            .chdir("/runs/1/clone")
            .setenv("HOME", "/runs/1/home");
        let mounts = sandbox.declared_mounts();
        assert!(mounts.contains(&DeclaredMount {
            mount_point: "/opt/h".into(),
            access: Access::ReadOnly,
            optional: false
        }));
        assert!(mounts.contains(&DeclaredMount {
            mount_point: "/runs/1/clone".into(),
            access: Access::Writable,
            optional: false
        }));
        assert_eq!(sandbox.declared_env(), ["HOME", "PWD"]);
    }
}
