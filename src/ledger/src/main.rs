use clap::{ArgGroup, Parser};
use many_client::client::blocking::ManyClient;
use many_error::ManyError;
use many_identity::{Address, AnonymousIdentity, Identity};
use many_identity_dsa::CoseKeyIdentity;
use many_identity_hsm::{Hsm, HsmIdentity, HsmMechanismType, HsmSessionType, HsmUserType};
use many_modules::r#async::{StatusArgs, StatusReturn};
use many_modules::{ledger, r#async};
use many_protocol::ResponseMessage;
use many_types::ledger::{Symbol, TokenAmount};
use minicbor::data::Tag;
use minicbor::encode::{Error, Write};
use minicbor::{Decoder, Encoder};
use num_bigint::BigUint;
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use tracing::{debug, error, info, trace};
use tracing_subscriber::filter::LevelFilter;

mod multisig;

#[derive(clap::ArgEnum, Clone, Debug)]
enum LogStrategy {
    Terminal,
    Syslog,
}

#[derive(Clone, Debug)]
#[repr(transparent)]
struct Amount(pub BigUint);

impl Display for Amount {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl minicbor::Encode<()> for Amount {
    fn encode<W: Write>(&self, e: &mut Encoder<W>, _: &mut ()) -> Result<(), Error<W::Error>> {
        e.tag(Tag::PosBignum)?.bytes(&self.0.to_bytes_be())?;
        Ok(())
    }
}
impl<'b> minicbor::Decode<'b, ()> for Amount {
    fn decode(d: &mut Decoder<'b>, _: &mut ()) -> Result<Self, minicbor::decode::Error> {
        let t = d.tag()?;
        if t != Tag::PosBignum {
            Err(minicbor::decode::Error::message("Invalid tag."))
        } else {
            Ok(Amount(BigUint::from_bytes_be(d.bytes()?)))
        }
    }
}

#[derive(Parser)]
#[clap(
    group(
        ArgGroup::new("hsm")
        .multiple(true)
        .args(&["module", "slot", "keyid"])
        .requires_all(&["module", "slot", "keyid"])
    )
)]
struct Opts {
    /// Many server URL to connect to.
    #[clap(default_value = "http://localhost:8000")]
    server: String,

    /// The identity of the server (an identity string), or anonymous if you don't know it.
    #[clap(default_value_t)]
    #[clap(long)]
    server_id: Address,

    /// A PEM file for the identity. If not specified, anonymous will be used.
    #[clap(long)]
    pem: Option<PathBuf>,

    /// HSM PKCS#11 module path
    #[clap(long, conflicts_with("pem"))]
    module: Option<PathBuf>,

    /// HSM PKCS#11 slot ID
    #[clap(long, conflicts_with("pem"))]
    slot: Option<u64>,

    /// HSM PKCS#11 key ID
    #[clap(long, conflicts_with("pem"))]
    keyid: Option<String>,

    /// Increase output logging verbosity to DEBUG level.
    #[clap(short, long, parse(from_occurrences))]
    verbose: i8,

    /// Suppress all output logging. Can be used multiple times to suppress more.
    #[clap(short, long, parse(from_occurrences))]
    quiet: i8,

    /// Use given logging strategy
    #[clap(long, arg_enum, default_value_t = LogStrategy::Terminal)]
    logmode: LogStrategy,

    #[clap(subcommand)]
    subcommand: SubCommand,
}

#[derive(Parser)]
enum SubCommand {
    /// Read the balance of an account.
    Balance(BalanceOpt),

    /// Send tokens to an account.
    Send(TargetCommandOpt),

    /// Perform a multisig operation.
    Multisig(multisig::CommandOpt),
}

#[derive(Parser)]
struct BalanceOpt {
    /// The identity to check. This can be a Pem file (which will be used to calculate a public
    /// identity) or an identity string. If omitted it will use the identity of the caller.
    identity: Option<String>,

