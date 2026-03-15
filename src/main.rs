mod dispatcher;
mod echo;
mod jmap_transport;
mod opengpg_utils;
mod security_layer;
//mod terminal;

use anyhow::Context;
use serde::Deserialize;
use std::time::Duration;

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

    let transport = jmap_transport::JmapTransport::new(
        &config.email,
        &config.username,
        &config.password,
        config.proxy,
        Duration::from_secs(5),
    )
    .await?;
    let security = security_layer::SecurityLayer::load("keys", config.pgp_password, true)?;
    let mut dispatcher = dispatcher::Dispatcher::new()?;
    let (t_in_tx, t_in_rx) = jmap_transport::MessageFile::new_channel();
    let (t_out_tx, t_out_rx) = jmap_transport::MessageFile::new_channel();
    let (s_in_tx, s_in_rx) = jmap_transport::MessageFile::new_channel();
    let (s_out_tx, s_out_rx) = jmap_transport::MessageFile::new_channel();

    tokio::spawn(async move { transport.run(t_in_rx, t_out_tx).await });

    tokio::spawn(async move { security.run(t_in_tx, t_out_rx, s_in_tx, s_out_rx).await });

    tokio::spawn(async move { dispatcher.run(s_out_tx, s_in_rx).await });

    tokio::signal::ctrl_c()
        .await
        .context("Failed to listen for Ctrl+C")?;

    Ok(())
}
