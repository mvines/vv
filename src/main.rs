use {
    clap::{
        crate_description, crate_name, crate_version, value_t_or_exit, App, AppSettings, Arg,
        SubCommand,
    },
    futures_util::StreamExt,
    solana_clap_utils::{
        input_parsers::pubkey_of,
        input_validators::{
            is_parsable, is_url_or_moniker, is_valid_pubkey, is_valid_signer,
            normalize_to_url_if_moniker,
        },
        keypair::DefaultSigner,
    },
    solana_client::nonblocking::{pubsub_client::PubsubClient, rpc_client::RpcClient},
    solana_remote_wallet::remote_wallet::RemoteWalletManager,
    solana_sdk::{
        clock::Slot, commitment_config::CommitmentConfig, hash::Hash, pubkey::Pubkey,
        signature::Signer,
    },
    solana_vote_program::vote_state::{Vote, VoteState},
    std::{
        collections::{HashMap, HashSet},
        process::exit,
        sync::Arc,
    },
};

mod vv;

struct Config {
    commitment_config: CommitmentConfig,
    default_signer: Box<dyn Signer>,
    json_rpc_url: String,
    verbose: bool,
    websocket_url: String,
}

// a slot is recent if it's newer than the last vote we have
pub fn is_recent(vote_state: &VoteState, slot: Slot) -> bool {
    if let Some(last_voted_slot) = vote_state.last_voted_slot() {
        if slot <= last_voted_slot {
            return false;
        }
    }
    true
}

pub fn is_locked_out(vote_state: &VoteState, slot: Slot, ancestors: &HashSet<Slot>) -> bool {
    // Check if a slot is locked out by simulating adding a vote for that
    // slot to the current lockouts to pop any expired votes. If any of the
    // remaining voted slots are on a different fork from the checked slot,
    // it's still locked out.
    let mut vote_state = vote_state.clone();
    vote_state.process_slot_vote_unchecked(slot);
    for vote in &vote_state.votes {
        if slot != vote.slot && !ancestors.contains(&vote.slot) {
            println!("vote.slot {} is not an ancestor of {}", vote.slot, slot);
            return true;
        }
    }
    false
}

