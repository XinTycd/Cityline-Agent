mod agents;
mod audit;
mod auth;
mod media;
mod model;
mod profile;
mod provider_config;
mod refresh;
mod settings;
mod store;
mod web;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = match std::env::var("ACCEPTANCE_RUST_PORT") {
        Ok(value) => value.parse::<u16>()?,
        Err(std::env::VarError::NotPresent) => 3412,
        Err(error) => return Err(error.into()),
    };
    let path = std::env::var_os("ACCEPTANCE_RUST_DATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ".operations-data/cityline.sqlite".into());
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let model = model::Model::from_environment();
    let store = store::Store::initialize(path)?;
    let agents = agents::Agents::initialize(store.clone(), model)?;
    let addr = listener.local_addr()?;
    let settings = settings::Settings::initialize(store.clone(), addr.port())?;
    println!("城线 · {} http://{addr}", settings::DEFAULT_DESCRIPTION);
    axum::serve(listener, web::app(addr.port(), store, agents, settings))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
