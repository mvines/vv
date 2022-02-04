use {
    solana_client::{
        nonblocking::rpc_client::RpcClient, rpc_client::GetConfirmedSignaturesForAddress2Config,
        rpc_response::RpcConfirmedTransactionStatusWithSignature,
    },
    solana_sdk::{
        clock::Slot, program_utils::limited_deserialize, pubkey::Pubkey, signature::Signature,
        transaction::SanitizedTransaction,
    },
    solana_transaction_status::UiTransactionEncoding,
    solana_vote_program::vote_instruction::VoteInstruction,
    solana_vote_program::vote_state::Vote,
    std::{
        cmp::Ordering,
        collections::{BTreeMap, HashMap},
        fmt,
    },
};

fn is_simple_vote_transaction(transaction: &SanitizedTransaction) -> Option<Vote> {
    if transaction.message().instructions().len() == 1 {
        let (program_pubkey, instruction) = transaction
            .message()
            .program_instructions_iter()
            .next()
            .unwrap();
        if program_pubkey == &solana_vote_program::id() {
            if let Ok(vote_instruction) = limited_deserialize::<VoteInstruction>(&instruction.data)
            {
                match vote_instruction {
                    VoteInstruction::Vote(vote) | VoteInstruction::VoteSwitch(vote, _) => {
                        return Some(vote)
                    }
                    _ => {}
                }
            }
        }
    }
    None
}

#[derive(Default, Debug, PartialEq, Eq, Clone)]
struct VoteMeta {
    signature: Signature,
    success: bool,
    vote_slots: Vec<Slot>,
    landed_slot: Slot,
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum TableEntryKind {
    Space,
    Vote,
    VoteGap,
    Waiting,
    Landed,
}

impl Default for TableEntryKind {
    fn default() -> Self {
        Self::Space
    }
}

impl PartialOrd for TableEntryKind {
    fn partial_cmp(&self, _other: &Self) -> Option<Ordering> {
        None
    }
}

#[derive(Default, Debug, PartialEq, Eq, Clone)]
struct TableEntry {
    kind: TableEntryKind,
    vote_meta: VoteMeta,
}

impl fmt::Display for TableEntry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let signature = self.vote_meta.signature.to_string();
        let sign = match self.kind {
            TableEntryKind::Space => {
                return write!(f, "         ");
            }
            TableEntryKind::Vote => "+",
            TableEntryKind::VoteGap => {
                return write!(f, "   xx    ");
            }
            TableEntryKind::Waiting => {
                return write!(f, "   ^^    ");
            }
            TableEntryKind::Landed => "=",
        };
        let success = if self.vote_meta.success { " " } else { "!" };

        write!(f, "{0}{1}{2}..{1}", sign, success, &signature[0..4],)
    }
}

impl Ord for TableEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        let mut cmp = self.vote_meta.success.cmp(&other.vote_meta.success);
        if cmp == Ordering::Equal {
            cmp = self
                .vote_meta
                .vote_slots
                .first()
                .cmp(&other.vote_meta.vote_slots.first());
        }
        if cmp == Ordering::Equal {
            cmp = self.vote_meta.landed_slot.cmp(&other.vote_meta.landed_slot);
        }
        if cmp == Ordering::Equal {
            cmp = self.vote_meta.signature.cmp(&other.vote_meta.signature);
        }
        cmp
    }
}

