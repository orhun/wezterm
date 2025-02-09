use crate::auth::*;
use crate::config::ConfigMap;
use crate::host::*;
use crate::pty::*;
use anyhow::{anyhow, Context};
use filedescriptor::{
    poll, pollfd, socketpair, AsRawSocketDescriptor, FileDescriptor, POLLIN, POLLOUT,
};
use portable_pty::{ExitStatus, PtySize};
use smol::channel::{bounded, Receiver, Sender, TryRecvError};
use ssh2::BlockDirections;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug)]
pub enum SessionEvent {
    Banner(Option<String>),
    HostVerify(HostVerificationEvent),
    Authenticate(AuthenticationEvent),
    Error(String),
    Authenticated,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionSender {
    pub tx: Sender<SessionRequest>,
    pub pipe: Arc<Mutex<FileDescriptor>>,
}

impl SessionSender {
    fn post_send(&self) {
        let mut pipe = self.pipe.lock().unwrap();
        let _ = pipe.write(b"x");
    }

    pub fn try_send(&self, event: SessionRequest) -> anyhow::Result<()> {
        self.tx.try_send(event)?;
        self.post_send();
        Ok(())
    }

    pub async fn send(&self, event: SessionRequest) -> anyhow::Result<()> {
        self.tx.send(event).await?;
        self.post_send();
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum SessionRequest {
    NewPty(NewPty),
    ResizePty(ResizePty),
    Exec(Exec),
}

#[derive(Debug)]
pub(crate) struct Exec {
    pub command_line: String,
    pub env: Option<HashMap<String, String>>,
    pub reply: Sender<ExecResult>,
}

pub(crate) struct DescriptorState {
    pub fd: Option<FileDescriptor>,
    pub buf: VecDeque<u8>,
}

pub(crate) struct ChannelInfo {
    pub channel_id: ChannelId,
    pub channel: ssh2::Channel,
    pub exit: Option<Sender<ExitStatus>>,
    pub descriptors: [DescriptorState; 3],
}

pub(crate) type ChannelId = usize;

pub(crate) struct SessionInner {
    pub config: ConfigMap,
    pub tx_event: Sender<SessionEvent>,
    pub rx_req: Receiver<SessionRequest>,
    pub channels: HashMap<ChannelId, ChannelInfo>,
    pub next_channel_id: ChannelId,
    pub sender_read: FileDescriptor,
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        log::trace!("Dropping SessionInner");
    }
}

impl SessionInner {
    fn run(&mut self) {
        if let Err(err) = self.run_impl() {
            self.tx_event
                .try_send(SessionEvent::Error(format!("{:#}", err)))
                .ok();
        }
    }

    fn run_impl(&mut self) -> anyhow::Result<()> {
        let hostname = self
            .config
            .get("hostname")
            .ok_or_else(|| anyhow!("hostname not present in config"))?
            .to_string();
        let user = self
            .config
            .get("user")
            .ok_or_else(|| anyhow!("username not present in config"))?
            .to_string();
        let port = self.config.get("port").unwrap().parse::<u16>()?;
        let remote_address = format!("{}:{}", hostname, port);

        let tcp: TcpStream = if let Some(proxy_command) =
            self.config.get("proxycommand").and_then(|c| {
                if !c.is_empty() && c != "none" {
                    Some(c)
                } else {
                    None
                }
            }) {
            let mut cmd;
            if cfg!(windows) {
                let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd".to_string());
                cmd = std::process::Command::new(comspec);
                cmd.args(&["/c", proxy_command]);
            } else {
                cmd = std::process::Command::new("sh");
                cmd.args(&["-c", &format!("exec {}", proxy_command)]);
            }

            let (a, b) = socketpair()?;

            cmd.stdin(b.as_stdio()?);
            cmd.stdout(b.as_stdio()?);
            cmd.stderr(std::process::Stdio::inherit());
            let _child = cmd
                .spawn()
                .with_context(|| format!("spawning ProxyCommand {}", proxy_command))?;

            #[cfg(unix)]
            unsafe {
                use std::os::unix::io::{FromRawFd, IntoRawFd};
                TcpStream::from_raw_fd(a.into_raw_fd())
            }
            #[cfg(windows)]
            unsafe {
                use std::os::windows::io::{FromRawSocket, IntoRawSocket};
                TcpStream::from_raw_socket(a.into_raw_socket())
            }
        } else {
            let socket = TcpStream::connect((hostname.as_str(), port))
                .with_context(|| format!("connecting to {}", remote_address))?;
            socket
                .set_nodelay(true)
                .context("setting TCP NODELAY on ssh connection")?;
            socket
        };

        let mut sess = ssh2::Session::new()?;
        // sess.trace(ssh2::TraceFlags::all());
        sess.set_blocking(true);
        sess.set_tcp_stream(tcp);
        sess.handshake()
            .with_context(|| format!("ssh handshake with {}", remote_address))?;

        self.tx_event
            .try_send(SessionEvent::Banner(sess.banner().map(|s| s.to_string())))
            .context("notifying user of banner")?;

        self.host_verification(&sess, &hostname, port, &remote_address)
            .context("host verification")?;

        self.authenticate(&sess, &user, &hostname)
            .context("authentication")?;

        self.tx_event
            .try_send(SessionEvent::Authenticated)
            .context("notifying user that session is authenticated")?;

        sess.set_blocking(false);
        self.request_loop(sess)
    }

