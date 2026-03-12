use anyhow::anyhow;
use async_sse::decode;
use futures_lite::StreamExt;
use futures_lite::io::BufReader;
use isahc::AsyncReadResponseExt;
use isahc::auth::{Authentication, Credentials};
use isahc::config::RedirectPolicy;
use isahc::http::Uri;
use isahc::{HttpClient, prelude::*};
use mail_builder::headers::HeaderType;
use mail_builder::mime::MimePart;
use mail_parser::{MimeHeaders, PartType};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::Duration;
use tokio::sync::mpsc;
mod opengpg_utils;

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct EmailGetResponse {
    state: String,
    list: Vec<EmailGetResponseItem>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct EmailGetResponseItem {
    blob_id: String,
    from: Vec<EmailAddress>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct EmailAddress {
    email: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct UploadResponse {
    blob_id: String,
}

fn find_response(json_value: &Value) -> Result<&Value, anyhow::Error> {
    json_value
        .get("methodResponses")
        .and_then(|arr| arr.as_array())
        .and_then(|arr| {
            arr.iter().find(|item| {
                item.get(2)
                    .and_then(|v| v.as_str())
                    .map(|s| s == "a")
                    .unwrap_or(false)
            })
        })
        .and_then(|item| item.get(1))
        .ok_or_else(|| anyhow!("No response found in JSON"))
}

fn get_identity(json: &Value, email: &str) -> Result<String, anyhow::Error> {
    find_response(json)?
        .get("list")
        .and_then(|list| list.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|item| {
                item.get("email")
                    .and_then(|e| e.as_str())
                    .filter(|&e| e == email)
                    .and_then(|_| {
                        item.get("id")
                            .and_then(|id| id.as_str())
                            .map(|s| s.to_string())
                    })
            })
        })
        .ok_or(anyhow!("No identity found for email: {}", email))
}

fn get_folder(json: &Value, role: &str) -> Result<String, anyhow::Error> {
    find_response(json)?
        .get("list")
        .and_then(|list| list.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|item| {
                item.get("role")
                    .and_then(|r| r.as_str())
                    .filter(|&r| r == role)
                    .and_then(|_| {
                        item.get("id")
                            .and_then(|id| id.as_str())
                            .map(|s| s.to_string())
                    })
            })
        })
        .ok_or_else(|| anyhow!("No folder found for role: {}", role))
}

fn get_last_mes_request(aid: &str) -> String {
    json!({"@type": "Request",
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [
            ["Email/query", {"accountId": aid, "sort": [{
                "property": "receivedAt", "isAscending": false}], "limit": 1}, "t"],
            ["Email/get", {"accountId": aid, "#ids": {"resultOf": "t",
                "name": "Email/query", "path": "/ids"}, "properties": ["from", "blobId"]}, "a"]
        ]
    })
    .to_string()
}

fn get_new_mes_request(aid: &str, last_state: &str) -> String {
    json!({"@type": "Request",
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [
            ["Email/changes", {"accountId": aid, "sinceState": last_state}, "t"],
            ["Email/get", {"accountId": aid, "#ids": {"resultOf": "t",
                "name": "Email/changes", "path": "/created"}, "properties": ["from", "blobId"]}, "a"]
        ]
    })
    .to_string()
}

fn get_identity_id_request(aid: &str) -> String {
    json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:submission"],
        "methodCalls": [["Identity/get", {"accountId": aid}, "a"]],
    })
    .to_string()
}

fn get_folders_request(aid: &str) -> String {
    json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [["Mailbox/get", {"accountId": aid}, "a"]],
    })
    .to_string()
}

