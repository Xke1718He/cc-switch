use crate::app_config::AppType;
use crate::error::AppError;
use crate::provider::Provider;
use crate::services::profile::{ProfilePayload, ProfileService};
use crate::services::{McpService, PromptService, ProviderService, SkillService, SpeedtestService};
use crate::store::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

#[derive(Clone)]
pub struct WebServerConfig {
    pub listen: SocketAddr,
    pub ui_dir: PathBuf,
}

#[derive(Clone)]
struct WebState {
    app_state: Arc<AppState>,
}

#[derive(Debug, Deserialize)]
struct InvokeRequest {
    #[serde(default)]
    args: Value,
}

#[derive(Debug, Serialize)]
struct InvokeError {
    error: String,
}

#[derive(Debug, Serialize)]
struct McpConfigResponse {
    config_path: String,
    servers: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileDto {
    id: String,
    name: String,
    payload: ProfilePayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<i64>,
}

impl From<crate::database::Profile> for ProfileDto {
    fn from(profile: crate::database::Profile) -> Self {
        let payload = serde_json::from_str(&profile.payload).unwrap_or_else(|e| {
            log::warn!(
                "Failed to parse profile '{}' payload in web mode, using default: {e}",
                profile.id
            );
            ProfilePayload::default()
        });
        Self {
            id: profile.id,
            name: profile.name,
            payload,
            created_at: profile.created_at,
            updated_at: profile.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentProfileIds {
    claude: Option<String>,
    claude_desktop: Option<String>,
    codex: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfilesResponse {
    profiles: Vec<ProfileDto>,
    current_ids: CurrentProfileIds,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelPricingInfo {
    model_id: String,
    display_name: String,
    input_cost_per_million: String,
    output_cost_per_million: String,
    cache_read_cost_per_million: String,
    cache_creation_cost_per_million: String,
}

pub async fn run(config: WebServerConfig, app_state: Arc<AppState>) -> Result<(), AppError> {
    let state = WebState { app_state };
    let app = build_router(config.ui_dir, state);
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .map_err(|e| AppError::Config(format!("failed to bind web server: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| AppError::Config(format!("failed to read web server address: {e}")))?;

    println!("CC Switch web UI: http://{addr}");
    axum::serve(listener, app)
        .await
        .map_err(|e| AppError::Config(format!("web server failed: {e}")))
}

fn build_router(ui_dir: PathBuf, state: WebState) -> Router {
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/invoke/:command", post(invoke))
        .with_state(state)
        .layer(CorsLayer::permissive());

    if ui_dir.join("index.html").is_file() {
        api.fallback_service(
            ServeDir::new(&ui_dir).fallback(ServeFile::new(ui_dir.join("index.html"))),
        )
    } else {
        api.fallback(|| async {
            (
                StatusCode::NOT_FOUND,
                "Web UI build not found. Run `pnpm build:web` first, or use the API endpoint.",
            )
        })
    }
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "mode": "web" }))
}

async fn invoke(
    State(state): State<WebState>,
    Path(command): Path<String>,
    Json(request): Json<InvokeRequest>,
) -> Response {
    match dispatch(&state.app_state, &command, request.args).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(InvokeError {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn dispatch(state: &AppState, command: &str, args: Value) -> Result<Value, AppError> {
    match command {
        "get_init_error" => ok(crate::init_status::get_init_error()),
        "get_migration_result" => ok(crate::init_status::take_migration_success()),
        "get_skills_migration_result" => ok(crate::init_status::take_skills_migration_result()),

        "get_settings" => ok(crate::settings::get_settings_for_frontend()),
        "save_settings" => {
            let settings = take::<crate::settings::AppSettings>(&args, "settings")?;
            crate::settings::update_settings(settings)?;
            ok(true)
        }
        "is_portable_mode" => ok(is_portable_mode()),
        "get_config_dir" => ok(get_config_dir(arg_string(&args, &["app"])?)),
        "get_app_config_path" => ok(crate::config::get_app_config_path()
            .to_string_lossy()
            .to_string()),
        "get_claude_code_config_path" => ok(crate::config::get_claude_settings_path()
            .to_string_lossy()
            .to_string()),
        "get_app_config_dir_override" => ok(None::<String>),
        "set_app_config_dir_override" => Err(AppError::Config(
            "app config directory override is not available in web mode".to_string(),
        )),
        "check_for_updates" | "restart_app" | "install_update_and_restart" => ok(false),

        "get_providers" => {
            let app = app_type(&args, "app")?;
            ok(ProviderService::list(state, app)?)
        }
        "get_current_provider" => {
            let app = app_type(&args, "app")?;
            ok(ProviderService::current(state, app)?)
        }
        "add_provider" => {
            let app = app_type(&args, "app")?;
            let provider = take::<Provider>(&args, "provider")?;
            let add_to_live = arg_bool_opt(&args, &["addToLive", "add_to_live"])?.unwrap_or(true);
            ok(ProviderService::add(state, app, provider, add_to_live)?)
        }
        "update_provider" => {
            let app = app_type(&args, "app")?;
            let provider = take::<Provider>(&args, "provider")?;
            let original_id = arg_string_opt(&args, &["originalId", "original_id"]);
            ok(ProviderService::update(
                state,
                app,
                original_id.as_deref(),
                provider,
            )?)
        }
        "delete_provider" => {
            let app = app_type(&args, "app")?;
            let id = arg_string(&args, &["id"])?;
            ProviderService::delete(state, app, &id)?;
            ok(true)
        }
        "remove_provider_from_live_config" => {
            let app = app_type(&args, "app")?;
            let id = arg_string(&args, &["id"])?;
            ProviderService::remove_from_live_config(state, app, &id)?;
            ok(true)
        }
        "switch_provider" => {
            let app = app_type(&args, "app")?;
            let id = arg_string(&args, &["id"])?;
            ok(ProviderService::switch(state, app, &id)?)
        }
        "import_default_config" => {
            let app = app_type(&args, "app")?;
            ok(ProviderService::import_default_config(state, app)?)
        }
        "read_live_provider_settings" => {
            let app = app_type(&args, "app")?;
            ok(ProviderService::read_live_settings(app)?)
        }
        "update_providers_sort_order" => {
            let app = app_type(&args, "app")?;
            let updates = take::<Vec<crate::services::ProviderSortUpdate>>(&args, "updates")?;
            ok(ProviderService::update_sort_order(state, app, updates)?)
        }
        "get_custom_endpoints" => {
            let app = app_type(&args, "app")?;
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            ok(ProviderService::get_custom_endpoints(
                state,
                app,
                &provider_id,
            )?)
        }
        "add_custom_endpoint" => {
            let app = app_type(&args, "app")?;
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            let url = arg_string(&args, &["url"])?;
            ok(ProviderService::add_custom_endpoint(
                state,
                app,
                &provider_id,
                url,
            )?)
        }
        "remove_custom_endpoint" => {
            let app = app_type(&args, "app")?;
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            let url = arg_string(&args, &["url"])?;
            ok(ProviderService::remove_custom_endpoint(
                state,
                app,
                &provider_id,
                url,
            )?)
        }
        "update_endpoint_last_used" => {
            let app = app_type(&args, "app")?;
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            let url = arg_string(&args, &["url"])?;
            ok(ProviderService::update_endpoint_last_used(
                state,
                app,
                &provider_id,
                url,
            )?)
        }
        "test_api_endpoints" => {
            let urls = take::<Vec<String>>(&args, "urls")?;
            let timeout_secs = arg_u64_opt(&args, &["timeoutSecs", "timeout_secs"])?;
            ok(SpeedtestService::test_endpoints(urls, timeout_secs).await?)
        }
        "queryProviderUsage" => {
            let app = app_type(&args, "app")?;
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            ok(ProviderService::query_usage(state, app, &provider_id).await?)
        }

        "get_universal_providers" => ok(ProviderService::list_universal(state)?),
        "get_universal_provider" => {
            let id = arg_string(&args, &["id"])?;
            ok(ProviderService::get_universal(state, &id)?)
        }
        "upsert_universal_provider" => {
            let provider = take::<crate::provider::UniversalProvider>(&args, "provider")?;
            ok(ProviderService::upsert_universal(state, provider)?)
        }
        "delete_universal_provider" => {
            let id = arg_string(&args, &["id"])?;
            ok(ProviderService::delete_universal(state, &id)?)
        }
        "sync_universal_provider" => {
            let id = arg_string(&args, &["id"])?;
            ok(ProviderService::sync_universal_to_apps(state, &id)?)
        }

        "get_claude_desktop_status" => ok(crate::claude_desktop_config::get_status(
            state.db.as_ref(),
            state.proxy_service.is_running().await,
        )?),
        "get_claude_desktop_default_routes" => {
            ok(crate::claude_desktop_config::default_proxy_routes())
        }
        "ensure_claude_desktop_official_provider" => ok(state.db.ensure_official_seed_by_id(
            crate::database::CLAUDE_DESKTOP_OFFICIAL_PROVIDER_ID,
            AppType::ClaudeDesktop,
        )?),
        "import_claude_desktop_providers_from_claude" => Err(AppError::Config(
            "Claude Desktop import is not available in web mode yet".to_string(),
        )),
        "import_opencode_providers_from_live" => {
            ok(crate::services::provider::import_opencode_providers_from_live(state)?)
        }
        "import_openclaw_providers_from_live" => {
            ok(crate::services::provider::import_openclaw_providers_from_live(state)?)
        }
        "import_hermes_providers_from_live" => {
            ok(crate::services::provider::import_hermes_providers_from_live(state)?)
        }
        "get_opencode_live_provider_ids" => ok(crate::opencode_config::get_providers()
            .map(|providers| providers.keys().cloned().collect::<Vec<_>>())?),
        "get_openclaw_live_provider_ids" => ok(crate::openclaw_config::get_providers()
            .map(|providers| providers.keys().cloned().collect::<Vec<_>>())?),
        "get_hermes_live_provider_ids" => ok(crate::hermes_config::get_providers()
            .map(|providers| providers.keys().cloned().collect::<Vec<_>>())?),
        "get_openclaw_live_provider" => {
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            ok(crate::openclaw_config::get_provider(&provider_id)?)
        }
        "get_hermes_live_provider" => {
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            ok(crate::hermes_config::get_provider(&provider_id)?)
        }

        "start_proxy_server" => ok(state
            .proxy_service
            .start()
            .await
            .map_err(AppError::Config)?),
        "stop_proxy_server" => {
            let takeover = state
                .proxy_service
                .get_takeover_status()
                .await
                .map_err(AppError::Config)?;
            if takeover.claude
                || takeover.codex
                || takeover.gemini
                || takeover.opencode
                || takeover.openclaw
            {
                return Err(AppError::Config(
                    "仍有应用处于代理接管状态，请先关闭对应应用接管后再停止本地路由。".to_string(),
                ));
            }
            state.proxy_service.stop().await.map_err(AppError::Config)?;
            ok(())
        }
        "stop_proxy_with_restore" => {
            state
                .proxy_service
                .stop_with_restore()
                .await
                .map_err(AppError::Config)?;
            ok(())
        }
        "get_proxy_status" => ok(state
            .proxy_service
            .get_status()
            .await
            .map_err(AppError::Config)?),
        "is_proxy_running" => ok(state.proxy_service.is_running().await),
        "is_live_takeover_active" => ok(state
            .proxy_service
            .is_takeover_active()
            .await
            .map_err(AppError::Config)?),
        "get_proxy_takeover_status" => ok(state
            .proxy_service
            .get_takeover_status()
            .await
            .map_err(AppError::Config)?),
        "set_proxy_takeover_for_app" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            let enabled = arg_bool(&args, &["enabled"])?;
            state
                .proxy_service
                .set_takeover_for_app(&app_type, enabled)
                .await
                .map_err(AppError::Config)?;
            ok(())
        }
        "switch_proxy_provider" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            let provider = state
                .db
                .get_provider_by_id(&provider_id, &app_type)?
                .ok_or_else(|| AppError::Config(format!("provider not found: {provider_id}")))?;
            if provider.category.as_deref() == Some("official") {
                return Err(AppError::Config(
                    "代理接管模式下不能切换到官方供应商".to_string(),
                ));
            }
            state
                .proxy_service
                .switch_proxy_target(&app_type, &provider_id)
                .await
                .map_err(AppError::Config)?;
            ok(())
        }
        "get_proxy_config" => ok(state
            .proxy_service
            .get_config()
            .await
            .map_err(AppError::Config)?),
        "update_proxy_config" => {
            let config = take::<crate::proxy::types::ProxyConfig>(&args, "config")?;
            state
                .proxy_service
                .update_config(&config)
                .await
                .map_err(AppError::Config)?;
            ok(())
        }
        "get_global_proxy_config" => ok(state.db.get_global_proxy_config().await?),
        "update_global_proxy_config" => {
            let config = take::<crate::proxy::types::GlobalProxyConfig>(&args, "config")?;
            state.db.update_global_proxy_config(config).await?;
            ok(())
        }
        "get_proxy_config_for_app" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            ok(state.db.get_proxy_config_for_app(&app_type).await?)
        }
        "update_proxy_config_for_app" => {
            let config = take::<crate::proxy::types::AppProxyConfig>(&args, "config")?;
            let app_type = config.app_type.clone();
            let circuit = crate::proxy::CircuitBreakerConfig::from(&config);
            state.db.update_proxy_config_for_app(config).await?;
            state
                .proxy_service
                .update_circuit_breaker_config_for_app(&app_type, circuit)
                .await
                .map_err(AppError::Config)?;
            ok(())
        }
        "get_default_cost_multiplier" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            ok(state.db.get_default_cost_multiplier(&app_type).await?)
        }
        "set_default_cost_multiplier" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            let value = arg_string(&args, &["value"])?;
            state
                .db
                .set_default_cost_multiplier(&app_type, &value)
                .await?;
            ok(())
        }
        "get_pricing_model_source" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            ok(state.db.get_pricing_model_source(&app_type).await?)
        }
        "set_pricing_model_source" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            let value = arg_string(&args, &["value"])?;
            state.db.set_pricing_model_source(&app_type, &value).await?;
            ok(())
        }
        "get_provider_health" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            ok(state
                .db
                .get_provider_health(&provider_id, &app_type)
                .await?)
        }
        "get_failover_queue" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            ok(state.db.get_failover_queue(&app_type)?)
        }
        "get_available_providers_for_failover" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            ok(state.db.get_available_providers_for_failover(&app_type)?)
        }
        "add_to_failover_queue" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            state.db.add_to_failover_queue(&app_type, &provider_id)?;
            ok(())
        }
        "remove_from_failover_queue" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            state
                .db
                .remove_from_failover_queue(&app_type, &provider_id)?;
            ok(())
        }
        "get_auto_failover_enabled" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            ok(state
                .db
                .get_proxy_config_for_app(&app_type)
                .await?
                .auto_failover_enabled)
        }

        "get_common_config_snippet" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            ok(state.db.get_config_snippet(&app_type)?)
        }
        "set_common_config_snippet" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            let snippet = arg_string(&args, &["snippet"])?;
            validate_common_config_snippet(&app_type, &snippet)?;
            let is_cleared = snippet.trim().is_empty();
            let value = if is_cleared { None } else { Some(snippet) };
            state.db.set_config_snippet(&app_type, value)?;
            state.db.set_config_snippet_cleared(&app_type, is_cleared)?;
            ok(())
        }
        "get_claude_common_config_snippet" => ok(state.db.get_config_snippet("claude")?),
        "set_claude_common_config_snippet" => {
            let snippet = arg_string(&args, &["snippet"])?;
            validate_common_config_snippet("claude", &snippet)?;
            let is_cleared = snippet.trim().is_empty();
            let value = if is_cleared { None } else { Some(snippet) };
            state.db.set_config_snippet("claude", value)?;
            state.db.set_config_snippet_cleared("claude", is_cleared)?;
            ok(())
        }
        "update_toml_common_config_snippet" => {
            let config_toml = arg_string(&args, &["configToml", "config_toml"])?;
            let snippet_toml = arg_string(&args, &["snippetToml", "snippet_toml"])?;
            let enabled = arg_bool(&args, &["enabled"])?;
            ok(
                crate::services::provider::update_toml_common_config_snippet(
                    &config_toml,
                    &snippet_toml,
                    enabled,
                )?,
            )
        }

        "get_global_proxy_url" => ok(state.db.get_global_proxy_url()?),
        "set_global_proxy_url" => {
            let url = arg_string(&args, &["url"])?;
            let normalized = if url.trim().is_empty() {
                None
            } else {
                Some(url.as_str())
            };
            state.db.set_global_proxy_url(normalized)?;
            crate::proxy::http_client::apply_proxy(normalized)
                .map_err(|e| AppError::Config(e.to_string()))?;
            ok(())
        }

        "get_claude_mcp_status" => ok(crate::claude_mcp::get_mcp_status()?),
        "read_claude_mcp_config" => ok(crate::claude_mcp::read_mcp_json()?),
        "upsert_claude_mcp_server" => {
            let id = arg_string(&args, &["id"])?;
            let spec = args
                .get("spec")
                .cloned()
                .ok_or_else(|| AppError::Config("missing argument `spec`".to_string()))?;
            ok(crate::claude_mcp::upsert_mcp_server(&id, spec)?)
        }
        "delete_claude_mcp_server" => {
            let id = arg_string(&args, &["id"])?;
            ok(crate::claude_mcp::delete_mcp_server(&id)?)
        }
        "validate_mcp_command" => {
            let cmd = arg_string(&args, &["cmd"])?;
            ok(crate::claude_mcp::validate_command_in_path(&cmd)?)
        }
        "get_mcp_config" => {
            let app = app_type(&args, "app")?;
            let servers = get_mcp_servers_compat(state, app)?;
            ok(McpConfigResponse {
                config_path: crate::config::get_app_config_path()
                    .to_string_lossy()
                    .to_string(),
                servers,
            })
        }
        "upsert_mcp_server_in_config" => {
            let app = app_type(&args, "app")?;
            let id = arg_string(&args, &["id"])?;
            let spec = args
                .get("spec")
                .cloned()
                .ok_or_else(|| AppError::Config("missing argument `spec`".to_string()))?;
            let sync_other_side =
                arg_bool_opt(&args, &["syncOtherSide", "sync_other_side"])?.unwrap_or(false);
            let existing_server = state.db.get_all_mcp_servers()?.get(&id).cloned();
            let mut server = if let Some(mut existing) = existing_server {
                existing.server = spec.clone();
                existing.apps.set_enabled_for(&app, true);
                existing
            } else {
                let mut apps = crate::app_config::McpApps::default();
                apps.set_enabled_for(&app, true);
                let name = spec
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_string();
                crate::app_config::McpServer {
                    id: id.clone(),
                    name,
                    server: spec,
                    apps,
                    description: None,
                    homepage: None,
                    docs: None,
                    tags: Vec::new(),
                }
            };
            if sync_other_side {
                server.apps.claude = true;
                server.apps.codex = true;
                server.apps.gemini = true;
                server.apps.opencode = true;
            }
            McpService::upsert_server(state, server)?;
            ok(true)
        }
        "delete_mcp_server_in_config" => {
            let id = arg_string(&args, &["id"])?;
            ok(McpService::delete_server(state, &id)?)
        }
        "set_mcp_enabled" => {
            let app = app_type(&args, "app")?;
            let id = arg_string(&args, &["id"])?;
            let enabled = arg_bool(&args, &["enabled"])?;
            ok(set_mcp_enabled_compat(state, app, &id, enabled)?)
        }
        "get_mcp_servers" => ok(McpService::get_all_servers(state)?),
        "upsert_mcp_server" => {
            let server = take::<crate::app_config::McpServer>(&args, "server")?;
            McpService::upsert_server(state, server)?;
            ok(())
        }
        "delete_mcp_server" => {
            let id = arg_string(&args, &["id"])?;
            ok(McpService::delete_server(state, &id)?)
        }
        "toggle_mcp_app" => {
            let server_id = arg_string(&args, &["serverId", "server_id"])?;
            let app = app_type(&args, "app")?;
            let enabled = arg_bool(&args, &["enabled"])?;
            McpService::toggle_app(state, &server_id, app, enabled)?;
            ok(())
        }
        "import_mcp_from_apps" => ok(McpService::import_from_all_apps(state)?),

        "get_prompts" => {
            let app = app_type(&args, "app")?;
            ok(PromptService::get_prompts(state, app)?)
        }
        "upsert_prompt" => {
            let app = app_type(&args, "app")?;
            let id = arg_string(&args, &["id"])?;
            let prompt = take::<crate::prompt::Prompt>(&args, "prompt")?;
            PromptService::upsert_prompt(state, app, &id, prompt)?;
            ok(())
        }
        "delete_prompt" => {
            let app = app_type(&args, "app")?;
            let id = arg_string(&args, &["id"])?;
            PromptService::delete_prompt(state, app, &id)?;
            ok(())
        }
        "enable_prompt" => {
            let app = app_type(&args, "app")?;
            let id = arg_string(&args, &["id"])?;
            PromptService::enable_prompt(state, app, &id)?;
            ok(())
        }
        "import_prompt_from_file" => {
            let app = app_type(&args, "app")?;
            ok(PromptService::import_from_file(state, app)?)
        }
        "get_current_prompt_file_content" => {
            let app = app_type(&args, "app")?;
            ok(PromptService::get_current_file_content(app)?)
        }

        "get_usage_summary" => ok(state.db.get_usage_summary(
            arg_i64_opt(&args, &["startDate", "start_date"])?,
            arg_i64_opt(&args, &["endDate", "end_date"])?,
            arg_string_opt(&args, &["appType", "app_type"]).as_deref(),
            arg_string_opt(&args, &["providerName", "provider_name"]).as_deref(),
            arg_string_opt(&args, &["model"]).as_deref(),
        )?),
        "get_usage_summary_by_app" => ok(state.db.get_usage_summary_by_app(
            arg_i64_opt(&args, &["startDate", "start_date"])?,
            arg_i64_opt(&args, &["endDate", "end_date"])?,
            arg_string_opt(&args, &["providerName", "provider_name"]).as_deref(),
            arg_string_opt(&args, &["model"]).as_deref(),
        )?),
        "get_usage_trends" => ok(state.db.get_daily_trends(
            arg_i64_opt(&args, &["startDate", "start_date"])?,
            arg_i64_opt(&args, &["endDate", "end_date"])?,
            arg_string_opt(&args, &["appType", "app_type"]).as_deref(),
            arg_string_opt(&args, &["providerName", "provider_name"]).as_deref(),
            arg_string_opt(&args, &["model"]).as_deref(),
        )?),
        "get_provider_stats" => ok(state.db.get_provider_stats(
            arg_i64_opt(&args, &["startDate", "start_date"])?,
            arg_i64_opt(&args, &["endDate", "end_date"])?,
            arg_string_opt(&args, &["appType", "app_type"]).as_deref(),
            arg_string_opt(&args, &["providerName", "provider_name"]).as_deref(),
            arg_string_opt(&args, &["model"]).as_deref(),
        )?),
        "get_model_stats" => ok(state.db.get_model_stats(
            arg_i64_opt(&args, &["startDate", "start_date"])?,
            arg_i64_opt(&args, &["endDate", "end_date"])?,
            arg_string_opt(&args, &["appType", "app_type"]).as_deref(),
            arg_string_opt(&args, &["providerName", "provider_name"]).as_deref(),
            arg_string_opt(&args, &["model"]).as_deref(),
        )?),
        "get_request_logs" => {
            let filters = take::<crate::services::usage_stats::LogFilters>(&args, "filters")?;
            let page = arg_u64_opt(&args, &["page"])?.unwrap_or(0) as u32;
            let page_size = arg_u64_opt(&args, &["pageSize", "page_size"])?.unwrap_or(20) as u32;
            ok(state.db.get_request_logs(&filters, page, page_size)?)
        }
        "get_request_detail" => {
            let request_id = arg_string(&args, &["requestId", "request_id"])?;
            ok(state.db.get_request_detail(&request_id)?)
        }
        "get_model_pricing" => ok(get_model_pricing(state)?),
        "update_model_pricing" => {
            update_model_pricing(
                state,
                arg_string(&args, &["modelId", "model_id"])?,
                arg_string(&args, &["displayName", "display_name"])?,
                arg_string(&args, &["inputCost", "input_cost"])?,
                arg_string(&args, &["outputCost", "output_cost"])?,
                arg_string(&args, &["cacheReadCost", "cache_read_cost"])?,
                arg_string(&args, &["cacheCreationCost", "cache_creation_cost"])?,
            )?;
            ok(())
        }
        "delete_model_pricing" => {
            let model_id = arg_string(&args, &["modelId", "model_id"])?;
            delete_model_pricing(state, model_id)?;
            ok(())
        }
        "check_provider_limits" => {
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            ok(state.db.check_provider_limits(&provider_id, &app_type)?)
        }
        "sync_session_usage" => ok(sync_session_usage(state)?),
        "get_usage_data_sources" => {
            ok(crate::services::session_usage::get_data_source_breakdown(&state.db)?)
        }

        "list_profiles" => ok(list_profiles(state)?),
        "create_profile" => {
            let name = arg_string(&args, &["name"])?;
            let scope =
                crate::services::profile::ProfileScope::parse(&arg_string(&args, &["scope"])?)
                    .map_err(|e| AppError::Config(e.to_string()))?;
            ok(ProfileDto::from(ProfileService::create(state, &name, scope)?))
        }
        "update_profile" => {
            let id = arg_string(&args, &["id"])?;
            let scope = arg_string_opt(&args, &["scope"])
                .map(|scope| crate::services::profile::ProfileScope::parse(&scope))
                .transpose()
                .map_err(|e| AppError::Config(e.to_string()))?;
            ok(ProfileDto::from(ProfileService::update(
                state,
                &id,
                arg_string_opt(&args, &["name"]),
                arg_bool_opt(&args, &["resnapshot"])?.unwrap_or(false),
                scope,
            )?))
        }
        "delete_profile" => {
            let id = arg_string(&args, &["id"])?;
            ProfileService::delete(state, &id)?;
            ok(())
        }
        "clear_current_profile" => {
            let scope =
                crate::services::profile::ProfileScope::parse(&arg_string(&args, &["scope"])?)
                    .map_err(|e| AppError::Config(e.to_string()))?;
            state.db.set_current_profile_id(scope.as_str(), None)?;
            ok(())
        }
        "apply_profile" => {
            let id = arg_string(&args, &["id"])?;
            let scope =
                crate::services::profile::ProfileScope::parse(&arg_string(&args, &["scope"])?)
                    .map_err(|e| AppError::Config(e.to_string()))?;
            let (warnings, should_stop_proxy) = ProfileService::apply(state, &id, scope)?;
            if should_stop_proxy {
                state
                    .proxy_service
                    .stop()
                    .await
                    .map_err(AppError::Config)?;
            }
            ok(warnings)
        }

        "get_installed_skills" => ok(SkillService::get_all_installed(&state.db)
            .map_err(|e| AppError::Config(e.to_string()))?),
        "get_skill_backups" => {
            ok(SkillService::list_backups().map_err(|e| AppError::Config(e.to_string()))?)
        }
        "delete_skill_backup" => {
            let backup_id = arg_string(&args, &["backupId", "backup_id"])?;
            SkillService::delete_backup(&backup_id).map_err(|e| AppError::Config(e.to_string()))?;
            ok(true)
        }
        "install_skill_unified" => {
            let skill = take::<crate::services::skill::DiscoverableSkill>(&args, "skill")?;
            let current_app = app_type_from_string(arg_string(&args, &["currentApp", "current_app"])?)?;
            ok(SkillService::new()
                .install(&state.db, &skill, &current_app)
                .await
                .map_err(|e| AppError::Config(e.to_string()))?)
        }
        "uninstall_skill_unified" => {
            let id = arg_string(&args, &["id"])?;
            ok(SkillService::uninstall(&state.db, &id)
                .map_err(|e| AppError::Config(e.to_string()))?)
        }
        "restore_skill_backup" => {
            let backup_id = arg_string(&args, &["backupId", "backup_id"])?;
            let current_app = app_type_from_string(arg_string(&args, &["currentApp", "current_app"])?)?;
            ok(SkillService::restore_from_backup(
                &state.db,
                &backup_id,
                &current_app,
            )
            .map_err(|e| AppError::Config(e.to_string()))?)
        }
        "toggle_skill_app" => {
            let id = arg_string(&args, &["id"])?;
            let app = app_type_from_string(arg_string(&args, &["app"])?)?;
            let enabled = arg_bool(&args, &["enabled"])?;
            SkillService::toggle_app(&state.db, &id, &app, enabled)
                .map_err(|e| AppError::Config(e.to_string()))?;
            ok(true)
        }
        "scan_unmanaged_skills" => ok(SkillService::scan_unmanaged(&state.db)
            .map_err(|e| AppError::Config(e.to_string()))?),
        "import_skills_from_apps" => {
            let imports =
                take::<Vec<crate::services::skill::ImportSkillSelection>>(&args, "imports")?;
            ok(SkillService::import_from_apps(&state.db, imports)
                .map_err(|e| AppError::Config(e.to_string()))?)
        }
        "discover_available_skills" => {
            let repos = state.db.get_skill_repos()?;
            ok(SkillService::new()
                .discover_available(repos)
                .await
                .map_err(|e| AppError::Config(e.to_string()))?)
        }
        "check_skill_updates" => ok(SkillService::new()
            .check_updates(&state.db)
            .await
            .map_err(|e| AppError::Config(e.to_string()))?),
        "update_skill" => {
            let id = arg_string(&args, &["id"])?;
            ok(SkillService::new()
                .update_skill(&state.db, &id)
                .await
                .map_err(|e| AppError::Config(e.to_string()))?)
        }
        "migrate_skill_storage" => {
            let target =
                take::<crate::services::skill::SkillStorageLocation>(&args, "target")?;
            ok(SkillService::migrate_storage(&state.db, target)
                .map_err(|e| AppError::Config(e.to_string()))?)
        }
        "search_skills_sh" => {
            let query = arg_string(&args, &["query"])?;
            let limit = arg_u64_opt(&args, &["limit"])?.unwrap_or(20) as usize;
            let offset = arg_u64_opt(&args, &["offset"])?.unwrap_or(0) as usize;
            ok(SkillService::search_skills_sh(&query, limit, offset)
                .await
                .map_err(|e| AppError::Config(e.to_string()))?)
        }
        "get_skills" => {
            let repos = state.db.get_skill_repos()?;
            ok(SkillService::new()
                .list_skills(repos, &state.db)
                .await
                .map_err(|e| AppError::Config(e.to_string()))?)
        }
        "get_skills_for_app" => {
            let _ = app_type(&args, "app")?;
            let repos = state.db.get_skill_repos()?;
            ok(SkillService::new()
                .list_skills(repos, &state.db)
                .await
                .map_err(|e| AppError::Config(e.to_string()))?)
        }
        "install_skill" => {
            let directory = arg_string(&args, &["directory"])?;
            install_skill_for_app(state, AppType::Claude, directory).await?;
            ok(true)
        }
        "install_skill_for_app" => {
            let app = app_type(&args, "app")?;
            let directory = arg_string(&args, &["directory"])?;
            install_skill_for_app(state, app, directory).await?;
            ok(true)
        }
        "uninstall_skill" => {
            let directory = arg_string(&args, &["directory"])?;
            ok(uninstall_skill_by_directory(
                state,
                AppType::Claude,
                directory,
            )?)
        }
        "uninstall_skill_for_app" => {
            let app = app_type(&args, "app")?;
            let directory = arg_string(&args, &["directory"])?;
            ok(uninstall_skill_by_directory(state, app, directory)?)
        }
        "get_skill_repos" => ok(state.db.get_skill_repos()?),
        "add_skill_repo" => {
            let repo = take::<crate::services::skill::SkillRepo>(&args, "repo")?;
            state.db.save_skill_repo(&repo)?;
            ok(true)
        }
        "remove_skill_repo" => {
            let owner = arg_string(&args, &["owner"])?;
            let name = arg_string(&args, &["name"])?;
            state.db.delete_skill_repo(&owner, &name)?;
            ok(true)
        }
        "install_skills_from_zip" => {
            let file_path = arg_string(&args, &["filePath", "file_path"])?;
            let current_app = app_type_from_string(arg_string(&args, &["currentApp", "current_app"])?)?;
            ok(SkillService::install_from_zip(
                &state.db,
                std::path::Path::new(&file_path),
                &current_app,
            )
            .map_err(|e| AppError::Config(e.to_string()))?)
        }
        "open_zip_file_dialog" => Err(AppError::Config(
            "ZIP file picker is not available in web mode; provide a local file path instead"
                .to_string(),
        )),

        "list_sessions" => ok(tokio::task::spawn_blocking(crate::session_manager::scan_sessions)
            .await
            .map_err(|e| AppError::Config(format!("failed to scan sessions: {e}")))?),
        "get_session_messages" => {
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            let source_path = arg_string(&args, &["sourcePath", "source_path"])?;
            ok(tokio::task::spawn_blocking(move || {
                crate::session_manager::load_messages(&provider_id, &source_path)
            })
            .await
            .map_err(|e| AppError::Config(format!("failed to load session messages: {e}")))?
            .map_err(AppError::Config)?)
        }
        "delete_session" => {
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            let session_id = arg_string(&args, &["sessionId", "session_id"])?;
            let source_path = arg_string(&args, &["sourcePath", "source_path"])?;
            ok(tokio::task::spawn_blocking(move || {
                crate::session_manager::delete_session(&provider_id, &session_id, &source_path)
            })
            .await
            .map_err(|e| AppError::Config(format!("failed to delete session: {e}")))?
            .map_err(AppError::Config)?)
        }
        "delete_sessions" => {
            let items = take::<Vec<crate::session_manager::DeleteSessionRequest>>(&args, "items")?;
            ok(tokio::task::spawn_blocking(move || {
                crate::session_manager::delete_sessions(&items)
            })
            .await
            .map_err(|e| AppError::Config(format!("failed to delete sessions: {e}")))?)
        }
        "launch_session_terminal" => Err(AppError::Config(
            "Launching a native terminal is not available in web mode; copy and run the command manually"
                .to_string(),
        )),

        "check_env_conflicts" => {
            let app = arg_string(&args, &["app"])?;
            ok(
                crate::services::env_checker::check_env_conflicts(&app)
                    .map_err(AppError::Config)?,
            )
        }
        "delete_env_vars" => {
            let conflicts =
                take::<Vec<crate::services::env_checker::EnvConflict>>(&args, "conflicts")?;
            ok(crate::services::env_manager::delete_env_vars(conflicts)
                .map_err(AppError::Config)?)
        }
        "restore_env_backup" => {
            let backup_path = arg_string(&args, &["backupPath", "backup_path"])?;
            crate::services::env_manager::restore_from_backup(backup_path)
                .map_err(AppError::Config)?;
            ok(())
        }

        "copy_text_to_clipboard" => ok(false),
        "update_tray_menu" | "set_window_theme" => ok(false),
        "open_config_folder" | "open_app_config_folder" | "open_external" => ok(false),

        other => Err(AppError::Config(format!(
            "command `{other}` is not available in web mode yet"
        ))),
    }
}

