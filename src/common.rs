use bytes::Bytes;

#[derive(Clone)]
pub struct MessageFile {
    pub client: String,
    //server: String,
    pub message_file: Bytes,
}
