use tokio::sync::mpsc::{Receiver, Sender};

pub struct Echo {}

impl Echo {
    pub fn new(_arg: String) -> anyhow::Result<Self> {
        Ok(Echo {})
    }

    pub async fn run(&self, out_tx: Sender<Vec<u8>>, mut in_rx: Receiver<Vec<u8>>) {
        while let Some(msg) = in_rx.recv().await {
            if out_tx.send(msg).await.is_err() {
                break;
            }
        }
    }
}