fn ok<T: Serialize>(value: T) -> Result<Value, AppError> {
    serde_json::to_value(value).map_err(|e| AppError::Config(e.to_string()))
}

fn app_type(args: &Value, key: &str) -> Result<AppType, AppError> {
    app_type_from_string(arg_string(args, &[key])?)
}

fn app_type_from_string(app: String) -> Result<AppType, AppError> {
    AppType::from_str(&app).map_err(|e| AppError::Config(e.to_string()))
}

async fn install_skill_for_app(
    state: &AppState,
    app: AppType,
    directory: String,
) -> Result<(), AppError> {
    let repos = state.db.get_skill_repos()?;
    let skills = SkillService::new()
        .discover_available(repos)
        .await
        .map_err(|e| AppError::Config(e.to_string()))?;

    let skill = skills
        .into_iter()
        .find(|skill| {
            let install_name = std::path::Path::new(&skill.directory)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| skill.directory.clone());

            install_name.eq_ignore_ascii_case(&directory)
                || skill.directory.eq_ignore_ascii_case(&directory)
        })
        .ok_or_else(|| AppError::Config(format!("Skill not found: {directory}")))?;

    SkillService::new()
        .install(&state.db, &skill, &app)
        .await
        .map_err(|e| AppError::Config(e.to_string()))?;
    Ok(())
}