    fn request_loop(&mut self, sess: ssh2::Session) -> anyhow::Result<()> {
        let mut sleep_delay = Duration::from_millis(100);

        loop {
            self.tick_io()?;
            self.drain_request_pipe();
            self.dispatch_pending_requests(&sess)?;

            let mut poll_array = vec![
                pollfd {
                    fd: self.sender_read.as_socket_descriptor(),
                    events: POLLIN,
                    revents: 0,
                },
                pollfd {
                    fd: sess.as_socket_descriptor(),
                    events: match sess.block_directions() {
                        BlockDirections::None => 0,
                        BlockDirections::Inbound => POLLIN,
                        BlockDirections::Outbound => POLLOUT,
                        BlockDirections::Both => POLLIN | POLLOUT,
                    },
                    revents: 0,
                },
            ];
            let mut mapping = vec![];

            for info in self.channels.values() {
                for (fd_num, state) in info.descriptors.iter().enumerate() {
                    if let Some(fd) = state.fd.as_ref() {
                        poll_array.push(pollfd {
                            fd: fd.as_socket_descriptor(),
                            events: if fd_num == 0 {
                                POLLIN
                            } else if !state.buf.is_empty() {
                                POLLOUT
                            } else {
                                0
                            },
                            revents: 0,
                        });
                        mapping.push((info.channel_id, fd_num));
                    }
                }
            }

            poll(&mut poll_array, Some(sleep_delay)).context("poll")?;
            sleep_delay += sleep_delay;

            for (idx, poll) in poll_array.iter().enumerate() {
                if poll.revents != 0 {
                    sleep_delay = Duration::from_millis(100);
                }
                if idx == 0 || idx == 1 {
                    // Dealt with at the top of the loop
                } else if poll.revents != 0 {
                    let (channel_id, fd_num) = mapping[idx - 2];
                    let info = self.channels.get_mut(&channel_id).unwrap();
                    let state = &mut info.descriptors[fd_num];
                    let fd = state.fd.as_mut().unwrap();

                    if fd_num == 0 {
                        // There's data we can read into the buffer
                        match read_into_buf(fd, &mut state.buf) {
                            Ok(_) => {}
                            Err(err) => {
                                log::debug!("error reading from stdin pipe: {:#}", err);
                                let _ = info.channel.close();
                                state.fd.take();
                            }
                        }
                    } else {
                        // We can write our buffered output
                        match write_from_buf(fd, &mut state.buf) {
                            Ok(_) => {}
                            Err(err) => {
                                log::debug!(
                                    "error while writing to channel {} fd {}: {:#}",
                                    channel_id,
                                    fd_num,
                                    err
                                );

                                // Close it out
                                state.fd.take();
                            }
                        }
                    }
                }
            }
        }
    }