    /// The symbol to check the balance of. This can either be an identity or
    /// a local name for a symbol. If it doesn't parse to an identity an
    /// additional call will be made to retrieve local names.
    #[clap(last = true)]
    symbols: Vec<String>,
}

#[derive(Parser)]
pub(crate) struct TargetCommandOpt {
    /// The from identity, if different than the one provided by the
    /// PEM argument.
    #[clap(long)]
    account: Option<Address>,

    /// The account or target identity.
    identity: Address,

    /// The amount of tokens.
    amount: BigUint,

    /// The symbol to use.  This can either be an identity or
    /// a local name for a symbol. If it doesn't parse to an identity an
    /// additional call will be made to retrieve local names.
    symbol: String,
}

pub fn resolve_symbol(
    client: &ManyClient<impl Identity>,
    symbol: String,
) -> Result<Address, ManyError> {
    if let Ok(symbol) = Address::from_str(&symbol) {
        Ok(symbol)
    } else {
        // Get info.
        let info: ledger::InfoReturns =
            minicbor::decode(&client.call_("ledger.info", ())?).unwrap();
        info.local_names
            .into_iter()
            .find(|(_, y)| y == &symbol)
            .map(|(x, _)| x)
            .ok_or_else(|| ManyError::unknown(format!("Could not resolve symbol '{}'", &symbol)))
    }
}

