use clap::Parser;
use many_client::ManyClient;
use many_identity::verifiers::AnonymousVerifier;
use many_identity::{Address, AnonymousIdentity, Identity};
use many_identity_dsa::{CoseKeyIdentity, CoseKeyVerifier};
use many_identity_webauthn::WebAuthnVerifier;
use many_modules::{base, blockchain, r#async};
use many_protocol::ManyUrl;
use many_server::transport::http::HttpServer;
use many_server::ManyServer;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tendermint_abci::ServerBuilder;
use tendermint_rpc::Client;
use tracing::{debug, error, info, trace};
use tracing_subscriber::filter::LevelFilter;

mod abci_app;
mod many_app;
mod module;

use abci_app::AbciApp;
use many_app::AbciModuleMany;
use module::AbciBlockchainModuleImpl;

#[derive(clap::ArgEnum, Clone, Debug)]
enum LogStrategy {
    Terminal,
    Syslog,
}

#[derive(Debug, Parser)]
struct Opts {
    /// Address and port to bind the ABCI server to.
    #[clap(long)]
    abci: String,

    /// URL for the tendermint server. Tendermint must already be running.
    #[clap(long)]
    tendermint: String,

    /// URL (including scheme) that has the MANY application running.
    #[clap(long)]
    many_app: String,

    /// Address and port to bind the MANY server to.
    #[clap(long)]
    many: String,

    /// A pem file for the MANY frontend.
    #[clap(long)]
    many_pem: PathBuf,

    /// The default server read buffer size, in bytes, for each incoming client connection.
    #[clap(short, long, default_value = "1048576")]
    abci_read_buf_size: usize,

    /// Increase output logging verbosity to DEBUG level.
    #[clap(short, long, parse(from_occurrences))]
    verbose: i8,

    /// Suppress all output logging. Can be used multiple times to suppress more.
    #[clap(short, long, parse(from_occurrences))]
    quiet: i8,

    /// Application absolute URLs allowed to communicate with this server. Any
    /// application will be able to communicate with this server if left empty.
    /// Multiple occurences of this argument can be given.
    #[clap(long)]
    allow_origin: Option<Vec<ManyUrl>>,

    /// Use given logging strategy
    #[clap(long, arg_enum, default_value_t = LogStrategy::Terminal)]
    logmode: LogStrategy,

    /// Path to a JSON file containing an array of MANY addresses
    /// Only addresses from this array will be able to execute commands, e.g., send, put, ...
    /// Any addresses will be able to execute queries, e.g., balance, get, ...
    #[clap(long)]
    allow_addrs: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let Opts {
        abci,
        tendermint,
        many_app,
        many,
        many_pem,
        abci_read_buf_size,
        verbose,
        quiet,
        allow_origin,
        logmode,
        allow_addrs,
    } = Opts::parse();

    let verbose_level = 2 + verbose - quiet;
    let log_level = match verbose_level {
        x if x > 3 => LevelFilter::TRACE,
        3 => LevelFilter::DEBUG,
        2 => LevelFilter::INFO,
        1 => LevelFilter::WARN,
        0 => LevelFilter::ERROR,
        x if x < 0 => LevelFilter::OFF,
        _ => unreachable!(),
    };
    let subscriber = tracing_subscriber::fmt::Subscriber::builder().with_max_level(log_level);

    match logmode {
        LogStrategy::Terminal => {
            let subscriber = subscriber.with_writer(std::io::stderr);
            subscriber.init();
        }
        LogStrategy::Syslog => {
            let identity = std::ffi::CStr::from_bytes_with_nul(b"many-abci\0").unwrap();
            let (options, facility) = Default::default();
            let syslog = syslog_tracing::Syslog::new(identity, options, facility).unwrap();

            let subscriber = subscriber.with_writer(syslog);
            subscriber.init();
        }
    };

    debug!("{:?}", Opts::parse());
    info!(
        version = env!("CARGO_PKG_VERSION"),
        git_sha = env!("VERGEN_GIT_SHA")
    );

    // Try to get the status of the backend MANY app.
    let many_client = ManyClient::new(&many_app, Address::anonymous(), AnonymousIdentity).unwrap();

    let start = std::time::SystemTime::now();
    trace!("Connecting to the backend app...");

    let status = loop {
        let many_client = many_client.clone();
        let result = many_client.status().await;

        match result {
            Err(e) => {
                if start.elapsed().unwrap().as_secs() > 60 {
                    error!("\nCould not connect to the ABCI server in 60 seconds... Terminating.");
                    error!(error = e.to_string().as_str());
                    std::process::exit(1);
                }
                debug!(error = e.to_string().as_str());
            }
            Ok(s) => {
                trace!(" Connected.");
                break s;
            }
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    };

    let abci_app = tokio::task::spawn_blocking(move || {
        AbciApp::create(many_app, Address::anonymous()).unwrap()
    })
    .await
    .unwrap();

    let abci_server = ServerBuilder::new(abci_read_buf_size)
        .bind(abci, abci_app)
        .unwrap();
    let _j_abci = tokio::task::spawn_blocking(move || abci_server.listen().unwrap());

    let abci_client = tendermint_rpc::HttpClient::new(tendermint.as_str()).unwrap();

    // Wait for 60 seconds until we can contact the ABCI server.
    let start = std::time::SystemTime::now();
    loop {
        let info = abci_client.abci_info().await;
        if info.is_ok() {
            break;
        }
        if start.elapsed().unwrap().as_secs() > 300 {
            error!("\nCould not connect to the ABCI server in 300 seconds... Terminating.");
            std::process::exit(1);
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    let key = CoseKeyIdentity::from_pem(&std::fs::read_to_string(&many_pem).unwrap()).unwrap();
    info!(many_address = key.address().to_string().as_str());
    let server = ManyServer::new(
        format!("AbciModule({})", &status.name),
        key.clone(),
        (
            AnonymousVerifier,
            CoseKeyVerifier,
            WebAuthnVerifier::new(allow_origin),
        ),
        key.public_key(),
    );
    let allowed_addrs: Option<BTreeSet<Address>> =
        allow_addrs.map(|path| json5::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap());
    let backend = AbciModuleMany::new(abci_client.clone(), status, key, allowed_addrs).await;
    let blockchain_impl = Arc::new(Mutex::new(AbciBlockchainModuleImpl::new(abci_client)));

    {
        let mut s = server.lock().unwrap();
        s.add_module(base::BaseModule::new(server.clone()));
        s.add_module(blockchain::BlockchainModule::new(blockchain_impl.clone()));
        s.add_module(r#async::AsyncModule::new(blockchain_impl));
        s.set_fallback_module(backend);
    }

    let mut many_server = HttpServer::new(server);

    signal_hook::flag::register(signal_hook::consts::SIGTERM, many_server.term_signal())
        .expect("Could not register signal handler");
    signal_hook::flag::register(signal_hook::consts::SIGHUP, many_server.term_signal())
        .expect("Could not register signal handler");
    signal_hook::flag::register(signal_hook::consts::SIGINT, many_server.term_signal())
        .expect("Could not register signal handler");

    info!("Starting MANY server on addr {}", many.clone());
    match many_server.bind(many).await {
        Ok(_) => {}
        Err(error) => {
            error!("{}", error);
            panic!("Error happened in many: {:?}", error);
        }
    }

    // It seems that ABCI does not have a graceful way to shutdown. If we make it here
    // though we already gracefully shutdown the MANY part of the server, so lets just
    // get on with it, shall we?
    std::process::exit(0);
    // j_abci.join().unwrap();
}
