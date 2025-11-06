use crate::handlers::HandlerState;
use anyhow::Result;
use common::http::HttpResponse;
use common::join_streams;
use common::router::HandlerContext;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{error, info};

// Security: Allowed port range (exclude system ports < 1024)
const ALLOWED_PORT_RANGE: std::ops::RangeInclusive<u16> = 1024..=65535;

// Security: Blocked ports for safety (common services that should not be proxied)
const BLOCKED_PORTS: &[u16] = &[
    22,    // SSH
    23,    // Telnet
    25,    // SMTP
    110,   // POP3
    143,   // IMAP
    3306,  // MySQL
    5432,  // PostgreSQL
    6379,  // Redis
    27017, // MongoDB
];

/// Validate port number for security
fn validate_port(port: u16) -> Result<u16, String> {
    if !ALLOWED_PORT_RANGE.contains(&port) {
        return Err(format!(
            "Port {} is outside allowed range {}-{}",
            port,
            ALLOWED_PORT_RANGE.start(),
            ALLOWED_PORT_RANGE.end()
        ));
    }

    if BLOCKED_PORTS.contains(&port) {
        return Err(format!(
            "Port {} is blocked for security reasons (common service port)",
            port
        ));
    }

    Ok(port)
}

/// Handle TCP proxy requests
pub async fn handle_proxy(ctx: HandlerContext, state: HandlerState) -> Result<HttpResponse> {
    let proxy_conn_id = &ctx.proxy_conn_id;
    let config = &state.config;

    // Connect to local service
    let local_stream = TcpStream::connect(config.local_service_addr()).await?;
    info!(
        "('{}') Connected to local service at {}.",
        proxy_conn_id,
        config.local_service_addr()
    );

    // Join streams (proxy <-> local service)
    info!("('{}') Joining streams...", proxy_conn_id);
    join_streams(ctx.stream, local_stream).await?;
    info!("('{}') Streams joined and finished.", proxy_conn_id);

    // Return a dummy response (stream already handled)
    Ok(HttpResponse::ok())
}

/// Handle dynamic proxy requests to local ports
/// Route pattern: /proxy/{port}/{*path}
pub async fn handle_dynamic_proxy(
    ctx: HandlerContext,
    _state: HandlerState,
) -> Result<HttpResponse> {
    let proxy_conn_id = &ctx.proxy_conn_id;

    // Extract and validate port
    let port: u16 = match ctx.path_params.get("port").and_then(|p| p.parse().ok()) {
        Some(p) => match validate_port(p) {
            Ok(validated_port) => validated_port,
            Err(err_msg) => {
                error!("('{}') Port validation failed: {}", proxy_conn_id, err_msg);
                let mut stream = ctx.stream;
                let _ = HttpResponse::new(403)
                    .json(&serde_json::json!({
                        "type": "error",
                        "message": err_msg
                    }))
                    .send(&mut stream)
                    .await;
                return Ok(HttpResponse::ok());
            }
        },
        None => {
            error!("('{}') Invalid port parameter", proxy_conn_id);
            let mut stream = ctx.stream;
            let _ = HttpResponse::bad_request()
                .json(&serde_json::json!({
                    "type": "error",
                    "message": "Invalid port parameter"
                }))
                .send(&mut stream)
                .await;
            return Ok(HttpResponse::ok());
        }
    };

    // Build target path with query string
    let target_path = ctx
        .path_params
        .get("path")
        .map(|p| {
            if p.is_empty() || p == "/" {
                "/".to_string()
            } else {
                format!("/{}", p.trim_start_matches('/'))
            }
        })
        .unwrap_or_else(|| "/".to_string());

    let target_url = if !ctx.request.query_params.is_empty() {
        let query_string: Vec<String> = ctx
            .request
            .query_params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect();
        format!("{}?{}", target_path, query_string.join("&"))
    } else {
        target_path
    };

    info!(
        "('{}') Proxy: {} {} -> 127.0.0.1:{}{}",
        proxy_conn_id,
        ctx.request.method.as_str(),
        ctx.request.path,
        port,
        target_url
    );

    // Connect to target
    let target_addr = format!("127.0.0.1:{}", port);
    let mut target_stream = match TcpStream::connect(&target_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!("('{}') Connection failed: {}", proxy_conn_id, e);
            let mut stream = ctx.stream;
            let _ = HttpResponse::new(502)
                .json(&serde_json::json!({
                    "type": "error",
                    "message": format!("Failed to connect to {}: {}", target_addr, e)
                }))
                .send(&mut stream)
                .await;
            return Ok(HttpResponse::ok());
        }
    };

    // Build and send HTTP request
    let mut request_data = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n",
        ctx.request.method.as_str(),
        target_url,
        port
    )
    .into_bytes();

    // Add headers (skip host and connection)
    for (key, value) in &ctx.request.headers {
        if !matches!(key.to_lowercase().as_str(), "host" | "connection") {
            request_data.extend_from_slice(format!("{}: {}\r\n", key, value).as_bytes());
        }
    }

    request_data.extend_from_slice(b"Connection: close\r\n");

    if !ctx.request.body.is_empty() {
        request_data.extend_from_slice(
            format!("Content-Length: {}\r\n", ctx.request.body.len()).as_bytes(),
        );
    }

    request_data.extend_from_slice(b"\r\n");
    request_data.extend_from_slice(&ctx.request.body);

    // Send request
    if let Err(e) = target_stream.write_all(&request_data).await {
        error!("('{}') Send failed: {}", proxy_conn_id, e);
        let mut stream = ctx.stream;
        let _ = HttpResponse::new(502)
            .json(&serde_json::json!({ "type": "error", "message": format!("Send failed: {}", e) }))
            .send(&mut stream)
            .await;
        return Ok(HttpResponse::ok());
    }

    target_stream.flush().await?;
    info!(
        "('{}') Request forwarded, streaming response...",
        proxy_conn_id
    );

    // Stream response back using join_streams
    join_streams(ctx.stream, target_stream).await?;
    info!("('{}') Proxy completed", proxy_conn_id);

    Ok(HttpResponse::ok())
}
