//! One tokio runtime for the app's server calls (sign-in, the computer list). Sessions run
//! their own (see `rdesk_core::conn::spawn`).

use std::future::Future;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};

pub fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rdesk-app")
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Run `f` on the app's runtime (reqwest and iroh need tokio's) and wait for it.
pub async fn run<T: Send + 'static>(f: impl Future<Output = Result<T>> + Send + 'static) -> Result<T> {
    rt().spawn(f).await.map_err(|e| anyhow!("{e}"))?
}