fn uninstall_skill_by_directory(
    state: &AppState,
    app: AppType,
    directory: String,
) -> Result<crate::services::skill::SkillUninstallResult, AppError> {
    let _ = app;
    let skills =
        SkillService::get_all_installed(&state.db).map_err(|e| AppError::Config(e.to_string()))?;

    let skill = skills
        .into_iter()
        .find(|skill| skill.directory.eq_ignore_ascii_case(&directory))
        .ok_or_else(|| AppError::Config(format!("Installed skill not found: {directory}")))?;

    SkillService::uninstall(&state.db, &skill.id).map_err(|e| AppError::Config(e.to_string()))
}

fn list_profiles(state: &AppState) -> Result<ProfilesResponse, AppError> {
    let profiles = ProfileService::list(state)?;
    let current_ids = CurrentProfileIds {
        claude: state
            .db
            .get_current_profile_id(crate::services::profile::ProfileScope::Claude.as_str())?,
        claude_desktop: state.db.get_current_profile_id(
            crate::services::profile::ProfileScope::ClaudeDesktop.as_str(),
        )?,
        codex: state
            .db
            .get_current_profile_id(crate::services::profile::ProfileScope::Codex.as_str())?,
    };
    Ok(ProfilesResponse {
        profiles: profiles.into_iter().map(ProfileDto::from).collect(),
        current_ids,
    })
}