    /// Goal: if we have data to write to channels, try to send it.
    /// If we have room in our channel fd write buffers, try to fill it
    fn tick_io(&mut self) -> anyhow::Result<()> {
        for chan in self.channels.values_mut() {
            if chan.exit.is_some() {
                if chan.channel.eof() && chan.channel.wait_close().is_ok() {
                    fn has_signal(chan: &ssh2::Channel) -> Option<ssh2::ExitSignal> {
                        if let Ok(sig) = chan.exit_signal() {
                            if sig.exit_signal.is_some() {
                                return Some(sig);
                            }
                        }
                        None
                    }

                    let status = if let Some(_sig) = has_signal(&chan.channel) {
                        Some(ExitStatus::with_exit_code(1))
                    } else if let Ok(status) = chan.channel.exit_status() {
                        Some(ExitStatus::with_exit_code(status as _))
                    } else {
                        None
                    };

                    if let Some(status) = status {
                        let exit = chan.exit.take().unwrap();
                        smol::block_on(exit.send(status)).ok();
                    }
                }
            }

            let stdin = &mut chan.descriptors[0];
            if stdin.fd.is_some() && !stdin.buf.is_empty() {
                match write_from_buf(&mut chan.channel, &mut stdin.buf) {
                    Ok(_) => {}
                    Err(err) => {
                        log::error!("Failed to write data to channel: {:#}. Now what?", err);
                    }
                }
            }

            for (idx, out) in chan
                .descriptors
                .get_mut(1..)
                .unwrap()
                .iter_mut()
                .enumerate()
            {
                if out.fd.is_none() {
                    continue;
                }
                let current_len = out.buf.len();
                let room = out.buf.capacity() - current_len;
                if room == 0 {
                    continue;
                }
                match read_into_buf(&mut chan.channel.stream(idx as i32), &mut out.buf) {
                    Ok(_) => {}
                    Err(err) => {
                        if out.buf.is_empty() {
                            log::trace!(
                                "Failed to read data from channel: {:#}, closing pipe",
                                err
                            );
                            out.fd.take();
                        } else {
                            log::trace!("Failed to read data from channel: {:#}, but still have some buffer to drain", err);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn drain_request_pipe(&mut self) {
        let mut buf = [0u8; 16];
        let _ = self.sender_read.read(&mut buf);
    }

    fn dispatch_pending_requests(&mut self, sess: &ssh2::Session) -> anyhow::Result<()> {
        while self.dispatch_one_request(sess)? {}
        Ok(())
    }

    fn dispatch_one_request(&mut self, sess: &ssh2::Session) -> anyhow::Result<bool> {
        match self.rx_req.try_recv() {
            Err(TryRecvError::Closed) => anyhow::bail!("all clients are closed"),
            Err(TryRecvError::Empty) => Ok(false),
            Ok(req) => {
                sess.set_blocking(true);
                let res = match req {
                    SessionRequest::NewPty(newpty) => {
                        if let Err(err) = self.new_pty(&sess, &newpty) {
                            log::error!("{:?} -> error: {:#}", newpty, err);
                        }
                        Ok(true)
                    }
                    SessionRequest::ResizePty(resize) => {
                        if let Err(err) = self.resize_pty(&sess, &resize) {
                            log::error!("{:?} -> error: {:#}", resize, err);
                        }
                        Ok(true)
                    }
                    SessionRequest::Exec(exec) => {
                        if let Err(err) = self.exec(&sess, &exec) {
                            log::error!("{:?} -> error: {:#}", exec, err);
                        }
                        Ok(true)
                    }
                };
                sess.set_blocking(false);
                res
            }
        }
    }

    pub fn exec(&mut self, sess: &ssh2::Session, exec: &Exec) -> anyhow::Result<()> {
        sess.set_blocking(true);

        let mut channel = sess.channel_session()?;

        if let Some(env) = &exec.env {
            for (key, val) in env {
                if let Err(err) = channel.setenv(key, val) {
                    // Depending on the server configuration, a given
                    // setenv request may not succeed, but that doesn't
                    // prevent the connection from being set up.
                    log::warn!("ssh: setenv {}={} failed: {}", key, val, err);
                }
            }
        }

        channel.exec(&exec.command_line)?;

        let channel_id = self.next_channel_id;
        self.next_channel_id += 1;

        let (write_to_stdin, mut read_from_stdin) = socketpair()?;
        let (mut write_to_stdout, read_from_stdout) = socketpair()?;
        let (mut write_to_stderr, read_from_stderr) = socketpair()?;

        read_from_stdin.set_non_blocking(true)?;
        write_to_stdout.set_non_blocking(true)?;
        write_to_stderr.set_non_blocking(true)?;

        let (exit_tx, exit_rx) = bounded(1);

        let child = SshChildProcess {
            channel: channel_id,
            tx: None,
            exit: exit_rx,
            exited: None,
        };

        let result = ExecResult {
            stdin: write_to_stdin,
            stdout: read_from_stdout,
            stderr: read_from_stderr,
            child,
        };

        let info = ChannelInfo {
            channel_id,
            channel,
            exit: Some(exit_tx),
            descriptors: [
                DescriptorState {
                    fd: Some(read_from_stdin),
                    buf: VecDeque::with_capacity(8192),
                },
                DescriptorState {
                    fd: Some(write_to_stdout),
                    buf: VecDeque::with_capacity(8192),
                },
                DescriptorState {
                    fd: Some(write_to_stderr),
                    buf: VecDeque::with_capacity(8192),
                },
            ],
        };

        exec.reply.try_send(result)?;
        self.channels.insert(channel_id, info);

        Ok(())
    }
}

#[derive(Clone)]
pub struct Session {
    tx: SessionSender,
}

impl Drop for Session {
    fn drop(&mut self) {
        log::trace!("Drop Session");
    }
}

impl Session {
    pub fn connect(config: ConfigMap) -> anyhow::Result<(Self, Receiver<SessionEvent>)> {
        let (tx_event, rx_event) = bounded(8);
        let (tx_req, rx_req) = bounded(8);
        let (mut sender_write, mut sender_read) = socketpair()?;
        sender_write.set_non_blocking(true)?;
        sender_read.set_non_blocking(true)?;

        let session_sender = SessionSender {
            tx: tx_req,
            pipe: Arc::new(Mutex::new(sender_write)),
        };

        let mut inner = SessionInner {
            config,
            tx_event,
            rx_req,
            channels: HashMap::new(),
            next_channel_id: 1,
            sender_read,
        };
        std::thread::spawn(move || inner.run());
        Ok((Self { tx: session_sender }, rx_event))
    }

    pub async fn request_pty(
        &self,
        term: &str,
        size: PtySize,
        command_line: Option<&str>,
        env: Option<HashMap<String, String>>,
    ) -> anyhow::Result<(SshPty, SshChildProcess)> {
        let (reply, rx) = bounded(1);
        self.tx
            .send(SessionRequest::NewPty(NewPty {
                term: term.to_string(),
                size,
                command_line: command_line.map(|s| s.to_string()),
                env,
                reply,
            }))
            .await?;
        let (mut ssh_pty, mut child) = rx.recv().await?;
        ssh_pty.tx.replace(self.tx.clone());
        child.tx.replace(self.tx.clone());
        Ok((ssh_pty, child))
    }

    pub async fn exec(
        &self,
        command_line: &str,
        env: Option<HashMap<String, String>>,
    ) -> anyhow::Result<ExecResult> {
        let (reply, rx) = bounded(1);
        self.tx
            .send(SessionRequest::Exec(Exec {
                command_line: command_line.to_string(),
                env,
                reply,
            }))
            .await?;
        let mut exec = rx.recv().await?;
        exec.child.tx.replace(self.tx.clone());
        Ok(exec)
    }
}

#[derive(Debug)]
pub struct ExecResult {
    pub stdin: FileDescriptor,
    pub stdout: FileDescriptor,
    pub stderr: FileDescriptor,
    pub child: SshChildProcess,
}

fn write_from_buf<W: Write>(w: &mut W, buf: &mut VecDeque<u8>) -> std::io::Result<()> {
    match w.write(buf.make_contiguous()) {
        Ok(len) => {
            buf.drain(0..len);
            Ok(())
        }
        Err(err) => {
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(());
            }
            Err(err)
        }
    }
}

fn read_into_buf<R: Read>(r: &mut R, buf: &mut VecDeque<u8>) -> std::io::Result<()> {
    let current_len = buf.len();
    buf.resize(buf.capacity(), 0);
    let target_buf = &mut buf.make_contiguous()[current_len..];
    match r.read(target_buf) {
        Ok(len) => {
            buf.resize(current_len + len, 0);
            if len == 0 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF",
                ))
            } else {
                Ok(())
            }
        }
        Err(err) => {
            buf.resize(current_len, 0);

            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(());
            }
            Err(err)
        }
    }
}
