use std::{borrow::Cow, collections::HashMap};

//pub use crate::echo::Echo;
pub use crate::common::{MailInfo, MessageFile};
pub use crate::terminal::Terminal as Echo;
use bytes::Bytes;
use mail_parser::PartType;
use std::sync::Arc;
use tokio::sync::{
    Mutex,
    mpsc::{Receiver, Sender, channel},
};

impl MailInfo {
    fn from_bytes(bytes: Bytes) -> Result<(Bytes, MailInfo), anyhow::Error> {
        let result = {
            let mail = mail_parser::MessageParser::default()
                .parse(bytes.as_ref())
                .ok_or(anyhow::anyhow!("Failed to parse email message"))?;
            let reference = mail
                .references()
                .as_address()
                .and_then(|a| a.as_list())
                .and_then(|l| {
                    l.iter()
                        .map(|a| a.address().map(|s| s.to_string()))
                        .collect::<Option<Vec<String>>>()
                });
            let body_idx = mail
                .text_body
                .first()
                .ok_or(anyhow::anyhow!("No text body part"))?;
            let part = mail
                .part(*body_idx)
                .ok_or(anyhow::anyhow!("Failed to extract body"))?;
            let body = match &part.body {
                PartType::Text(cow) => match cow {
                    Cow::Borrowed(s) => Bytes::copy_from_slice(s.as_bytes()),
                    Cow::Owned(s) => Bytes::from(s.clone()),
                },
                _ => return Err(anyhow::anyhow!("Unexpected body part type")),
            };
            let in_reply_to = mail
                .in_reply_to()
                .as_address()
                .and_then(|a| a.as_list())
                .and_then(|l| l.first().map(|a| a.address().map(|s| s.to_string())))
                .flatten();
            let from = mail
                .from()
                .and_then(|f| f.as_list())
                .ok_or(anyhow::anyhow!("Failed to parse client emails"))?;
            if from.len() != 1 {
                return Err(anyhow::anyhow!("Found {} adresses in From", from.len()));
            }
            let from = from[0]
                .address()
                .map(|s| s.to_string())
                .ok_or(anyhow::anyhow!("Failed to extract client email"))?;
            let to = mail
                .to()
                .and_then(|t| t.as_list())
                .ok_or(anyhow::anyhow!("Failed to parse server emails"))?;
            if to.len() != 1 {
                return Err(anyhow::anyhow!("Found {} adresses in To", to.len()));
            }
            let to = to[0]
                .address()
                .map(|s| s.to_string())
                .ok_or(anyhow::anyhow!("Failed to extract server email"))?;

            (
                body,
                Self {
                    subject: mail.subject().unwrap_or("...").to_string(),
                    message_id: mail.message_id().map(|s| s.to_string()),
                    reference,
                    in_reply_to,
                    from,
                    to,
                },
            )
        };
        return Ok(result);
    }

    fn create_reply(self) -> Self {
        Self {
            subject: self.subject.clone(),
            message_id: None,
            reference: self.reference.clone(),
            in_reply_to: self.message_id.clone(),
            from: self.to,
            to: self.from,
        }
    }

    fn to_message_file(self, body: Bytes) -> Result<Bytes, anyhow::Error> {
        let mut mail = mail_builder::MessageBuilder::new()
            .to(self.to)
            .from(self.from)
            .subject(self.subject)
            .text_body(String::from_utf8_lossy(body.as_ref()));
        if let Some(message_id) = &self.in_reply_to {
            mail = mail.in_reply_to(message_id.clone());
        }
        if let Some(reference) = &self.reference {
            mail = mail.references(reference.clone());
        }
        let mut buf = Vec::with_capacity((body.len() as f64 * 1.37) as usize + 1024);
        mail.write_to(&mut buf)?;
        buf.shrink_to_fit();
        Ok(Bytes::from(buf))
    }
}

struct ClientContext {
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
        let mut tr = Echo::new()?;

        tokio::spawn(async move {
            tr.run(tr_out_tx, tr_in_rx).await;
        });

        let context = Arc::new(Self {
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
}

pub struct Dispatcher {
    clients: HashMap<String, Arc<ClientContext>>,
    tick_interval: std::time::Duration,
}

impl Dispatcher {
    pub fn new(tick_interval: std::time::Duration) -> Result<Self, anyhow::Error> {
        Ok(Dispatcher {
            clients: HashMap::new(),
            tick_interval,
        })
    }

    pub async fn run(
        &mut self,
        out_tx: Sender<MessageFile>,
        mut in_rx: Receiver<MessageFile>,
    ) -> Result<(), anyhow::Error> {
        while let Some(msg) = in_rx.recv().await {
            let (bytes, info) = MailInfo::from_bytes(msg.message_file)?;
            let context = self.clients.entry(info.from.clone()).or_insert_with(|| {
                ClientContext::create(out_tx.clone(), self.tick_interval)
                    .unwrap_or_else(|e| panic!("Failed to create client context: {}", e))
            });
            context.send_to_tr(bytes, info).await;
        }
        Ok(())
    }
}