#[allow(deprecated)]
fn get_mcp_servers_compat(
    state: &AppState,
    app: AppType,
) -> Result<HashMap<String, serde_json::Value>, AppError> {
    McpService::get_servers(state, app)
}

#[allow(deprecated)]
fn set_mcp_enabled_compat(
    state: &AppState,
    app: AppType,
    id: &str,
    enabled: bool,
) -> Result<bool, AppError> {
    McpService::set_enabled(state, app, id, enabled)
}

fn get_model_pricing(state: &AppState) -> Result<Vec<ModelPricingInfo>, AppError> {
    state.db.ensure_model_pricing_seeded()?;
    let db = state.db.clone();
    let conn = crate::database::lock_conn!(db.conn);

    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='model_pricing'",
            [],
            |row| row.get::<_, i64>(0).map(|count| count > 0),
        )
        .unwrap_or(false);
    if !table_exists {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT model_id, display_name, input_cost_per_million, output_cost_per_million,
                cache_read_cost_per_million, cache_creation_cost_per_million
         FROM model_pricing
         ORDER BY display_name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ModelPricingInfo {
            model_id: row.get(0)?,
            display_name: row.get(1)?,
            input_cost_per_million: row.get(2)?,
            output_cost_per_million: row.get(3)?,
            cache_read_cost_per_million: row.get(4)?,
            cache_creation_cost_per_million: row.get(5)?,
        })
    })?;

    let mut pricing = Vec::new();
    for row in rows {
        pricing.push(row?);
    }
    Ok(pricing)
}

