use crate::app_config::AppType;
use crate::error::AppError;
use crate::provider::Provider;
use crate::services::{ProviderService, SpeedtestService};
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
    AppType::from_str(&arg_string(args, &[key])?).map_err(|e| AppError::Config(e.to_string()))
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
