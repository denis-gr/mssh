use crate::common::MessageFile;
use crate::opengpg_utils::Helper;

use anyhow::anyhow;
use bytes::Bytes;
use log;
use mail_parser::{MimeHeaders, PartType};
use std::io::Write;
use tokio::sync::mpsc::{Receiver, Sender};

pub struct SecurityLayer {
    helper: Helper,
    pass_raw: bool,
}

impl SecurityLayer {
    pub fn load(
        path: &str,
        pgp_password: Option<String>,
        pass_raw: bool,
    ) -> Result<Self, anyhow::Error> {
        Ok(Self {
            helper: Helper::load_dir(path, pgp_password)?,
            pass_raw: pass_raw,
        })
    }

    pub async fn run(
        &self,
        in_tx: Sender<MessageFile>,
        mut in_rx: Receiver<MessageFile>,
        out_tx: Sender<MessageFile>,
        mut out_rx: Receiver<MessageFile>,
    ) -> Result<(), anyhow::Error> {
        loop {
            tokio::select! {
                Some(msg) = in_rx.recv() => {
                    match self.decrypt(msg) {
                        Ok(decrypted) => out_tx.send(decrypted).await?,
                        Err(e) => {
                            log::error!("Decryption failed: {}", e);
                        }
                    }
                }
                Some(msg) = out_rx.recv() => {
                    match self.encrypt(msg) {
                        Ok(encrypted) => in_tx.send(encrypted).await?,
                        Err(e) => {
                            log::error!("Encryption failed: {}", e);
                        }
                    }
                }
            }
        }
    }

    fn decrypt(&self, mes: MessageFile) -> Result<MessageFile, anyhow::Error> {
        let mail = mail_parser::MessageParser::default()
            .parse(mes.message_file.as_ref())
            .ok_or(anyhow!("Failed to parse email message"))?;
        let ct = mail.content_type().ok_or(anyhow!("Content-Type missing"))?;
        let typ = ct.ctype();
        let subtyp = ct.subtype().ok_or(anyhow!("Subtype missing"))?;
        let protocol = ct.attribute("protocol");
        if typ == "multipart"
            && subtyp == "encrypted"
            && protocol == Some("application/pgp-encrypted")
        {
            log::info!("Decrypting message from {} (pgp-encrypted)", mes.client);
            let part = mail
                .attachments()
                .find(|a| {
                    a.content_type()
                        .map(|ct| {
                            ct.ctype() == "application" && ct.subtype() == Some("octet-stream")
                        })
                        .unwrap_or(false)
                })
                .ok_or(anyhow!("Encrypted part not found"))?;
            let content = match part.body {
                PartType::Text(ref text) => text.as_bytes(),
                PartType::Binary(ref data) => data.as_ref(),
                _ => {
                    return Err(anyhow!("Unsupported content type in encrypted part"));
                }
            };
            let decrypted = self.helper.decrypt_message(content)?;
            drop(mes);
            let inner_mail = mail_parser::MessageParser::default()
                .parse(&decrypted)
                .ok_or(anyhow!("Failed to parse decrypted message"))?;
            let real_addr = inner_mail
                .from()
                .ok_or(anyhow!("From header missing in decrypted message"))?
                .iter()
                .filter_map(|i| i.address())
                .collect::<Vec<_>>();
            if real_addr.len() != 1 {
                return Err(anyhow!(
                    "Expected exactly one From address in decrypted message"
                ));
            }
            return Ok(MessageFile {
                client: real_addr[0].to_string(), // Always trust the decrypted From header
                info: None,
                message_file: Bytes::from(decrypted),
            });
        } else if typ == "multipart"
            && subtyp == "signed"
            && protocol == Some("application/pgp-signature")
        {
            log::info!("Decrypting message from {} (pgp-signature)", mes.client);
            // TODO: support signed messages
            return Err(anyhow!("Signed messages are not supported"));
        } else if self.pass_raw {
            log::info!("Decrypting message from {} (Non-encrypted)", mes.client);
            return Ok(mes.clone());
        } else {
            return Err(anyhow!(
                "Message is not encrypted/signed and pass_raw is false"
            ));
        }
    }

    fn encrypt(&self, msg: MessageFile) -> Result<MessageFile, anyhow::Error> {
        let encrypted_bytes = match self
            .helper
            .encrypt_message(msg.message_file.as_ref(), msg.client.clone())
        {
            Ok(encrypted) => Ok(encrypted),
            Err(e) => {
                if self.pass_raw {
                    log::info!("Encrypting message to {} (Non-encrypted)", msg.client);
                    return Ok(msg.clone());
                } else {
                    Err(anyhow!("Encryption failed: {}", e))
                }
            }
        }?;
        let encrypted = String::from_utf8(encrypted_bytes)?;
        let encrypted_len = encrypted.len();
        log::info!("Encrypting message to {} (pgp-encrypted)", msg.client);
        let info = msg.info.as_ref().unwrap().clone();
        let client = msg.client.clone();
        drop(msg);
        let mut b = Vec::with_capacity(encrypted_len + 1024);
        let boundary = "mssh-pgp-boundary";
        let domain = info.from.split('@').nth(1);
        let domain = domain.ok_or(anyhow!("Invalid from email address"))?;
        write!(b, "From: <{}>\r\n", info.from)?;
        write!(b, "To: <{}>\r\n", info.to)?;
        write!(b, "Subject: {}\r\n", info.subject)?;
        write!(b, "MIME-Version: 1.0\r\n")?;
        write!(b, "Date: {}\r\n", chrono::Utc::now().to_rfc2822())?;
        write!(b, "Message-ID: <{}@{}>\r\n", uuid::Uuid::new_v4(), domain)?;
        if let Some(in_reply) = info.in_reply_to {
            write!(b, "In-Reply-To: <{}>\r\n", in_reply)?;
        }
        if let Some(r) = info.reference {
            let r = r.iter().map(|s: &String| format!("<{}>", s)).collect::<Vec<_>>();
            write!(b, "References: {}\r\n", r.join("\r\n "))?;
        }
        write!(b, "Content-Type: multipart/encrypted;\r\n")?;
        write!(b, " protocol=\"application/pgp-encrypted\";\r\n")?;
        write!(b, " boundary=\"{}\"\r\n\r\n", boundary)?;
        write!(b, "--{}\r\n", boundary)?;
        write!(b, "Content-Type: application/pgp-encrypted\r\n\r\n")?;
        write!(b, "Version: 1\r\n\r\n")?;
        write!(b, "--{}\r\n", boundary)?;
        write!(b, "Content-Type: application/octet-stream\r\n")?;
        write!(b, "Content-Disposition: attachment; filename=\"m.asc\"\r\n")?;
        write!(b, "\r\n{}", encrypted)?;
        write!(b, "\r\n--{}--\r\n", boundary)?;
        b.shrink_to_fit();
        Ok(MessageFile {
            client: client,
            info: None,
            message_file: Bytes::from(b),
        })
    }
}
