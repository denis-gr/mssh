use crate::common::MessageFile;
use crate::opengpg_utils::Helper;

use bytes::Bytes;
use log;
use mail_builder::{headers::HeaderType, mime::MimePart};
use mail_parser::{MimeHeaders, PartType};
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
                    match self.decrypt(&msg) {
                        Ok(decrypted) => out_tx.send(decrypted).await?,
                        Err(e) => {
                            log::error!("Decryption failed: {}", e);
                        }
                    }
                }
                Some(msg) = out_rx.recv() => {
                    match self.encrypt(&msg) {
                        Ok(encrypted) => in_tx.send(encrypted).await?,
                        Err(e) => {
                            log::error!("Encryption failed: {}", e);
                        }
                    }
                }
            }
        }
    }

    fn decrypt(&self, mes: &MessageFile) -> Result<MessageFile, anyhow::Error> {
        let mail = mail_parser::MessageParser::default()
            .parse(mes.message_file.as_ref())
            .ok_or_else(|| anyhow::anyhow!("Failed to parse email message"))?;
        let ct = mail
            .content_type()
            .ok_or(anyhow::anyhow!("Content-Type missing"))?;
        let typ = ct.ctype();
        let subtyp = ct.subtype().ok_or(anyhow::anyhow!("Subtype missing"))?;
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
                .ok_or_else(|| anyhow::anyhow!("Encrypted part not found"))?;
            let content = match part.body {
                PartType::Text(ref text) => Bytes::copy_from_slice(text.as_bytes()),
                PartType::Binary(ref data) => Bytes::copy_from_slice(data.as_ref()),
                _ => {
                    return Err(anyhow::anyhow!(
                        "Unsupported content type in encrypted part"
                    ));
                }
            };
            let decrypted = self.helper.decrypt_message(content.as_ref())?;
            let inner_mail = mail_parser::MessageParser::default()
                .parse(&decrypted)
                .ok_or_else(|| anyhow::anyhow!("Failed to parse decrypted message"))?;
            let real_addr = inner_mail
                .from()
                .ok_or_else(|| anyhow::anyhow!("From header missing in decrypted message"))?
                .iter()
                .filter_map(|i| i.address())
                .collect::<Vec<_>>();
            if real_addr.len() != 1 {
                return Err(anyhow::anyhow!(
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
            return Err(anyhow::anyhow!("Signed messages are not supported"));
        } else if self.pass_raw {
            log::info!("Decrypting message from {} (Non-encrypted)", mes.client);
            return Ok(mes.clone());
        } else {
            return Err(anyhow::anyhow!(
                "Message is not encrypted/signed and pass_raw is false"
            ));
        }
    }

    fn encrypt(&self, msg: &MessageFile) -> Result<MessageFile, anyhow::Error> {
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
                    Err(anyhow::anyhow!("Encryption failed: {}", e))
                }
            }
        }?;
        let encrypted = String::from_utf8(encrypted_bytes)?;
        log::info!("Encrypting message to {} (pgp-encrypted)", msg.client);
        let info = msg.info.as_ref().unwrap().clone();
        let mut out_mail = mail_builder::MessageBuilder::new()
            .from(info.from)
            .to(info.to)
            .subject(info.subject)
            .body(MimePart::new(
                "multipart/encrypted; protocol=\"application/pgp-encrypted\"",
                vec![
                    MimePart::new("application/pgp-encrypted", "Version: 1"),
                    MimePart::new("application/octet-stream", encrypted).header(
                        "Content-Disposition",
                        HeaderType::Text("attachment; filename=m.asc".into()),
                    ),
                ],
            ));
        if let Some(in_reply) = info.in_reply_to {
            out_mail = out_mail.in_reply_to(in_reply);
        }
        if let Some(reference) = info.reference {
            out_mail = out_mail.header(
                "References",
                mail_builder::headers::HeaderType::Address(reference.into()),
            );
        }
        Ok(MessageFile {
            client: msg.client.clone(),
            info: None,
            message_file: Bytes::from(out_mail.write_to_vec()?),
        })
    }
}