impl PartialOrd for TableEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub async fn process_view_votes(
    rpc_client: &RpcClient,
    vote_account_address: &Pubkey,
    limit: usize,
    before: Option<Signature>,
) -> Result<(), Box<dyn std::error::Error>> {
    let signatures_for_address = rpc_client
        .get_signatures_for_address_with_config(
            vote_account_address,
            GetConfirmedSignaturesForAddress2Config {
                limit: Some(limit),
                before,
                ..GetConfirmedSignaturesForAddress2Config::default()
            },
        )
        .await?;

    println!("{} transaction to processs:", signatures_for_address.len(),);
    if signatures_for_address.is_empty() {
        return Ok(());
    }

    let mut vote_metas = vec![];
    let mut slot_vote_count = HashMap::<Slot, usize>::default();

    for RpcConfirmedTransactionStatusWithSignature {
        signature,
        slot: landed_slot,
        err,
        ..
    } in signatures_for_address
    {
        let signature = signature.parse::<Signature>().unwrap();
        println!("{}", signature);
        let transaction = rpc_client
            .get_transaction(&signature, UiTransactionEncoding::Base64)
            .await?
            .transaction
            .transaction
            .decode()
            .expect("decode");

        let transaction = SanitizedTransaction::try_from_legacy_transaction(transaction)?;

        if let Some(vote) = is_simple_vote_transaction(&transaction) {
            /*
            println!(
                "VOTE! {} slot {}: {:?}",
                if err.is_none() { " OK " } else { "FAIL" },
                slot,
                vote
            );
            */
            if !vote.slots.is_empty() {
                let mut vote_slots = vote.slots.clone();
                vote_slots.sort_unstable();

                for slot in *vote_slots.first().unwrap()..=landed_slot + 1 {
                    slot_vote_count
                        .entry(slot)
                        .and_modify(|e| *e += 1)
                        .or_insert(1);
                }

                vote_metas.push(VoteMeta {
                    signature,
                    success: err.is_none(),
                    landed_slot,
                    vote_slots,
                });
            }
        }
    }

    let slot_vote_max_depth = slot_vote_count.values().max().unwrap();

    let mut table = BTreeMap::<Slot, Vec<Option<TableEntry>>>::default();
    let mut failed_vote_count = 0;

    vote_metas.sort_by(|a, b| b.landed_slot.cmp(&a.landed_slot));
    let mut max_last_vote_slot = 0;
    for vote_meta in vote_metas {
        let first_vote_slot = vote_meta.vote_slots[0];
        let last_vote_slot = *vote_meta.vote_slots.last().unwrap();
        max_last_vote_slot = last_vote_slot.max(max_last_vote_slot);
        if !vote_meta.success {
            failed_vote_count += 1;
        }

        let mut depth = 0;
        loop {
            let mut occupied = false;
            assert!(depth < *slot_vote_max_depth);
            for slot in first_vote_slot..=vote_meta.landed_slot + 1 {
                let e = table
                    .entry(slot)
                    .or_insert_with(|| vec![None; *slot_vote_max_depth]);
                if e[depth].is_some() {
                    occupied = true;
                    break;
                }
            }

            if !occupied {
                for slot in first_vote_slot..=vote_meta.landed_slot + 1 {
                    table.entry(slot).and_modify(|e| {
                        assert!(e[depth].is_none());
                        e[depth] = Some(TableEntry {
                            kind: if slot == vote_meta.landed_slot {
                                TableEntryKind::Landed
                            } else if vote_meta.vote_slots.contains(&slot) {
                                TableEntryKind::Vote
                            } else if slot < last_vote_slot {
                                TableEntryKind::VoteGap
                            } else if slot < vote_meta.landed_slot {
                                TableEntryKind::Waiting
                            } else {
                                TableEntryKind::Space
                            },
                            vote_meta: vote_meta.clone(),
                        });
                    });
                }
                break;
            }
            depth += 1;
        }
    }

    {
        let end_slot = *table.keys().last().unwrap();
        table.remove_entry(&end_slot);
    }

    let start_slot = *table.keys().next().unwrap();
    let end_slot = *table.keys().last().unwrap();
    for slot in start_slot..end_slot {
        table.entry(slot).or_default();
    }
    let confirmed_slots = rpc_client.get_blocks(start_slot, Some(end_slot)).await?;

    let mut miss_count = 0;

    println!();
    for (slot, row_entries) in table {
        let confirmed = confirmed_slots.contains(&slot);
        let miss = slot < max_last_vote_slot
            && !row_entries.iter().any(|entry| {
                entry
                    .as_ref()
                    .map(|entry| entry.kind == TableEntryKind::Vote && entry.vote_meta.success)
                    .unwrap_or(false)
            });
        if confirmed && miss {
            miss_count += 1
        }
        println!(
            "{0}{1:8}{0} {2}",
            if confirmed {
                if miss {
                    " MISS "
                } else {
                    "      "
                }
            } else {
                " SKIP "
            },
            slot,
            row_entries
                .into_iter()
                .map(|entry| format!("{} | ", entry.unwrap_or_default()))
                .collect::<String>()
        );
    }

    println!(
        "\nSlot Range: {}..{}\n{} of {} confirmed",
        start_slot,
        end_slot,
        confirmed_slots.len(),
        end_slot - start_slot + 1
    );
    if miss_count > 0 {
        println!("Missed slots: {}", miss_count);
    }
    if failed_vote_count > 0 {
        println!("Failed vote transactions: {}", failed_vote_count);
    }

    Ok(())
}
