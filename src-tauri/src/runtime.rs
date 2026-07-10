#[cfg(feature = "desktop")]
pub type AppHandle = tauri::AppHandle;

#[cfg(not(feature = "desktop"))]
#[derive(Clone, Debug)]
pub struct AppHandle;
