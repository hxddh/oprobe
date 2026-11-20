//! The MCP server: an `rmcp` [`ServerHandler`] over [`tools`](super::tools),
//! [`resources`](super::resources) and [`prompts`](super::prompts), served on stdio or
//! Streamable HTTP.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, Implementation, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    ProgressNotificationParam, Prompt, PromptArgument, PromptMessage, ProtocolVersion,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, Role, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};

use super::McpConfig;
use super::tools::{self, CallCx, ProgressSink, State};

/// Instructions sent to clients on `initialize`.
const INSTRUCTIONS: &str = "oprobe measures object storage (S3, GCS, Azure Blob, …) and traces \
every request. Safe workflow: list_targets → doctor → plan_run → start_run (budgets; returns a \
run id, poll get_run) → explain_run → analyze_run → compare_runs. The server is read-only unless \
started with --allow-write (memory:// and `existing` runs are always allowed); its budgets cap \
every run. Errors carry a stable code (e.g. safety.read_only, safety.budget_exceeded) and a hint. \
Read oprobe://docs/getting-started and oprobe://docs/findings.";

/// The protocol versions this server speaks.
pub fn protocol_versions() -> Vec<String> {
    ProtocolVersion::KNOWN_VERSIONS
        .iter()
        .map(|v| v.as_str().to_owned())
        .collect()
}

/// The oprobe MCP server (cheap to clone: shared state).
#[derive(Clone)]
pub struct OprobeMcp {
    state: Arc<State>,
}

impl OprobeMcp {
    pub fn new(cfg: McpConfig) -> Self {
        Self {
            state: Arc::new(State::new(cfg)),
        }
    }

    /// Cancel every run this server started (graceful: cleanup runs, partial runs are saved),
    /// and wait for them to end.
    pub async fn shutdown(&self) {
        self.state.jobs.cancel_all();
        for j in self.state.jobs.all() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(30), j.wait()).await;
        }
    }

    fn tool_defs() -> Vec<Tool> {
        tools::SPECS
            .iter()
            .map(|s| {
                let ann = ToolAnnotations::with_title(s.title)
                    .read_only(s.read_only)
                    .destructive(s.name == "cleanup")
                    .idempotent(s.read_only)
                    .open_world(s.open_world);
                Tool::new(s.name, s.description, Arc::new((s.input)()))
                    .with_title(s.title)
                    .with_raw_output_schema(Arc::new((s.output)()))
                    .with_annotations(ann)
            })
            .collect()
    }
}

/// A progress sink that sends MCP progress notifications in order.
fn progress_sink(peer: rmcp::Peer<RoleServer>, token: rmcp::model::ProgressToken) -> ProgressSink {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProgressNotificationParam>();
    tokio::spawn(async move {
        while let Some(p) = rx.recv().await {
            if peer.notify_progress(p).await.is_err() {
                break;
            }
        }
    });
    Arc::new(move |progress, total, message| {
        let mut p = ProgressNotificationParam::new(token.clone(), progress);
        if let Some(t) = total {
            p = p.with_total(t);
        }
        if let Some(m) = message {
            p = p.with_message(m);
        }
        let _ = tx.send(p);
    })
}

fn error_result(e: &crate::errors::ErrorBody) -> CallToolResult {
    let mut text = format!("error [{}]: {}", e.code, e.message);
    if let Some(h) = &e.hint {
        text += &format!("\nhint: {h}");
    }
    let json = serde_json::json!({"type": "error", "error": e});
    CallToolResult::error(vec![
        ContentBlock::text(text),
        ContentBlock::text(json.to_string()),
    ])
}

