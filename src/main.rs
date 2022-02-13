use {
    clap::{crate_description, crate_name, crate_version, value_t, value_t_or_exit, App, Arg},
    solana_clap_utils::{
        input_parsers::pubkey_of,
        input_validators::{
            is_parsable, is_url_or_moniker, is_valid_pubkey, is_valid_signer,
            normalize_to_url_if_moniker,
        },
        keypair::DefaultSigner,
    },
    solana_client::nonblocking::rpc_client::RpcClient,
    solana_remote_wallet::remote_wallet::RemoteWalletManager,
    solana_sdk::{
        commitment_config::CommitmentConfig,
        signature::{Signature, Signer},
    },
    std::{process::exit, sync::Arc},
};

mod vv;

struct Config {
    commitment_config: CommitmentConfig,
    default_signer: Box<dyn Signer>,
    json_rpc_url: String,
    verbose: bool,
    websocket_url: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(crate_version!())
        .arg({
            let arg = Arg::with_name("config_file")
                .short("C")
                .long("config")
                .value_name("PATH")
                .takes_value(true)
                .global(true)
                .help("Configuration file to use");
            if let Some(ref config_file) = *solana_cli_config::CONFIG_FILE {
                arg.default_value(config_file)
            } else {
                arg
            }
        })
        .arg(
            Arg::with_name("keypair")
                .long("keypair")
                .value_name("KEYPAIR")
                .validator(is_valid_signer)
                .takes_value(true)
                .help("Filepath or URL to a keypair [default: client keypair]"),
        )
        .arg(
            Arg::with_name("verbose")
                .long("verbose")
                .short("v")
                .takes_value(false)
                .help("Show additional information"),
        )
        .arg(
            Arg::with_name("json_rpc_url")
                .short("u")
                .long("url")
                .value_name("URL")
                .takes_value(true)
                .validator(is_url_or_moniker)
                .help("JSON RPC URL for the cluster [default: value from configuration file]"),
        )
        .arg(
            Arg::with_name("vote_account_address")
                .validator(is_valid_pubkey)
                .value_name("ADDRESS")
                .takes_value(true)
                .help("Vote account address"),
        )
        .arg(
            Arg::with_name("limit")
                .long("limit")
                .short("l")
                .validator(is_parsable::<usize>)
                .takes_value(true)
                .value_name("LIMIT")
                .default_value("10")
                .help("Number of transactions to process"),
        )
        .arg(
            Arg::with_name("before")
                .long("before")
                .short("b")
                .validator(is_parsable::<Signature>)
                .takes_value(true)
                .value_name("TRANSACTION_SIGNATURE")
                .help("Start with the first vote older than this transaction signature"),
        )
        .get_matches();

    let mut wallet_manager: Option<Arc<RemoteWalletManager>> = None;

    let config = {
        let cli_config = if let Some(config_file) = matches.value_of("config_file") {
            solana_cli_config::Config::load(config_file).unwrap_or_default()
        } else {
            solana_cli_config::Config::default()
        };

        let default_signer = DefaultSigner::new(
            "keypair",
            matches
                .value_of(&"keypair")
                .map(|s| s.to_string())
                .unwrap_or_else(|| cli_config.keypair_path.clone()),
        );

        let json_rpc_url = normalize_to_url_if_moniker(
            matches
                .value_of("json_rpc_url")
                .unwrap_or(&cli_config.json_rpc_url)
                .to_string(),
        );

        let websocket_url = solana_cli_config::Config::compute_websocket_url(&json_rpc_url);
        Config {
            commitment_config: CommitmentConfig::confirmed(),
            default_signer: default_signer
                .signer_from_path(&matches, &mut wallet_manager)
                .unwrap_or_else(|err| {
                    eprintln!("error: {}", err);
                    exit(1);
                }),
            json_rpc_url,
            verbose: matches.is_present("verbose"),
            websocket_url,
        }
    };
    solana_logger::setup_with_default("solana=info");

    if config.verbose {
        println!("JSON RPC URL: {}", config.json_rpc_url);
        println!("Websocket URL: {}", config.websocket_url);
    }
    let rpc_client =
        RpcClient::new_with_commitment(config.json_rpc_url.clone(), config.commitment_config);

    let vote_account_address = pubkey_of(&matches, "vote_account_address")
        .unwrap_or_else(|| config.default_signer.pubkey());
    let limit = value_t_or_exit!(matches, "limit", usize);
    let before = value_t!(matches, "before", Signature).ok();
    vv::process_view_votes(&rpc_client, &vote_account_address, limit, before)
        .await
        .unwrap_or_else(|err| {
            eprintln!("error: {}", err);
            exit(1);
        });

    Ok(())
}