fn update_model_pricing(
    state: &AppState,
    model_id: String,
    display_name: String,
    input_cost: String,
    output_cost: String,
    cache_read_cost: String,
    cache_creation_cost: String,
) -> Result<(), AppError> {
    let model_id = model_id.trim().to_string();
    let display_name = display_name.trim().to_string();
    if model_id.is_empty() {
        return Err(AppError::localized(
            "usage.modelIdRequired",
            "模型 ID 不能为空",
            "Model ID is required",
        ));
    }
    if display_name.is_empty() {
        return Err(AppError::localized(
            "usage.displayNameRequired",
            "显示名称不能为空",
            "Display name is required",
        ));
    }

    for (label, value) in [
        ("input_cost", &input_cost),
        ("output_cost", &output_cost),
        ("cache_read_cost", &cache_read_cost),
        ("cache_creation_cost", &cache_creation_cost),
    ] {
        let parsed = rust_decimal::Decimal::from_str(value.trim()).map_err(|e| {
            AppError::localized(
                "usage.invalidPrice",
                format!("{label} 价格无效: {value} - {e}"),
                format!("{label} price is invalid: {value} - {e}"),
            )
        })?;
        if parsed < rust_decimal::Decimal::ZERO {
            return Err(AppError::localized(
                "usage.invalidPrice",
                format!("{label} 价格必须为非负数: {value}"),
                format!("{label} price must be non-negative: {value}"),
            ));
        }
    }

    {
        let db = state.db.clone();
        let conn = crate::database::lock_conn!(db.conn);
        conn.execute(
            "INSERT OR REPLACE INTO model_pricing (
                model_id, display_name, input_cost_per_million, output_cost_per_million,
                cache_read_cost_per_million, cache_creation_cost_per_million
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                model_id,
                display_name,
                input_cost.trim(),
                output_cost.trim(),
                cache_read_cost.trim(),
                cache_creation_cost.trim()
            ],
        )
        .map_err(|e| AppError::Database(format!("更新模型定价失败: {e}")))?;
    }

    if let Err(e) = state.db.backfill_missing_usage_costs_for_model(&model_id) {
        log::warn!(
            "Failed to backfill usage costs after pricing update (model_id={model_id}): {e}"
        );
    }
    Ok(())
}

