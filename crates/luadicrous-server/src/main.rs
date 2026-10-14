use anyhow::Result;
use luadicrous_server::{Config, serve};

#[tokio::main]
async fn main() -> Result<()> {
    serve(Config::from_env()?).await
}
