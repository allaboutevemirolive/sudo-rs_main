use std::{
    ffi::c_int,
    io::{self, Read, Write},
    os::unix::{net::UnixStream, process::CommandExt},
    process::{exit, Command},
};

use crate::{
    exec::{
        event::StopReason,
        signal_manager::SignalManager,
        use_pty::{SIGCONT_BG, SIGCONT_FG},
    },
    log::{dev_error, dev_info, dev_warn},
    system::poll::PollEvent,
};
use crate::{
    exec::{signal_manager::Signal, terminate_process},
    system::{
        fork, getpgid, getpgrp,
        interface::ProcessId,
        kill, setpgid, setsid,
        term::{PtyFollower, Terminal},
        wait::{Wait, WaitError, WaitOptions, WaitStatus},
        ForkResult,
    },
};

use signal_hook::consts::*;

use crate::exec::{
    event::{EventRegistry, Process},
    io_util::{retry_while_interrupted, was_interrupted},
    use_pty::backchannel::{MonitorBackchannel, MonitorMessage, ParentMessage},
};
use crate::exec::{opt_fmt, signal_fmt};

// FIXME: This should return `io::Result<!>` but `!` is not stable yet.
pub(super) fn exec_monitor(
    pty_follower: PtyFollower,
    command: Command,
    foreground: bool,
    backchannel: &mut MonitorBackchannel,
) -> io::Result<()> {
    let signal_manager = SignalManager::new()?;

    // FIXME (ogsudo): Any file descriptor not used by the monitor are closed here.

    // FIXME (ogsudo): SIGTTIN and SIGTTOU are ignored here but the docs state that it shouldn't
    // be possible to receive them in the first place. Investigate

    // Start a new terminal session with the monitor as the leader.
    setsid().map_err(|err| {
        dev_warn!("cannot start a new session: {err}");
        err
    })?;

    // Set the follower side of the pty as the controlling terminal for the session.
    pty_follower.make_controlling_terminal().map_err(|err| {
        dev_warn!("cannot set the controlling terminal: {err}");
        err
    })?;

    // Use a pipe to get the IO error if `exec_command` fails.
    let (mut errpipe_tx, errpipe_rx) = UnixStream::pair()?;

    // Wait for the parent to give us green light before spawning the command. This avoids race
    // conditions when the command exits quickly.
    let event = retry_while_interrupted(|| backchannel.recv()).map_err(|err| {
        dev_warn!("cannot receive green light from parent: {err}");
        err
    })?;
    // Given that `UnixStream` delivers messages in order it shouldn't be possible to
    // receive an event different to `ExecCommand` at the beginning.
    debug_assert_eq!(event, MonitorMessage::ExecCommand);

    // FIXME (ogsudo): Some extra config happens here if selinux is available.

    let ForkResult::Parent(command_pid) = fork().map_err(|err| {
        dev_warn!("unable to fork command process: {err}");
        err
    })? else {
        drop(errpipe_rx);

        let err = exec_command(command, foreground, pty_follower);
        dev_warn!("failed to execute command: {err}");
        // If `exec_command` returns, it means that executing the command failed. Send the error to
        // the monitor using the pipe.
        if let Some(error_code) = err.raw_os_error() {
            errpipe_tx.write_all(&error_code.to_ne_bytes()).ok();
        }
        drop(errpipe_tx);
        // FIXME: Calling `exit` doesn't run any destructors, clean everything up.
        exit(1)
    };

    // Send the command's PID to the parent.
    if let Err(err) = backchannel.send(&ParentMessage::CommandPid(command_pid)) {
        dev_warn!("cannot send command PID to parent: {err}");
    }

    let mut registry = EventRegistry::new();

    let mut closure = MonitorClosure::new(
        command_pid,
        pty_follower,
        errpipe_rx,
        backchannel,
        signal_manager,
        &mut registry,
    );

    // Set the foreground group for the pty follower.
    if foreground {
        if let Err(err) = closure.pty_follower.tcsetpgrp(closure.command_pgrp) {
            dev_error!(
                "cannot set foreground progess group to {} (command): {err}",
                closure.command_pgrp
            );
        }
    }

    // FIXME (ogsudo): Here's where the signal mask is removed because the handlers for the signals
    // have been setup after initializing the closure.

    // Start the event loop.
    let reason = registry.event_loop(&mut closure);

    // Terminate the command if it's not terminated.
    if let Some(command_pid) = closure.command_pid {
        terminate_process(command_pid, true);

        loop {
            match command_pid.wait(WaitOptions::new()) {
                Err(WaitError::Io(err)) if was_interrupted(&err) => {}
                _ => break,
            }
        }
    }

    // Take the controlling tty so the command's children don't receive SIGHUP when we exit.
    if let Err(err) = closure.pty_follower.tcsetpgrp(closure.monitor_pgrp) {
        dev_error!(
            "cannot set foreground process group to {} (monitor): {err}",
            closure.monitor_pgrp
        );
    }

    match reason {
        StopReason::Break(err) => match err.try_into() {
            Ok(msg) => {
                if let Err(err) = closure.backchannel.send(&msg) {
                    dev_warn!("cannot send message over backchannel: {err}")
                }
            }
            Err(err) => {
                dev_warn!("socket error `{err:?}` cannot be converted to a message")
            }
        },
        StopReason::Exit(command_status) => {
            if let Err(err) = closure.backchannel.send(&command_status.into()) {
                dev_warn!("cannot send message over backchannel: {err}")
            }
        }
    }

    // FIXME (ogsudo): The tty is restored here if selinux is available.

    drop(closure);

    exit(1)
}

