use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::str::FromStr;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::info;

/// HTTP request method
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpMethod {
    GET,
    POST,
    PUT,
    DELETE,
    PATCH,
    OPTIONS,
    HEAD,
}

impl HttpMethod {
    pub fn as_str(&self) -> &str {
        match self {
            HttpMethod::GET => "GET",
            HttpMethod::POST => "POST",
            HttpMethod::PUT => "PUT",
            HttpMethod::DELETE => "DELETE",
            HttpMethod::PATCH => "PATCH",
            HttpMethod::OPTIONS => "OPTIONS",
            HttpMethod::HEAD => "HEAD",
        }
    }
}

impl FromStr for HttpMethod {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "GET" => Ok(HttpMethod::GET),
            "POST" => Ok(HttpMethod::POST),
            "PUT" => Ok(HttpMethod::PUT),
            "DELETE" => Ok(HttpMethod::DELETE),
            "PATCH" => Ok(HttpMethod::PATCH),
            "OPTIONS" => Ok(HttpMethod::OPTIONS),
            "HEAD" => Ok(HttpMethod::HEAD),
            _ => Err(()),
        }
    }
}

/// Parsed HTTP request
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub path: String,
    pub query_params: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// Parse an HTTP request from a TCP stream
    pub async fn parse(stream: &mut TcpStream, proxy_conn_id: &str) -> Result<Self> {
        let mut reader = BufReader::new(stream);

        // Read request line
        let mut request_line = String::new();
        reader.read_line(&mut request_line).await?;
        info!(
            "('{}') Request line: {}",
            proxy_conn_id,
            request_line.trim()
        );

        // Parse request line: METHOD PATH HTTP/VERSION
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 3 {
            return Err(anyhow!("Invalid HTTP request line"));
        }

        let method = parts[0]
            .parse::<HttpMethod>()
            .map_err(|_| anyhow!("Invalid HTTP method: {}", parts[0]))?;

        let url_part = parts[1];

        // Parse path and query string
        let (path, query_string) = if let Some(pos) = url_part.find('?') {
            let (p, q) = url_part.split_at(pos);
            (p.to_string(), Some(&q[1..])) // Skip the '?'
        } else {
            (url_part.to_string(), None)
        };

        // Parse query parameters
        let mut query_params = HashMap::new();
        if let Some(qs) = query_string {
            for pair in qs.split('&') {
                if let Some((key, value)) = pair.split_once('=') {
                    // URL decode
                    let decoded_value = urlencoding::decode(value)
                        .unwrap_or(std::borrow::Cow::Borrowed(value))
                        .to_string();
                    query_params.insert(key.to_string(), decoded_value);
                }
            }
        }

        // Read headers
        let mut headers = HashMap::new();
        let mut content_length = 0usize;

        loop {
            let mut header_line = String::new();
            reader.read_line(&mut header_line).await?;

            if header_line == "\r\n" || header_line == "\n" {
                break; // End of headers
            }

            if let Some((key, value)) = header_line.split_once(':') {
                let key = key.trim().to_lowercase();
                let value = value.trim().to_string();

                if key == "content-length" {
                    content_length = value.parse().unwrap_or(0);
                }

                headers.insert(key, value);
            }
        }

        info!(
            "('{}') Method: {}, Path: {}, Query params: {:?}",
            proxy_conn_id,
            method.as_str(),
            path,
            query_params
        );
        info!("('{}') Content-Length: {}", proxy_conn_id, content_length);

        // Read request body
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body).await?;
        }

        Ok(HttpRequest {
            method,
            path,
            query_params,
            headers,
            body,
        })
    }

    /// Get body as string
    pub fn body_as_string(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    /// Get body as JSON
    pub fn body_as_json(&self) -> Result<Value> {
        if self.body.is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_slice(&self.body).map_err(|e| anyhow!("Invalid JSON body: {}", e))
    }

    /// Get a query parameter by key
    pub fn query_param(&self, key: &str) -> Option<&String> {
        self.query_params.get(key)
    }

    /// Get a header by key
    pub fn header(&self, key: &str) -> Option<&String> {
        self.headers.get(&key.to_lowercase())
    }
}

/// HTTP response builder
#[derive(Debug)]
pub struct HttpResponse {
    status_code: u16,
    status_text: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl HttpResponse {
    /// Create a new response with status code
    pub fn new(status_code: u16) -> Self {
        let status_text = match status_code {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            500 => "Internal Server Error",
            _ => "Unknown",
        }
        .to_string();

        HttpResponse {
            status_code,
            status_text,
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    /// Create a 200 OK response
    pub fn ok() -> Self {
        Self::new(200)
    }

    /// Create a 400 Bad Request response
    pub fn bad_request() -> Self {
        Self::new(400)
    }

    /// Create a 404 Not Found response
    pub fn not_found() -> Self {
        Self::new(404)
    }

    /// Create a 405 Method Not Allowed response
    pub fn method_not_allowed() -> Self {
        Self::new(405)
    }

    /// Create a 500 Internal Server Error response
    pub fn internal_error() -> Self {
        Self::new(500)
    }

    /// Set a header
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    /// Set JSON body
    pub fn json(mut self, value: &Value) -> Self {
        self.body = value.to_string().into_bytes();
        self.headers
            .insert("Content-Type".to_string(), "application/json".to_string());
        self
    }

    /// Set text body
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.body = text.into().into_bytes();
        self.headers
            .insert("Content-Type".to_string(), "text/plain".to_string());
        self
    }

    /// Set binary body
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// Send the response to a TCP stream
    pub async fn send(mut self, stream: &mut TcpStream) -> Result<()> {
        // Add CORS headers
        if !self.headers.contains_key("Access-Control-Allow-Origin") {
            self.headers
                .insert("Access-Control-Allow-Origin".to_string(), "*".to_string());
        }
        if !self.headers.contains_key("Access-Control-Allow-Methods") {
            self.headers.insert(
                "Access-Control-Allow-Methods".to_string(),
                "GET, POST, PUT, DELETE, PATCH, OPTIONS".to_string(),
            );
        }
        if !self.headers.contains_key("Access-Control-Allow-Headers") {
            self.headers.insert(
                "Access-Control-Allow-Headers".to_string(),
                "Content-Type, Authorization".to_string(),
            );
        }

        // Add content-length header
        self.headers
            .insert("Content-Length".to_string(), self.body.len().to_string());

        // Build response
        let mut response = format!("HTTP/1.1 {} {}\r\n", self.status_code, self.status_text);

        for (key, value) in &self.headers {
            response.push_str(&format!("{}: {}\r\n", key, value));
        }

        response.push_str("\r\n");

        // Send headers
        stream.write_all(response.as_bytes()).await?;

        // Send body
        if !self.body.is_empty() {
            stream.write_all(&self.body).await?;
        }

        stream.flush().await?;
        Ok(())
    }
}

/// Create a JSON error response
pub fn json_error(status_code: u16, message: impl Into<String>) -> HttpResponse {
    let error_body = json!({
        "type": "error",
        "message": message.into()
    });

    HttpResponse::new(status_code).json(&error_body)
}
