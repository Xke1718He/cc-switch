use crate::app_config::AppType;
use crate::database::Database;
use crate::error::AppError;
use crate::provider::Provider;
use crate::services::profile::{ProfilePayload, ProfileService};
use crate::services::{
    McpService, OmoService, PromptService, ProviderService, SkillService, SpeedtestService,
};
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
use std::time::Duration;
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProxyTestResult {
    success: bool,
    latency_ms: u64,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamProxyStatus {
    enabled: bool,
    proxy_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DetectedProxy {
    url: String,
    proxy_type: String,
    port: u16,
}

#[derive(Debug, Serialize)]
struct ToolVersion {
    name: String,
    version: Option<String>,
    latest_version: Option<String>,
    error: Option<String>,
    installed_but_broken: bool,
    env_type: String,
    wsl_distro: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WslShellPreferenceInput {
    #[serde(default)]
    wsl_shell: Option<String>,
    #[serde(default)]
    wsl_shell_flag: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DailyMemoryFileInfo {
    filename: String,
    date: String,
    size_bytes: u64,
    modified_at: u64,
    preview: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DailyMemorySearchResult {
    filename: String,
    date: String,
    size_bytes: u64,
    modified_at: u64,
    snippet: String,
    match_count: usize,
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
        "has_codex_unify_history_backup" => {
            ok(crate::codex_history_migration::has_codex_official_history_unify_backup())
        }
        "restore_codex_unified_history" => ok(restore_codex_unified_history().await?),
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
        "check_for_updates"
        | "restart_app"
        | "install_update_and_restart"
        | "set_auto_launch"
        | "get_auto_launch_status" => ok(false),
        "apply_claude_plugin_config" => Err(AppError::Config(
            "Claude plugin config is not available in headless web mode".to_string(),
        )),
        "apply_claude_onboarding_skip" => ok(crate::claude_mcp::set_has_completed_onboarding()?),
        "clear_claude_onboarding_skip" => ok(crate::claude_mcp::clear_has_completed_onboarding()?),

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
        "testUsageScript" => {
            let app = app_type(&args, "app")?;
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            let script_code = arg_string(&args, &["scriptCode", "script_code"])?;
            ok(ProviderService::test_usage_script(
                state,
                app,
                &provider_id,
                &script_code,
                arg_u64_opt(&args, &["timeout"])?.unwrap_or(10),
                arg_string_opt(&args, &["apiKey", "api_key"]).as_deref(),
                arg_string_opt(&args, &["baseUrl", "base_url"]).as_deref(),
                arg_string_opt(&args, &["accessToken", "access_token"]).as_deref(),
                arg_string_opt(&args, &["userId", "user_id"]).as_deref(),
                arg_string_opt(&args, &["templateType", "template_type"]).as_deref(),
            )
            .await?)
        }
        "fetch_models_for_config" => {
            let base_url = arg_string(&args, &["baseUrl", "base_url"])?;
            let api_key = arg_string(&args, &["apiKey", "api_key"])?;
            let is_full_url = arg_bool_opt(&args, &["isFullUrl", "is_full_url"])?.unwrap_or(false);
            let models_url = arg_string_opt(&args, &["modelsUrl", "models_url"]);
            let custom_user_agent =
                arg_string_opt(&args, &["customUserAgent", "custom_user_agent"]);
            let api_format = arg_string_opt(&args, &["apiFormat", "api_format"]);
            let request_headers = args
                .get("requestHeaders")
                .or_else(|| args.get("request_headers"))
                .cloned()
                .map(serde_json::from_value::<std::collections::BTreeMap<String, String>>)
                .transpose()
                .map_err(|error| AppError::Config(format!("invalid request headers: {error}")))?;
            let user_agent = crate::provider::parse_custom_user_agent(custom_user_agent.as_deref())
                .ok()
                .flatten();
            ok(crate::services::model_fetch::fetch_models(
                &base_url,
                &api_key,
                is_full_url,
                models_url.as_deref(),
                user_agent,
                api_format.as_deref(),
                request_headers.as_ref(),
            )
            .await
            .map_err(AppError::Config)?)
        }
        "get_balance" => {
            let base_url = arg_string(&args, &["baseUrl", "base_url"])?;
            let api_key = arg_string(&args, &["apiKey", "api_key"])?;
            ok(crate::services::balance::get_balance(&base_url, &api_key)
                .await
                .map_err(AppError::Config)?)
        }
        "get_coding_plan_quota" => {
            let base_url = arg_string(&args, &["baseUrl", "base_url"])?;
            let api_key = arg_string(&args, &["apiKey", "api_key"])?;
            ok(crate::services::coding_plan::get_coding_plan_quota(
                &base_url,
                &api_key,
                arg_string_opt(&args, &["accessKeyId", "access_key_id"]).as_deref(),
                arg_string_opt(&args, &["secretAccessKey", "secret_access_key"]).as_deref(),
                arg_string_opt(&args, &["codingPlanProvider", "coding_plan_provider"]).as_deref(),
                arg_string_opt(&args, &["teamOrganizationId", "team_organization_id"]).as_deref(),
                arg_string_opt(&args, &["teamProjectId", "team_project_id"]).as_deref(),
            )
            .await
            .map_err(AppError::Config)?)
        }
        "get_subscription_quota" => {
            let tool = arg_string(&args, &["tool"])?;
            let quota = crate::services::subscription::get_subscription_quota(&tool)
                .await
                .map_err(AppError::Config)?;
            if let Ok(app_type) = AppType::from_str(&tool) {
                state.usage_cache.put_subscription(app_type, quota.clone());
            }
            ok(quota)
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
        "scan_openclaw_config_health" => ok(crate::openclaw_config::scan_openclaw_config_health()?),
        "get_openclaw_default_model" => ok(crate::openclaw_config::get_default_model()?),
        "set_openclaw_default_model" => {
            let model = take::<crate::openclaw_config::OpenClawDefaultModel>(&args, "model")?;
            ok(crate::openclaw_config::set_default_model(&model)?)
        }
        "get_openclaw_model_catalog" => ok(crate::openclaw_config::get_model_catalog()?),
        "set_openclaw_model_catalog" => {
            let catalog = take::<
                HashMap<String, crate::openclaw_config::OpenClawModelCatalogEntry>,
            >(&args, "catalog")?;
            ok(crate::openclaw_config::set_model_catalog(&catalog)?)
        }
        "get_openclaw_agents_defaults" => ok(crate::openclaw_config::get_agents_defaults()?),
        "set_openclaw_agents_defaults" => {
            let defaults =
                take::<crate::openclaw_config::OpenClawAgentsDefaults>(&args, "defaults")?;
            ok(crate::openclaw_config::set_agents_defaults(&defaults)?)
        }
        "get_openclaw_env" => ok(crate::openclaw_config::get_env_config()?),
        "set_openclaw_env" => {
            let env = take::<crate::openclaw_config::OpenClawEnvConfig>(&args, "env")?;
            ok(crate::openclaw_config::set_env_config(&env)?)
        }
        "get_openclaw_tools" => ok(crate::openclaw_config::get_tools_config()?),
        "set_openclaw_tools" => {
            let tools = take::<crate::openclaw_config::OpenClawToolsConfig>(&args, "tools")?;
            ok(crate::openclaw_config::set_tools_config(&tools)?)
        }
        "get_hermes_model_config" => ok(crate::hermes_config::get_model_config()?),
        "get_hermes_memory" => {
            let kind = take::<crate::hermes_config::MemoryKind>(&args, "kind")?;
            ok(crate::hermes_config::read_memory(kind)?)
        }
        "set_hermes_memory" => {
            let kind = take::<crate::hermes_config::MemoryKind>(&args, "kind")?;
            let content = arg_string(&args, &["content"])?;
            crate::hermes_config::write_memory(kind, &content)?;
            ok(())
        }
        "get_hermes_memory_limits" => ok(crate::hermes_config::read_memory_limits()?),
        "set_hermes_memory_enabled" => {
            let kind = take::<crate::hermes_config::MemoryKind>(&args, "kind")?;
            let enabled = arg_bool(&args, &["enabled"])?;
            ok(crate::hermes_config::set_memory_enabled(kind, enabled)?)
        }
        "open_hermes_web_ui" => {
            let path = arg_string_opt(&args, &["path"]);
            probe_hermes_web_ui(path).await?;
            ok(true)
        }
        "launch_hermes_dashboard" => Err(AppError::Config(
            "launching a native terminal is not available in web mode; run `hermes dashboard` manually"
                .to_string(),
        )),

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
        "set_auto_failover_enabled" => {
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            let enabled = arg_bool(&args, &["enabled"])?;
            set_auto_failover_enabled_web(state, app_type, enabled).await?;
            ok(())
        }
        "reset_circuit_breaker" => {
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            let app_type = arg_string(&args, &["appType", "app_type"])?;
            state
                .db
                .update_provider_health(&provider_id, &app_type, true, None)
                .await?;
            state
                .proxy_service
                .reset_provider_circuit_breaker(&provider_id, &app_type)
                .await
                .map_err(AppError::Config)?;
            ok(())
        }
        "get_circuit_breaker_config" => ok(state.db.get_circuit_breaker_config().await?),
        "update_circuit_breaker_config" => {
            let config = take::<crate::proxy::CircuitBreakerConfig>(&args, "config")?;
            state.db.update_circuit_breaker_config(&config).await?;
            state
                .proxy_service
                .update_circuit_breaker_configs(config)
                .await
                .map_err(AppError::Config)?;
            ok(())
        }
        "get_circuit_breaker_stats" => ok(None::<crate::proxy::CircuitBreakerStats>),

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
        "test_proxy_url" => {
            let url = arg_string(&args, &["url"])?;
            ok(test_proxy_url(url).await?)
        }
        "get_upstream_proxy_status" => ok(UpstreamProxyStatus {
            enabled: crate::proxy::http_client::get_current_proxy_url().is_some(),
            proxy_url: crate::proxy::http_client::get_current_proxy_url(),
        }),
        "scan_local_proxies" => ok(scan_local_proxies()),

        "export_config_to_file" => {
            let file_path = arg_string(&args, &["filePath", "file_path"])?;
            ok(export_config_to_file(state, file_path).await?)
        }
        "import_config_from_file" => {
            let file_path = arg_string(&args, &["filePath", "file_path"])?;
            ok(import_config_from_file(state, file_path).await?)
        }
        "sync_current_providers_live" => ok(sync_current_providers_live(state).await?),
        "create_db_backup" => ok(create_db_backup(state).await?),
        "list_db_backups" => ok(Database::list_backups()?),
        "restore_db_backup" => {
            let filename = arg_string(&args, &["filename"])?;
            ok(restore_db_backup(state, filename).await?)
        }
        "rename_db_backup" => {
            let old_filename = arg_string(&args, &["oldFilename", "old_filename"])?;
            let new_name = arg_string(&args, &["newName", "new_name"])?;
            ok(Database::rename_backup(&old_filename, &new_name)?)
        }
        "delete_db_backup" => {
            let filename = arg_string(&args, &["filename"])?;
            Database::delete_backup(&filename)?;
            ok(())
        }
        "open_file_dialog" | "save_file_dialog" | "pick_directory" => Err(AppError::Config(
            "native file dialogs are not available in web mode; provide a local path explicitly"
                .to_string(),
        )),

        "webdav_test_connection" => {
            let settings = take::<crate::settings::WebDavSyncSettings>(&args, "settings")?;
            let preserve_empty =
                arg_bool_opt(&args, &["preserveEmptyPassword", "preserve_empty_password"])?
                    .unwrap_or(true);
            ok(webdav_test_connection(settings, preserve_empty).await?)
        }
        "webdav_sync_save_settings" => {
            let settings = take::<crate::settings::WebDavSyncSettings>(&args, "settings")?;
            let password_touched =
                arg_bool_opt(&args, &["passwordTouched", "password_touched"])?.unwrap_or(false);
            ok(webdav_sync_save_settings(settings, password_touched)?)
        }
        "webdav_sync_upload" => ok(webdav_sync_upload(state).await?),
        "webdav_sync_download" => ok(webdav_sync_download(state).await?),
        "webdav_sync_fetch_remote_info" => ok(webdav_sync_fetch_remote_info().await?),
        "s3_test_connection" => {
            let settings = take::<crate::settings::S3SyncSettings>(&args, "settings")?;
            let preserve_empty =
                arg_bool_opt(&args, &["preserveEmptyPassword", "preserve_empty_password"])?
                    .unwrap_or(true);
            ok(s3_test_connection(settings, preserve_empty).await?)
        }
        "s3_sync_save_settings" => {
            let settings = take::<crate::settings::S3SyncSettings>(&args, "settings")?;
            let password_touched =
                arg_bool_opt(&args, &["passwordTouched", "password_touched"])?.unwrap_or(false);
            ok(s3_sync_save_settings(settings, password_touched)?)
        }
        "s3_sync_upload" => ok(s3_sync_upload(state).await?),
        "s3_sync_download" => ok(s3_sync_download(state).await?),
        "s3_sync_fetch_remote_info" => ok(s3_sync_fetch_remote_info().await?),

        "get_rectifier_config" => ok(state.db.get_rectifier_config()?),
        "set_rectifier_config" => {
            let config = take::<crate::proxy::types::RectifierConfig>(&args, "config")?;
            state.db.set_rectifier_config(&config)?;
            ok(true)
        }
        "get_optimizer_config" => ok(state.db.get_optimizer_config()?),
        "set_optimizer_config" => {
            let config = take::<crate::proxy::types::OptimizerConfig>(&args, "config")?;
            state.db.set_optimizer_config(&config)?;
            ok(true)
        }
        "get_copilot_optimizer_config" => ok(state.db.get_copilot_optimizer_config()?),
        "set_copilot_optimizer_config" => {
            let config = take::<crate::proxy::types::CopilotOptimizerConfig>(&args, "config")?;
            state.db.set_copilot_optimizer_config(&config)?;
            ok(true)
        }
        "get_log_config" => ok(state.db.get_log_config()?),
        "set_log_config" => {
            let config = take::<crate::proxy::types::LogConfig>(&args, "config")?;
            state.db.set_log_config(&config)?;
            log::set_max_level(config.to_level_filter());
            ok(true)
        }
        "extract_common_config_snippet" => {
            let app = app_type_from_string(arg_string(&args, &["appType", "app_type"])?)?;
            if let Some(settings_config) =
                arg_string_opt(&args, &["settingsConfig", "settings_config"])
                    .filter(|s| !s.trim().is_empty())
            {
                let settings = serde_json::from_str::<Value>(&settings_config)
                    .map_err(|e| AppError::Config(e.to_string()))?;
                ok(ProviderService::extract_common_config_snippet_from_settings(
                    app, &settings,
                )?)
            } else {
                ok(ProviderService::extract_common_config_snippet(state, app)?)
            }
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

        "read_omo_local_file" => ok(OmoService::read_local_file(&crate::services::omo::STANDARD)?),
        "get_current_omo_provider_id" => ok(state
            .db
            .get_current_omo_provider("opencode", "omo")?
            .map(|p| p.id)
            .unwrap_or_default()),
        "disable_current_omo" => {
            disable_omo_variant(state, "omo", &crate::services::omo::STANDARD)?;
            ok(())
        }
        "read_omo_slim_local_file" => {
            ok(OmoService::read_local_file(&crate::services::omo::SLIM)?)
        }
        "get_current_omo_slim_provider_id" => ok(state
            .db
            .get_current_omo_provider("opencode", "omo-slim")?
            .map(|p| p.id)
            .unwrap_or_default()),
        "disable_current_omo_slim" => {
            disable_omo_variant(state, "omo-slim", &crate::services::omo::SLIM)?;
            ok(())
        }

        "get_stream_check_config" => ok(state.db.get_stream_check_config()?),
        "save_stream_check_config" => {
            let config = take::<crate::services::stream_check::StreamCheckConfig>(&args, "config")?;
            state.db.save_stream_check_config(&config)?;
            ok(())
        }
        "stream_check_provider" => {
            let app = app_type(&args, "appType")?;
            let provider_id = arg_string(&args, &["providerId", "provider_id"])?;
            ok(stream_check_provider(state, app, provider_id).await?)
        }
        "stream_check_all_providers" => {
            let app = app_type(&args, "appType")?;
            let proxy_targets_only =
                arg_bool_opt(&args, &["proxyTargetsOnly", "proxy_targets_only"])?.unwrap_or(false);
            ok(stream_check_all_providers(state, app, proxy_targets_only).await?)
        }

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

        "list_daily_memory_files" => ok(list_daily_memory_files()?),
        "read_daily_memory_file" => {
            let filename = arg_string(&args, &["filename"])?;
            ok(read_daily_memory_file(filename)?)
        }
        "write_daily_memory_file" => {
            let filename = arg_string(&args, &["filename"])?;
            let content = arg_string(&args, &["content"])?;
            write_daily_memory_file(filename, content)?;
            ok(())
        }
        "search_daily_memory_files" => {
            let query = arg_string(&args, &["query"])?;
            ok(search_daily_memory_files(query)?)
        }
        "delete_daily_memory_file" => {
            let filename = arg_string(&args, &["filename"])?;
            delete_daily_memory_file(filename)?;
            ok(())
        }
        "read_workspace_file" => {
            let filename = arg_string(&args, &["filename"])?;
            ok(read_workspace_file(filename)?)
        }
        "write_workspace_file" => {
            let filename = arg_string(&args, &["filename"])?;
            let content = arg_string(&args, &["content"])?;
            write_workspace_file(filename, content)?;
            ok(())
        }
        "open_workspace_directory" => ok(false),

        "parse_deeplink" => {
            let url = arg_string(&args, &["url"])?;
            ok(crate::deeplink::parse_deeplink_url(&url)?)
        }
        "merge_deeplink_config" => {
            let request = take::<crate::deeplink::DeepLinkImportRequest>(&args, "request")?;
            ok(crate::deeplink::parse_and_merge_config(&request)?)
        }
        "import_from_deeplink_unified" => {
            let request = take::<crate::deeplink::DeepLinkImportRequest>(&args, "request")?;
            ok(import_from_deeplink_unified(state, request)?)
        }
        "import_from_deeplink" => {
            let request = take::<crate::deeplink::DeepLinkImportRequest>(&args, "request")?;
            ok(crate::deeplink::import_provider_from_deeplink(state, request)?)
        }

        "get_tool_versions" => {
            let tools = args
                .get("tools")
                .cloned()
                .map(serde_json::from_value::<Vec<String>>)
                .transpose()
                .map_err(|e| AppError::Config(e.to_string()))?;
            let _wsl_shell_by_tool = args
                .get("wslShellByTool")
                .or_else(|| args.get("wsl_shell_by_tool"))
                .cloned()
                .map(serde_json::from_value::<HashMap<String, WslShellPreferenceInput>>)
                .transpose()
                .map_err(|e| AppError::Config(e.to_string()))?;
            ok(get_tool_versions(tools).await?)
        }
        "probe_tool_installations" => {
            let tools = args
                .get("tools")
                .cloned()
                .map(serde_json::from_value::<Vec<String>>)
                .transpose()
                .map_err(|e| AppError::Config(e.to_string()))?;
            ok(get_tool_versions(tools).await?)
        }
        "run_tool_lifecycle_action" => Err(AppError::Config(
            "tool install/update actions are not available in web mode; run the installer command manually"
                .to_string(),
        )),
        "open_provider_terminal" => Err(AppError::Config(
            "opening a native provider terminal is not available in web mode".to_string(),
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
        "auth_start_login"
        | "auth_poll_for_account"
        | "auth_list_accounts"
        | "auth_get_status"
        | "auth_remove_account"
        | "auth_set_default_account"
        | "auth_logout"
        | "copilot_start_device_flow"
        | "copilot_poll_for_auth"
        | "copilot_poll_for_account"
        | "copilot_list_accounts"
        | "copilot_remove_account"
        | "copilot_set_default_account"
        | "copilot_get_auth_status"
        | "copilot_logout"
        | "copilot_is_authenticated"
        | "copilot_get_token"
        | "copilot_get_token_for_account"
        | "copilot_get_models"
        | "copilot_get_models_for_account"
        | "copilot_get_usage"
        | "copilot_get_usage_for_account"
        | "get_codex_oauth_quota"
        | "get_codex_oauth_models" => Err(AppError::Config(
            "managed OAuth/Copilot account flows are not available in web mode yet".to_string(),
        )),

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

async fn export_config_to_file(state: &AppState, file_path: String) -> Result<Value, AppError> {
    let db = state.db.clone();
    tokio::task::spawn_blocking(move || {
        let target_path = PathBuf::from(&file_path);
        db.export_sql(&target_path)?;
        Ok::<_, AppError>(json!({
            "success": true,
            "message": "SQL exported successfully",
            "filePath": file_path
        }))
    })
    .await
    .map_err(|e| AppError::Config(format!("export task failed: {e}")))?
}

async fn import_config_from_file(state: &AppState, file_path: String) -> Result<Value, AppError> {
    let db = state.db.clone();
    let db_for_sync = db.clone();
    tokio::task::spawn_blocking(move || {
        let backup_id = db.import_sql(&PathBuf::from(&file_path))?;
        let warning = post_import_sync_warning(db_for_sync);
        Ok::<_, AppError>(success_payload_with_warning(backup_id, warning))
    })
    .await
    .map_err(|e| AppError::Config(format!("import task failed: {e}")))?
}

async fn sync_current_providers_live(state: &AppState) -> Result<Value, AppError> {
    let db = state.db.clone();
    tokio::task::spawn_blocking(move || {
        let app_state = AppState::new(db);
        ProviderService::sync_current_to_live(&app_state)?;
        Ok::<_, AppError>(json!({
            "success": true,
            "message": "Live configuration synchronized"
        }))
    })
    .await
    .map_err(|e| AppError::Config(format!("sync task failed: {e}")))?
}

async fn create_db_backup(state: &AppState) -> Result<String, AppError> {
    let db = state.db.clone();
    tokio::task::spawn_blocking(move || {
        db.backup_database_file()?
            .and_then(|path| path.file_name().map(|f| f.to_string_lossy().into_owned()))
            .ok_or_else(|| AppError::Config("Database file not found, backup skipped".to_string()))
    })
    .await
    .map_err(|e| AppError::Config(format!("backup task failed: {e}")))?
}

async fn restore_db_backup(state: &AppState, filename: String) -> Result<String, AppError> {
    let db = state.db.clone();
    tokio::task::spawn_blocking(move || db.restore_from_backup(&filename))
        .await
        .map_err(|e| AppError::Config(format!("restore task failed: {e}")))?
}

fn success_payload_with_warning(backup_id: String, warning: Option<String>) -> Value {
    let mut value = json!({
        "success": true,
        "message": "Configuration imported successfully",
        "backupId": backup_id
    });
    if let Some(warning) = warning {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("warning".to_string(), Value::String(warning));
        }
    }
    value
}

fn post_import_sync_warning(db: Arc<Database>) -> Option<String> {
    let app_state = AppState::new(db);
    ProviderService::sync_current_to_live(&app_state)
        .err()
        .map(|e| e.to_string())
}

fn webdav_not_configured() -> AppError {
    AppError::localized(
        "webdav.sync.not_configured",
        "未配置 WebDAV 同步",
        "WebDAV sync is not configured.",
    )
}

fn webdav_sync_disabled() -> AppError {
    AppError::localized(
        "webdav.sync.disabled",
        "WebDAV 同步未启用",
        "WebDAV sync is disabled.",
    )
}

fn require_enabled_webdav_settings() -> Result<crate::settings::WebDavSyncSettings, AppError> {
    let settings = crate::settings::get_webdav_sync_settings().ok_or_else(webdav_not_configured)?;
    if !settings.enabled {
        return Err(webdav_sync_disabled());
    }
    Ok(settings)
}

fn resolve_webdav_password(
    mut incoming: crate::settings::WebDavSyncSettings,
    preserve_empty_password: bool,
) -> crate::settings::WebDavSyncSettings {
    if preserve_empty_password && incoming.password.is_empty() {
        if let Some(existing) = crate::settings::get_webdav_sync_settings() {
            incoming.password = existing.password;
        }
    }
    incoming
}

async fn webdav_test_connection(
    settings: crate::settings::WebDavSyncSettings,
    preserve_empty_password: bool,
) -> Result<Value, AppError> {
    let settings = resolve_webdav_password(settings, preserve_empty_password);
    crate::services::webdav_sync::check_connection(&settings).await?;
    Ok(json!({ "success": true, "message": "WebDAV connection ok" }))
}

fn webdav_sync_save_settings(
    settings: crate::settings::WebDavSyncSettings,
    password_touched: bool,
) -> Result<Value, AppError> {
    let existing = crate::settings::get_webdav_sync_settings();
    let mut settings = if !password_touched && settings.password.is_empty() {
        if let Some(existing_settings) = existing.clone() {
            crate::settings::WebDavSyncSettings {
                password: existing_settings.password,
                ..settings
            }
        } else {
            settings
        }
    } else {
        settings
    };
    if let Some(existing_settings) = existing {
        settings.status = existing_settings.status;
    }
    settings.normalize();
    settings.validate()?;
    crate::settings::set_webdav_sync_settings(Some(settings))?;
    Ok(json!({ "success": true }))
}

async fn webdav_sync_upload(state: &AppState) -> Result<Value, AppError> {
    let db = state.db.clone();
    let mut settings = require_enabled_webdav_settings()?;
    let result = crate::services::webdav_sync::run_with_sync_lock(
        crate::services::webdav_sync::upload(&db, &mut settings),
    )
    .await;
    persist_webdav_error(&mut settings, &result);
    result
}

async fn webdav_sync_download(state: &AppState) -> Result<Value, AppError> {
    let db = state.db.clone();
    let db_for_sync = db.clone();
    let mut settings = require_enabled_webdav_settings()?;
    let result = crate::services::webdav_sync::run_with_sync_lock(
        crate::services::webdav_sync::download(&db, &mut settings),
    )
    .await;
    persist_webdav_error(&mut settings, &result);
    let mut value = result?;
    if let Some(warning) = post_import_sync_warning(db_for_sync) {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("warning".to_string(), Value::String(warning));
        }
    }
    Ok(value)
}

async fn webdav_sync_fetch_remote_info() -> Result<Value, AppError> {
    let settings = require_enabled_webdav_settings()?;
    Ok(crate::services::webdav_sync::fetch_remote_info(&settings)
        .await?
        .unwrap_or(json!({ "empty": true })))
}

fn persist_webdav_error(
    settings: &mut crate::settings::WebDavSyncSettings,
    result: &Result<Value, AppError>,
) {
    if let Err(error) = result {
        settings.status.last_error = Some(error.to_string());
        settings.status.last_error_source = Some("manual".to_string());
        let _ = crate::settings::update_webdav_sync_status(settings.status.clone());
    }
}

fn s3_not_configured() -> AppError {
    AppError::localized(
        "s3.sync.not_configured",
        "未配置 S3 同步",
        "S3 sync is not configured.",
    )
}

fn s3_sync_disabled() -> AppError {
    AppError::localized("s3.sync.disabled", "S3 同步未启用", "S3 sync is disabled.")
}

fn require_enabled_s3_settings() -> Result<crate::settings::S3SyncSettings, AppError> {
    let settings = crate::settings::get_s3_sync_settings().ok_or_else(s3_not_configured)?;
    if !settings.enabled {
        return Err(s3_sync_disabled());
    }
    Ok(settings)
}

fn resolve_s3_secret(
    mut incoming: crate::settings::S3SyncSettings,
    preserve_empty_secret: bool,
) -> crate::settings::S3SyncSettings {
    if preserve_empty_secret && incoming.secret_access_key.is_empty() {
        if let Some(existing) = crate::settings::get_s3_sync_settings() {
            incoming.secret_access_key = existing.secret_access_key;
        }
    }
    incoming
}

async fn s3_test_connection(
    settings: crate::settings::S3SyncSettings,
    preserve_empty_secret: bool,
) -> Result<Value, AppError> {
    let settings = resolve_s3_secret(settings, preserve_empty_secret);
    crate::services::s3_sync::check_connection(&settings).await?;
    Ok(json!({ "success": true, "message": "S3 connection ok" }))
}

fn s3_sync_save_settings(
    settings: crate::settings::S3SyncSettings,
    password_touched: bool,
) -> Result<Value, AppError> {
    let existing = crate::settings::get_s3_sync_settings();
    let mut settings = if !password_touched && settings.secret_access_key.is_empty() {
        if let Some(existing_settings) = existing.clone() {
            crate::settings::S3SyncSettings {
                secret_access_key: existing_settings.secret_access_key,
                ..settings
            }
        } else {
            settings
        }
    } else {
        settings
    };
    if let Some(existing_settings) = existing {
        settings.status = existing_settings.status;
    }
    settings.normalize();
    settings.validate()?;
    crate::settings::set_s3_sync_settings(Some(settings))?;
    Ok(json!({ "success": true }))
}

async fn s3_sync_upload(state: &AppState) -> Result<Value, AppError> {
    let db = state.db.clone();
    let mut settings = require_enabled_s3_settings()?;
    let result = crate::services::s3_sync::run_with_sync_lock(crate::services::s3_sync::upload(
        &db,
        &mut settings,
    ))
    .await;
    persist_s3_error(&mut settings, &result);
    result
}

async fn s3_sync_download(state: &AppState) -> Result<Value, AppError> {
    let db = state.db.clone();
    let db_for_sync = db.clone();
    let mut settings = require_enabled_s3_settings()?;
    let result = crate::services::s3_sync::run_with_sync_lock(crate::services::s3_sync::download(
        &db,
        &mut settings,
    ))
    .await;
    persist_s3_error(&mut settings, &result);
    let mut value = result?;
    if let Some(warning) = post_import_sync_warning(db_for_sync) {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("warning".to_string(), Value::String(warning));
        }
    }
    Ok(value)
}

async fn s3_sync_fetch_remote_info() -> Result<Value, AppError> {
    let settings = require_enabled_s3_settings()?;
    Ok(crate::services::s3_sync::fetch_remote_info(&settings)
        .await?
        .unwrap_or(json!({ "empty": true })))
}

fn persist_s3_error(
    settings: &mut crate::settings::S3SyncSettings,
    result: &Result<Value, AppError>,
) {
    if let Err(error) = result {
        settings.status.last_error = Some(error.to_string());
        settings.status.last_error_source = Some("manual".to_string());
        let _ = crate::settings::update_s3_sync_status(settings.status.clone());
    }
}

async fn restore_codex_unified_history() -> Result<Value, AppError> {
    let outcome = tokio::task::spawn_blocking(|| {
        crate::codex_history_migration::restore_codex_official_history_from_backups()
    })
    .await
    .map_err(|e| AppError::Config(format!("restore history task failed: {e}")))??;
    Ok(json!({
        "restoredJsonlFiles": outcome.restored_jsonl_files,
        "restoredStateRows": outcome.restored_state_rows,
        "skippedReason": outcome.skipped_reason,
    }))
}

async fn set_auto_failover_enabled_web(
    state: &AppState,
    app_type: String,
    enabled: bool,
) -> Result<(), AppError> {
    let mut config = state.db.get_proxy_config_for_app(&app_type).await?;
    if enabled && !config.enabled {
        return Err(AppError::Config(
            "需要先启用该应用的代理接管，再开启故障转移".to_string(),
        ));
    }

    let mut auto_added_provider_id: Option<String> = None;
    let p1_provider_id = if enabled {
        let mut queue = state.db.get_failover_queue(&app_type)?;
        if queue.is_empty() {
            let app_enum =
                AppType::from_str(&app_type).map_err(|e| AppError::Config(e.to_string()))?;
            let current_id = crate::settings::get_effective_current_provider(&state.db, &app_enum)?;
            let Some(current_id) = current_id else {
                return Err(AppError::Config(
                    "故障转移队列为空，且未设置当前供应商，无法开启故障转移".to_string(),
                ));
            };
            state.db.add_to_failover_queue(&app_type, &current_id)?;
            auto_added_provider_id = Some(current_id);
            queue = state.db.get_failover_queue(&app_type)?;
        }
        queue
            .first()
            .map(|item| item.provider_id.clone())
            .ok_or_else(|| AppError::Config("故障转移队列为空，无法开启故障转移".to_string()))?
    } else {
        String::new()
    };

    if enabled {
        if let Err(error) = state
            .proxy_service
            .switch_proxy_target(&app_type, &p1_provider_id)
            .await
        {
            if let Some(provider_id) = auto_added_provider_id {
                let _ = state.db.remove_from_failover_queue(&app_type, &provider_id);
            }
            return Err(AppError::Config(error));
        }
    }

    config.auto_failover_enabled = enabled;
    state.db.update_proxy_config_for_app(config).await?;
    Ok(())
}

async fn test_proxy_url(url: String) -> Result<ProxyTestResult, AppError> {
    let start = std::time::Instant::now();
    let proxy = reqwest::Proxy::all(&url).map_err(|e| AppError::Config(e.to_string()))?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| AppError::Config(e.to_string()))?;

    for target in [
        "https://httpbin.org/get",
        "https://www.google.com",
        "https://api.anthropic.com",
    ] {
        match client.head(target).send().await {
            Ok(response)
                if response.status().is_success() || response.status().is_redirection() =>
            {
                return Ok(ProxyTestResult {
                    success: true,
                    latency_ms: start.elapsed().as_millis() as u64,
                    error: None,
                });
            }
            Ok(response) => {
                return Ok(ProxyTestResult {
                    success: false,
                    latency_ms: start.elapsed().as_millis() as u64,
                    error: Some(format!("HTTP {}", response.status())),
                });
            }
            Err(error) => {
                if target == "https://api.anthropic.com" {
                    return Ok(ProxyTestResult {
                        success: false,
                        latency_ms: start.elapsed().as_millis() as u64,
                        error: Some(error.to_string()),
                    });
                }
            }
        }
    }

    Ok(ProxyTestResult {
        success: false,
        latency_ms: start.elapsed().as_millis() as u64,
        error: Some("proxy test failed".to_string()),
    })
}

fn scan_local_proxies() -> Vec<DetectedProxy> {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr as StdSocketAddr, TcpStream};

    let ports = [
        (7890, "http", "mixed"),
        (7891, "socks5", "socks5"),
        (1080, "socks5", "socks5"),
        (8080, "http", "http"),
        (8888, "http", "http"),
        (3128, "http", "http"),
        (10808, "socks5", "socks5"),
        (10809, "http", "http"),
    ];
    let mut detected = Vec::new();
    for (port, scheme, proxy_type) in ports {
        let addr = StdSocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            detected.push(DetectedProxy {
                url: format!("{scheme}://127.0.0.1:{port}"),
                proxy_type: proxy_type.to_string(),
                port,
            });
        }
    }
    detected
}

async fn probe_hermes_web_ui(path: Option<String>) -> Result<String, AppError> {
    let port = std::env::var("HERMES_WEB_PORT")
        .ok()
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .unwrap_or(9119);
    let base = format!("http://127.0.0.1:{port}");
    let probe_url = format!("{base}/api/status");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(1200))
        .no_proxy()
        .build()
        .map_err(|e| AppError::Config(e.to_string()))?;
    client
        .get(&probe_url)
        .send()
        .await
        .map_err(|_| AppError::Config("hermes_web_offline".to_string()))?;
    Ok(match path.as_deref() {
        Some(p) if p.starts_with('/') => format!("{base}{p}"),
        Some(p) if !p.is_empty() => format!("{base}/{p}"),
        _ => format!("{base}/"),
    })
}

fn disable_omo_variant(
    state: &AppState,
    category: &str,
    variant: &crate::services::omo::OmoVariant,
) -> Result<(), AppError> {
    let providers = state.db.get_all_providers("opencode")?;
    for (id, provider) in providers {
        if provider.category.as_deref() == Some(category) {
            state
                .db
                .clear_omo_provider_current("opencode", &id, category)?;
        }
    }
    OmoService::delete_config_file(variant)?;
    Ok(())
}

async fn stream_check_provider(
    state: &AppState,
    app: AppType,
    provider_id: String,
) -> Result<crate::services::stream_check::StreamCheckResult, AppError> {
    let config = state.db.get_stream_check_config()?;
    let providers = state.db.get_all_providers(app.as_str())?;
    let provider = providers
        .get(&provider_id)
        .ok_or_else(|| AppError::Message(format!("供应商 {provider_id} 不存在")))?;
    let result = crate::services::stream_check::StreamCheckService::check_with_retry(
        &app, provider, &config, None,
    )
    .await?;
    let _ = state
        .db
        .save_stream_check_log(&provider_id, &provider.name, app.as_str(), &result);
    Ok(result)
}

async fn stream_check_all_providers(
    state: &AppState,
    app: AppType,
    proxy_targets_only: bool,
) -> Result<Vec<(String, crate::services::stream_check::StreamCheckResult)>, AppError> {
    let config = state.db.get_stream_check_config()?;
    let providers = state.db.get_all_providers(app.as_str())?;
    let allowed_ids = if proxy_targets_only {
        let mut ids = std::collections::HashSet::new();
        if let Ok(Some(current_id)) = state.db.get_current_provider(app.as_str()) {
            ids.insert(current_id);
        }
        if let Ok(queue) = state.db.get_failover_queue(app.as_str()) {
            for item in queue {
                ids.insert(item.provider_id);
            }
        }
        Some(ids)
    } else {
        None
    };

    let mut results = Vec::new();
    for (id, provider) in providers {
        if allowed_ids.as_ref().is_some_and(|ids| !ids.contains(&id)) {
            continue;
        }
        let result = crate::services::stream_check::StreamCheckService::check_with_retry(
            &app, &provider, &config, None,
        )
        .await
        .unwrap_or_else(|e| crate::services::stream_check::StreamCheckResult {
            status: crate::services::stream_check::HealthStatus::Failed,
            success: false,
            message: e.to_string(),
            response_time_ms: None,
            http_status: None,
            model_used: String::new(),
            tested_at: chrono::Utc::now().timestamp(),
            retry_count: 0,
            error_category: None,
        });
        let _ = state
            .db
            .save_stream_check_log(&id, &provider.name, app.as_str(), &result);
        results.push((id, result));
    }
    Ok(results)
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
        AppType::GrokBuild => crate::grok_config::get_grok_config_dir(),
        AppType::Pi => crate::pi_config::get_pi_agent_dir()?,
    };
    Ok(dir.to_string_lossy().to_string())
}

const WORKSPACE_ALLOWED_FILES: &[&str] = &[
    "AGENTS.md",
    "SOUL.md",
    "USER.md",
    "IDENTITY.md",
    "TOOLS.md",
    "MEMORY.md",
    "HEARTBEAT.md",
    "BOOTSTRAP.md",
    "BOOT.md",
];

fn validate_workspace_filename(filename: &str) -> Result<(), AppError> {
    if WORKSPACE_ALLOWED_FILES.contains(&filename) {
        Ok(())
    } else {
        Err(AppError::Config(format!(
            "Invalid workspace filename: {filename}. Allowed: {}",
            WORKSPACE_ALLOWED_FILES.join(", ")
        )))
    }
}

fn validate_daily_memory_filename(filename: &str) -> Result<(), AppError> {
    let bytes = filename.as_bytes();
    let valid = bytes.len() == 13
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && filename.ends_with(".md")
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit);
    if valid {
        Ok(())
    } else {
        Err(AppError::Config(format!(
            "Invalid daily memory filename: {filename}. Expected: YYYY-MM-DD.md"
        )))
    }
}

fn openclaw_workspace_dir() -> PathBuf {
    crate::openclaw_config::get_openclaw_dir().join("workspace")
}

fn openclaw_memory_dir() -> PathBuf {
    openclaw_workspace_dir().join("memory")
}

fn list_daily_memory_files() -> Result<Vec<DailyMemoryFileInfo>, AppError> {
    let memory_dir = openclaw_memory_dir();
    if !memory_dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    let entries = std::fs::read_dir(&memory_dir).map_err(|e| AppError::io(&memory_dir, e))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if validate_daily_memory_filename(&name).is_err() {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let modified_at = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let preview = std::fs::read_to_string(entry.path())
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        files.push(DailyMemoryFileInfo {
            date: name.trim_end_matches(".md").to_string(),
            filename: name,
            size_bytes: meta.len(),
            modified_at,
            preview,
        });
    }
    files.sort_by(|a, b| b.filename.cmp(&a.filename));
    Ok(files)
}

fn read_daily_memory_file(filename: String) -> Result<Option<String>, AppError> {
    validate_daily_memory_filename(&filename)?;
    read_optional_text(openclaw_memory_dir().join(filename))
}

fn write_daily_memory_file(filename: String, content: String) -> Result<(), AppError> {
    validate_daily_memory_filename(&filename)?;
    let dir = openclaw_memory_dir();
    std::fs::create_dir_all(&dir).map_err(|e| AppError::io(&dir, e))?;
    crate::config::write_text_file(&dir.join(filename), &content)
}

fn search_daily_memory_files(query: String) -> Result<Vec<DailyMemorySearchResult>, AppError> {
    let memory_dir = openclaw_memory_dir();
    if !memory_dir.exists() || query.is_empty() {
        return Ok(Vec::new());
    }
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();
    let entries = std::fs::read_dir(&memory_dir).map_err(|e| AppError::io(&memory_dir, e))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if validate_daily_memory_filename(&name).is_err() {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
        let content_lower = content.to_lowercase();
        let date = name.trim_end_matches(".md").to_string();
        let matches: Vec<usize> = content_lower
            .match_indices(&query_lower)
            .map(|(i, _)| i)
            .collect();
        if matches.is_empty() && !date.to_lowercase().contains(&query_lower) {
            continue;
        }
        let snippet = if let Some(first) = matches.first().copied() {
            let start = floor_char_boundary(&content, first.saturating_sub(50));
            let end = ceil_char_boundary(&content, (first + 70).min(content.len()));
            format!(
                "{}{}{}",
                if start > 0 { "..." } else { "" },
                &content[start..end],
                if end < content.len() { "..." } else { "" }
            )
        } else {
            let end = ceil_char_boundary(&content, 120.min(content.len()));
            format!(
                "{}{}",
                &content[..end],
                if end < content.len() { "..." } else { "" }
            )
        };
        let modified_at = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        results.push(DailyMemorySearchResult {
            filename: name,
            date,
            size_bytes: meta.len(),
            modified_at,
            snippet,
            match_count: matches.len(),
        });
    }
    results.sort_by(|a, b| b.filename.cmp(&a.filename));
    Ok(results)
}

fn delete_daily_memory_file(filename: String) -> Result<(), AppError> {
    validate_daily_memory_filename(&filename)?;
    let path = openclaw_memory_dir().join(filename);
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| AppError::io(&path, e))?;
    }
    Ok(())
}