// FIXME: This should return `io::Result<!>` but `!` is not stable yet.
fn exec_command(mut command: Command, foreground: bool, pty_follower: PtyFollower) -> io::Error {
    // FIXME (ogsudo): Do any additional configuration that needs to be run after `fork` but before `exec`
    let command_pid = std::process::id() as ProcessId;

    setpgid(0, command_pid).ok();

    // Wait for the monitor to set us as the foreground group for the pty if we are in the
    // foreground.
    if foreground {
        while !pty_follower.tcgetpgrp().is_ok_and(|pid| pid == command_pid) {
            std::thread::sleep(std::time::Duration::from_micros(1));
        }
    }

    // Done with the pty follower.
    drop(pty_follower);

    command.exec()
}

struct MonitorClosure<'a> {
    /// The command PID.
    ///
    /// This is `Some` iff the process is still running.
    command_pid: Option<ProcessId>,
    command_pgrp: ProcessId,
    monitor_pgrp: ProcessId,
    pty_follower: PtyFollower,
    errpipe_rx: UnixStream,
    backchannel: &'a mut MonitorBackchannel,
    signal_manager: SignalManager,
}

impl<'a> MonitorClosure<'a> {
    fn new(
        command_pid: ProcessId,
        pty_follower: PtyFollower,
        errpipe_rx: UnixStream,
        backchannel: &'a mut MonitorBackchannel,
        signal_manager: SignalManager,
        registry: &mut EventRegistry<Self>,
    ) -> Self {
        // Store the pgid of the monitor.
        let monitor_pgrp = getpgrp();

        // Register the callback to receive the IO error if the command fails to execute.
        registry.register_event(&errpipe_rx, PollEvent::Readable, |_| {
            MonitorEvent::ReadableErrPipe
        });

        // Register the callback to receive events from the backchannel
        registry.register_event(backchannel, PollEvent::Readable, |_| {
            MonitorEvent::ReadableBackchannel
        });

        signal_manager.register_handlers(registry, MonitorEvent::Signal);

        // Put the command in its own process group.
        let command_pgrp = command_pid;
        if let Err(err) = setpgid(command_pid, command_pgrp) {
            dev_warn!("cannot set process group ID for process: {err}");
        };

        Self {
            command_pid: Some(command_pid),
            command_pgrp,
            monitor_pgrp,
            pty_follower,
            errpipe_rx,
            signal_manager,
            backchannel,
        }
    }

    /// Based on `mon_backchannel_cb`
    fn read_backchannel(&mut self, registry: &mut EventRegistry<Self>) {
        match self.backchannel.recv() {
            // Read interrupted, we can try again later.
            Err(err) if was_interrupted(&err) => {}
            // There's something wrong with the backchannel, break the event loop
            Err(err) => {
                dev_warn!("cannot read from backchannel: {}", err);
                registry.set_break(err);
            }
            Ok(event) => {
                match event {
                    // We shouldn't receive this event more than once.
                    MonitorMessage::ExecCommand => unreachable!(),
                    // Forward signal to the command.
                    MonitorMessage::Signal(signal) => {
                        if let Some(command_pid) = self.command_pid {
                            self.send_signal(signal, command_pid, true)
                        }
                    }
                }
            }
        }
    }

    fn handle_sigchld(&mut self, command_pid: ProcessId, registry: &mut EventRegistry<Self>) {
        let status = loop {
            match command_pid.wait(WaitOptions::new().untraced().no_hang()) {
                Err(WaitError::Io(err)) if was_interrupted(&err) => {}
                // This only happens if we receive `SIGCHLD` but there's no status update from the
                // command.
                Err(WaitError::Io(err)) => {
                    dev_info!("cannot wait for {command_pid} (command): {err}")
                }
                // This only happens if the command exited and any process already waited for the
                // command.
                Err(WaitError::NotReady) => {
                    dev_info!("{command_pid} (command) has no status report")
                }
                Ok((_pid, status)) => break status,
            }
        };

        if let Some(exit_code) = status.exit_status() {
            dev_info!("{command_pid} (command) exited with status code {exit_code}");
            // The command did exit, set it's PID to `None`.
            self.command_pid = None;
            registry.set_exit(status);
        } else if let Some(signal) = status.term_signal() {
            dev_info!(
                "{command_pid} (command) was terminated by {}",
                signal_fmt(signal),
            );
            // The command was terminated, set it's PID to `None`.
            self.command_pid = None;
            registry.set_exit(status);
        } else if let Some(signal) = status.stop_signal() {
            dev_info!(
                "{command_pid} (command) was stopped by {}",
                signal_fmt(signal),
            );
            // Save the foreground process group ID so we can restore it later.
            if let Ok(pgrp) = self.pty_follower.tcgetpgrp() {
                if pgrp != self.monitor_pgrp {
                    self.command_pgrp = pgrp;
                }
            }
            self.backchannel
                .send(&ParentMessage::CommandStatus(status))
                .ok();
        } else if status.did_continue() {
            dev_info!("{command_pid} (command) continued execution");
        } else {
            dev_warn!("unexpected wait status for {command_pid} (command)")
        }
    }

