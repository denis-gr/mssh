use bytes::Bytes;

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
pub struct MailInfo {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub message_id: Option<String>,
    pub reference: Option<Vec<String>>,
    pub in_reply_to: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MessageFile {
    pub client: String,
    //server: String,
    pub info: Option<MailInfo>,
    pub message_file: Bytes,
}
