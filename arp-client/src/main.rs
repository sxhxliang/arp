mod config;
mod handlers;
mod jsonrpc;
mod router;
mod routes;
mod session;

use anyhow::{Result, anyhow};
use clap::Parser;
use common::http;
use common::{Command, read_command, write_command};
use config::ClientConfig;
use handlers::HandlerState;
use router::{HandlerContext, Router};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::ACPConfig;
use crate::session::SessionManager;

#[tokio::main]
async fn main() -> Result<()> {
    let mut config = ClientConfig::parse();

    // Setup dual logging: all levels -> file, INFO -> terminal
    let log_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("arpc/logs");
    std::fs::create_dir_all(&log_dir)?;

    let (non_blocking, _guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(log_dir, "arpc.log"));

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
        )
        .init();

    // Validate configuration
    if let Err(e) = config.validate() {
        error!("Configuration validation failed: {}", e);
        return Err(anyhow!("Invalid configuration: {}", e));
    }
    let acp_config = match ACPConfig::load(&config.config) {
        Ok(acp_config) => acp_config,
        Err(e) => {
            eprintln!("Failed to load configuration file: {}", e);
            std::process::exit(1);
        }
    };

    config.acp_config = Some(acp_config.clone());

    let session_manager = Arc::new(Mutex::new(SessionManager::new(acp_config.clone())));
    // let (global_broadcast_tx, _global_broadcast_rx) = broadcast::channel::<String>(1000);

    // Create shared state
    // let state = HandlerState::new(config.clone());
    let state = HandlerState {
        config: Arc::new(config.clone()),
        quiet: config.quiet,
        verbose: config.verbose,
        raw: config.raw,
        config_path: config.config.clone(),
        upload_dir: acp_config.upload_dir.clone(),
        session_manager: session_manager.clone(),
    };

    state.log(&format!(
        "HTTP server listening on port {}",
        config.http_port
    ));
    state.log(&format!(
        "Available agents: {}",
        acp_config
            .agent_servers
            .keys()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if config.verbose {
        state.log("Verbose mode enabled - showing timestamps and data length");
    }
    if config.raw {
        state.log("Raw mode enabled - showing raw data without JSON formatting");
    }

    info!(
        "✅ Starting arpc with client_id（Token）: {}",
        config.client_id
    );
    info!("Starting arpc...");
    debug!("Server address: {}", config.control_addr());
    if config.command_mode {
        info!("Running in command mode.");
    } else {
        info!("Local service: {}", config.local_service_addr());
    }

    // Extract Arc-wrapped config to avoid repeated cloning in the loop
    let config_arc = state.config.clone();

    // Build router and wrap in Arc to avoid repeated cloning
    let router = Arc::new(routes::build_router(state));

    'main_loop: loop {
        match run_client_loop(config_arc.clone(), router.clone()).await {
            Ok(_) => break,
            Err(e) if config_arc.auto_reconnect => {
                error!(
                    "Connection error: {}. Reconnecting in {}s...",
                    e, config_arc.reconnect_interval
                );
                let sleep = tokio::time::sleep(tokio::time::Duration::from_secs(
                    config_arc.reconnect_interval,
                ));
                tokio::pin!(sleep);
                tokio::select! {
                    _ = &mut sleep => {},
                    _ = tokio::signal::ctrl_c() => {
                        info!("Received Ctrl+C signal. Shutting down before reconnect...");
                        break 'main_loop;
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

async fn run_client_loop(config: Arc<ClientConfig>, router: Arc<Router>) -> Result<()> {
    let control_stream = TcpStream::connect(config.control_addr()).await?;
    info!("Connected to control port.");

    let (mut reader, mut writer) = tokio::io::split(control_stream);

    let register_cmd = Command::Register {
        client_id: config.client_id.clone(),
    };
    write_command(&mut writer, &register_cmd).await?;
    debug!("Sent registration command");

    match tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        read_command(&mut reader),
    )
    .await?
    {
        Ok(Command::RegisterResult { success, error }) if success => {
            info!("Successfully registered with the server.");
        }
        Ok(Command::RegisterResult { error, .. }) => {
            return Err(anyhow!(
                "Registration failed: {}",
                error.unwrap_or_default()
            ));
        }
        Ok(cmd) => return Err(anyhow!("Unexpected command: {:?}", cmd)),
        Err(e) => return Err(e),
    }

    if config.server_addr != "proxy.agentx.plus" {
        info!(
            "🌐 Public API URL: http://{}:17003?token={}",
            config.server_addr, config.client_id
        );
    } else {
        info!(
            "🌐 Public URL: https://console.agentx.plus/?token={}",
            config.client_id
        );
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C signal. Shutting down gracefully...");
                return Ok(());
            }
            result = read_command(&mut reader) => {
                match result {
                    Ok(Command::RequestNewProxyConn { proxy_conn_id }) => {
                        debug!("Received request for new proxy connection: {}", proxy_conn_id);
                        let config_ref = Arc::clone(&config);
                        let router_ref = Arc::clone(&router);
                        tokio::spawn(async move {
                            if let Err(e) = create_proxy_connection(config_ref, router_ref, proxy_conn_id).await {
                                error!("Failed to create proxy connection: {}", e);
                            }
                        });
                    }
                    Ok(cmd) => warn!("Received unexpected command: {:?}", cmd),
                    Err(ref e) if e.downcast_ref::<io::Error>().is_some_and(|io_err| io_err.kind() == io::ErrorKind::UnexpectedEof) => {
                        return Err(anyhow!("Control connection closed by server"));
                    }
                    Err(e) => return Err(anyhow!("Error reading from control connection: {}", e)),
                }
            }
        }
    }
}

async fn create_proxy_connection(
    config: Arc<ClientConfig>,
    router: Arc<Router>,
    proxy_conn_id: String,
) -> Result<()> {
    let command_mode_enabled = config.command_mode;
    let mut proxy_stream = TcpStream::connect(config.proxy_addr()).await?;
    debug!("('{}') Connected to proxy port.", proxy_conn_id);

    let notify_cmd = Command::NewProxyConn {
        proxy_conn_id: proxy_conn_id.clone(),
        client_id: config.client_id.clone(),
    };
    write_command(&mut proxy_stream, &notify_cmd).await?;
    debug!(
        "('{}') Sent new proxy connection notification.",
        proxy_conn_id
    );

    if command_mode_enabled {
        handle_command_mode_connection(proxy_stream, router, proxy_conn_id).await
    } else {
        handle_tcp_proxy_connection(config, proxy_stream, proxy_conn_id).await
    }
}

async fn handle_command_mode_connection(
    mut proxy_stream: TcpStream,
    router: Arc<Router>,
    proxy_conn_id: String,
) -> Result<()> {
    debug!(
        "('{}') Running in command mode (HTTP routing)",
        proxy_conn_id
    );

    match http::HttpRequest::parse(&mut proxy_stream, &proxy_conn_id).await {
        Ok(request) => {
            // Handle CORS preflight early to avoid empty responses
            if request.method == http::HttpMethod::OPTIONS {
                let stream = &mut proxy_stream;
                let _ = http::HttpResponse::new(204)
                    .header("Access-Control-Allow-Origin", "*")
                    .header(
                        "Access-Control-Allow-Methods",
                        "GET, POST, PUT, DELETE, PATCH, OPTIONS",
                    )
                    .header(
                        "Access-Control-Allow-Headers",
                        "Content-Type, Authorization",
                    )
                    .header("Access-Control-Max-Age", "86400")
                    .body(Vec::new())
                    .send(stream)
                    .await;
                info!(
                    "('{}') Responded to CORS preflight (OPTIONS)",
                    proxy_conn_id
                );
                return Ok(());
            }

            let ctx = HandlerContext {
                request,
                stream: proxy_stream,
                proxy_conn_id: proxy_conn_id.clone(),
                path_params: HashMap::new(),
            };

            match router.handle(ctx).await {
                Ok(_response) => {
                    info!("('{}') Request handled successfully", proxy_conn_id);
                }
                Err(e) => {
                    error!("('{}') Handler error: {}", proxy_conn_id, e);
                }
            }
        }
        Err(e) => {
            error!("('{}') Failed to parse HTTP request: {}", proxy_conn_id, e);
        }
    }

    Ok(())
}

async fn handle_tcp_proxy_connection(
    config: Arc<ClientConfig>,
    proxy_stream: TcpStream,
    proxy_conn_id: String,
) -> Result<()> {
    // Clone the config from Arc for HandlerState::new
    let state = HandlerState::new((*config).clone());
    let ctx = HandlerContext {
        request: http::HttpRequest {
            method: http::HttpMethod::GET,
            path: "/".to_string(),
            query_params: HashMap::new(),
            headers: HashMap::new(),
            body: Vec::new(),
        },
        stream: proxy_stream,
        proxy_conn_id: proxy_conn_id.clone(),
        path_params: HashMap::new(),
    };

    match handlers::proxy::handle_proxy(ctx, state).await {
        Ok(_) => {
            info!("('{}') TCP proxy completed successfully", proxy_conn_id);
        }
        Err(e) => {
            error!("('{}') TCP proxy error: {}", proxy_conn_id, e);
        }
    }

    Ok(())
}
