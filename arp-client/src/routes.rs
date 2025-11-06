use crate::handlers::{self, HandlerState};
use common::router::{Router, RouterBuilder};

/// Build and return the router with all application routes registered.
pub fn build_router(state: HandlerState) -> Router {
    let mut builder = RouterBuilder::new();

    register_session_routes(&mut builder, &state);
    register_proxy_routes(&mut builder, &state);
    builder.build()
}

fn register_session_routes(router_builder: &mut RouterBuilder, state: &HandlerState) {
    // GET /api/working-directories - Get all working directories from all agents
    router_builder.get("/api/working-directories", {
        let state = state.clone();
        move |ctx| {
            let state = state.clone();
            async move { handlers::session::handle_working_directories(ctx, state).await }
        }
    });

    // GET /api/agents - List all agents
    router_builder.get("/api/agents", {
        let state = state.clone();
        move |ctx| {
            let state = state.clone();
            async move { handlers::session::handle_agents(ctx, state, "list").await }
        }
    });
    router_builder.post("/api/agents", {
        let state = state.clone();
        move |ctx| {
            let state = state.clone();
            async move { handlers::session::handle_agents(ctx, state, "add").await }
        }
    });
    router_builder.delete("/api/agents/{name}", {
        let state = state.clone();
        move |ctx| {
            let state = state.clone();
            async move { handlers::session::handle_agents(ctx, state, "delete").await }
        }
    });
    // POST /acp/session/message - Create new command execution session
    router_builder.post("/acp/session/message", {
        let state = state.clone();
        move |ctx| {
            let state = state.clone();
            async move { handlers::session::handle_session(ctx, state).await }
        }
    });

    router_builder.get("/acp/session/message", {
        let state = state.clone();
        move |ctx| {
            let state = state.clone();
            async move { handlers::session::handle_session(ctx, state).await }
        }
    });

    if state.config.enable_fs {}
}

fn register_proxy_routes(router_builder: &mut RouterBuilder, state: &HandlerState) {
    // Dynamic proxy route: /proxy/{port}/{*path}
    // This forwards requests to local services on different ports
    // Examples:
    //   /proxy/8080/api/users -> 127.0.0.1:8080/api/users
    //   /proxy/3000/ -> 127.0.0.1:3000/
    //   /proxy/9000/health?check=true -> 127.0.0.1:9000/health?check=true
    router_builder.route("/proxy/{port}/{*path}", {
        let state = state.clone();
        move |ctx| {
            let state = state.clone();
            async move { handlers::proxy::handle_dynamic_proxy(ctx, state).await }
        }
    });
}
