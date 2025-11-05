use anyhow::Result;
use common::http::{HttpMethod, HttpRequest, HttpResponse};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tracing::warn;

/// Handler context containing request and connection info
pub struct HandlerContext {
    pub request: HttpRequest,
    pub stream: TcpStream,
    pub proxy_conn_id: String,
    pub path_params: HashMap<String, String>,
}

/// Handler function type
pub type Handler = Arc<
    dyn Fn(
            HandlerContext,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<HttpResponse>> + Send>>
        + Send
        + Sync,
>;

/// Route definition
struct Route {
    method: Option<HttpMethod>,
    path_pattern: String,
    handler: Handler,
}

impl Route {
    fn matches(&self, method: &HttpMethod, path: &str) -> Option<HashMap<String, String>> {
        // Check method
        if let Some(ref route_method) = self.method
            && route_method != method {
                return None;
            }

        // Simple path matching (exact match or wildcard)
        if self.path_pattern == path {
            return Some(HashMap::new());
        }

        // Check for path parameters (e.g., /file/{path})
        let pattern_parts: Vec<&str> = self.path_pattern.split('/').collect();
        let path_parts: Vec<&str> = path.split('/').collect();

        let mut params = HashMap::new();

        // Check if pattern contains wildcard parameter (e.g., {*path})
        let mut wildcard_index = None;
        for (i, pattern_part) in pattern_parts.iter().enumerate() {
            if pattern_part.starts_with("{*") && pattern_part.ends_with('}') {
                wildcard_index = Some(i);
                break;
            }
        }

        if let Some(wildcard_idx) = wildcard_index {
            // Handle wildcard matching
            // Pattern must have at least as many parts before wildcard as path has
            if path_parts.len() < wildcard_idx {
                return None;
            }

            // Match all parts before the wildcard
            for i in 0..wildcard_idx {
                let pattern_part = pattern_parts[i];
                let path_part = path_parts[i];

                if pattern_part.starts_with('{') && pattern_part.ends_with('}') {
                    // Regular parameter
                    let param_name = &pattern_part[1..pattern_part.len() - 1];
                    params.insert(param_name.to_string(), path_part.to_string());
                } else if pattern_part != path_part {
                    return None;
                }
            }

            // Capture remaining path as wildcard parameter
            let wildcard_pattern = pattern_parts[wildcard_idx];
            let param_name = &wildcard_pattern[2..wildcard_pattern.len() - 1]; // Remove {* and }

            // Join remaining path parts
            let remaining_path = if wildcard_idx < path_parts.len() {
                path_parts[wildcard_idx..].join("/")
            } else {
                String::new()
            };

            params.insert(param_name.to_string(), remaining_path);

            return Some(params);
        }

        // Regular matching without wildcard (original logic)
        if pattern_parts.len() != path_parts.len() {
            return None;
        }

        for (pattern_part, path_part) in pattern_parts.iter().zip(path_parts.iter()) {
            if pattern_part.starts_with('{') && pattern_part.ends_with('}') {
                // Extract parameter name
                let param_name = &pattern_part[1..pattern_part.len() - 1];
                params.insert(param_name.to_string(), path_part.to_string());
            } else if pattern_part != path_part {
                return None;
            }
        }

        Some(params)
    }
}

/// HTTP router for handling requests
#[derive(Clone)]
pub struct Router {
    routes: Arc<Vec<Route>>,
}

/// Builder for constructing a Router
pub struct RouterBuilder {
    routes: Vec<Route>,
}

impl RouterBuilder {
    /// Create a new router builder
    pub fn new() -> Self {
        RouterBuilder { routes: Vec::new() }
    }

    /// Add a route with any HTTP method
    pub fn route<F, Fut>(&mut self, path: impl Into<String>, handler: F)
    where
        F: Fn(HandlerContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<HttpResponse>> + Send + 'static,
    {
        let handler_arc = Arc::new(move |ctx: HandlerContext| {
            Box::pin(handler(ctx))
                as std::pin::Pin<Box<dyn std::future::Future<Output = Result<HttpResponse>> + Send>>
        });

        self.routes.push(Route {
            method: None,
            path_pattern: path.into(),
            handler: handler_arc,
        });
    }

    /// Add a GET route
    pub fn get<F, Fut>(&mut self, path: impl Into<String>, handler: F)
    where
        F: Fn(HandlerContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<HttpResponse>> + Send + 'static,
    {
        let handler_arc = Arc::new(move |ctx: HandlerContext| {
            Box::pin(handler(ctx))
                as std::pin::Pin<Box<dyn std::future::Future<Output = Result<HttpResponse>> + Send>>
        });

        self.routes.push(Route {
            method: Some(HttpMethod::GET),
            path_pattern: path.into(),
            handler: handler_arc,
        });
    }

    /// Add a POST route
    pub fn post<F, Fut>(&mut self, path: impl Into<String>, handler: F)
    where
        F: Fn(HandlerContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<HttpResponse>> + Send + 'static,
    {
        let handler_arc = Arc::new(move |ctx: HandlerContext| {
            Box::pin(handler(ctx))
                as std::pin::Pin<Box<dyn std::future::Future<Output = Result<HttpResponse>> + Send>>
        });

        self.routes.push(Route {
            method: Some(HttpMethod::POST),
            path_pattern: path.into(),
            handler: handler_arc,
        });
    }

    /// Add a DELETE route
    pub fn delete<F, Fut>(&mut self, path: impl Into<String>, handler: F)
    where
        F: Fn(HandlerContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<HttpResponse>> + Send + 'static,
    {
        let handler_arc = Arc::new(move |ctx: HandlerContext| {
            Box::pin(handler(ctx))
                as std::pin::Pin<Box<dyn std::future::Future<Output = Result<HttpResponse>> + Send>>
        });

        self.routes.push(Route {
            method: Some(HttpMethod::DELETE),
            path_pattern: path.into(),
            handler: handler_arc,
        });
    }

    /// Build the final Router
    pub fn build(self) -> Router {
        Router {
            routes: Arc::new(self.routes),
        }
    }
}

impl Router {
    /// Handle a request
    pub async fn handle(&self, mut ctx: HandlerContext) -> Result<HttpResponse> {
        // Handle OPTIONS requests for CORS preflight
        if ctx.request.method == HttpMethod::OPTIONS {
            return Ok(HttpResponse::new(204)
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
                .body(Vec::new()));
        }

        // Find matching route
        for route in self.routes.iter() {
            if let Some(params) = route.matches(&ctx.request.method, &ctx.request.path) {
                // Inject path parameters into context
                ctx.path_params = params;
                return (route.handler)(ctx).await;
            }
        }

        // No route found
        warn!(
            "No route found for {} {}",
            ctx.request.method.as_str(),
            ctx.request.path
        );
        Ok(HttpResponse::not_found().json(&serde_json::json!({
            "type": "error",
            "message": format!("Route not found: {} {}", ctx.request.method.as_str(), ctx.request.path)
        })))
    }
}

impl Default for Router {
    fn default() -> Self {
        RouterBuilder::new().build()
    }
}

impl Default for RouterBuilder {
    fn default() -> Self {
        Self::new()
    }
}
