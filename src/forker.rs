mod clone3;

use crate::async_engine::{AsyncFd, SpawnedFuture};
use crate::forker::clone3::{fork_with_pidfd, Forked};
use crate::utils::buffd::{BufFdIn, BufFdOut};
use crate::utils::copyhashmap::CopyHashMap;
use crate::utils::vec_ext::VecExt;
use crate::{AsyncEngine, AsyncQueue, ErrorFmt, EventLoop, State, Wheel};
use bincode::{Decode, Encode};
use i4config::_private::bincode_ops;
use log::Level;
use parking_lot::Mutex;
use std::cell::Cell;
use std::ffi::OsStr;
use std::io::Read;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::rc::Rc;
use thiserror::Error;
use uapi::{c, pipe2, IntoUstr, OwnedFd, UstrPtr};

pub struct ForkerProxy {
    pidfd: Rc<OwnedFd>,
    pid: c::pid_t,
    socket: Rc<OwnedFd>,
    task_in: Cell<Option<SpawnedFuture<()>>>,
    task_out: Cell<Option<SpawnedFuture<()>>>,
    task_proc: Cell<Option<SpawnedFuture<()>>>,
    outgoing: AsyncQueue<ServerMessage>,
}

#[derive(Debug, Error)]
pub enum ForkerError {
    #[error("Could not create a socketpair")]
    Socketpair(#[source] std::io::Error),
    #[error("Could not fork")]
    Fork(#[source] std::io::Error),
}

impl ForkerProxy {
    pub fn create() -> Result<Self, ForkerError> {
        let (parent, child) = match uapi::socketpair(
            c::AF_UNIX,
            c::SOCK_STREAM | c::SOCK_CLOEXEC | c::SOCK_NONBLOCK,
            0,
        ) {
            Ok(o) => o,
            Err(e) => return Err(ForkerError::Socketpair(e.into())),
        };
        match fork_with_pidfd(false)? {
            Forked::Parent { pid, pidfd } => Ok(ForkerProxy {
                pidfd: Rc::new(pidfd),
                pid,
                socket: Rc::new(parent),
                task_in: Cell::new(None),
                task_out: Cell::new(None),
                task_proc: Cell::new(None),
                outgoing: Default::default(),
            }),
            Forked::Child { .. } => Forker::handle(child),
        }
    }

    pub fn install(self: &Rc<Self>, state: &Rc<State>) {
        state.forker.set(Some(self.clone()));
        let socket = state.eng.fd(&self.socket).unwrap();
        self.task_proc.set(Some(
            state.eng.spawn(self.clone().check_process(state.clone())),
        ));
        self.task_in
            .set(Some(state.eng.spawn(self.clone().incoming(socket.clone()))));
        self.task_out.set(Some(
            state
                .eng
                .spawn(self.clone().outgoing(state.clone(), socket.clone())),
        ));
    }

    pub fn setenv(&self, key: &[u8], val: &[u8]) {
        self.outgoing.push(ServerMessage::SetEnv {
            var: key.to_vec(),
            val: val.to_vec(),
        })
    }

    pub fn spawn(&self, prog: String, args: Vec<String>, env: Vec<(String, String)>) {
        self.outgoing.push(ServerMessage::Spawn {
            prog,
            args,
            env,
        })
    }

    async fn incoming(self: Rc<Self>, socket: AsyncFd) {
        let mut buffd = BufFdIn::new(socket);
        let mut buf = vec![];
        loop {
            let mut len = 0usize;
            if let Err(e) = buffd.read_full(&mut len).await {
                log::error!("Cannot read from the ol' forker: {}", ErrorFmt(e));
                self.task_in.take();
                return;
            }
            buf.clear();
            buf.reserve(len);
            let space = buf.split_at_spare_mut_ext().1;
            buffd.read_full(&mut space[..len]).await.unwrap();
            unsafe {
                buf.set_len(len);
            }
            let (msg, _) =
                bincode::decode_from_slice::<ForkerMessage, _>(&buf, bincode_ops()).unwrap();
            self.handle_msg(msg);
        }
    }

    fn handle_msg(&self, msg: ForkerMessage) {
        match msg {
            ForkerMessage::Log { level, msg } => self.handle_log(level, &msg),
        }
    }

    fn handle_log(&self, level: usize, msg: &str) {
        let level = match level {
            1 => Level::Error,
            2 => Level::Warn,
            3 => Level::Info,
            4 => Level::Debug,
            5 => Level::Trace,
            _ => Level::Error,
        };
        log::log!(level, "{}", msg);
    }

    async fn outgoing(self: Rc<Self>, state: Rc<State>, socket: AsyncFd) {
        let mut buffd = BufFdOut::new(socket);
        let mut buf = vec![];
        let mut fds = vec![];
        loop {
            let msg = self.outgoing.pop().await;
            buf.clear();
            buf.extend_from_slice(uapi::as_bytes(&0usize));
            let len = bincode::encode_into_std_write(&msg, &mut buf, bincode_ops()).unwrap();
            let _ = (&mut buf[..]).write_all(uapi::as_bytes(&len));
            if let Err(e) = buffd.flush2(&buf, &mut fds).await {
                log::error!("Could not write to the ol' forker: {}", ErrorFmt(e));
                state.forker.set(None);
                self.task_out.take();
                return;
            }
        }
    }

    async fn check_process(self: Rc<Self>, state: Rc<State>) {
        let pidfd = state.eng.fd(&self.pidfd).unwrap();
        let _ = pidfd.readable().await;
        let _ = uapi::waitpid(self.pid, 0);
        log::error!("The ol' forker died. Cannot spawn further processes.");
        state.forker.set(None);
        self.task_out.take();
        self.task_proc.take();
    }
}

#[derive(Encode, Decode)]
enum ServerMessage {
    SetEnv { var: Vec<u8>, val: Vec<u8> },
    Spawn { prog: String, args: Vec<String>, env: Vec<(String, String)> },
}

#[derive(Encode, Decode)]
enum ForkerMessage {
    Log { level: usize, msg: String },
}

struct Forker {
    socket: AsyncFd,
    ae: Rc<AsyncEngine>,
    outgoing: AsyncQueue<ForkerMessage>,
    pending_spawns: CopyHashMap<c::pid_t, SpawnedFuture<()>>,
}

impl Forker {
    fn handle(mut socket: OwnedFd) -> ! {
        std::env::set_var("XDG_SESSION_TYPE", "wayland");
        std::env::remove_var("DISPLAY");
        std::env::remove_var("WAYLAND_DISPLAY");
        setup_deathsig();
        reset_signals();
        socket = setup_fds(socket);
        std::panic::set_hook({
            let socket = Mutex::new(uapi::fcntl_dupfd_cloexec(socket.raw(), 0).unwrap());
            Box::new(move |pi| {
                let msg = ForkerMessage::Log {
                    level: log::Level::Error as _,
                    msg: format!("The ol' forker panicked: {}", pi),
                };
                let msg = bincode::encode_to_vec(&msg, bincode_ops()).unwrap();
                let _ = socket.lock().write_all(&msg);
            })
        });
        let el = EventLoop::new().unwrap();
        let wheel = Wheel::install(&el).unwrap();
        let ae = AsyncEngine::install(&el, &wheel).unwrap();
        let forker = Rc::new(Forker {
            socket: ae.fd(&Rc::new(socket)).unwrap(),
            ae: ae.clone(),
            outgoing: Default::default(),
            pending_spawns: Default::default(),
        });
        let _f1 = ae.spawn(forker.clone().incoming());
        let _f2 = ae.spawn(forker.clone().outgoing());
        let _ = el.run();
        unreachable!();
    }

    async fn outgoing(self: Rc<Self>) {
        let mut buffd = BufFdOut::new(self.socket.clone());
        let mut buf = vec![];
        let mut fds = vec![];
        loop {
            let msg = self.outgoing.pop().await;
            buf.clear();
            buf.extend_from_slice(uapi::as_bytes(&0usize));
            let len = bincode::encode_into_std_write(&msg, &mut buf, bincode_ops()).unwrap();
            let _ = (&mut buf[..]).write_all(uapi::as_bytes(&len));
            buffd.flush2(&buf, &mut fds).await.unwrap();
        }
    }

    async fn incoming(self: Rc<Self>) {
        let mut buffd = BufFdIn::new(self.socket.clone());
        let mut buf = vec![];
        loop {
            let mut len = 0usize;
            buffd.read_full(&mut len).await.unwrap();
            buf.clear();
            buf.reserve(len);
            let space = buf.split_at_spare_mut_ext().1;
            buffd.read_full(&mut space[..len]).await.unwrap();
            unsafe {
                buf.set_len(len);
            }
            let (msg, _) =
                bincode::decode_from_slice::<ServerMessage, _>(&buf, bincode_ops()).unwrap();
            self.handle_msg(msg);
        }
    }

    fn handle_msg(self: &Rc<Self>, msg: ServerMessage) {
        match msg {
            ServerMessage::SetEnv { var, val } => self.handle_set_env(&var, &val),
            ServerMessage::Spawn { prog, args, env } => self.handle_spawn(prog, args, env),
        }
    }

    fn handle_set_env(self: &Rc<Self>, var: &[u8], val: &[u8]) {
        std::env::set_var(OsStr::from_bytes(var), OsStr::from_bytes(val));
    }

    fn handle_spawn(self: &Rc<Self>, prog: String, args: Vec<String>, env: Vec<(String, String)>) {
        let (mut read, mut write) = pipe2(c::O_CLOEXEC).unwrap();
        let res = match fork_with_pidfd(false) {
            Ok(o) => o,
            Err(e) => {
                self.outgoing.push(ForkerMessage::Log {
                    level: log::Level::Error as usize,
                    msg: ErrorFmt(e).to_string(),
                });
                return;
            }
        };
        match res {
            Forked::Parent { pidfd, pid } => {
                drop(write);
                let slf = self.clone();
                let spawn = self.ae.spawn(async move {
                    let pidfd = slf.ae.fd(&Rc::new(pidfd)).unwrap();
                    let _ = pidfd.readable().await;
                    let mut s = String::new();
                    let _ = read.read_to_string(&mut s);
                    if s.len() > 0 {
                        slf.outgoing.push(ForkerMessage::Log {
                            level: log::Level::Error as _,
                            msg: format!("Could not spawn `{}`: {}", prog, s),
                        });
                    }
                    slf.pending_spawns.remove(&pid);
                });
                self.pending_spawns.set(pid, spawn);
            }
            Forked::Child { .. } => {
                unsafe {
                    c::signal(c::SIGCHLD, c::SIG_DFL);
                }
                for (key, val) in env {
                    std::env::set_var(&key, &val);
                }
                let prog = prog.into_ustr();
                let mut argsnt = UstrPtr::new();
                argsnt.push(&prog);
                for arg in args {
                    argsnt.push(arg);
                }
                if let Err(e) = uapi::execvp(&prog, &argsnt) {
                    let _ = write.write_all(std::io::Error::from(e).to_string().as_bytes());
                }
                std::process::exit(1);
            }
        }
    }
}

fn setup_fds(mut socket: OwnedFd) -> OwnedFd {
    if socket.raw() != 0 {
        uapi::dup3(socket.unwrap(), 0, 0).unwrap();
        socket = OwnedFd::new(0);
    }
    uapi::close_range(1, c::c_uint::MAX, 0).unwrap();
    uapi::dup3(socket.raw(), 3, c::O_CLOEXEC).unwrap();
    socket = OwnedFd::new(3);
    let fd = uapi::open("/dev/null", c::O_RDWR, 0).unwrap().unwrap();
    assert!(fd == 0);
    uapi::dup2(0, 1).unwrap();
    uapi::dup2(0, 2).unwrap();
    socket
}

fn reset_signals() {
    const NSIG: c::c_int = 64;
    unsafe {
        for sig in 1..=NSIG {
            c::signal(sig, c::SIG_DFL);
        }
        c::signal(c::SIGCHLD, c::SIG_IGN);
    }
}

fn setup_deathsig() {
    unsafe {
        let res = c::prctl(c::PR_SET_PDEATHSIG, c::SIGKILL as c::c_ulong);
        uapi::map_err!(res).unwrap();
    }
}
