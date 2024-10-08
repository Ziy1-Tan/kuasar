/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::{
    ffi::CString,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
    process::exit,
    str::FromStr,
};

use anyhow::anyhow;
use clap::Parser;
use containerd_shim::asynchronous::monitor::monitor_notify_by_pid;
use futures::StreamExt;
use log::{debug, error, warn, LevelFilter};
use nix::{
    errno::Errno,
    fcntl::{fcntl, FcntlArg, FdFlag, OFlag},
    libc,
    sched::{setns, unshare, CloneFlags},
    sys::{
        signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, SIGCHLD},
        stat::Mode,
        wait,
        wait::{WaitPidFlag, WaitStatus},
    },
    unistd::{fork, pause, pipe, read, write, ForkResult, Pid},
};
use prctl::PrctlMM;
use signal_hook_tokio::Signals;
use uuid::Uuid;

use crate::{
    sandbox::{RuncSandboxer, SandboxParent},
    task::fork_task_server,
};

mod args;
mod common;
mod runc;
mod sandbox;
mod task;
mod version;

fn main() {
    let args = args::Args::parse();
    if args.version {
        version::print_version_info();
        return;
    }

    // Update args log level if it not presents args but in config.
    let log_level =
        LevelFilter::from_str(&args.log_level.unwrap_or_default()).unwrap_or(LevelFilter::Info);
    env_logger::Builder::from_default_env()
        .format_timestamp_micros()
        .filter_module("containerd_sandbox", log_level)
        .filter_module("runc_sandboxer", log_level)
        .init();

    let sandbox_parent = fork_sandbox_parent().unwrap();

    let task_socket = format!("{}/task-{}.sock", &args.dir, Uuid::new_v4());
    fork_task_server(&task_socket, &args.dir).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async move {
        start_sandboxer(sandbox_parent, task_socket, &args.listen, &args.dir)
            .await
            .unwrap();
    });
}

// Call it instantly after enter the main function,
// it will fork a process and make it as the parent of all the sandbox processes.
// any time we want to fork a sandbox process, we will send a message to this parent process
// and this parent will fork a process for sandbox and return the pid.
fn fork_sandbox_parent() -> Result<SandboxParent, anyhow::Error> {
    let (reqr, reqw) = pipe().map_err(|e| anyhow!("failed to create pipe {}", e))?;
    let (respr, respw) = pipe().map_err(|e| anyhow!("failed to create pipe {}", e))?;

    match unsafe { fork().map_err(|e| anyhow!("failed to fork sandbox parent {}", e))? } {
        ForkResult::Parent { child } => {
            debug!("forked process {} for the sandbox parent", child);
            drop(reqr);
            drop(respw);
        }
        ForkResult::Child => {
            drop(reqw);
            drop(respr);
            prctl::set_child_subreaper(true).unwrap();
            let comm = "[sandbox-parent]";
            let comm_cstr = CString::new(comm).unwrap();
            let addr = comm_cstr.as_ptr();
            set_process_comm(addr as u64, comm_cstr.as_bytes_with_nul().len() as u64);
            let sig_action = SigAction::new(
                SigHandler::Handler(sandbox_parent_handle_signals),
                SaFlags::empty(),
                SigSet::empty(),
            );
            unsafe {
                sigaction(SIGCHLD, &sig_action).unwrap();
            }
            loop {
                let buffer = read_count(reqr.as_raw_fd(), 512).unwrap();
                let id = String::from_utf8_lossy(&buffer[0..64]).to_string();
                let mut zero_index = 64;
                for (i, &b) in buffer.iter().enumerate().take(512).skip(64) {
                    if b == 0 {
                        zero_index = i;
                        break;
                    }
                }
                let netns = String::from_utf8_lossy(&buffer[64..zero_index]).to_string();
                let sandbox_pid = fork_sandbox(&id, &netns).unwrap();
                write_all(&respw, sandbox_pid.to_le_bytes().as_slice()).unwrap();
            }
        }
    }
    fcntl(reqw.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).unwrap_or_default();
    fcntl(respr.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).unwrap_or_default();
    Ok(SandboxParent::new(reqw, respr))
}

pub fn read_count(fd: RawFd, count: usize) -> Result<Vec<u8>, anyhow::Error> {
    let mut buf = vec![0u8; count];
    let mut idx = 0;
    loop {
        let l = match read(fd, &mut buf[idx..]) {
            Ok(l) => l,
            Err(e) => {
                if e == Errno::EINTR {
                    continue;
                } else {
                    return Err(anyhow!("failed to read from pipe {}", e));
                }
            }
        };
        idx += l;
        if idx == count || l == 0 {
            return Ok(buf);
        }
    }
}

pub fn write_all(fd: &OwnedFd, buf: &[u8]) -> Result<(), anyhow::Error> {
    let mut idx = 0;
    let count = buf.len();
    loop {
        let l = match write(fd, &buf[idx..]) {
            Ok(l) => l,
            Err(e) => {
                if e == Errno::EINTR {
                    continue;
                } else {
                    return Err(anyhow!("failed to write to pipe {}", e));
                }
            }
        };
        idx += l;
        if idx == count {
            return Ok(());
        }
    }
}