fn read_workspace_file(filename: String) -> Result<Option<String>, AppError> {
    validate_workspace_filename(&filename)?;
    read_optional_text(openclaw_workspace_dir().join(filename))
}

fn write_workspace_file(filename: String, content: String) -> Result<(), AppError> {
    validate_workspace_filename(&filename)?;
    let dir = openclaw_workspace_dir();
    std::fs::create_dir_all(&dir).map_err(|e| AppError::io(&dir, e))?;
    crate::config::write_text_file(&dir.join(filename), &content)
}

fn read_optional_text(path: PathBuf) -> Result<Option<String>, AppError> {
    if !path.exists() {
        return Ok(None);
    }
    std::fs::read_to_string(&path)
        .map(Some)
        .map_err(|e| AppError::io(&path, e))
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn import_from_deeplink_unified(
    state: &AppState,
    request: crate::deeplink::DeepLinkImportRequest,
) -> Result<Value, AppError> {
    match request.resource.as_str() {
        "provider" => {
            let id = crate::deeplink::import_provider_from_deeplink(state, request)?;
            Ok(json!({ "type": "provider", "id": id }))
        }
        "prompt" => {
            let id = crate::deeplink::import_prompt_from_deeplink(state, request)?;
            Ok(json!({ "type": "prompt", "id": id }))
        }
        "mcp" => {
            let result = crate::deeplink::import_mcp_from_deeplink(state, request)?;
            Ok(json!({
                "type": "mcp",
                "importedCount": result.imported_count,
                "importedIds": result.imported_ids,
                "failed": result.failed
            }))
        }
        "skill" => {
            let key = crate::deeplink::import_skill_from_deeplink(state, request)?;
            Ok(json!({ "type": "skill", "key": key }))
        }
        other => Err(AppError::Config(format!(
            "Unsupported resource type: {other}"
        ))),
    }
}

async fn get_tool_versions(tools: Option<Vec<String>>) -> Result<Vec<ToolVersion>, AppError> {
    const VALID_TOOLS: &[&str] = &[
        "claude", "codex", "gemini", "opencode", "openclaw", "hermes",
    ];
    let requested: Vec<&str> = if let Some(tools) = tools.as_ref() {
        VALID_TOOLS
            .iter()
            .copied()
            .filter(|tool| tools.iter().any(|requested| requested == tool))
            .collect()
    } else {
        VALID_TOOLS.to_vec()
    };

    let mut versions = Vec::new();
    for tool in requested {
        versions.push(
            tokio::task::spawn_blocking(move || get_single_tool_version(tool))
                .await
                .map_err(|e| AppError::Config(format!("tool version task failed: {e}")))?,
        );
    }
    Ok(versions)
}

fn get_single_tool_version(tool: &str) -> ToolVersion {
    let output = std::process::Command::new(tool).arg("--version").output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            let fallback = String::from_utf8_lossy(&output.stderr);
            ToolVersion {
                name: tool.to_string(),
                version: first_non_empty_line(&text)
                    .or_else(|| first_non_empty_line(&fallback))
                    .map(str::to_string),
                latest_version: None,
                error: None,
                installed_but_broken: false,
                env_type: tool_env_type(),
                wsl_distro: None,
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            ToolVersion {
                name: tool.to_string(),
                version: None,
                latest_version: None,
                error: Some(if stderr.trim().is_empty() {
                    stdout.trim().to_string()
                } else {
                    stderr.trim().to_string()
                }),
                installed_but_broken: true,
                env_type: tool_env_type(),
                wsl_distro: None,
            }
        }
        Err(error) => ToolVersion {
            name: tool.to_string(),
            version: None,
            latest_version: None,
            error: Some(error.to_string()),
            installed_but_broken: false,
            env_type: tool_env_type(),
            wsl_distro: None,
        },
    }
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
}

fn tool_env_type() -> String {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
    .to_string()
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