fn delete_model_pricing(state: &AppState, model_id: String) -> Result<(), AppError> {
    let db = state.db.clone();
    let conn = crate::database::lock_conn!(db.conn);
    conn.execute(
        "DELETE FROM model_pricing WHERE model_id = ?1",
        rusqlite::params![model_id],
    )
    .map_err(|e| AppError::Database(format!("删除模型定价失败: {e}")))?;
    Ok(())
}

fn sync_session_usage(
    state: &AppState,
) -> Result<crate::services::session_usage::SessionSyncResult, AppError> {
    let mut result = crate::services::session_usage::sync_claude_session_logs(&state.db)?;

    match crate::services::session_usage_codex::sync_codex_usage(&state.db) {
        Ok(next) => merge_session_sync_result(&mut result, next),
        Err(e) => result.errors.push(format!("Codex sync failed: {e}")),
    }
    match crate::services::session_usage_gemini::sync_gemini_usage(&state.db) {
        Ok(next) => merge_session_sync_result(&mut result, next),
        Err(e) => result.errors.push(format!("Gemini sync failed: {e}")),
    }
    match crate::services::session_usage_opencode::sync_opencode_usage(&state.db) {
        Ok(next) => merge_session_sync_result(&mut result, next),
        Err(e) => result.errors.push(format!("OpenCode sync failed: {e}")),
    }

    Ok(result)
}