fn fork_sandbox(id: &str, netns: &str) -> Result<i32, anyhow::Error> {
    let (r, w) = pipe().map_err(|e| anyhow!("failed to create pipe {}", e))?;
    match unsafe { fork().map_err(|e| anyhow!("failed to fork sandbox {}", e))? } {
        ForkResult::Parent { child } => {
            debug!("forked process {} for the sandbox {}", child, id);
            drop(w);
            let mut resp = [0u8; 4];
            let r = read_count(r.as_raw_fd(), 4)?;
            resp[..].copy_from_slice(r.as_slice());
            let pid = i32::from_le_bytes(resp);
            Ok(pid)
        }
        ForkResult::Child => {
            drop(r);
            unshare(CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWPID)
                .unwrap();
            match unsafe { fork().unwrap() } {
                ForkResult::Parent { child } => {
                    debug!("forked process {} for the sandbox {}", child, id);
                    write_all(&w, child.as_raw().to_le_bytes().as_slice()).unwrap();
                    exit(0);
                }
                ForkResult::Child => {
                    let comm = format!("[sandbox-{}]", id);
                    let comm_cstr = CString::new(comm).unwrap();
                    let addr = comm_cstr.as_ptr();
                    set_process_comm(addr as u64, comm_cstr.as_bytes_with_nul().len() as u64);
                    if !netns.is_empty() {
                        let netns_fd =
                            safe_open_file(Path::new(&netns), OFlag::O_CLOEXEC, Mode::empty())
                                .unwrap();
                        setns(netns_fd, CloneFlags::CLONE_NEWNET).unwrap();
                    }
                    loop {
                        pause();
                    }
                }
            }
        }
    }
}

pub fn safe_open_file<P: ?Sized + nix::NixPath>(
    path: &P,
    oflag: OFlag,
    mode: Mode,
) -> Result<OwnedFd, nix::Error> {
    let fd = nix::fcntl::open(path, oflag, mode)?;
    // SAFETY: contruct a OwnedFd from RawFd, close fd when OwnedFd drop
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn set_process_comm(addr: u64, len: u64) {
    if prctl::set_mm(PrctlMM::PR_SET_MM_ARG_START, addr).is_err() {
        prctl::set_mm(PrctlMM::PR_SET_MM_ARG_END, addr + len).unwrap();
        prctl::set_mm(PrctlMM::PR_SET_MM_ARG_START, addr).unwrap()
    } else {
        prctl::set_mm(PrctlMM::PR_SET_MM_ARG_END, addr + len).unwrap();
    }
}

extern "C" fn sandbox_parent_handle_signals(_: libc::c_int) {
    loop {
        match wait::waitpid(Some(Pid::from_raw(-1)), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, status)) => {
                debug!("child {} exit ({})", pid, status);
            }
            Ok(WaitStatus::Signaled(pid, sig, _)) => {
                debug!("child {} terminated({})", pid, sig);
            }
            Err(Errno::ECHILD) | Ok(WaitStatus::StillAlive) => {
                break;
            }
            Err(e) => {
                warn!("error occurred in signal handler: {}", e);
            }
            _ => {}
        }
    }
}

async fn start_sandboxer(
    sandbox_parent: SandboxParent,
    task_socket: String,
    listen: &str,
    dir: &str,
) -> anyhow::Result<()> {
    let task_address = format!("unix://{}", task_socket);
    let sandboxer = RuncSandboxer::new(sandbox_parent, &task_address).await?;
    sandboxer.recover(dir).await?;
    containerd_sandbox::run("kuasar-runc-sandboxer", listen, dir, sandboxer).await?;
    Ok(())
}

async fn handle_signals(signals: Signals) {
    let mut signals = signals.fuse();
    while let Some(sig) = signals.next().await {
        match sig {
            libc::SIGTERM | libc::SIGINT => {
                debug!("received {}", sig);
            }
            libc::SIGCHLD => loop {
                // Note: see comment at the counterpart in synchronous/mod.rs for details.
                match wait::waitpid(Some(Pid::from_raw(-1)), Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::Exited(pid, status)) => {
                        debug!("child {} exit ({})", pid, status);
                        monitor_notify_by_pid(pid.as_raw(), status)
                            .await
                            .unwrap_or_else(|e| error!("failed to send exit event {}", e))
                    }
                    Ok(WaitStatus::Signaled(pid, sig, _)) => {
                        debug!("child {} terminated({})", pid, sig);
                        let exit_code = 128 + sig as i32;
                        monitor_notify_by_pid(pid.as_raw(), exit_code)
                            .await
                            .unwrap_or_else(|e| error!("failed to send signal event {}", e))
                    }
                    Err(Errno::ECHILD) | Ok(WaitStatus::StillAlive) => {
                        break;
                    }
                    Err(e) => {
                        warn!("error occurred in signal handler: {}", e);
                    }
                    _ => {}
                }
            },
            _ => {
                if let Ok(sig) = nix::sys::signal::Signal::try_from(sig) {
                    debug!("received {}", sig);
                } else {
                    warn!("received invalid signal {}", sig);
                }
            }
        }
    }
}
