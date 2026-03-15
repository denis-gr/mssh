pub use crate::jmap_transport::MessageFile;
pub use crate::opengpg_utils::Helper;

use mail_builder::{headers::HeaderType, mime::MimePart};
use mail_parser::{HeaderValue, MimeHeaders, PartType};
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
        mut in_rx: Receiver<MessageFile>,
        in_tx: Sender<MessageFile>,
        mut out_rx: Receiver<MessageFile>,
        out_tx: Sender<MessageFile>,
    ) {
        loop {
            tokio::select! {
                Some(msg) = in_rx.recv() => {
                    match self.decrypt(&msg) {
                        Ok(decrypted) => out_tx.send(decrypted).await.unwrap(),
                        Err(e) => {
                            eprintln!("Decryption failed: {}", e);
                        }
                    }
                }
                Some(msg) = out_rx.recv() => {
                    match self.encrypt(&msg) {
                        Ok(encrypted) => in_tx.send(encrypted).await.unwrap(),
                        Err(e) => {
                            eprintln!("Encryption failed: {}", e);
                        }
                    }
                }
            }
        }
    }

    fn decrypt(&self, mes: &MessageFile) -> Result<MessageFile, anyhow::Error> {
        let mail = mail_parser::MessageParser::default()
            .parse(mes.content.as_bytes())
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
                PartType::Text(ref text) => text.clone(),
                PartType::Binary(ref data) => String::from_utf8_lossy(data),
                _ => {
                    return Err(anyhow::anyhow!(
                        "Unsupported content type in encrypted part"
                    ));
                }
            };
            let decrypted = self.helper.decrypt_message(content.to_string())?;
            let inner_mail = mail_parser::MessageParser::default()
                .parse(decrypted.as_bytes())
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
                client: real_addr[0].to_string(),
                content: decrypted,
            });
        } else if typ == "multipart"
            && subtyp == "signed"
            && protocol == Some("application/pgp-signature")
        {
            // TODO: support signed messages
            return Err(anyhow::anyhow!("Signed messages are not supported"));
        } else if self.pass_raw {
            return Ok(mes.clone());
        } else {
            return Err(anyhow::anyhow!(
                "Message is not encrypted/signed and pass_raw is false"
            ));
        }
    }

    fn encrypt(&self, msg: &MessageFile) -> Result<MessageFile, anyhow::Error> {
        let encrypted = match self
            .helper
            .encrypt_message(msg.content.clone(), msg.client.clone())
        {
            Ok(encrypted) => Ok(encrypted),
            Err(e) => {
                eprintln!("Encryption failed for client {}: {}", msg.client, e);
                if self.pass_raw {
                    return Ok(msg.clone());
                } else {
                    Err(anyhow::anyhow!("Encryption failed and pass_raw is false"))
                }
            }
        }?;
        let mail = mail_parser::MessageParser::default()
            .parse(msg.content.as_bytes())
            .ok_or_else(|| anyhow::anyhow!("Failed to parse encrypted message"))?;
        let from = mail
            .to()
            .ok_or(anyhow::anyhow!("To header missing in encrypted message"))?
            .iter()
            .filter_map(|i| i.address())
            .collect::<Vec<_>>();
        if from.len() != 1 {
            return Err(anyhow::anyhow!(
                "Expected exactly one To address in encrypted message"
            ));
        }
        let to = mail
            .from()
            .ok_or(anyhow::anyhow!("From header missing in decrypted message"))?
            .iter()
            .filter_map(|i| i.address())
            .collect::<Vec<_>>();
        if to.len() != 1 {
            return Err(anyhow::anyhow!(
                "Expected exactly one From address in decrypted message"
            ));
        }
        let mut out_mail = mail_builder::MessageBuilder::new()
            .from(to[0].to_string())
            .to(from[0].to_string())
            .subject(mail.subject().unwrap_or_else(|| "...".into()))
            .body(MimePart::new(
                "multipart/encrypted; protocol=\"application/pgp-encrypted\"",
                vec![
                    MimePart::new("application/pgp-encrypted", b"Version: 1".to_vec()).header(
                        "Content-Description",
                        HeaderType::Text("PGP/MIME version identification".into()),
                    ),
                    MimePart::new("application/octet-stream", encrypted.as_bytes().to_vec())
                        .header(
                            "Content-Description",
                            HeaderType::Text("OpenPGP encrypted message".into()),
                        )
                        .header(
                            "Content-Disposition",
                            HeaderType::Text("attachment; filename=\"msg.asc\"".into()),
                        ),
                ],
            ));
        if let HeaderValue::Address(addr) = mail.in_reply_to() {
            let a = addr
                .first()
                .ok_or_else(|| anyhow::anyhow!("In-Reply-To address list is empty"))?
                .address()
                .ok_or_else(|| anyhow::anyhow!("In-Reply-To address is not valid"))?;
            out_mail = out_mail.in_reply_to(a);
        }
        if let Some(header) = mail.header("References") {
            let ref_text = header.as_text().unwrap_or("");
            out_mail = out_mail.header(
                "References",
                mail_builder::headers::HeaderType::Text(ref_text.to_string().into()),
            );
        }
        Ok(MessageFile {
            client: msg.client.clone(),
            content: out_mail.write_to_string()?,
        })
    }
}