fn merge_session_sync_result(
    target: &mut crate::services::session_usage::SessionSyncResult,
    source: crate::services::session_usage::SessionSyncResult,
) {
    target.imported += source.imported;
    target.skipped += source.skipped;
    target.files_scanned += source.files_scanned;
    target.errors.extend(source.errors);
}

fn get_config_dir(app: String) -> Result<String, AppError> {
    let dir = match AppType::from_str(&app).map_err(|e| AppError::Config(e.to_string()))? {
        AppType::Claude => crate::config::get_claude_config_dir(),
        AppType::ClaudeDesktop => crate::claude_desktop_config::get_config_library_path()
            .map_err(|e| AppError::Config(e.to_string()))?,
        AppType::Codex => crate::codex_config::get_codex_config_dir(),
        AppType::Gemini => crate::gemini_config::get_gemini_dir(),
        AppType::OpenCode => crate::opencode_config::get_opencode_dir(),
        AppType::OpenClaw => crate::openclaw_config::get_openclaw_dir(),
        AppType::Hermes => crate::hermes_config::get_hermes_dir(),
    };
    Ok(dir.to_string_lossy().to_string())
}

fn is_portable_mode() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(FsPath::to_path_buf))
        .map(|dir| dir.join("portable.ini").is_file())
        .unwrap_or(false)
}