fn get_send_blob_as_email_request(aid: &str, iid: &str, fid: &str, bid: &str) -> String {
    json!({
        "using": [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:mail",
            "urn:ietf:params:jmap:submission"
        ],
        "methodCalls": [
            [
                "Email/import",
                {
                    "accountId": aid,
                    "emails": {
                        "0": { "blobId": bid, "mailboxIds": { fid: true } }
                    }
                },
                "t"
            ],
            [
                "EmailSubmission/set",
                {
                    "accountId": aid,
                    "create": {
                        "1": {
                            "identityId": iid,
                            "emailId": "#0"
                        }
                    },
                },
                "a"
            ],
            [
                "Email/set",
                {
                    "accountId": aid,
                    "destroy": ["#b/created/1/emailId"]
                },
                "b"
            ]
        ]
    })
    .to_string()
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct JmapSettings {
    pub api_url: String,
    pub upload_url: String,
    pub download_url: String,
    pub event_source_url: String,
    #[serde(default)]
    accounts: HashMap<String, serde::de::IgnoredAny>,
}

impl JmapSettings {
    fn account_id(&self) -> Result<&String, anyhow::Error> {
        self.accounts.keys().next().ok_or(anyhow!("No aid found"))
    }
}

struct JmapTransport {
    jmap_session: JmapSettings,
    client: HttpClient,
    last_sse_state: String,
    domain: String,
    email: String,
    iid: String,
    fid: String,
    aid: String,
    pgp_helper: Option<opengpg_utils::Helper>,
}

struct IncomingMessage {
    from: String,
    to: String,
    subject: String,
    body: String,
    message_id: String,
    references: String,
}

struct PtySession {
    writer: Box<dyn Write + Send>,
    _child: Box<dyn portable_pty::Child + Send>,
}

impl JmapTransport {
    fn start_pty_session() -> Result<(PtySession, mpsc::Receiver<Vec<u8>>), anyhow::Error> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system.openpty(PtySize {
            rows: 30,
            cols: 160,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-i");
        cmd.env("TERM", "dumb");
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        writer.write_all(b"stty -echo\n")?;
        writer.flush()?;

        let (tx, rx) = mpsc::channel::<Vec<u8>>(128);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok((
            PtySession {
                writer,
                _child: child,
            },
            rx,
        ))
    }

    fn write_email_to_pty(
        &self,
        pty: &mut PtySession,
        incoming: &IncomingMessage,
    ) -> Result<(), anyhow::Error> {
        pty.writer.write_all(incoming.body.as_bytes())?;
        pty.writer.flush()?;
        Ok(())
    }

    async fn new(
        email: &str,
        username: &str,
        password: &str,
        proxy: Option<String>,
        pgp_password: Option<String>,
    ) -> Result<Self, anyhow::Error> {
        let client = HttpClient::builder()
            .proxy(proxy.map(|url| url.parse::<Uri>()).transpose()?)
            .authentication(Authentication::basic())
            .credentials(Credentials::new(username, password))
            .redirect_policy(RedirectPolicy::Follow)
            .build()?;

        let domain = email.split('@').nth(1).unwrap().to_string();

        let mut res = client
            .get_async(&format!("https://{}/.well-known/jmap", domain))
            .await?;
        let mut settings: JmapSettings = res.json().await?;
        let aid = settings.account_id().unwrap().clone();
        settings.event_source_url = settings
            .event_source_url
            .replace("{types}", "*")
            .replace("{closeafter}", "no")
            .replace("{ping}", "15");
        settings.download_url = settings.download_url.replace("{accountId}", aid.as_str());
        settings.upload_url = settings.upload_url.replace("{accountId}", aid.as_str());

        let body = get_last_mes_request(aid.as_str());
        let mut response = client.post_async(&settings.api_url, body).await?;
        let json: Value = response.json::<Value>().await?;
        let resp = serde_json::from_value::<EmailGetResponse>(find_response(&json)?.clone())?;

        let last_sse_state = resp.state;

        let mut response = client
            .post_async(&settings.api_url, get_identity_id_request(&aid))
            .await?;
        let iid = get_identity(&response.json::<Value>().await?, email)?;

        let mut response = client
            .post_async(&settings.api_url, get_folders_request(&aid))
            .await?;
        let fid = get_folder(&response.json::<Value>().await?, "sent")?;

        let helper = opengpg_utils::Helper::load_dir("keys", pgp_password).ok();

        Ok(Self {
            jmap_session: settings,
            client,
            last_sse_state,
            domain,
            email: email.to_string(),
            iid,
            fid,
            aid,
            pgp_helper: helper,
        })
    }

    async fn run(&mut self) -> Result<(), anyhow::Error> {
        let sse_response = self
            .client
            .get_async(self.jmap_session.event_source_url.clone())
            .await?;
        let reader = BufReader::new(sse_response.into_body());
        let mut sse_reader = decode(reader);
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        let (mut pty, mut pty_output_rx) = Self::start_pty_session()?;
        let mut outgoing_buffer = String::new();
        let mut current_recipient: Option<String> = None;
        let mut current_subject: Option<String> = None;
        let mut current_in_reply_to: Option<String> = None;
        let mut current_references: Option<String> = None;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !outgoing_buffer.trim().is_empty() {
                        if let Some(to) = current_recipient.clone() {
                            let body = std::mem::take(&mut outgoing_buffer);
                            let subject = current_subject
                                .clone()
                                .unwrap_or_else(|| format!("Terminal message from {}", self.email));
                            let in_reply_to = current_in_reply_to.as_deref();
                            let references = current_references.as_deref();
                            self.send_text_email(&to, &subject, &body, in_reply_to, references).await?;
                        }
                    }
                }
                maybe_chunk = pty_output_rx.recv() => {
                    match maybe_chunk {
                        Some(chunk) => {
                            let text = String::from_utf8_lossy(&chunk);
                            outgoing_buffer.push_str(&text);
                        }
                        None => break,
                    }
                }
                event = sse_reader.next() => {
                    let Some(event) = event else {
                        break;
                    };

                    match event {
                        Ok(async_sse::Event::Message(msg)) => {
                            if msg.name() != "state" {
                                continue;
                            }
                            let mut resp = self
                                .client
                                .post_async(
                                    &self.jmap_session.api_url,
                                    get_new_mes_request(&self.aid, &self.last_sse_state),
                                )
                                .await?;
                            let json = resp.json::<Value>().await?;
                            let val = find_response(&json)?.clone();
                            let resp = serde_json::from_value::<EmailGetResponse>(val)?;
                            self.last_sse_state = resp.state;
                            let url_template = self.jmap_session.download_url.clone();
                            for item in resp.list {
                                let from = item.from.last().map(|a| a.email.as_str()).unwrap_or("");
                                if from.contains(&("@".to_owned() + &self.domain)) {
                                    continue;
                                }
                                let url = url_template.replace("{blobId}", &item.blob_id);
                                let mut resp = self.client.get_async(&url).await?;
                                let text = resp.text().await?;
                                if let Some(incoming) = self.parse_incoming_message(&text)? {
                                    self.write_email_to_pty(&mut pty, &incoming)?;
                                    current_recipient = Some(incoming.from.clone());
                                    current_subject = Some(incoming.subject);
                                    current_in_reply_to = Some(incoming.message_id.clone());
                                    current_references = Some(if incoming.references.trim().is_empty() {
                                        incoming.message_id.clone()
                                    } else {
                                        format!("{} {}", incoming.references.trim(), incoming.message_id)
                                    });
                                }
                            }
                        }
                        Ok(async_sse::Event::Retry(duration)) => println!("Retry after: {:?}", duration),
                        Err(e) => eprintln!("Error in SSE stream: {}", e),
                    }
                }
            }
        }

        Ok(())
    }

    fn parse_incoming_message(&self, mes: &str) -> Result<Option<IncomingMessage>, anyhow::Error> {
        let mail = mail_parser::MessageParser::default()
            .parse(mes.as_bytes())
            .ok_or_else(|| anyhow::anyhow!("Failed to parse email message"))?;
        println!("{:?}", mail);

        if let Some(helper) = &self.pgp_helper {
            if let Some(ct) = mail.content_type() {
                if ct.c_type == "multipart" && ct.c_subtype == Some("encrypted".into()) {
                    let encrypted_part = mail
                        .attachments()
                        .find(|att| {
                            att.content_type()
                                .map(|ct| {
                                    ct.c_type == "application"
                                        && ct.c_subtype == Some("octet-stream".into())
                                })
                                .unwrap_or(false)
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "No PGP encrypted part found in multipart/encrypted email"
                            )
                        })?;
                    println!("Found encrypted part: {:?}", encrypted_part);
                    let encrypted_content = match encrypted_part.body {
                        PartType::Text(ref text) => text.clone(),
                        PartType::Binary(ref data) => String::from_utf8_lossy(data),
                        _ => {
                            return Err(anyhow::anyhow!(
                                "Unsupported content type in encrypted part"
                            ));
                        }
                    }
                    .to_string();
                    println!("Encrypted content: {}", encrypted_content);
                    let decrypted_content = helper.decrypt_message(encrypted_content)?;
                    println!("Decrypted content: {}", decrypted_content);
                    return self.parse_incoming_message(&decrypted_content);
                }
            }
        }
        
        println!("{:?}", mail);
        let from = mail
            .from()
            .and_then(|v| v.last())
            .and_then(|v| v.address())
            .unwrap_or_default()
            .to_string();
        let to = mail
            .to()
            .and_then(|v| v.last())
            .and_then(|v| v.address())
            .unwrap_or_default()
            .to_string();

        if from.is_empty() || to.is_empty() {
            return Ok(None);
        }

        let body = mail
            .body_text(0)
            .map(|v| v.into_owned())
            .unwrap_or_else(|| "(no text body)".to_string());

        Ok(Some(IncomingMessage {
            from,
            to,
            subject: mail.subject().unwrap_or("No subject").to_string(),
            body,
            message_id: mail.message_id().unwrap_or_default().to_string(),
            references: mail.references().as_text().unwrap_or_default().to_string(),
        }))
    }

    async fn send_text_email(
        &self,
        to: &str,
        subject: &str,
        body: &str,
        in_reply_to: Option<&str>,
        references: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        let mut new_mail = mail_builder::MessageBuilder::new()
            .from(self.email.as_str())
            .to(to)
            .subject(subject)
            .header(
                "References",
                HeaderType::Text(references.unwrap_or_default().to_string().into()),
            )
            .in_reply_to(in_reply_to.unwrap_or_default())
            .body(MimePart::new("text/plain", body.as_bytes().to_vec()))
            .write_to_string()
            .map_err(|e| anyhow::anyhow!("Failed to build response email: {}", e))?;
        if let Some(helper) = &self.pgp_helper {
            if helper.has_client_cert(to) {
                println!("Encrypting email to {}", to);
                let encrypted_mail = helper.encrypt_message(new_mail.clone(), to.to_string())?;
                new_mail = mail_builder::MessageBuilder::new()
                    .from(self.email.as_str())
                    .to(to)
                    .subject(subject)
                    .header(
                        "References",
                        HeaderType::Text(references.unwrap_or_default().to_string().into()),
                    )
                    .in_reply_to(in_reply_to.unwrap_or_default())
                    .body(MimePart::new(
                        "multipart/encrypted; protocol=\"application/pgp-encrypted\"",
                        vec![
                            MimePart::new("application/pgp-encrypted", b"Version: 1".to_vec())
                                .header(
                                    "Content-Description",
                                    HeaderType::Text("PGP/MIME version identification".into()),
                                ),
                            MimePart::new("application/octet-stream", encrypted_mail.into_bytes())
                                .header(
                                    "Content-Description",
                                    HeaderType::Text("OpenPGP encrypted message".into()),
                                )
                                .header(
                                    "Content-Disposition",
                                    HeaderType::Text("attachment; filename=\"msg.asc\"".into()),
                                ),
                        ],
                    ))
                    .write_to_string()
                    .map_err(|e| anyhow::anyhow!("Failed to build encrypted email: {}", e))?;
            }
        }
        let settings = &self.jmap_session;
        let mut response = self
            .client
            .post_async(&settings.upload_url, new_mail)
            .await?;
        let upload_resp = response.json::<UploadResponse>().await?;
        let new_blob_id = upload_resp.blob_id;
        let mut response = self
            .client
            .post_async(
                &settings.api_url,
                get_send_blob_as_email_request(&self.aid, &self.iid, &self.fid, &new_blob_id),
            )
            .await?;
        let val: Value = response.json().await?;
        if let Some(not_created) = find_response(&val)?.get("notCreated") {
            println!("Failed to send email: {}", not_created);
        } else {
            println!("Email sent successfully");
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Config {
    email: String,
    username: String,
    password: String,
    proxy: Option<String>,
    pgp_password: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let config_str = std::fs::read_to_string("config.toml")?;
    let config: Config = toml::from_str(&config_str)?;

    let mut transport = JmapTransport::new(
        &config.email,
        &config.username,
        &config.password,
        config.proxy,
        config.pgp_password,
    )
    .await?;

    transport.run().await
}