    fn read_errpipe(&mut self, registry: &mut EventRegistry<Self>) {
        let mut buf = 0i32.to_ne_bytes();
        match self.errpipe_rx.read_exact(&mut buf) {
            Err(err) if was_interrupted(&err) => { /* Retry later */ }
            Err(err) => registry.set_break(err),
            Ok(_) => {
                // Received error code from the command, forward it to the parent.
                let error_code = i32::from_ne_bytes(buf);
                self.backchannel
                    .send(&ParentMessage::IoError(error_code))
                    .ok();
            }
        }
    }

    /// Send a signal to the command.
    fn send_signal(&self, signal: c_int, command_pid: ProcessId, from_parent: bool) {
        dev_info!(
            "sending {}{} to command",
            signal_fmt(signal),
            opt_fmt(from_parent, " from parent"),
        );
        // FIXME: We should call `killpg` instead of `kill`.
        match signal {
            SIGALRM => {
                terminate_process(command_pid, false);
            }
            SIGCONT_FG => {
                // Continue with the command as the foreground process group
                if let Err(err) = self.pty_follower.tcsetpgrp(self.command_pgrp) {
                    dev_error!(
                        "cannot set the foreground process group to {} (command): {err}",
                        self.command_pgrp
                    );
                }
                kill(command_pid, SIGCONT).ok();
            }
            SIGCONT_BG => {
                // Continue with the monitor as the foreground process group
                if let Err(err) = self.pty_follower.tcsetpgrp(self.monitor_pgrp) {
                    dev_error!(
                        "cannot set the foreground process group to {} (monitor): {err}",
                        self.monitor_pgrp
                    );
                }
                kill(command_pid, SIGCONT).ok();
            }
            signal => {
                // Send the signal to the command.
                kill(command_pid, signal).ok();
            }
        }
    }

    fn on_signal(&mut self, signal: Signal, registry: &mut EventRegistry<Self>) {
        let info = match self.signal_manager.recv(signal) {
            Ok(info) => info,
            Err(err) => {
                dev_error!("monitor could not receive signal {signal:?}: {err}");
                return;
            }
        };

        dev_info!(
            "monitor received{} {} from {}",
            opt_fmt(info.is_user_signaled(), " user signaled"),
            signal_fmt(info.signal()),
            info.pid()
        );

        // Don't do anything if the command has terminated already
        let Some(command_pid) = self.command_pid else {
            dev_info!("command was terminated, ignoring signal");
            return;
        };

        match info.signal() {
            SIGCHLD => self.handle_sigchld(command_pid, registry),
            // Skip the signal if it was sent by the user and it is self-terminating.
            _ if info.is_user_signaled()
                && is_self_terminating(info.pid(), command_pid, self.command_pgrp) => {}
            signal => self.send_signal(signal, command_pid, false),
        }
    }
}

/// Decides if the signal sent by the process with `signaler_pid` PID is self-terminating.
///
/// A signal is self-terminating if `signaler_pid`:
/// - is the same PID of the command, or
/// - is in the process group of the command and the command is the leader.
fn is_self_terminating(
    signaler_pid: ProcessId,
    command_pid: ProcessId,
    command_pgrp: ProcessId,
) -> bool {
    if signaler_pid != 0 {
        if signaler_pid == command_pid {
            return true;
        }

        if let Ok(grp_leader) = getpgid(signaler_pid) {
            if grp_leader == command_pgrp {
                return true;
            }
        }
    }

    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MonitorEvent {
    Signal(Signal),
    ReadableErrPipe,
    ReadableBackchannel,
}

impl<'a> Process for MonitorClosure<'a> {
    type Event = MonitorEvent;
    type Break = io::Error;
    type Exit = WaitStatus;

    fn on_event(&mut self, event: Self::Event, registry: &mut EventRegistry<Self>) {
        match event {
            MonitorEvent::Signal(signal) => self.on_signal(signal, registry),
            MonitorEvent::ReadableErrPipe => self.read_errpipe(registry),
            MonitorEvent::ReadableBackchannel => self.read_backchannel(registry),
        }
    }
}