fn balance(
    client: ManyClient<impl Identity>,
    account: Option<Address>,
    symbols: Vec<String>,
) -> Result<(), ManyError> {
    // Get info.
    let info: ledger::InfoReturns = minicbor::decode(&client.call_("ledger.info", ())?).unwrap();
    let local_names: BTreeMap<String, Symbol> = info
        .local_names
        .iter()
        .map(|(x, y)| (y.clone(), *x))
        .collect();

    let argument = ledger::BalanceArgs {
        account,
        symbols: if symbols.is_empty() {
            None
        } else {
            Some(
                symbols
                    .iter()
                    .map(|x| {
                        if let Ok(i) = Address::from_str(x) {
                            Ok(i)
                        } else if let Some(i) = local_names.get(x.as_str()) {
                            Ok(*i)
                        } else {
                            Err(ManyError::unknown(format!(
                                "Could not resolve symbol '{}'",
                                x
                            )))
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            )
        },
    };
    let payload = client.call_("ledger.balance", argument)?;

    if payload.is_empty() {
        Err(ManyError::unexpected_empty_response())
    } else {
        let balance: ledger::BalanceReturns = minicbor::decode(&payload).unwrap();
        for (symbol, amount) in balance.balances {
            if let Some(symbol_name) = info.local_names.get(&symbol) {
                println!("{:>12} {} ({})", amount, symbol_name, symbol);
            } else {
                println!("{:>12} {}", amount, symbol);
            }
        }

        Ok(())
    }
}

pub(crate) fn wait_response(
    client: ManyClient<impl Identity>,
    response: ResponseMessage,
) -> Result<Vec<u8>, ManyError> {
    let ResponseMessage {
        data, attributes, ..
    } = response;

    let payload = data?;
    debug!("response: {}", hex::encode(&payload));
    if payload.is_empty() {
        let attr = match attributes.get::<r#async::attributes::AsyncAttribute>() {
            Ok(attr) => attr,
            _ => {
                info!("Empty payload.");
                return Ok(Vec::new());
            }
        };
        info!("Async token: {}", hex::encode(&attr.token));

        let progress =
            indicatif::ProgressBar::new_spinner().with_message("Waiting for async response");
        progress.enable_steady_tick(100);

        // TODO: improve on this by using duration and thread and watchdog.
        // Wait for the server for ~60 seconds by pinging it every second.
        for _ in 0..60 {
            let response = client.call(
                "async.status",
                StatusArgs {
                    token: attr.token.clone(),
                },
            )?;
            let status: StatusReturn = minicbor::decode(&response.data?)
                .map_err(|e| ManyError::deserialization_error(e.to_string()))?;
            match status {
                StatusReturn::Done { response } => {
                    progress.finish();
                    return wait_response(client, *response);
                }
                StatusReturn::Expired => {
                    progress.finish();
                    info!("Async token expired before we could check it.");
                    return Ok(Vec::new());
                }
                _ => {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
        Err(ManyError::unknown(
            "Transport timed out waiting for async result.",
        ))
    } else {
        Ok(payload)
    }
}

fn send(
    client: ManyClient<impl Identity>,
    from: Address,
    to: Address,
    amount: BigUint,
    symbol: String,
) -> Result<(), ManyError> {
    let symbol = resolve_symbol(&client, symbol)?;

    if from.is_anonymous() {
        Err(ManyError::invalid_identity())
    } else {
        let arguments = ledger::SendArgs {
            from: Some(from),
            to,
            symbol,
            amount: TokenAmount::from(amount),
        };
        let response = client.call("ledger.send", arguments)?;
        let payload = wait_response(client, response)?;
        println!("{}", minicbor::display(&payload));
        Ok(())
    }
}

fn main() {
    let Opts {
        pem,
        module,
        slot,
        keyid,
        server,
        server_id,
        subcommand,
        verbose,
        quiet,
        logmode,
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
            let identity = std::ffi::CStr::from_bytes_with_nul(b"ledger\0").unwrap();
            let (options, facility) = Default::default();
            let syslog = syslog_tracing::Syslog::new(identity, options, facility).unwrap();

            let subscriber = subscriber.with_writer(syslog);
            subscriber.init();
        }
    };

    let key: Box<dyn Identity> = if let (Some(module), Some(slot), Some(keyid)) =
        (module, slot, keyid)
    {
        trace!("Getting user PIN");
        let pin = rpassword::prompt_password("Please enter the HSM user PIN: ")
            .expect("I/O error when reading HSM PIN");
        let keyid = hex::decode(keyid).expect("Failed to decode keyid to hex");

        {
            let mut hsm = Hsm::get_instance().expect("HSM mutex poisoned");
            hsm.init(module, keyid)
                .expect("Failed to initialize HSM module");

            // The session will stay open until the application terminates
            hsm.open_session(slot, HsmSessionType::RO, Some(HsmUserType::User), Some(pin))
                .expect("Failed to open HSM session");
        }

        trace!("Creating CoseKeyIdentity");
        // Only ECDSA is supported at the moment. It should be easy to add support for new EC mechanisms
        Box::new(
            HsmIdentity::new(HsmMechanismType::ECDSA)
                .expect("Unable to create CoseKeyIdentity from HSM"),
        )
    } else {
        pem.map_or_else(
            || Box::new(AnonymousIdentity) as Box<dyn Identity>,
            |p| Box::new(CoseKeyIdentity::from_pem(&std::fs::read_to_string(&p).unwrap()).unwrap()),
        )
    };

    let client_address = key.address();
    let client = ManyClient::new(&server, server_id, key).unwrap();
    let result = match subcommand {
        SubCommand::Balance(BalanceOpt { identity, symbols }) => {
            let identity = identity.map(|identity| {
                Address::from_str(&identity)
                    .or_else(|_| {
                        let bytes = std::fs::read_to_string(PathBuf::from(identity))?;

                        Ok(CoseKeyIdentity::from_pem(&bytes).unwrap().address())
                    })
                    .map_err(|_: std::io::Error| ())
                    .expect("Unable to decode identity command-line argument")
            });

            balance(client, identity, symbols)
        }
        SubCommand::Send(TargetCommandOpt {
            account,
            identity,
            amount,
            symbol,
        }) => {
            let from = account.unwrap_or(client_address);
            send(client, from, identity, amount, symbol)
        }
        SubCommand::Multisig(opts) => multisig::multisig(client, opts),
    };

    if let Err(err) = result {
        error!(
            "Error returned by server:\n|  {}\n",
            err.to_string()
                .split('\n')
                .collect::<Vec<&str>>()
                .join("\n|  ")
        );
        std::process::exit(1);
    }
}
