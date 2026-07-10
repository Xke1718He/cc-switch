use crate::app_config::AppType;
use crate::error::AppError;
use crate::store::AppState;
use std::sync::Arc;

/// Initialize the subset of application state that is shared by the Tauri UI
/// and the headless web server.
///
/// This intentionally avoids native-window concerns (tray, updater, deep links,
/// dialog plugins) while preserving database migrations, provider seeding, and
/// live-config imports.
pub fn init_headless_state() -> Result<Arc<AppState>, AppError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    crate::panic_hook::setup_panic_hook();
    crate::panic_hook::init_app_config_dir(crate::config::get_app_config_dir());

    let db = Arc::new(crate::database::Database::init()?);
    let state = Arc::new(AppState::new(db));

    run_startup_imports(&state);
    init_global_proxy_client(&state);

    Ok(state)
}

fn run_startup_imports(state: &AppState) {
    match state.db.init_default_skill_repos() {
        Ok(count) if count > 0 => log::info!("Initialized {count} default skill repositories"),
        Ok(_) => {}
        Err(e) => log::warn!("Failed to initialize default skill repos: {e}"),
    }

    match state.db.get_setting("skills_ssot_migration_pending") {
        Ok(Some(flag)) if flag == "true" || flag == "1" => {
            let has_existing = state
                .db
                .get_all_installed_skills()
                .map(|skills| !skills.is_empty())
                .unwrap_or(false);
            if has_existing {
                let _ = state
                    .db
                    .set_setting("skills_ssot_migration_pending", "false");
            } else {
                match crate::services::skill::migrate_skills_to_ssot(&state.db) {
                    Ok(_) => {
                        let _ = state
                            .db
                            .set_setting("skills_ssot_migration_pending", "false");
                    }
                    Err(e) => log::warn!("Failed to migrate legacy skills to SSOT: {e}"),
                }
            }
        }
        Ok(_) => {}
        Err(e) => log::warn!("Failed to read skills migration flag: {e}"),
    }

    for app_type in AppType::all().filter(|t| !t.is_additive_mode()) {
        let should_import =
            crate::services::provider::should_import_default_config_on_startup(state, &app_type)
                .unwrap_or(false);
        if should_import {
            if let Err(e) = crate::services::provider::import_default_config(state, app_type) {
                log::debug!("No live config imported: {e}");
            }
        }
    }

    if let Err(e) = state.db.init_default_official_providers() {
        log::warn!("Failed to seed official providers: {e}");
    }

    if let Err(e) = crate::services::provider::import_opencode_providers_from_live(state) {
        log::warn!("Failed to import OpenCode providers: {e}");
    }
    if let Err(e) = crate::services::provider::import_openclaw_providers_from_live(state) {
        log::warn!("Failed to import OpenClaw providers: {e}");
    }
    if let Err(e) = crate::services::provider::import_hermes_providers_from_live(state) {
        log::warn!("Failed to import Hermes providers: {e}");
    }

    let has_omo = state
        .db
        .get_all_providers("opencode")
        .map(|providers| {
            providers
                .values()
                .any(|p| p.category.as_deref() == Some("omo"))
        })
        .unwrap_or(false);
    if !has_omo {
        match crate::services::OmoService::import_from_local(state, &crate::services::omo::STANDARD)
        {
            Ok(_) | Err(AppError::OmoConfigNotFound) => {}
            Err(e) => log::warn!("Failed to import OMO config from local: {e}"),
        }
    }

    let has_omo_slim = state
        .db
        .get_all_providers("opencode")
        .map(|providers| {
            providers
                .values()
                .any(|p| p.category.as_deref() == Some("omo-slim"))
        })
        .unwrap_or(false);
    if !has_omo_slim {
        match crate::services::OmoService::import_from_local(state, &crate::services::omo::SLIM) {
            Ok(_) | Err(AppError::OmoConfigNotFound) => {}
            Err(e) => log::warn!("Failed to import OMO Slim config from local: {e}"),
        }
    }

    if state.db.is_mcp_table_empty().unwrap_or(false) {
        let _ = crate::services::mcp::McpService::import_from_claude(state);
        let _ = crate::services::mcp::McpService::import_from_codex(state);
        let _ = crate::services::mcp::McpService::import_from_gemini(state);
        let _ = crate::services::mcp::McpService::import_from_opencode(state);
        let _ = crate::services::mcp::McpService::import_from_hermes(state);
    }

    if state.db.is_prompts_table_empty().unwrap_or(false) {
        for app in [
            AppType::Claude,
            AppType::Codex,
            AppType::Gemini,
            AppType::OpenCode,
            AppType::OpenClaw,
            AppType::Hermes,
        ] {
            let _ = crate::services::prompt::PromptService::import_from_file_on_first_launch(
                state, app,
            );
        }
    }
}

fn init_global_proxy_client(state: &AppState) {
    let proxy_url = state.db.get_global_proxy_url().ok().flatten();
    if let Err(e) = crate::proxy::http_client::init(proxy_url.as_deref()) {
        log::warn!("Failed to initialize global proxy client: {e}");
        if proxy_url.is_some() {
            let _ = state.db.set_global_proxy_url(None);
        }
        if let Err(fallback_err) = crate::proxy::http_client::init(None) {
            log::warn!("Failed to initialize direct HTTP client: {fallback_err}");
        }
    }
}
