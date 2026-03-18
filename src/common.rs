use bytes::Bytes;

#[derive(Clone)]
pub struct MailInfo {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub message_id: Option<String>,
    pub reference: Option<Vec<String>>,
    pub in_reply_to: Option<String>,
}

#[derive(Clone)]
pub struct MessageFile {
    pub client: String,
    //server: String,
    pub info: Option<MailInfo>,
    pub message_file: Bytes,
}
