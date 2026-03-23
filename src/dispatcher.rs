use core::panic;
use std::collections::HashMap;

pub use crate::common::{MailInfo, MessageFile};
pub use crate::echo::Echo;
pub use crate::subsystem::SubSystem;
pub use crate::terminal::Terminal;
use bytes::Bytes;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::{
    Mutex,
    mpsc::{Receiver, Sender, channel},
};

enum Session {
    Echo(Echo),
    Terminal(Terminal),
    SubSystem(SubSystem),
}

impl Session {
    fn new(typ: String, arg: String) -> anyhow::Result<Self> {
        match typ.as_str() {
            "echo" => Ok(Session::Echo(Echo::new(arg)?)),
            "term" => Ok(Session::Terminal(Terminal::new(arg)?)),
            "subs" => Ok(Session::SubSystem(SubSystem::new(arg)?)),
            "" => Ok(Session::Terminal(Terminal::new(arg)?)),
            _ => Err(anyhow::anyhow!("Unknown session type: {}", typ)),
        }
    }

    async fn run(self, out_tx: Sender<Vec<u8>>, in_rx: Receiver<Vec<u8>>) {
        match self {
            Session::Echo(echo) => echo.run(out_tx, in_rx).await,
            Session::Terminal(mut terminal) => terminal.run(out_tx, in_rx).await,
            Session::SubSystem(subsystem) => subsystem.run(out_tx, in_rx).await,
        }
    }
}

#[derive(Debug)]
struct ClientContext {
    jh: tokio::task::JoinHandle<()>,
    tr_in_tx: Option<Sender<Vec<u8>>>,
    last_mail: Mutex<Option<MailInfo>>,
}

impl ClientContext {
    pub fn create(
        typ: String,
        arg: String,
        out_tx: Sender<MessageFile>,
        tick_interval: std::time::Duration,
    ) -> Result<Arc<Self>, anyhow::Error> {
        let (tr_in_tx, tr_in_rx) = channel::<Vec<u8>>(128);
        let (tr_out_tx, mut tr_out_rx) = channel::<Vec<u8>>(128);
        let tr = Session::new(typ, arg)?;
        let jh = tokio::spawn(async move {
            tr.run(tr_out_tx, tr_in_rx).await;
        });
        let context = Arc::new(Self {
            jh,
            tr_in_tx: Some(tr_in_tx),
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
        let file = reply_info
            .clone()
            .into_file(Bytes::from(buf))
            .expect("Failed to create message file from terminal data");
        MessageFile {
            file,
            info: Some(reply_info),
            client: info.from.clone(), // is reply_info.to
        }
    }

    pub async fn send_to_tr(&self, mes: Bytes, info: MailInfo) -> Result<(), anyhow::Error> {
        let mut last_mail = self.last_mail.lock().await;
        *last_mail = Some(info);
        match &self.tr_in_tx {
            Some(tx) => tx.send(mes.to_vec()).await?,
            None => return Err(anyhow::anyhow!("Terminal input channel is closed")),
        }
        Ok(())
    }

    pub fn close(&self) {
        self.jh.abort();
    }

    //pub fn eof(&mut self) {
    //    self.tr_in_tx.take();
    //}
}

pub struct Dispatcher {
    clients: HashMap<String, Arc<ClientContext>>,
    last_email: HashMap<String, MailInfo>,
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
            if let Ok((bytes, info)) = MailInfo::from_bytes(msg.file) {
                if self.update_last_email(&info).await.is_err() {
                    continue;
                }
                let id = info.clone().flow_id.unwrap();
                if id == "MAIN" {
                    self.pull_main(bytes, info, out_tx.clone()).await;
                    continue;
                }
                if !self.clients.contains_key(&id) {
                    let typ = info.typ.clone().unwrap_or_default();
                    let arg = info.args.clone().unwrap_or_default();
                    match ClientContext::create(typ, arg, out_tx.clone(), self.tick_interval) {
                        Ok(context) => {
                            self.clients.insert(id.clone(), context);
                        }
                        Err(e) => {
                            log::error!("Failed to create client context: {}", e);
                            continue;
                        }
                    }
                }
                self.clients
                    .get(&id)
                    .unwrap()
                    .send_to_tr(bytes, info)
                    .await?;
            } else {
                log::debug!("Skipping message with invalid format");
                continue;
            }
        }
        Ok(())
    }

    async fn update_last_email(&mut self, info: &MailInfo) -> Result<(), anyhow::Error> {
        if let Some(m) = self.last_email.get_mut(&info.from) {
            if m.date.unwrap() > info.date.unwrap() {
                return Err(anyhow::anyhow!("Ignoring an older email {}", info.from));
            }
            m.clone_from(info);
        } else {
            self.last_email.insert(info.from.clone(), info.clone());
        }
        Ok(())
    }

    async fn pull_main(&mut self, mes: Bytes, info: MailInfo, out_tx: Sender<MessageFile>) {
        let text = String::from_utf8_lossy(&mes);
        for rec in text.split("\r\n\r\n") {
            if rec.starts_with("#MAIN info") {
                let mut answer = Vec::new();
                write!(answer, "#MAIN 0 {:#?}", self.clients).unwrap();
                let reply_info = info.clone().create_reply();
                let file = reply_info.clone().into_file(Bytes::from(answer)).unwrap();
                let mes = MessageFile {
                    file,
                    info: Some(reply_info),
                    client: info.from.clone(),
                };
                out_tx.send(mes).await.unwrap_or_else(|e| {
                    log::error!("Failed to send main info response: {}", e);
                });
            } else if rec.starts_with("#MAIN killself") {
                panic!("Received killself command, shutting down");
            } else if let Some(id) = rec.strip_prefix("#MAIN kill ")
                && let Some(context) = self.clients.remove(id)
            {
                context.close();
            }
        }
    }
}
