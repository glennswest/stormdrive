use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use stormdrive::api::AppState;
use stormdrive::config::Config;
use stormdrive::events::EventLog;
use stormdrive::inventory::Inventory;
use stormdrive::stormblock::StormBlockClient;
use tokio::sync::RwLock;

#[derive(Parser, Debug)]
#[command(name = "stormdrive", version, about = "Physical drive management for the Storm ecosystem")]
struct Args {
    /// Config file (missing file = defaults)
    #[arg(long, default_value = "/etc/stormdrive/stormdrive.toml")]
    config: PathBuf,
    /// Override listen address
    #[arg(long)]
    listen: Option<String>,
    /// Override data directory
    #[arg(long)]
    data_dir: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let mut config = Config::load(&args.config)?;
    if let Some(l) = args.listen {
        config.listen_addr = l;
    }
    if let Some(d) = args.data_dir {
        config.data_dir = Some(d);
    }
    config.validate()?;

    let inventory_path = config
        .data_dir
        .as_ref()
        .map(|d| PathBuf::from(d).join("inventory.json"));
    let inventory = match &inventory_path {
        Some(p) => Inventory::load(p)?,
        None => {
            tracing::warn!("no data_dir configured — inventory is in-memory only");
            Inventory::default()
        }
    };
    tracing::info!(
        drives = inventory.drives.len(),
        "inventory loaded"
    );

    let node_name = config.node_name();
    let stormblock = StormBlockClient::new(config.stormblock.clone());
    let listen = config.listen_addr.clone();
    let state = Arc::new(AppState {
        config,
        inventory: RwLock::new(inventory),
        events: RwLock::new(EventLog::new(4096)),
        stormblock,
        tests: RwLock::new(std::collections::HashMap::new()),
        formats: RwLock::new(std::collections::HashMap::new()),
        firmware: RwLock::new(std::collections::HashMap::new()),
        fleet_firmware_lock: tokio::sync::Mutex::new(()),
        shelves: RwLock::new(std::collections::BTreeMap::new()),
        inventory_path,
        node_name,
    });

    tokio::spawn(stormdrive::monitor::run(state.clone()));

    let app = stormdrive::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(%listen, version = stormdrive::VERSION, "stormdrive management API up");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    state.persist().await;
    Ok(())
}
