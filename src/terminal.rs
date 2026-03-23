use std::io::{Read, Write};

use nix::sys::termios::{SetArg, tcsetattr};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::os::fd::BorrowedFd;
use tokio::sync::mpsc::{Receiver, Sender, channel};

pub struct Terminal {
    _child: Box<dyn portable_pty::Child + Send>,
    reader: Option<Box<dyn Read + Send>>,
    writer: Box<dyn Write + Send>,
}

impl Terminal {
    pub fn new(_arg: String) -> anyhow::Result<Self> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut termios = pair
            .master
            .get_termios()
            .ok_or(anyhow::anyhow!("Termios missing"))?;
        let echo_bits = nix::sys::termios::LocalFlags::ECHO
            | nix::sys::termios::LocalFlags::ECHOE
            | nix::sys::termios::LocalFlags::ECHOK
            | nix::sys::termios::LocalFlags::ECHONL;
        termios.local_flags.remove(echo_bits);
        let raw_fd = pair.master.as_raw_fd().ok_or(anyhow::anyhow!(""))?;
        let fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
        tcsetattr(fd, SetArg::TCSANOW, &termios)?;
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.env("TERM", "dumb");
        cmd.env("PS1", "");
        let _child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        Ok(Self {
            _child,
            reader: Some(reader),
            writer,
        })
    }

    pub async fn run(&mut self, out_tx: Sender<Vec<u8>>, mut in_rx: Receiver<Vec<u8>>) {
        let (tx, mut rx) = channel::<Vec<u8>>(128);
        let mut reader = self
            .reader
            .take()
            .expect("Terminal reader should exist when run() is called");
        tokio::task::spawn_blocking(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(buffer[..n].to_vec()).is_err() {
                            log::error!("Failed to send data from terminal reader to channel");
                            break;
                        }
                        log::debug!("Read {} bytes from terminal", n);
                    }
                    Err(e) => {
                        log::error!("Failed to read from terminal: {}", e);
                        break;
                    }
                }
            }
        });
        loop {
            tokio::select! {
                Some(data) = in_rx.recv() => {
                    if let Err(e) = self.writer.write_all(&data) {
                        log::error!("Failed to write to terminal: {}", e);
                        break;
                    }
                    if let Err(e) = self.writer.flush() {
                        log::error!("Failed to flush terminal writer: {}", e);
                        break;
                    }
                }
                Some(data) = rx.recv() => {
                    if let Err(e) = out_tx.send(data).await {
                        log::error!("Failed to send data to output channel: {}", e);
                        break;
                    }
                }
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self._child.kill();
        let _ = self._child.wait();
    }
}
