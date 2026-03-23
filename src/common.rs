use std::borrow::Cow;

use bytes::Bytes;
use mail_parser::PartType;

pub fn compact_u32(mut n: u32) -> String {
    let mut buf = [0u8; 5];
    for i in (0..5).rev() {
        buf[i] = (n % 85) as u8 + 38; // &..z in ASCII
        buf[i] -= (buf[i] == 47) as u8 * (47 - 37); // / -> %
        n /= 85;
    }
    unsafe { String::from_utf8_unchecked(buf.to_vec()) }
}

#[derive(Debug, Clone)]
pub struct MessageFile {
    pub client: String,
    //server: String,
    pub info: Option<MailInfo>,
    pub file: Bytes,
}

#[derive(Debug, Clone)]
pub struct MailInfo {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub message_id: Option<String>,
    pub reference: Option<Vec<String>>,
    pub in_reply_to: Option<String>,
    pub flow_id: Option<String>,
    pub typ: Option<String>,
    pub args: Option<String>,
}

impl MailInfo {
    pub fn from_bytes(bytes: Bytes) -> Result<(Bytes, MailInfo), anyhow::Error> {
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
            let mut subject = mail
                .subject()
                .ok_or(anyhow::anyhow!("Subject not found"))?
                .to_string();
            let subject_parts = subject.split("/").collect::<Vec<_>>();
            if subject_parts.len() != 3 {
                return Err(anyhow::anyhow!("Subject format is invalid"));
            }
            let mut arg = subject_parts[1].to_string();
            let magic_parts = subject_parts[1].split_whitespace().collect::<Vec<_>>();
            let mut id = magic_parts
                .iter()
                .find(|s| s.starts_with("#"))
                .map(|i| i.to_string());
            let typ = magic_parts
                .iter()
                .find(|s| !s.starts_with("#"))
                .map(|i| i.to_string());
            if let Some(ref id_val) = id {
                arg = arg.replacen(id_val, "", 1);
            } else {
                id = Some(compact_u32(rand::random()));
                subject = format!(
                    "{}/#{} {}/{}",
                    subject_parts[0],
                    id.as_ref().unwrap(),
                    subject_parts[1],
                    subject_parts[2]
                );
            }
            let id = id.map(|i| i.replace("#", ""));
            if let Some(ref typ_val) = typ {
                arg = arg.replacen(typ_val, "", 1);
            }
            let arg = arg.trim().to_string();
            (
                body,
                Self {
                    subject,
                    message_id: mail.message_id().map(|s| s.to_string()),
                    reference,
                    in_reply_to,
                    from,
                    to,
                    typ,
                    flow_id: id,
                    args: Some(arg),
                },
            )
        };
        return Ok(result);
    }

    pub fn create_reply(self) -> Self {
        Self {
            subject: self.subject.clone(),
            message_id: None,
            reference: self.reference.clone(),
            in_reply_to: self.message_id.clone(),
            from: self.to,
            to: self.from,
            flow_id: self.flow_id,
            typ: self.typ,
            args: self.args,
        }
    }

    pub fn to_file(self, body: Bytes) -> Result<Bytes, anyhow::Error> {
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
        let mut buf = Vec::with_capacity((body.len() + 2) / 3 * 4 + 1024);
        mail.write_to(&mut buf)?;
        buf.shrink_to_fit();
        Ok(Bytes::from(buf))
    }
}
