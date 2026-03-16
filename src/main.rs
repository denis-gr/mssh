mod dispatcher;
//mod echo;
mod common;
mod jmap_transport;
mod opengpg_utils;
mod security_layer;
mod terminal;

use serde::Deserialize;
use tokio::{sync::mpsc::channel, task::JoinSet};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::common::MessageFile;

#[derive(Deserialize)]
struct Config {
    email: String,
    username: String,
    password: String,
    proxy: Option<String>,
    pgp_keys_dir: String,
    pgp_password: Option<String>,
    tick_interval_seconds: u64,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::new("warn,mssh=trace"))
        .init();

    let config_str = std::fs::read_to_string("config.toml")?;
    let config: Config = toml::from_str(&config_str)?;

    let transport = jmap_transport::JmapTransport::new(
        &config.email,
        &config.username,
        &config.password,
        config.proxy,
    )
    .await?;
    let security =
        security_layer::SecurityLayer::load(&config.pgp_keys_dir, config.pgp_password, true)?;
    let mut dispatcher =
        dispatcher::Dispatcher::new(std::time::Duration::from_secs(config.tick_interval_seconds))?;
    let (t_in_tx, t_in_rx) = channel::<MessageFile>(128);
    let (t_out_tx, t_out_rx) = channel::<MessageFile>(128);
    let (s_in_tx, s_in_rx) = channel::<MessageFile>(128);
    let (s_out_tx, s_out_rx) = channel::<MessageFile>(128);

    let mut set = JoinSet::new();
    set.spawn(async move { transport.run(t_in_tx, t_out_rx).await });

    set.spawn(async move { security.run(t_out_tx, t_in_rx, s_in_tx, s_out_rx).await });

    set.spawn(async move { dispatcher.run(s_out_tx, s_in_rx).await });

    tokio::select! {
        res = set.join_next() => {
            if let Some(Err(result)) = res {
                log::error!("Task failed: {}", result);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            log::info!("Ctrl+C received, shutting down...");
        }
    }
    Ok(())
}
