use async_sse::decode;
use futures_lite::StreamExt;
use futures_lite::io::BufReader;
use isahc::AsyncReadResponseExt;
use isahc::auth::{Authentication, Credentials};
use isahc::config::RedirectPolicy;
use isahc::{HttpClient, prelude::*};
use mail_builder::headers::HeaderType;
use mail_builder::mime::MimePart;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;

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

fn find_response(json_value: &Value) -> Option<&Value> {
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
}

fn get_identity(json: &Value, email: &str) -> Option<String> {
    find_response(json)
        .and_then(|resp| resp.get("list"))
        .and_then(|list| list.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|item| {
                item.get("email")
                    .and_then(|e| e.as_str())
                    .filter(|&e| e == email)
                    .map(|_| item.get("id").unwrap().as_str().unwrap().to_string())
            })
        })
}

fn get_folder(json: &Value, role: &str) -> Option<String> {
    find_response(json)
        .and_then(|resp| resp.get("list"))
        .and_then(|list| list.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|item| {
                item.get("role")
                    .and_then(|r| r.as_str())
                    .filter(|&r| r == role)
                    .map(|_| item.get("id").unwrap().as_str().unwrap().to_string())
            })
        })
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
    fn account_id(&self) -> Option<&String> {
        self.accounts.keys().next()
    }
}

struct JmapTransport {
    jmap_session: Option<JmapSettings>,
    client: HttpClient,
    last_sse_state: Option<String>,
    domain: String,
    self_email: String,
    identity_id: Option<String>,
    folder_id: Option<String>,
}

impl JmapTransport {
    fn new(
        email: &str,
        username: &str,
        password: &str,
        proxy: Option<String>,
    ) -> Result<Self, anyhow::Error> {
        let client = HttpClient::builder()
            .proxy(proxy.map(|url| url.parse::<isahc::http::Uri>().unwrap()))
            .authentication(Authentication::basic())
            .credentials(Credentials::new(username, password))
            .redirect_policy(RedirectPolicy::Follow)
            .build()?;

        Ok(Self {
            jmap_session: None,
            client,
            last_sse_state: None,
            domain: email.split('@').nth(1).unwrap().to_string(),
            self_email: email.to_string(),
            identity_id: None,
            folder_id: None,
        })
    }

    async fn fetch_setting(&mut self) -> Result<&mut Self, anyhow::Error> {
        let mut response = self
            .client
            .get_async(&format!("https://{}/.well-known/jmap", self.domain))
            .await?;
        let mut settings: JmapSettings = response.json().await?;
        settings.event_source_url = settings
            .event_source_url
            .replace("{types}", "*")
            .replace("{closeafter}", "no")
            .replace("{ping}", "15");
        settings.download_url = settings
            .download_url
            .replace("{accountId}", settings.account_id().unwrap());
        settings.upload_url = settings
            .upload_url
            .replace("{accountId}", settings.account_id().unwrap());
        let body = get_last_mes_request(settings.account_id().unwrap());
        let mut response = self
            .client
            .post_async(settings.api_url.clone(), body)
            .await?;
        let json: Value = response.json::<Value>().await?;
        let resp =
            serde_json::from_value::<EmailGetResponse>(find_response(&json).unwrap().clone())?;

        self.last_sse_state = Some(resp.state);
        self.jmap_session = Some(settings.clone());

        let aid = settings.account_id().unwrap();

        let mut response = self
            .client
            .post_async(settings.api_url.clone(), get_identity_id_request(aid))
            .await?;
        self.identity_id = get_identity(&response.json::<Value>().await.unwrap(), &self.self_email);

        let mut response = self
            .client
            .post_async(settings.api_url.clone(), get_folders_request(aid))
            .await?;
        self.folder_id = get_folder(&response.json::<Value>().await.unwrap(), "sent");

        Ok(self)
    }

    async fn run_echo(&mut self) -> Result<(), anyhow::Error> {
        let settings = self.jmap_session.as_ref().unwrap().clone();
        let sse_response = self.client.get_async(&settings.event_source_url).await?;
        let reader = BufReader::new(sse_response.into_body());
        let mut sse_reader = decode(reader);
        while let Some(event) = sse_reader.next().await {
            match event {
                Ok(async_sse::Event::Message(msg)) => {
                    if msg.name() != "state" {
                        continue;
                    }
                    let aid = settings.account_id().unwrap();
                    let state = self.last_sse_state.clone().ok_or(anyhow::anyhow!(""))?;
                    let mut resp = self
                        .client
                        .post_async(&settings.api_url, get_new_mes_request(aid, &state))
                        .await?;
                    let val: Value = resp.json().await?;
                    let resp = serde_json::from_value::<EmailGetResponse>(
                        find_response(&val).unwrap().clone(),
                    )?;
                    self.last_sse_state = Some(resp.state);
                    for item in resp.list {
                        let from = item.from.last().map(|a| a.email.as_str()).unwrap_or("");
                        if from.contains(&("@".to_owned() + &self.domain)) {
                            continue;
                        }
                        let blob_id = item.blob_id;
                        let url = settings.download_url.replace("{blobId}", &blob_id);
                        let mut resp = self.client.get_async(&url).await?;
                        let text = resp.text().await?;
                        self.handle_new_message(&text).await?;
                    }
                }
                Ok(async_sse::Event::Retry(duration)) => println!("Retry after: {:?}", duration),
                Err(e) => eprintln!("Error in SSE stream: {}", e),
            }
        }
        Ok(())
    }
    async fn handle_new_message(&mut self, mes: &str) -> Result<(), anyhow::Error> {
        let mail = mail_parser::MessageParser::default()
            .parse(mes.as_bytes())
            .ok_or_else(|| anyhow::anyhow!("Failed to parse email message"))?;
        let m_id = mail.message_id().unwrap_or("unknown");
        let from = mail.from().unwrap().last().unwrap().address().unwrap();
        let to = mail.to().unwrap().last().unwrap().address().unwrap();
        let subject = mail.subject().unwrap_or("No subject");
        let references = mail.references().as_text().unwrap_or_default();
        let new_mail = mail_builder::MessageBuilder::new()
            .from(to)
            .to(from)
            .subject(format!("Re: {}", subject))
            .header("References", HeaderType::Text(references.into()))
            .in_reply_to(m_id)
            .body(MimePart::new(
                "text/plain",
                format!("Echoing back your message with ID: {}", m_id).into_bytes(),
            ))
            .write_to_string()
            .map_err(|e| anyhow::anyhow!("Failed to build response email: {}", e))?;
        let settings = self.jmap_session.as_ref().unwrap();
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
                get_send_blob_as_email_request(
                    settings.account_id().unwrap(),
                    &self.identity_id.clone().unwrap(),
                    &self.folder_id.clone().unwrap(),
                    &new_blob_id,
                ),
            )
            .await?;
        let val: Value = response.json().await?;
        if let Some(not_created) = find_response(&val).unwrap().get("notCreated") {
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
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let config_str = std::fs::read_to_string("config.toml")?;
    let config: Config = toml::from_str(&config_str)?;

    let mut _transport = JmapTransport::new(
        &config.email,
        &config.username,
        &config.password,
        config.proxy,
    )?
    .fetch_setting()
    .await?
    .run_echo()
    .await?;
    Ok(())
}
