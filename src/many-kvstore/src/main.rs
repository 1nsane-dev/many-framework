use crate::module::account::AccountFeatureModule;
use clap::Parser;
use many_identity::verifiers::AnonymousVerifier;
use many_identity::Address;
use many_identity_dsa::{CoseKeyIdentity, CoseKeyVerifier};
use many_modules::account::features::Feature;
use many_modules::{abci_backend, account, events, kvstore};
use many_server::transport::http::HttpServer;
use many_server::ManyServer;
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::level_filters::LevelFilter;
use tracing::{debug, info};

mod error;
mod module;
mod storage;

use module::*;

#[derive(clap::ArgEnum, Clone, Debug)]
enum LogStrategy {
    Terminal,
    Syslog,
}

#[derive(Debug, Parser)]
struct Opts {
    /// Increase output logging verbosity to DEBUG level.
    #[clap(short, long, parse(from_occurrences))]
    verbose: i8,

    /// Suppress all output logging. Can be used multiple times to suppress more.
    #[clap(short, long, parse(from_occurrences))]
    quiet: i8,

    /// The location of a PEM file for the identity of this server.
    #[clap(long)]
    pem: PathBuf,

    /// The address and port to bind to for the MANY Http server.
    #[clap(long, short, default_value = "127.0.0.1:8000")]
    addr: SocketAddr,

    /// Uses an ABCI application module.
    #[clap(long)]
    abci: bool,

    /// Path of a state file (that will be used for the initial setup).
    #[clap(long)]
    state: Option<PathBuf>,

    /// Path to a persistent store database (rocksdb).
    #[clap(long)]
    persistent: PathBuf,

    /// Delete the persistent storage to start from a clean state.
    /// If this is not specified the initial state will not be used.
    #[clap(long, short)]
    clean: bool,

    /// Use given logging strategy
    #[clap(long, arg_enum, default_value_t = LogStrategy::Terminal)]
    logmode: LogStrategy,

    /// Path to a JSON file containing an array of MANY addresses
    /// Only addresses from this array will be able to execute commands, e.g., send, put, ...
    /// Any addresses will be able to execute queries, e.g., balance, get, ...
    #[clap(long)]
    allow_addrs: Option<PathBuf>,
}

fn main() {
    let Opts {
        verbose,
        quiet,
        pem,
        addr,
        abci,
        mut state,
        persistent,
        clean,
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
            let identity = std::ffi::CStr::from_bytes_with_nul(b"many-kvstore\0").unwrap();
            let (options, facility) = Default::default();
            let syslog = syslog_tracing::Syslog::new(identity, options, facility).unwrap();

            let subscriber = subscriber.with_ansi(false).with_writer(syslog);
            subscriber.init();
        }
    };

    debug!("{:?}", Opts::parse());
    info!(
        version = env!("CARGO_PKG_VERSION"),
        git_sha = env!("VERGEN_GIT_SHA")
    );

    if clean {
        // Delete the persistent storage.
        let _ = std::fs::remove_dir_all(persistent.as_path());
    } else if persistent.exists() {
        // Initial state is ignored.
        state = None;
    }

    let key = CoseKeyIdentity::from_pem(&std::fs::read_to_string(&pem).unwrap()).unwrap();

    let state = state.map(|state| {
        let content = std::fs::read_to_string(&state).unwrap();
        json5::from_str(&content).unwrap()
    });

    let module = if let Some(state) = state {
        KvStoreModuleImpl::new(state, persistent, abci).unwrap()
    } else {
        KvStoreModuleImpl::load(persistent, abci).unwrap()
    };

    let module = Arc::new(Mutex::new(module));

    let many = ManyServer::simple(
        "many-kvstore",
        key,
        (AnonymousVerifier, CoseKeyVerifier),
        Some(env!("CARGO_PKG_VERSION").to_string()),
    );

    {
        let mut s = many.lock().unwrap();
        s.add_module(kvstore::KvStoreModule::new(module.clone()));
        let kvstore_command_module = kvstore::KvStoreCommandsModule::new(module.clone());
        if let Some(path) = allow_addrs {
            let allow_addrs: BTreeSet<Address> =
                json5::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            s.add_module(allow_addrs::AllowAddrsModule {
                inner: kvstore_command_module,
                allow_addrs,
            });
        } else {
            s.add_module(kvstore_command_module);
        }
        s.add_module(events::EventsModule::new(module.clone()));

        s.add_module(AccountFeatureModule::new(
            account::AccountModule::new(module.clone()),
            [Feature::with_id(2)],
        ));
        if abci {
            s.set_timeout(u64::MAX);
            s.add_module(abci_backend::AbciModule::new(module));
        }
    }
    let mut many_server = HttpServer::new(many);

    signal_hook::flag::register(signal_hook::consts::SIGTERM, many_server.term_signal())
        .expect("Could not register signal handler");
    signal_hook::flag::register(signal_hook::consts::SIGHUP, many_server.term_signal())
        .expect("Could not register signal handler");
    signal_hook::flag::register(signal_hook::consts::SIGINT, many_server.term_signal())
        .expect("Could not register signal handler");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(many_server.bind(addr)).unwrap();
}
