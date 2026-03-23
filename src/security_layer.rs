use crate::common::MessageFile;
use crate::opengpg_utils::Helper;

use anyhow::anyhow;
use bytes::Bytes;
use mail_parser::{MimeHeaders, PartType};
use std::io::Write;

pub struct SecurityLayer {
    h: Helper,
    pass_raw: bool,
}

impl SecurityLayer {
    pub fn load(
        path: &str,
        pgp_password: Option<String>,
        pass_raw: bool,
    ) -> Result<Self, anyhow::Error> {
        Ok(Self {
            h: Helper::load_dir(path, pgp_password)?,
            pass_raw: pass_raw,
        })
    }

    pub fn decrypt(&self, mes: MessageFile) -> Result<MessageFile, anyhow::Error> {
        let mail = mail_parser::MessageParser::default()
            .parse(mes.file.as_ref())
            .ok_or(anyhow!("Failed to parse email message"))?;
        let ct = mail.content_type().ok_or(anyhow!("Content-Type missing"))?;
        let typ = ct.ctype();
        let subtyp = ct.subtype().ok_or(anyhow!("Subtype missing"))?;
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
                .ok_or(anyhow!("Encrypted part not found"))?;
            let content = match part.body {
                PartType::Text(ref text) => text.as_bytes(),
                PartType::Binary(ref data) => data.as_ref(),
                _ => {
                    return Err(anyhow!("Unsupported content type in encrypted part"));
                }
            };
            let decrypted = self.h.decrypt_message(content)?;
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
                file: Bytes::from(decrypted),
            });
        } else if typ == "multipart"
            && subtyp == "signed"
            && protocol == Some("application/pgp-signature")
        {
            // TODO: support signed messages
            return Err(anyhow!("Signed messages are not supported"));
        } else if self.pass_raw {
            return Ok(mes.clone());
        } else {
            return Err(anyhow!("Message unencrypted/unsigned and !pass_raw"));
        }
    }

    pub fn encrypt(&self, msg: MessageFile) -> Result<MessageFile, anyhow::Error> {
        let email = msg.client.clone();
        let val = match self.h.encrypt_message(msg.file.as_ref(), email) {
            Ok(encrypted) => Ok(encrypted),
            Err(e) if self.pass_raw => return Ok(msg),
            Err(e) => Err(anyhow!("Encryption failed: {}", e)),
        }?;
        let encrypted = String::from_utf8(val)?;
        let encrypted_len = encrypted.len();
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
            let r: Vec<String> = r.iter().map(|s| format!("<{}>", s)).collect();
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
            file: Bytes::from(b),
        })
    }
}
