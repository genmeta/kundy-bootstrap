use std::error::Error;

use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::{activation::ActivationService, api, cli, config, dhttp_server, host};

pub type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

pub async fn run() -> Result<()> {
    let command = cli::parse()?;
    match &command {
        cli::Command::Help => {
            cli::print_help();
            return Ok(());
        }
        cli::Command::SetupHelp => {
            cli::print_setup_help();
            return Ok(());
        }
        cli::Command::Version => {
            cli::print_version();
            return Ok(());
        }
        _ => init_tracing(&command),
    }

    match command {
        cli::Command::Serve => serve().await?,
        cli::Command::Activate => activate().await?,
        cli::Command::Deactivate => deactivate().await?,
        cli::Command::Setup { runtime_user } => cli::setup(&runtime_user)?,
        cli::Command::ApplyRuntimeConfig => host::apply_runtime_config()?,
        cli::Command::ReloadPishoo => host::reload_pishoo()?,
        cli::Command::SetupHelp | cli::Command::Help | cli::Command::Version => unreachable!(),
    }
    Ok(())
}

async fn serve() -> Result<()> {
    install_crypto_provider()?;
    let config = config::AppConfig::embedded()?;
    let endpoint = dhttp_server::build(&config.dhttp).await?;
    let activation = ActivationService::new(&config.certserver)?;
    let app = api::router(
        config.certserver,
        config.ui,
        endpoint.clone(),
        activation.clone(),
    );
    let _runtime_controller = tokio::spawn(activation.run_host_runtime_controller());
    dhttp_server::serve(endpoint, app).await?;
    Ok(())
}

async fn activate() -> Result<()> {
    install_crypto_provider()?;
    let config = config::AppConfig::embedded()?;
    cli::activate(config.certserver).await?;
    Ok(())
}

async fn deactivate() -> Result<()> {
    install_crypto_provider()?;
    let config = config::AppConfig::embedded()?;
    cli::deactivate(config.certserver).await?;
    Ok(())
}

fn install_crypto_provider() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| std::io::Error::other("failed to install the rustls ring crypto provider"))?;
    Ok(())
}

fn init_tracing(command: &cli::Command) {
    let default_filter = match command {
        cli::Command::Activate | cli::Command::Deactivate | cli::Command::Setup { .. } => "off",
        _ => "info,h3=warn,quinn=warn",
    };
    tracing_subscriber::registry()
        .with(fmt::layer().with_target(false).with_writer(std::io::stderr))
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)))
        .init();
}