impl ServerHandler for OprobeMcp {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_instructions(INSTRUCTIONS);
        let mut imp = Implementation::new("oprobe", env!("CARGO_PKG_VERSION"));
        imp.title = Some("oprobe — object storage microscope".into());
        info.server_info = imp;
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(Self::tool_defs()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let Some(spec) = tools::spec(&request.name) else {
            return Err(McpError::invalid_params(
                format!("unknown tool `{}`", request.name),
                None,
            ));
        };
        let progress = context
            .meta
            .get_progress_token()
            .map(|t| progress_sink(context.peer.clone(), t));
        let cx = CallCx {
            progress,
            ct: context.ct.clone(),
        };
        let args = request.arguments.unwrap_or_default();
        let res = tools::call(&self.state, spec.name, args, cx).await;
        Ok(match res {
            Ok(r) => {
                let mut out = CallToolResult::success(vec![ContentBlock::text(r.text)]);
                out.structured_content = Some(r.structured);
                out
            }
            Err(e) => error_result(&e),
        }
        .into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let items = super::resources::list(&self.state)
            .into_iter()
            .map(|l| {
                Resource::new(l.uri, l.name)
                    .with_description(l.description)
                    .with_mime_type(l.mime_type)
            })
            .collect();
        Ok(ListResourcesResult::with_all_items(items))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let items = super::resources::TEMPLATES
            .iter()
            .map(|t| {
                ResourceTemplate::new(t.uri_template, t.name)
                    .with_description(t.description)
                    .with_mime_type(t.mime_type)
            })
            .collect();
        Ok(ListResourceTemplatesResult::with_all_items(items))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        match super::resources::read(&self.state, &request.uri) {
            Ok(c) => Ok(ReadResourceResult::new(vec![
                ResourceContents::text(c.text, request.uri).with_mime_type(c.mime_type),
            ])
            .into()),
            Err(e) => {
                let body = crate::errors::classify(&e, "s3");
                let data = serde_json::to_value(&body).ok();
                Err(McpError::resource_not_found(body.message, data))
            }
        }
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        let items = super::prompts::PROMPTS
            .iter()
            .map(|p| {
                let args = p
                    .args
                    .iter()
                    .map(|a| {
                        PromptArgument::new(a.name)
                            .with_description(a.description)
                            .with_required(a.required)
                    })
                    .collect();
                Prompt::new(p.name, Some(p.description), Some(args)).with_title(p.title)
            })
            .collect();
        Ok(ListPromptsResult::with_all_items(items))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        let args = request.arguments.unwrap_or_default();
        match super::prompts::render(&request.name, &args) {
            Ok((description, text)) => Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                Role::User,
                text,
            )])
            .with_description(description)
            .into()),
            Err(e) => Err(McpError::invalid_params(format!("{e:#}"), None)),
        }
    }
}

/// Serve MCP on stdin/stdout until the client disconnects.
pub async fn serve_stdio(cfg: McpConfig) -> anyhow::Result<()> {
    let server = OprobeMcp::new(cfg);
    let running = server
        .clone()
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP initialization failed: {e}"))?;
    let res = running.waiting().await;
    server.shutdown().await;
    res.map(|_| ())
        .map_err(|e| anyhow::anyhow!("MCP server task failed: {e}"))
}

/// Serve MCP over Streamable HTTP on `addr` (path `/mcp`) until Ctrl-C. `ready` is called
/// with the bound address.
pub async fn serve_http(
    cfg: McpConfig,
    addr: &str,
    ready: impl FnOnce(std::net::SocketAddr),
) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
    use tower_service::Service as _;

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let server = OprobeMcp::new(cfg);
    let ct = tokio_util::sync::CancellationToken::new();
    let mut hosts = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
        local.ip().to_string(),
    ];
    if let Some((h, _)) = addr.rsplit_once(':') {
        hosts.push(h.trim_matches(['[', ']']).to_owned());
    }
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(ct.clone())
        .with_allowed_hosts(hosts);
    let s2 = server.clone();
    let service = StreamableHttpService::new(
        move || Ok(s2.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    ready(local);
    loop {
        let (stream, _) = tokio::select! {
            r = listener.accept() => r?,
            _ = tokio::signal::ctrl_c() => break,
        };
        let svc = service.clone();
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let handler =
                hyper::service::service_fn(move |req: http::Request<hyper::body::Incoming>| {
                    let mut svc = svc.clone();
                    async move {
                        if req.uri().path() != "/mcp" {
                            let mut r = http::Response::new(
                                http_body_util::Full::new(bytes::Bytes::from_static(
                                    b"oprobe MCP server: POST JSON-RPC to /mcp\n",
                                ))
                                .map_err(|never| match never {})
                                .boxed(),
                            );
                            *r.status_mut() = http::StatusCode::NOT_FOUND;
                            return Ok::<_, std::convert::Infallible>(r);
                        }
                        svc.call(req).await
                    }
                });
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(io, handler)
                    .await;
        });
    }
    ct.cancel();
    server.shutdown().await;
    Ok(())
}

use http_body_util::BodyExt as _;
