use anyhow::{anyhow, Context, Result};
use clap::Parser;
use rdproto::ControlMessage;
use tracing::{error, info};
use tracing_subscriber::prelude::*;

mod capture;
mod encode;
mod input;

/// Windows host agent for the custom remote desktop protocol. Phase 1 MVP: single
/// display (real or a manually-configured virtual monitor), no clipboard/audio.
#[derive(Parser, Debug)]
struct Args {
    /// Address to listen on, e.g. 0.0.0.0:5900
    #[arg(long, default_value = "0.0.0.0:5900")]
    listen: std::net::SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to both stdout (for whoever's watching the terminal) and rdhost.log next to the
    // executable — an absolute path derived from current_exe(), not CWD, so it's the same
    // file regardless of where the binary was launched from (and can be pulled over SSH
    // without needing someone to copy-paste from the terminal).
    let log_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| ".".into());
    eprintln!("logging to {}", log_dir.join("rdhost.log").display());
    let file_appender = tracing_appender::rolling::never(&log_dir, "rdhost.log");
    let (file_writer, _guard) = tracing_appender::non_blocking(file_appender);
    let env_filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(env_filter()))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer)
                .with_filter(env_filter()),
        )
        .init();

    let args = Args::parse();

    let dirs = directories::ProjectDirs::from("dev", "trowbridge", "rdhost")
        .ok_or_else(|| anyhow!("could not determine config directory"))?;
    let cert_path = dirs.data_dir().join("host_cert.der");
    let key_path = dirs.data_dir().join("host_key.der");

    let identity = rdnet::load_or_generate_host_identity(
        &cert_path,
        &key_path,
        vec!["cwtrow".to_string(), "localhost".to_string()],
    )?;
    let fingerprint = rdnet::fingerprint_hex(&identity.cert_der);
    info!(fingerprint, "host certificate ready — pass this to the client with --fingerprint");

    let server_config = rdnet::build_server_endpoint_config(identity)?;
    let endpoint = quinn::Endpoint::server(server_config, args.listen)?;
    info!(listen = %args.listen, "listening");

    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            match incoming.await {
                Ok(connection) => {
                    if let Err(e) = handle_connection(connection).await {
                        error!("connection handler failed: {e:?}");
                    }
                }
                Err(e) => error!(error = %e, "failed to accept connection"),
            }
        });
    }

    Ok(())
}

async fn handle_connection(connection: quinn::Connection) -> Result<()> {
    info!(remote = %connection.remote_address(), "client connecting");
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .context("waiting for client's control stream")?;

    match recv_control(&mut recv).await? {
        ControlMessage::ClientHello { protocol_version } => {
            let accepted = protocol_version == rdproto::PROTOCOL_VERSION;
            send_control(
                &mut send,
                &ControlMessage::ServerHello {
                    protocol_version: rdproto::PROTOCOL_VERSION,
                    accepted,
                },
            )
            .await?;
            if !accepted {
                return Err(anyhow!(
                    "client protocol version {protocol_version} != {}",
                    rdproto::PROTOCOL_VERSION
                ));
            }
        }
        other => return Err(anyhow!("expected ClientHello, got {other:?}")),
    }
    info!("client accepted");

    match recv_control(&mut recv).await? {
        ControlMessage::Topology(topology) => {
            info!(monitors = topology.monitors.len(), "client topology received");
            // Phase 1 MVP: no virtual display driver yet, so we don't reconfigure
            // anything here — we just stream whatever display `capture` is pointed at.
            send_control(&mut send, &ControlMessage::TopologyAck).await?;
        }
        other => return Err(anyhow!("expected Topology, got {other:?}")),
    }

    let capture_task = tokio::spawn(capture::run(connection.clone()));
    let input_task = tokio::spawn(input::run(recv));

    tokio::select! {
        res = capture_task => res??,
        res = input_task => res??,
    }

    Ok(())
}

async fn send_control(send: &mut quinn::SendStream, msg: &ControlMessage) -> Result<()> {
    let framed = rdproto::encode_control_message(msg)?;
    send.write_all(&framed).await?;
    Ok(())
}

async fn recv_control(recv: &mut quinn::RecvStream) -> Result<ControlMessage> {
    let mut len_bytes = [0u8; 4];
    recv.read_exact(&mut len_bytes)
        .await
        .context("reading control message length")?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body)
        .await
        .context("reading control message body")?;
    rdproto::decode_control_message(&body)
}
