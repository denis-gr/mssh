use std::io::{Write, Read};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use tokio::sync::mpsc::{Receiver, Sender, channel};

pub struct Terminal {
    child: Box<dyn portable_pty::Child + Send>,
    reader: Option<Box<dyn Read + Send>>,
    writer: Box<dyn Write + Send>,
}

impl Terminal {
    pub fn new() -> anyhow::Result<Self> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-i");
        cmd.env("TERM", "dumb");
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        writer.write_all(b"stty -echo\n")?;
        writer.flush()?;
        Ok(Terminal {
            child,
            reader: Some(reader),
            writer,
        })
    }

    pub async fn run(&mut self, mut in_rx: Receiver<Vec<u8>>, out_tx: Sender<Vec<u8>>) {
        let (tx, mut rx) = channel::<Vec<u8>>(128);

        let mut reader = self
            .reader
            .take()
            .expect("Terminal reader should exist when run() is called");

        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        loop {
            tokio::select! {
                Some(data) = in_rx.recv() => {
                    if self.writer.write_all(&data).is_err() {
                        break;
                    }
                    if self.writer.flush().is_err() {
                        break;
                    }
                }
                Some(data) = rx.recv() => {
                    if out_tx.send(data).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}
