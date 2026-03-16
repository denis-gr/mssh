use std::collections::HashMap;

//pub use crate::echo::Echo;
pub use crate::common::MessageFile;
pub use crate::terminal::Terminal as Echo;
use bytes::Bytes;
use mail_parser::PartType;
use std::sync::Arc;
use tokio::sync::{
    Mutex,
    mpsc::{Receiver, Sender, channel},
};

struct LastMailState {
    subject: Option<String>,
    client_message_id: Option<String>,
    reference: Option<Vec<String>>,
    in_reply_to: Option<String>,
    body: Option<Vec<u8>>,
}

impl LastMailState {
    fn from_bytes(bytes: &[u8]) -> Result<Self, anyhow::Error> {
        let mail = mail_parser::MessageParser::default()
            .parse(bytes)
            .ok_or(anyhow::anyhow!("Failed to parse email message"))?;
        let refs = mail
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
        Ok(Self {
            subject: mail.subject().map(|s| s.to_string()),
            client_message_id: mail.message_id().map(|s| s.to_string()),
            reference: refs,
            in_reply_to: None,
            body: match mail.part(body_idx.clone()) {
                Some(part) => match &part.body {
                    PartType::Text(t) => Some(t.as_bytes().to_vec()),
                    _ => None,
                },
                None => None,
            },
        })
    }

    fn create_reply(&self, body: Vec<u8>) -> Self {
        Self {
            subject: self.subject.clone(),
            client_message_id: None,
            reference: self.reference.clone(),
            in_reply_to: self.client_message_id.clone(),
            body: Some(body),
        }
    }

    fn to_message_file(
        &self,
        client: String,
        server: String,
    ) -> Result<MessageFile, anyhow::Error> {
        let mut mail = mail_builder::MessageBuilder::new()
            .to(client.clone())
            .from(server)
            .subject(self.subject.clone().unwrap_or("...".to_string()))
            .text_body(String::from_utf8_lossy(self.body.as_ref().unwrap_or(&vec![])).into_owned());

        if let Some(message_id) = &self.in_reply_to {
            mail = mail.in_reply_to(message_id.clone());
        }
        if let Some(reference) = &self.reference {
            mail = mail.references(reference.clone());
        }
        Ok(MessageFile {
            client: client,
            message_file: Bytes::from(mail.write_to_vec()?),
        })
    }
}

struct ClientContext {
    tr_in_tx: Sender<Vec<u8>>,
    client: String,
    server: String,
    last_mail_state: Mutex<LastMailState>,
}

fn get_emails(msg: &[u8]) -> Result<(String, String), anyhow::Error> {
    let mail = mail_parser::MessageParser::default()
        .parse(msg)
        .ok_or(anyhow::anyhow!("Failed to parse email message"))?;
    let clients = mail
        .from()
        .and_then(|f| f.as_list())
        .ok_or(anyhow::anyhow!("Failed to parse client emails"))?;
    if clients.len() != 1 {
        return Err(anyhow::anyhow!("Found {} adresses in From", clients.len()));
    }
    let client = clients[0]
        .address()
        .map(|s| s.to_string())
        .ok_or(anyhow::anyhow!("Failed to extract client email"))?;
    let servers = mail
        .to()
        .and_then(|t| t.as_list())
        .ok_or(anyhow::anyhow!("Failed to parse server emails"))?;
    if servers.len() != 1 {
        return Err(anyhow::anyhow!("Found {} adresses in To", servers.len()));
    }
    let server = servers[0]
        .address()
        .map(|s| s.to_string())
        .ok_or(anyhow::anyhow!("Failed to extract server email"))?;
    Ok((client, server))
}

impl ClientContext {
    pub fn create(
        msg: MessageFile,
        out_tx: Sender<MessageFile>,
        tick_interval: std::time::Duration,
    ) -> Result<Arc<Self>, anyhow::Error> {
        log::info!("Creating context for client {}", msg.client);
        let (client, server) = get_emails(&msg.message_file.as_ref())?;
        let (tr_in_tx, tr_in_rx) = channel::<Vec<u8>>(128);
        let (tr_out_tx, mut tr_out_rx) = channel::<Vec<u8>>(128);
        let mut tr = Echo::new()?;

        tokio::spawn(async move {
            tr.run(tr_out_tx, tr_in_rx).await;
        });

        let context = Arc::new(Self {
            tr_in_tx,
            client,
            server,
            last_mail_state: Mutex::new(LastMailState {
                subject: None,
                client_message_id: None,
                reference: None,
                body: None,
                in_reply_to: None,
            }),
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
                                log::info!("Output channel closed for client {}", context2.client);
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

    async fn send_from_tr(&self, mes: Vec<u8>) -> MessageFile {
        log::info!("New data from thread for client {}", self.client);
        let mes = self.last_mail_state.lock().await.create_reply(mes);
        mes.to_message_file(self.client.clone(), self.server.clone())
            .unwrap()
    }

    pub async fn send_to_tr(&self, mes: MessageFile) {
        log::info!("New data for thread for client {}", self.client);
        let state = LastMailState::from_bytes(&mes.message_file.as_ref()).unwrap();
        let body = state.body.clone().unwrap_or_default();
        {
            let mut state_lock = self.last_mail_state.lock().await;
            *state_lock = state;
        }
        let _ = self.tr_in_tx.send(body).await;
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
            let client = msg.client.clone();
            let context = self.clients.entry(client.clone()).or_insert_with(|| {
                ClientContext::create(msg.clone(), out_tx.clone(), self.tick_interval).unwrap()
            });
            context.send_to_tr(msg).await;
        }
        Ok(())
    }
}
