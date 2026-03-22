use core::panic;
use std::collections::HashMap;

//pub use crate::echo::Echo;
pub use crate::common::{MailInfo, MessageFile};
pub use crate::terminal::Terminal;
use bytes::Bytes;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::{
    Mutex,
    mpsc::{Receiver, Sender, channel},
};

#[derive(Debug)]
struct ClientContext {
    jh: tokio::task::JoinHandle<()>,
    tr_in_tx: Sender<Vec<u8>>,
    last_mail: Mutex<Option<MailInfo>>,
}

impl ClientContext {
    pub fn create(
        out_tx: Sender<MessageFile>,
        tick_interval: std::time::Duration,
    ) -> Result<Arc<Self>, anyhow::Error> {
        let (tr_in_tx, tr_in_rx) = channel::<Vec<u8>>(128);
        let (tr_out_tx, mut tr_out_rx) = channel::<Vec<u8>>(128);
        let mut tr = Terminal::new("".to_string())?;

        let jh = tokio::spawn(async move {
            tr.run(tr_out_tx, tr_in_rx).await;
        });

        let context = Arc::new(Self {
            jh,
            tr_in_tx,
            last_mail: Mutex::new(None),
        });

        let context2 = Arc::clone(&context);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(tick_interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            let mut buffer = Vec::<u8>::new();
            loop {
                tokio::select! {
                    Some(data) = tr_out_rx.recv() => {
                        buffer.extend_from_slice(&data);
                    }
                    _ = tick.tick() => {
                        if !buffer.is_empty() {
                            let mes = context2.send_from_tr(buffer).await;
                            if out_tx.send(mes).await.is_err() {
                                break;
                            }
                            buffer = Vec::new();
                        }
                    }
                }
            }
        });
        Ok(context)
    }

    async fn send_from_tr(&self, buf: Vec<u8>) -> MessageFile {
        let last_mail = self.last_mail.lock().await;
        let info = last_mail
            .as_ref()
            .expect("No mail received yet, but terminal sent data");
        let reply_info = info.clone().create_reply();
        let message_file = reply_info
            .clone()
            .to_message_file(Bytes::from(buf))
            .unwrap_or_else(|e| {
                panic!("Failed to create message file from terminal data: {}", e);
            });
        MessageFile {
            message_file,
            info: Some(reply_info),
            client: info.from.clone(), // is reply_info.to
        }
    }

    pub async fn send_to_tr(&self, mes: Bytes, info: MailInfo) {
        let mut last_mail = self.last_mail.lock().await;
        *last_mail = Some(info);
        self.tr_in_tx.send(mes.to_vec()).await.unwrap_or_else(|e| {
            log::error!("Failed to send data to terminal: {}", e);
        });
    }

    pub fn close(&self) {
        self.jh.abort();
    }
}

pub struct Dispatcher {
    clients: HashMap<String, Arc<ClientContext>>,
    last_email: HashMap<String, Mutex<MailInfo>>,
    tick_interval: std::time::Duration,
}

impl Dispatcher {
    pub fn new(tick_interval: std::time::Duration) -> Result<Self, anyhow::Error> {
        Ok(Dispatcher {
            clients: HashMap::new(),
            last_email: HashMap::new(),
            tick_interval,
        })
    }

    pub async fn run(
        &mut self,
        out_tx: Sender<MessageFile>,
        mut in_rx: Receiver<MessageFile>,
    ) -> Result<(), anyhow::Error> {
        while let Some(msg) = in_rx.recv().await {
            if let Ok((bytes, info)) = MailInfo::from_bytes(msg.message_file) {
                self.update_last_email(&info).await;
                let id = info.clone().flow_id.unwrap();
                if id == "#MAIN" {
                    self.pull_main(bytes, info, out_tx.clone()).await;
                    continue;
                }
                let context = self.clients.entry(id).or_insert_with(|| {
                    ClientContext::create(out_tx.clone(), self.tick_interval)
                        .unwrap_or_else(|e| panic!("Failed to create client context: {}", e))
                });
                context.send_to_tr(bytes, info).await;
            } else {
                log::error!("Failed to parse email message, skipping");
                continue;
            }
        }
        Ok(())
    }

    async fn update_last_email(&mut self, email: &MailInfo) {
        if let Some(mutex) = self.last_email.get(&email.from) {
            mutex.lock().await.clone_from(email);
        } else {
            self.last_email
                .insert(email.from.clone(), Mutex::new(email.clone()));
        }
    }

    async fn pull_main(&mut self, mes: Bytes, info: MailInfo, out_tx: Sender<MessageFile>) {
        let text = String::from_utf8(mes.to_vec()).unwrap();
        for rec in text.split("\r\n\r\n") {
            if rec.starts_with("#MAIN info") {
                let mut answer = Vec::new();
                write!(answer, "#MAIN 0 {:#?}", self.clients).unwrap();
                let reply_info = info.clone().create_reply();
                let message_file = reply_info
                    .clone()
                    .to_message_file(Bytes::from(answer))
                    .unwrap();
                let mes = MessageFile {
                    message_file,
                    info: Some(reply_info),
                    client: info.from.clone(),
                };
                out_tx.send(mes).await.unwrap_or_else(|e| {
                    log::error!("Failed to send message to terminal: {}", e);
                });
            } else if rec.starts_with("#MAIN killself") {
                panic!("Received killself command, shutting down");
            } else if rec.starts_with("#MAIN kill ") {
                let id = rec["#MAIN kill ".len()..].trim();
                if let Some(context) = self.clients.remove(id) {
                    context.close();
                }
            }
        }
    }
}
