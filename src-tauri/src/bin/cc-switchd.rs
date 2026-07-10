use cc_switch_lib::{init_headless_state, run_web_server, WebServerConfig};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("cc-switchd failed: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), cc_switch_lib::AppError> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER").is_err() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
        if std::env::var("WEBKIT_DISABLE_COMPOSITING_MODE").is_err() {
            std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        }
    }

    let listen = listen_addr();
    let ui_dir = ui_dir();
    let state = init_headless_state()?;

    run_web_server(WebServerConfig { listen, ui_dir }, state).await
}

fn listen_addr() -> SocketAddr {
    let host = std::env::var("CC_SWITCH_WEB_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("CC_SWITCH_WEB_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(31235);
    let ip = host
        .parse::<IpAddr>()
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    SocketAddr::new(ip, port)
}

fn ui_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CC_SWITCH_WEB_UI_DIR") {
        return PathBuf::from(dir);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("../dist")
}
