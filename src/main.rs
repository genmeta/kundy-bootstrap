mod activation;
mod api;
mod cli;
mod config;
mod dhttp_server;
mod host;
mod init;
mod runtime;

#[tokio::main]
async fn main() -> init::Result<()> {
    init::run().await
}
