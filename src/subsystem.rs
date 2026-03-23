use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::{Receiver, Sender};

pub struct SubSystem {
    child: Child,
}

impl SubSystem {
    pub fn new(arg: String) -> anyhow::Result<Self> {
        let p: Vec<&str> = arg.splitn(2, " ").collect();
        let c = p.first().ok_or(anyhow::anyhow!("No command"))?;
        let arg = p.get(1).unwrap_or(&"").to_string();
        log::info!("Starting subsystem with command: {} {}", c, arg);
        let mut command = Command::new(c.replace("\\", "/"));
        if !arg.is_empty() {
            command.arg(arg);
        }
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        Ok(Self { child })
    }

    pub async fn run(mut self, out_tx: Sender<Vec<u8>>, mut in_rx: Receiver<Vec<u8>>) {
        let mut stdin = self.child.stdin.take().unwrap();
        let mut stdout = self.child.stdout.take().unwrap();
        let mut stderr = self.child.stderr.take().unwrap();

        let stdout_task = tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buffer).await {
                if n == 0 || out_tx.send(buffer[..n].to_vec()).await.is_err() {
                    break;
                }
            }
        });
        let stderr_task = tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                log::error!("Process stderr: {}", String::from_utf8_lossy(&buffer[..n]));
            }
        });

        loop {
            tokio::select! {
                data = in_rx.recv() => {match data {
                    Some(data) => {
                        if stdin.write_all(&data).await.is_err() { break; }
                        if stdin.flush().await.is_err() { break; }
                    }
                    None => {
                        drop(stdin);
                        let _ = self.child.wait().await;
                        break;
                    },
                }}
                status = self.child.wait() => {
                    log::debug!("Process exited: {:?}", status);
                    break;
                }
            }
        }

        stdout_task.abort();
        stderr_task.abort();
    }
}