async fn process_votes(websocket_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let pubsub_client = PubsubClient::new(websocket_url).await?;

    let (mut rpc_votes, votes_unsubscribe) = pubsub_client.vote_subscribe().await?;
    let (mut slots, slot_unsubscribe) = pubsub_client.slot_subscribe().await?;

    let mut votes_by_slot = HashMap::<Slot, Vec<(Pubkey, Vote)>>::default();
    let mut slot_parents = HashMap::</*child=*/ Slot, /*parent=*/ Slot>::default();
    let mut vote_states = HashMap::<Pubkey, VoteState>::default();
    loop {
        tokio::select! {
            Some(rpc_vote) = rpc_votes.next() => {
                println!("Vote: {:?}", rpc_vote);

                if !rpc_vote.slots.is_empty() {
                    let vote_pubkey = rpc_vote.vote_pubkey.parse::<Pubkey>().unwrap();

                    let vote = Vote {
                        slots: rpc_vote.slots.clone(),
                        hash: rpc_vote.hash.parse::<Hash>().unwrap(),
                        timestamp: rpc_vote.timestamp,
                    };
                    votes_by_slot.entry(*vote.slots.last().unwrap()).or_default().push((vote_pubkey,vote));
                }
            }

            Some(slot_info) = slots.next() => {
                println!("{:?}", slot_info);
                slot_parents.insert(slot_info.slot, slot_info.parent);
            }
            else => break,
        }

        if slot_parents.is_empty() {
            continue;
        }

        let lowest_slot = loop {
            let lowest_slot = *slot_parents.keys().min().unwrap();
            if slot_parents.len() < 1_000 {
                break lowest_slot;
            }
            slot_parents.remove(&lowest_slot);
        };
        votes_by_slot.retain(|slot, _| *slot >= lowest_slot);

        let highest_slot = *slot_parents.keys().max().unwrap();

        let mut slots = votes_by_slot
            .keys()
            .filter(|slot| **slot <= highest_slot)
            .cloned()
            .collect::<Vec<_>>();
        slots.sort_unstable();

        if !slots.is_empty() {
            println!("Vote slots to process: {:?}", slots);
        }

        for slot in slots {
            let votes = votes_by_slot.remove(&slot).unwrap();

            fn build_ancestors(
                mut slot: Slot,
                slot_parents: &HashMap</*child=*/ Slot, /*parent=*/ Slot>,
            ) -> HashSet<Slot> {
                let mut ancestors = HashSet::default();
                while let Some(parent) = slot_parents.get(&slot) {
                    ancestors.insert(*parent);
                    slot = *parent;
                }
                ancestors
            }

            println!("   slot {}:", slot);
            for (vote_pubkey, vote) in votes {
                println!("  Processing {} vote: {:?}", vote_pubkey, vote);

                let vote_state = vote_states.entry(vote_pubkey).or_default();

                if let Some(lowest_vote_state_slot) = vote_state.votes.iter().map(|l| l.slot).min()
                {
                    if !slot_parents.contains_key(&lowest_vote_state_slot) {
                        println!(
                            "  WARN: Unable to process due to lowest vote_state slot = {}",
                            lowest_vote_state_slot
                        );
                        continue;
                    }
                }

                if let Some(lowest_vote_slot) = vote.slots.first() {
                    if !slot_parents.contains_key(lowest_vote_slot) {
                        println!(
                            "  WARN: Unable to process due to lowest vote slot = {}",
                            lowest_vote_slot
                        );
                        continue;
                    }
                }

                let highest_vote_slot = *vote.slots.last().unwrap();
                if is_recent(vote_state, highest_vote_slot) {
                    let ancestors = build_ancestors(highest_vote_slot, &slot_parents);
                    if is_locked_out(vote_state, highest_vote_slot, &ancestors) {
                        panic!(
                            "locked out at {} with state {:?}. ancestors {:?}",
                            highest_vote_slot, vote_state, ancestors
                        );
                    }

                    vote_state.process_vote_unchecked(vote);
                    println!(
                        "    tower depth: {}, credits: {}",
                        vote_state.votes.len(),
                        vote_state.credits()
                    );
                }
            }
        }
    }

    slot_unsubscribe().await;
    votes_unsubscribe().await;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app_matches = App::new(crate_name!())
        .about(crate_description!())
        .version(crate_version!())
        .setting(AppSettings::SubcommandRequiredElseHelp)
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
                .global(true)
                .help("Filepath or URL to a keypair [default: client keypair]"),
        )
        .arg(
            Arg::with_name("verbose")
                .long("verbose")
                .short("v")
                .takes_value(false)
                .global(true)
                .help("Show additional information"),
        )
        .arg(
            Arg::with_name("json_rpc_url")
                .short("u")
                .long("url")
                .value_name("URL")
                .takes_value(true)
                .global(true)
                .validator(is_url_or_moniker)
                .help("JSON RPC URL for the cluster [default: value from configuration file]"),
        )
        .subcommand(SubCommand::with_name("votes").about("Stream votes"))
        .subcommand(
            SubCommand::with_name("vv")
                .about("Vote Viewer")
                .arg(
                    Arg::with_name("vote_account_address")
                        .validator(is_valid_pubkey)
                        .value_name("ADDRESS")
                        .takes_value(true)
                        .index(1)
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
                ),
        )
        .get_matches();

    let (sub_command, sub_matches) = app_matches.subcommand();
    let matches = sub_matches.unwrap();
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
                .signer_from_path(matches, &mut wallet_manager)
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

    match (sub_command, sub_matches) {
        ("vv", Some(arg_matches)) => {
            let vote_account_address = pubkey_of(arg_matches, "vote_account_address")
                .unwrap_or_else(|| config.default_signer.pubkey());
            let limit = value_t_or_exit!(arg_matches, "limit", usize);
            vv::process_view_votes(&rpc_client, &vote_account_address, limit)
                .await
                .unwrap_or_else(|err| {
                    eprintln!("error: {}", err);
                    exit(1);
                });
        }
        ("votes", Some(_arg_matches)) => {
            process_votes(&config.websocket_url)
                .await
                .unwrap_or_else(|err| {
                    eprintln!("error: {}", err);
                    exit(1);
                });
        }
        _ => unreachable!(),
    };

    Ok(())
}
