//! auwsx-web — thin HTTP translator over the daemon IPC socket.

use auwsx_core::ipc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "auwsx_web=info,tower_http=info".into()),
        )
        .init();

    let addr = auwsx_web::default_addr()?;
    let socket_path = ipc::default_socket_path();
    tracing::info!(
        %addr,
        socket = %socket_path.display(),
        "starting auwsx-web"
    );
    auwsx_web::serve(addr, socket_path).await
}