fn take<T: DeserializeOwned>(args: &Value, key: &str) -> Result<T, AppError> {
    let value = args
        .get(key)
        .cloned()
        .ok_or_else(|| AppError::Config(format!("missing argument `{key}`")))?;
    serde_json::from_value(value).map_err(|e| AppError::Config(e.to_string()))
}

fn arg_string(args: &Value, keys: &[&str]) -> Result<String, AppError> {
    keys.iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .map(ToString::to_string)
        .ok_or_else(|| AppError::Config(format!("missing string argument `{}`", keys[0])))
}

fn arg_string_opt(args: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .map(ToString::to_string)
}

fn arg_bool(args: &Value, keys: &[&str]) -> Result<bool, AppError> {
    arg_bool_opt(args, keys)?
        .ok_or_else(|| AppError::Config(format!("missing boolean argument `{}`", keys[0])))
}

fn arg_bool_opt(args: &Value, keys: &[&str]) -> Result<Option<bool>, AppError> {
    for key in keys {
        if let Some(value) = args.get(*key) {
            return value
                .as_bool()
                .map(Some)
                .ok_or_else(|| AppError::Config(format!("argument `{key}` must be boolean")));
        }
    }
    Ok(None)
}

fn arg_u64_opt(args: &Value, keys: &[&str]) -> Result<Option<u64>, AppError> {
    for key in keys {
        if let Some(value) = args.get(*key) {
            return value
                .as_u64()
                .map(Some)
                .ok_or_else(|| AppError::Config(format!("argument `{key}` must be number")));
        }
    }
    Ok(None)
}

fn arg_i64_opt(args: &Value, keys: &[&str]) -> Result<Option<i64>, AppError> {
    for key in keys {
        if let Some(value) = args.get(*key) {
            return value
                .as_i64()
                .map(Some)
                .ok_or_else(|| AppError::Config(format!("argument `{key}` must be number")));
        }
    }
    Ok(None)
}

fn validate_common_config_snippet(app_type: &str, snippet: &str) -> Result<(), AppError> {
    if snippet.trim().is_empty() {
        return Ok(());
    }

    match app_type {
        "claude" | "gemini" | "omo" | "omo-slim" => {
            serde_json::from_str::<Value>(snippet).map_err(|e| AppError::Config(e.to_string()))?;
        }
        "codex" => {
            snippet
                .parse::<toml_edit::DocumentMut>()
                .map_err(|e| AppError::Config(e.to_string()))?;
        }
        _ => {}
    }

    Ok(())
}
