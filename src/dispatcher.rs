use std::collections::HashMap;

pub use crate::echo::Echo;
pub use crate::jmap_transport::MessageFile;
use mail_parser::PartType;
use std::sync::Arc;
use tokio::sync::{
    Mutex,
    mpsc::{Receiver, Sender, channel},
};

struct LastMailState {
    subject: Option<String>,
    client_message_id: Option<String>,
    reference: Option<String>,
}

struct ClientContext {
    client: String,
    tr_in_tx: Sender<Vec<u8>>,
    client_email: String,
    server_email: String,
    last_mail_state: Mutex<LastMailState>,
}

impl ClientContext {
    pub fn create(msg: MessageFile, out_tx: Sender<MessageFile>) -> Arc<Self> {
        let mail = mail_parser::MessageParser::default()
            .parse(msg.content.as_bytes())
            .unwrap();
        let client_email = mail
            .from()
            .unwrap()
            .first()
            .unwrap()
            .address()
            .unwrap()
            .to_string();
        let server_email = mail
            .to()
            .unwrap()
            .first()
            .unwrap()
            .address()
            .unwrap()
            .to_string();
        let (tr_in_tx, tr_in_rx) = channel::<Vec<u8>>(128);
        let (tr_out_tx, mut tr_out_rx) = channel::<Vec<u8>>(128);
        let mut tr = Echo::new().expect("Failed to create Echo instance");

        tokio::spawn(async move {
            tr.run(tr_in_rx, tr_out_tx).await;
        });

        let context = Arc::new(Self {
            client: msg.client,
            tr_in_tx,
            client_email,
            server_email,
            last_mail_state: Mutex::new(LastMailState {
                subject: None,
                client_message_id: None,
                reference: None,
            }),
        });

        let context2 = Arc::clone(&context);
        tokio::spawn(async move {
            while let Some(data) = tr_out_rx.recv().await {
                let mes = ClientContext::send_from_tr(&context2, data).await;
                if out_tx.send(mes).await.is_err() {
                    break;
                }
            }
        });

        context
    }

    async fn send_from_tr(&self, mes: Vec<u8>) -> MessageFile {
        let state = self.last_mail_state.lock().await;
        let subject = state.subject.clone().unwrap_or("...".to_string());
        let in_reply_to = state.client_message_id.clone();
        let references = state.reference.clone();
        drop(state);

        let mut mail = mail_builder::MessageBuilder::new()
            .to(self.client_email.clone())
            .from(self.server_email.clone())
            .subject(subject)
            .text_body(String::from_utf8_lossy(&mes).into_owned());

        if let Some(message_id) = in_reply_to {
            mail = mail.in_reply_to(message_id);
        }

        if let Some(reference) = references {
            mail = mail.references(reference);
        }

        MessageFile {
            client: self.client.clone(),
            content: mail.write_to_string().unwrap(),
        }
    }

    pub async fn send_to_tr(&self, mes: MessageFile) {
        let mail = mail_parser::MessageParser::default()
            .parse(mes.content.as_bytes())
            .unwrap();
        let subject = mail.subject().unwrap_or("...").to_string();
        let client_message_id = mail.message_id().map(|s| s.to_string());
        let reference = mail
            .header("References")
            .and_then(|h| h.as_text())
            .map(|s| s.to_string());
        {
            let mut state = self.last_mail_state.lock().await;
            state.subject = Some(subject);
            state.client_message_id = client_message_id;
            state.reference = reference;
        }
        let part = &mail
            .part(mail.text_body.first().unwrap().clone())
            .unwrap()
            .body;
        let body = match part {
            PartType::Text(t) => Some(t.as_bytes().to_vec()),
            _ => None,
        }
        .unwrap();
        let _ = self.tr_in_tx.send(body).await;
    }
}

pub struct Dispatcher {
    clients: HashMap<String, Arc<ClientContext>>,
}

impl Dispatcher {
    pub fn new() -> Result<Self, anyhow::Error> {
        Ok(Dispatcher {
            clients: HashMap::new(),
        })
    }

    pub async fn run(&mut self, mut in_rx: Receiver<MessageFile>, out_tx: Sender<MessageFile>) {
        while let Some(msg) = in_rx.recv().await {
            let client = msg.client.clone();
            let context = self
                .clients
                .entry(client.clone())
                .or_insert_with(|| ClientContext::create(msg.clone(), out_tx.clone()));
            context.send_to_tr(msg).await;
        }
    }
}
