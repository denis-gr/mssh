use crate::common::MessageFile;
use anyhow::anyhow;
use async_sse::decode;
use bytes::Bytes;
use futures_lite::StreamExt;
use futures_lite::io::BufReader;
use isahc::AsyncReadResponseExt;
use isahc::auth::{Authentication, Credentials};
use isahc::config::RedirectPolicy;
use isahc::http::Uri;
use isahc::{HttpClient, prelude::*};
use log;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::sync::mpsc;

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

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct IdentityResponseItem {
    id: String,
    email: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct IdentityResponse {
    list: Vec<IdentityResponseItem>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct FolderResponseItem {
    id: String,
    role: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct FolderResponse {
    list: Vec<FolderResponseItem>,
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
    serde_json::from_value::<IdentityResponse>(find_response(&json)?.clone())?
        .list
        .into_iter()
        .find(|item| item.email == email)
        .map(|item| item.id)
        .ok_or_else(|| anyhow!("No identity found for email: {}", email))
}

fn get_folder(json: &Value, role: &str) -> Result<String, anyhow::Error> {
    serde_json::from_value::<FolderResponse>(find_response(&json)?.clone())?
        .list
        .into_iter()
        .find(|item| item.role == role)
        .map(|item| item.id)
        .ok_or_else(|| anyhow!("No folder found for role: {}", role))
}

fn get_last_mes_request(aid: &str) -> String {
    json!({"@type": "Request",
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [
            ["Email/query", {"accountId": aid, "sort": [{
                "property": "receivedAt", "isAscending": false}], "limit": 1}, "t"],
            ["Email/get", {"accountId": aid, "#ids": {"resultOf": "t",
                "name": "Email/query", "path": "/ids", "properties": ["from", "blobId"] }}, "a"]
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

pub struct JmapTransport {
    jmap_session: JmapSettings,
    client: HttpClient,
    last_sse_state: String,
    iid: String,
    fid: String,
    aid: String,
    email: String,
}

impl JmapTransport {
    pub async fn new(
        email: &str,
        username: &str,
        password: &str,
        proxy: Option<String>,
    ) -> Result<Self, anyhow::Error> {
        let client = HttpClient::builder()
            .proxy(proxy.map(|url| url.parse::<Uri>()).transpose()?)
            .authentication(Authentication::basic())
            .credentials(Credentials::new(username, password))
            .redirect_policy(RedirectPolicy::Follow)
            .build()?;

        let domain = email
            .split('@')
            .nth(1)
            .map(|s| s.to_string())
            .ok_or(anyhow::anyhow!("Email incorrect"))?;

        let mut res = client
            .get_async(&format!("https://{}/.well-known/jmap", domain))
            .await?;
        let mut settings: JmapSettings = res.json().await?;
        let aid = settings.account_id()?.clone();
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

        Ok(Self {
            jmap_session: settings,
            client,
            last_sse_state,
            iid,
            fid,
            aid,
            email: email.to_string(),
        })
    }

    pub async fn run(
        mut self,
        outgoing: mpsc::Sender<MessageFile>,
        mut incoming: mpsc::Receiver<MessageFile>,
    ) -> Result<(), anyhow::Error> {
        let sse_response = self
            .client
            .get_async(self.jmap_session.event_source_url.clone())
            .await?;
        let reader = BufReader::new(sse_response.into_body());
        let mut sse_reader = decode(reader);
        loop {
            tokio::select! {
                Some(msg) = incoming.recv() => {
                    log::info!("Sending email to {}", msg.client);
                    self.send_message(msg).await?;
                }
                event = sse_reader.next() => {
                    match event {
                        Some(Ok(async_sse::Event::Message(msg))) => {
                            if msg.name() != "state" {
                                continue;
                            }
                            log::info!("New SSE state");
                            self.download_messages(&outgoing).await?;
                        }
                        Some(Err(e)) => {
                            log::warn!("Error reading SSE: {}", e);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn send_message(&self, msg: MessageFile) -> Result<(), anyhow::Error> {
        let settings = &self.jmap_session;
        let mut response = self
            .client
            .post_async(&settings.upload_url, msg.message_file.as_ref())
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
            log::warn!("Failed to send email: {}", not_created);
        } else {
            log::info!("Email sent successfully");
        }
        Ok(())
    }

    async fn download_messages(
        &mut self,
        tx: &mpsc::Sender<MessageFile>,
    ) -> Result<(), anyhow::Error> {
        let mut r = self
            .client
            .post_async(
                &self.jmap_session.api_url,
                get_new_mes_request(&self.aid, &self.last_sse_state),
            )
            .await?;
        let json = r.json::<Value>().await?;
        let val = find_response(&json)?.clone();
        let resp = serde_json::from_value::<EmailGetResponse>(val)?;
        self.last_sse_state = resp.state;
        let url_template = self.jmap_session.download_url.clone();
        for item in resp.list {
            if item.from.len() != 1 {
                continue;
            }
            if item.from[0].email == self.email {
                continue;
            }
            let url = url_template.replace("{blobId}", &item.blob_id);
            let mut resp = self.client.get_async(&url).await?;
            log::info!("Downloading email from {}", item.from[0].email);
            tx.send(MessageFile {
                client: item.from.last().unwrap().email.clone(), // len == 1
                info: None,
                message_file: Bytes::from(resp.bytes().await?),
            })
            .await?;
        }
        Ok(())
    }
}
