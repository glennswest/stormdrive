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
    let data_dir = config.data_dir.clone();
    // #46: destructive engine verbs fall back to stormdrive's own Kubernetes
    // credential (storage-admin) when no engine admin token is readable.
    let stormblock = StormBlockClient::new(config.stormblock.clone()).with_kube_token_file(
        stormdrive::kubeapi::resolve(&config.kubernetes).and_then(|(_, _, t)| t).map(std::path::PathBuf::from),
    );
    let listen = config.listen_addr.clone();
    let poller = stormdrive::poller::Sampler::new(
        Arc::new(stormdrive::smart::collect),
        std::time::Duration::from_secs(config.monitor.interval_secs),
        config.monitor.max_concurrent,
        std::time::Duration::from_secs(config.monitor.sample_timeout_secs),
    );
    let kube = stormdrive::kubeapi::KubeApi::from_config(&config.kubernetes)?.map(Arc::new);
    match &kube {
        Some(k) => tracing::info!(apiserver = k.base(), "writes are reviewed by the apiserver (storage.storm.io)"),
        None => tracing::warn!("no apiserver ([kubernetes] api_url): only the admin token may write, and no Drive/DriveOperation objects are kept"),
    }
    let gate = stormdrive::kubeauth::Gate::new(&config.api, kube.clone(), data_dir.as_deref());
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
        hbas: RwLock::new(std::collections::BTreeMap::new()),
        inventory_path,
        node_name,
        poller,
        persisted: Default::default(),
        worker: stormdrive::worker::Worker::load(data_dir.as_deref()),
        gate,
    });

    // What was in flight when we stopped — worker jobs, and drives a
    // format/test/firmware run left busy (#39): watch it or idle it,
    // before the monitor or the API sees a stale `activity`.
    stormdrive::worker::recover(state.clone()).await;
    tokio::spawn(stormdrive::monitor::run(state.clone()));
    // Drive objects and DriveOperations in the apiserver (#45).
    if let (Some(k), true) = (&kube, state.config.kubernetes.controller) {
        tokio::spawn(stormdrive::controller::run(state.clone(), k.clone()));
    }

    // :9092 (#19): TLS from the stormcert pair, and plain HTTP for health,
    // on one port; the router's guard decides every other request.
    let api = &state.config.api;
    let cert = stormdrive::tls::ServingCert::new(api.tls_cert_file.clone().into(), api.tls_key_file.clone().into());
    let cas: Vec<PathBuf> = api.client_ca_files.iter().map(PathBuf::from).collect();
    let tls = stormdrive::tls::server_config(cert, &cas)?;
    let app = stormdrive::api::router(state.clone());
    let tcp = tokio::net::TcpListener::bind(&listen).await?;
    let listener = stormdrive::tls::Listener::new(tcp, tls)?;
    tracing::info!(
        %listen,
        version = stormdrive::VERSION,
        cert = %api.tls_cert_file,
        anonymous = api.allow_anonymous,
        "stormdrive management API up: TLS, and plain HTTP for health"
    );
    axum::serve(listener, app.into_make_service_with_connect_info::<stormdrive::tls::Peer>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    state.persist().await;
    Ok(())
}
