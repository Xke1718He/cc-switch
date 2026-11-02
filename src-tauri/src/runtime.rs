#[cfg(feature = "desktop")]
pub type AppHandle = tauri::AppHandle;

#[cfg(not(feature = "desktop"))]
#[derive(Clone, Debug)]
pub struct AppHandle;

#[cfg(feature = "desktop")]
pub use tauri::async_runtime;

#[cfg(not(feature = "desktop"))]
pub mod async_runtime {
    use std::future::Future;
    use std::sync::OnceLock;
    use tokio::runtime::Runtime;
    use tokio::task::JoinHandle;

    fn runtime() -> &'static Runtime {
        static RUNTIME: OnceLock<Runtime> = OnceLock::new();
        RUNTIME.get_or_init(|| Runtime::new().expect("failed to initialize background runtime"))
    }

    pub fn block_on<F: Future>(future: F) -> F::Output {
        runtime().block_on(future)
    }

    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        runtime().spawn(future)
    }

    pub fn spawn_blocking<F, R>(work: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        runtime().spawn_blocking(work)
    }
}
