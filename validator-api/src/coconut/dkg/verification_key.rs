// Copyright 2022 - Nym Technologies SA <contact@nymtech.net>
// SPDX-License-Identifier: Apache-2.0

use crate::coconut::dkg::client::DkgClient;
use crate::coconut::dkg::complaints::ComplaintReason;
use crate::coconut::dkg::state::{ConsistentState, State};
use crate::coconut::error::CoconutError;
use coconut_dkg_common::event_attributes::DKG_PROPOSAL_ID;
use coconut_dkg_common::types::{NodeIndex, TOTAL_DEALINGS};
use coconut_dkg_common::verification_key::owner_from_cosmos_msgs;
use coconut_interface::KeyPair as CoconutKeyPair;
use cosmwasm_std::Addr;
use credentials::coconut::bandwidth::{PRIVATE_ATTRIBUTES, PUBLIC_ATTRIBUTES};
use cw3::{ProposalResponse, Status};
use dkg::bte::{decrypt_share, setup};
use dkg::{combine_shares, try_recover_verification_keys, Dealing, Threshold};
use nymcoconut::tests::helpers::transpose_matrix;
use nymcoconut::{check_vk_pairing, Base58, KeyPair, Parameters, SecretKey, VerificationKey};
use pemstore::KeyPairPath;
use std::collections::BTreeMap;
use validator_client::nymd::cosmwasm_client::logs::find_attribute;

// Filter the dealers based on what dealing they posted (or not) in the contract
async fn deterministic_filter_dealers(
    dkg_client: &DkgClient,
    state: &mut State,
    threshold: Threshold,
) -> Result<Vec<BTreeMap<NodeIndex, (Addr, Dealing)>>, CoconutError> {
    let mut dealings_maps = vec![];
    let initial_dealers_by_addr = state.current_dealers_by_addr();
    let initial_receivers = state.current_dealers_by_idx();
    let params = setup();

    for idx in 0..TOTAL_DEALINGS {
        let dealings = dkg_client.get_dealings(idx).await?;
        let dealings_map =
            BTreeMap::from_iter(dealings.into_iter().filter_map(|contract_dealing| {
                match Dealing::try_from(&contract_dealing.dealing) {
                    Ok(dealing) => {
                        if dealing
                            .verify(&params, threshold, &initial_receivers, None)
                            .is_err()
                        {
                            state.mark_bad_dealer(
                                &contract_dealing.dealer,
                                ComplaintReason::DealingVerificationError,
                            );
                            None
                        } else if let Some(idx) =
                            initial_dealers_by_addr.get(&contract_dealing.dealer)
                        {
                            Some((*idx, (contract_dealing.dealer, dealing)))
                        } else {
                            None
                        }
                    }
                    Err(_) => {
                        state.mark_bad_dealer(
                            &contract_dealing.dealer,
                            ComplaintReason::MalformedDealing,
                        );
                        None
                    }
                }
            }));
        dealings_maps.push(dealings_map);
    }
    for (addr, _) in initial_dealers_by_addr.iter() {
        for dealings_map in dealings_maps.iter() {
            if !dealings_map.iter().any(|(_, (address, _))| address == addr) {
                state.mark_bad_dealer(addr, ComplaintReason::MissingDealing);
                break;
            }
        }
    }

    Ok(dealings_maps)
}

fn derive_partial_keypair(
    state: &mut State,
    threshold: Threshold,
    dealings_maps: Vec<BTreeMap<NodeIndex, (Addr, Dealing)>>,
) -> Result<KeyPair, CoconutError> {
    let filtered_receivers_by_idx = state.current_dealers_by_idx();
    let filtered_dealers_by_addr = state.current_dealers_by_addr();
    let dk = state.dkg_keypair().private_key();
    let node_index_value = state.receiver_index_value()?;
    let mut scalars = vec![];
    let mut recovered_vks = vec![];
    for dealings_map in dealings_maps.into_iter() {
        let filtered_dealings: Vec<_> = dealings_map
            .into_iter()
            .filter_map(|(_, (addr, dealing))| {
                if filtered_dealers_by_addr.keys().any(|a| addr == *a) {
                    Some(dealing)
                } else {
                    None
                }
            })
            .collect();
        let recovered = try_recover_verification_keys(
            &filtered_dealings,
            threshold,
            &filtered_receivers_by_idx,
        )?;
        recovered_vks.push(recovered);

        let shares = filtered_dealings
            .iter()
            .map(|dealing| decrypt_share(dk, node_index_value, &dealing.ciphertexts, None))
            .collect::<Result<_, _>>()?;
        let scalar = combine_shares(
            shares,
            &filtered_receivers_by_idx
                .keys()
                .copied()
                .collect::<Vec<_>>(),
        )?;
        scalars.push(scalar);
    }
    state.set_recovered_vks(recovered_vks);

    let params = Parameters::new(PUBLIC_ATTRIBUTES + PRIVATE_ATTRIBUTES)?;
    let x = scalars.pop().unwrap();
    let sk = SecretKey::create_from_raw(x, scalars);
    let vk = sk.verification_key(&params);

    Ok(CoconutKeyPair::from_keys(sk, vk))
}

pub(crate) async fn verification_key_submission(
    dkg_client: &DkgClient,
    state: &mut State,
    keypair_path: &KeyPairPath,
) -> Result<(), CoconutError> {
    if state.coconut_keypair_is_some().await {
        return Ok(());
    }

    let threshold = state.threshold()?;
    let dealings_maps = deterministic_filter_dealers(dkg_client, state, threshold).await?;
    let coconut_keypair = derive_partial_keypair(state, threshold, dealings_maps)?;
    let vk_share = coconut_keypair.verification_key().to_bs58();
    pemstore::store_keypair(&coconut_keypair, keypair_path)?;
    let res = dkg_client.submit_verification_key_share(vk_share).await?;
    let proposal_id = find_attribute(&res.logs, "wasm", DKG_PROPOSAL_ID)
        .ok_or(CoconutError::ProposalIdError {
            reason: String::from("proposal id not found"),
        })?
        .value
        .parse::<u64>()
        .map_err(|_| CoconutError::ProposalIdError {
            reason: String::from("proposal id could not be parsed to u64"),
        })?;
    state.set_proposal_id(proposal_id);
    state.set_coconut_keypair(coconut_keypair).await;
    info!("DKG: Submitted own verification key");

    Ok(())
}

fn validate_proposal(proposal: &ProposalResponse) -> Option<(Addr, u64)> {
    if proposal.status == Status::Open {
        if let Some(owner) = owner_from_cosmos_msgs(&proposal.msgs) {
            return Some((owner, proposal.id));
        }
    }
    None
}

pub(crate) async fn verification_key_validation(
    dkg_client: &DkgClient,
    state: &mut State,
) -> Result<(), CoconutError> {
    if state.voted_vks() {
        return Ok(());
    }

    let vk_shares = dkg_client.get_verification_key_shares().await?;
    let proposal_ids = BTreeMap::from_iter(
        dkg_client
            .list_proposals()
            .await?
            .iter()
            .filter_map(validate_proposal),
    );
    let filtered_receivers_by_idx: Vec<_> =
        state.current_dealers_by_idx().keys().copied().collect();
    let recovered_partials: Vec<_> = state
        .recovered_vks()
        .iter()
        .map(|recovered_vk| recovered_vk.recovered_partials.clone())
        .collect();
    let recovered_partials = transpose_matrix(recovered_partials);
    let params = Parameters::new(PUBLIC_ATTRIBUTES + PRIVATE_ATTRIBUTES)?;
    for contract_share in vk_shares {
        if let Some(proposal_id) = proposal_ids.get(&contract_share.owner).copied() {
            match VerificationKey::try_from_bs58(contract_share.share) {
                Ok(vk) => {
                    if let Some(idx) = filtered_receivers_by_idx
                        .iter()
                        .position(|node_index| contract_share.node_index == *node_index)
                    {
                        if !check_vk_pairing(&params, &recovered_partials[idx], &vk) {
                            dkg_client
                                .vote_verification_key_share(proposal_id, false)
                                .await?;
                        } else {
                            dkg_client
                                .vote_verification_key_share(proposal_id, true)
                                .await?;
                        }
                    }
                }
                Err(_) => {
                    dkg_client
                        .vote_verification_key_share(proposal_id, false)
                        .await?
                }
            }
        }
    }
    state.set_voted_vks();
    info!("DKG: Validated the other verification keys");
    Ok(())
}

pub(crate) async fn verification_key_finalization(
    dkg_client: &DkgClient,
    state: &mut State,
) -> Result<(), CoconutError> {
    if state.executed_proposal() {
        return Ok(());
    }

    let proposal_id = state.proposal_id_value()?;
    dkg_client
        .execute_verification_key_share(proposal_id)
        .await?;
    state.set_executed_proposal();
    info!("DKG: Finalized own verification key on chain");

    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::coconut::dkg::dealing::dealing_exchange;
    use crate::coconut::dkg::public_key::public_key_submission;
    use crate::coconut::tests::DummyClient;
    use crate::coconut::KeyPair;
    use coconut_dkg_common::dealer::DealerDetails;
    use coconut_dkg_common::verification_key::ContractVKShare;
    use contracts_common::dealings::ContractSafeBytes;
    use dkg::bte::keys::KeyPair as DkgKeyPair;
    use rand::rngs::OsRng;
    use rand::Rng;
    use std::collections::HashMap;
    use std::env::temp_dir;
    use std::str::FromStr;
    use std::sync::{Arc, RwLock};
    use url::Url;
    use validator_client::nymd::AccountId;

    const TEST_VALIDATORS_ADDRESS: [&str; 3] = [
        "n1aq9kakfgwqcufr23lsv644apavcntrsqsk4yus",
        "n1s9l3xr4g0rglvk4yctktmck3h4eq0gp6z2e20v",
        "n19kl4py32vsk297dm93ezem992cdyzdy4zuc2x6",
    ];

    async fn prepare_clients_and_states(
        dealer_details_db: &Arc<RwLock<HashMap<String, DealerDetails>>>,
        dealings_db: &Arc<RwLock<HashMap<String, Vec<ContractSafeBytes>>>>,
        proposal_db: &Arc<RwLock<HashMap<u64, ProposalResponse>>>,
        verification_share_db: &Arc<RwLock<HashMap<String, ContractVKShare>>>,
    ) -> Vec<(DkgClient, State)> {
        let params = setup();
        let mut clients_and_states = vec![];

        for addr in TEST_VALIDATORS_ADDRESS {
            let dkg_client = DkgClient::new(
                DummyClient::new(AccountId::from_str(addr).unwrap())
                    .with_dealer_details(dealer_details_db)
                    .with_dealings(dealings_db)
                    .with_proposal_db(proposal_db)
                    .with_verification_share(verification_share_db),
            );
            let keypair = DkgKeyPair::new(&params, OsRng);
            let state = State::new(
                Url::parse("localhost:8000").unwrap(),
                keypair,
                KeyPair::new(),
            );
            clients_and_states.push((dkg_client, state));
        }
        for (dkg_client, state) in clients_and_states.iter_mut() {
            public_key_submission(dkg_client, state).await.unwrap();
        }
        for (dkg_client, state) in clients_and_states.iter_mut() {
            dealing_exchange(dkg_client, state, OsRng).await.unwrap();
        }
        clients_and_states
    }

    async fn prepare_clients_and_states_with_submission(
        dealer_details_db: &Arc<RwLock<HashMap<String, DealerDetails>>>,
        dealings_db: &Arc<RwLock<HashMap<String, Vec<ContractSafeBytes>>>>,
        proposal_db: &Arc<RwLock<HashMap<u64, ProposalResponse>>>,
        verification_share_db: &Arc<RwLock<HashMap<String, ContractVKShare>>>,
    ) -> Vec<(DkgClient, State)> {
        let mut clients_and_states = prepare_clients_and_states(
            dealer_details_db,
            dealings_db,
            proposal_db,
            verification_share_db,
        )
        .await;
        for (dkg_client, state) in clients_and_states.iter_mut() {
            let random_file: usize = OsRng.gen();
            let private_key_path = temp_dir().join(format!("private{}.pem", random_file));
            let public_key_path = temp_dir().join(format!("public{}.pem", random_file));
            let keypair_path = KeyPairPath::new(private_key_path.clone(), public_key_path.clone());
            verification_key_submission(dkg_client, state, &keypair_path)
                .await
                .unwrap();
            std::fs::remove_file(private_key_path).unwrap();
            std::fs::remove_file(public_key_path).unwrap();
        }
        clients_and_states
    }

    async fn prepare_clients_and_states_with_validation(
        dealer_details_db: &Arc<RwLock<HashMap<String, DealerDetails>>>,
        dealings_db: &Arc<RwLock<HashMap<String, Vec<ContractSafeBytes>>>>,
        proposal_db: &Arc<RwLock<HashMap<u64, ProposalResponse>>>,
        verification_share_db: &Arc<RwLock<HashMap<String, ContractVKShare>>>,
    ) -> Vec<(DkgClient, State)> {
        let mut clients_and_states = prepare_clients_and_states_with_submission(
            dealer_details_db,
            dealings_db,
            proposal_db,
            verification_share_db,
        )
        .await;
        for (dkg_client, state) in clients_and_states.iter_mut() {
            verification_key_validation(dkg_client, state)
                .await
                .unwrap();
        }
        clients_and_states
    }

    async fn prepare_clients_and_states_with_finalization(
        dealer_details_db: &Arc<RwLock<HashMap<String, DealerDetails>>>,
        dealings_db: &Arc<RwLock<HashMap<String, Vec<ContractSafeBytes>>>>,
        proposal_db: &Arc<RwLock<HashMap<u64, ProposalResponse>>>,
        verification_share_db: &Arc<RwLock<HashMap<String, ContractVKShare>>>,
    ) -> Vec<(DkgClient, State)> {
        let mut clients_and_states = prepare_clients_and_states_with_validation(
            dealer_details_db,
            dealings_db,
            proposal_db,
            verification_share_db,
        )
        .await;
        for (dkg_client, state) in clients_and_states.iter_mut() {
            verification_key_finalization(dkg_client, state)
                .await
                .unwrap();
        }
        clients_and_states
    }

    #[tokio::test]
    async fn check_dealers_filter_all_good() {
        let dealer_details_db = Arc::new(RwLock::new(HashMap::new()));
        let dealings_db = Arc::new(RwLock::new(HashMap::new()));
        let proposal_db = Arc::new(RwLock::new(HashMap::new()));
        let verification_share_db = Arc::new(RwLock::new(HashMap::new()));
        let mut clients_and_states = prepare_clients_and_states(
            &dealer_details_db,
            &dealings_db,
            &proposal_db,
            &verification_share_db,
        )
        .await;
        for (dkg_client, state) in clients_and_states.iter_mut() {
            let filtered = deterministic_filter_dealers(dkg_client, state, 2)
                .await
                .unwrap();
            assert_eq!(filtered.len(), TOTAL_DEALINGS);
            for mapping in filtered.iter() {
                assert_eq!(mapping.len(), 3);
            }
        }
    }

    #[tokio::test]
    async fn check_dealers_filter_one_bad_dealing() {
        let dealer_details_db = Arc::new(RwLock::new(HashMap::new()));
        let dealings_db = Arc::new(RwLock::new(HashMap::new()));
        let proposal_db = Arc::new(RwLock::new(HashMap::new()));
        let verification_share_db = Arc::new(RwLock::new(HashMap::new()));
        let mut clients_and_states = prepare_clients_and_states(
            &dealer_details_db,
            &dealings_db,
            &proposal_db,
            &verification_share_db,
        )
        .await;

        // corrupt just one dealing
        dealings_db
            .write()
            .unwrap()
            .entry(TEST_VALIDATORS_ADDRESS[0].to_string())
            .and_modify(|dealings| {
                let mut last = dealings.pop().unwrap();
                last.0.pop();
                dealings.push(last);
            });

        for (dkg_client, state) in clients_and_states.iter_mut().skip(1) {
            let filtered = deterministic_filter_dealers(dkg_client, state, 2)
                .await
                .unwrap();
            assert_eq!(filtered.len(), TOTAL_DEALINGS);
            let corrupted_status = state
                .all_dealers()
                .get(&Addr::unchecked(TEST_VALIDATORS_ADDRESS[0]))
                .unwrap()
                .as_ref()
                .unwrap_err();
            assert_eq!(*corrupted_status, ComplaintReason::MissingDealing);
        }
    }

    #[tokio::test]
    async fn check_dealers_filter_all_bad_dealings() {
        let dealer_details_db = Arc::new(RwLock::new(HashMap::new()));
        let dealings_db = Arc::new(RwLock::new(HashMap::new()));
        let proposal_db = Arc::new(RwLock::new(HashMap::new()));
        let verification_share_db = Arc::new(RwLock::new(HashMap::new()));
        let mut clients_and_states = prepare_clients_and_states(
            &dealer_details_db,
            &dealings_db,
            &proposal_db,
            &verification_share_db,
        )
        .await;

        // corrupt all dealings of one address
        dealings_db
            .write()
            .unwrap()
            .entry(TEST_VALIDATORS_ADDRESS[0].to_string())
            .and_modify(|dealings| {
                dealings.iter_mut().for_each(|dealing| {
                    dealing.0.pop();
                });
            });

        for (dkg_client, state) in clients_and_states.iter_mut().skip(1) {
            let filtered = deterministic_filter_dealers(dkg_client, state, 2)
                .await
                .unwrap();
            assert_eq!(filtered.len(), TOTAL_DEALINGS);
            for mapping in filtered.iter() {
                assert_eq!(mapping.len(), 2);
            }
            let corrupted_status = state
                .all_dealers()
                .get(&Addr::unchecked(TEST_VALIDATORS_ADDRESS[0]))
                .unwrap()
                .as_ref()
                .unwrap_err();
            assert_eq!(*corrupted_status, ComplaintReason::MissingDealing);
        }
    }

    #[tokio::test]
    async fn check_dealers_filter_malformed_dealing() {
        let dealer_details_db = Arc::new(RwLock::new(HashMap::new()));
        let dealings_db = Arc::new(RwLock::new(HashMap::new()));
        let proposal_db = Arc::new(RwLock::new(HashMap::new()));
        let verification_share_db = Arc::new(RwLock::new(HashMap::new()));
        let mut clients_and_states = prepare_clients_and_states(
            &dealer_details_db,
            &dealings_db,
            &proposal_db,
            &verification_share_db,
        )
        .await;

        // corrupt just one dealing
        dealings_db
            .write()
            .unwrap()
            .entry(TEST_VALIDATORS_ADDRESS[0].to_string())
            .and_modify(|dealings| {
                let mut last = dealings.pop().unwrap();
                last.0.pop();
                dealings.push(last);
            });

        for (dkg_client, state) in clients_and_states.iter_mut().skip(1) {
            deterministic_filter_dealers(dkg_client, state, 2)
                .await
                .unwrap();
            // second filter will leave behind the bad dealer and surface why it was left out
            // in the first place
            let filtered = deterministic_filter_dealers(dkg_client, state, 2)
                .await
                .unwrap();
            assert_eq!(filtered.len(), TOTAL_DEALINGS);
            let corrupted_status = state
                .all_dealers()
                .get(&Addr::unchecked(TEST_VALIDATORS_ADDRESS[0]))
                .unwrap()
                .as_ref()
                .unwrap_err();
            assert_eq!(*corrupted_status, ComplaintReason::MalformedDealing);
        }
    }

    #[tokio::test]
    async fn check_dealers_filter_dealing_verification_error() {
        let dealer_details_db = Arc::new(RwLock::new(HashMap::new()));
        let dealings_db = Arc::new(RwLock::new(HashMap::new()));
        let proposal_db = Arc::new(RwLock::new(HashMap::new()));
        let verification_share_db = Arc::new(RwLock::new(HashMap::new()));
        let mut clients_and_states = prepare_clients_and_states(
            &dealer_details_db,
            &dealings_db,
            &proposal_db,
            &verification_share_db,
        )
        .await;

        // corrupt just one dealing
        dealings_db
            .write()
            .unwrap()
            .entry(TEST_VALIDATORS_ADDRESS[0].to_string())
            .and_modify(|dealings| {
                let mut last = dealings.pop().unwrap();
                last.0.pop();
                last.0.push(42);
                dealings.push(last);
            });

        for (dkg_client, state) in clients_and_states.iter_mut().skip(1) {
            deterministic_filter_dealers(dkg_client, state, 2)
                .await
                .unwrap();
            // second filter will leave behind the bad dealer and surface why it was left out
            // in the first place
            let filtered = deterministic_filter_dealers(dkg_client, state, 2)
                .await
                .unwrap();
            assert_eq!(filtered.len(), TOTAL_DEALINGS);
            let corrupted_status = state
                .all_dealers()
                .get(&Addr::unchecked(TEST_VALIDATORS_ADDRESS[0]))
                .unwrap()
                .as_ref()
                .unwrap_err();
            assert_eq!(*corrupted_status, ComplaintReason::DealingVerificationError);
        }
    }

    #[tokio::test]
    async fn partial_keypair_derivation() {
        let dealer_details_db = Arc::new(RwLock::new(HashMap::new()));
        let dealings_db = Arc::new(RwLock::new(HashMap::new()));
        let proposal_db = Arc::new(RwLock::new(HashMap::new()));
        let verification_share_db = Arc::new(RwLock::new(HashMap::new()));
        let mut clients_and_states = prepare_clients_and_states(
            &dealer_details_db,
            &dealings_db,
            &proposal_db,
            &verification_share_db,
        )
        .await;
        for (dkg_client, state) in clients_and_states.iter_mut() {
            let filtered = deterministic_filter_dealers(dkg_client, state, 2)
                .await
                .unwrap();
            assert!(derive_partial_keypair(state, 2, filtered).is_ok());
        }
    }

    #[tokio::test]
    async fn partial_keypair_derivation_with_threshold() {
        let dealer_details_db = Arc::new(RwLock::new(HashMap::new()));
        let dealings_db = Arc::new(RwLock::new(HashMap::new()));
        let proposal_db = Arc::new(RwLock::new(HashMap::new()));
        let verification_share_db = Arc::new(RwLock::new(HashMap::new()));
        let mut clients_and_states = prepare_clients_and_states(
            &dealer_details_db,
            &dealings_db,
            &proposal_db,
            &verification_share_db,
        )
        .await;

        // corrupt just one dealing
        dealings_db
            .write()
            .unwrap()
            .entry(TEST_VALIDATORS_ADDRESS[0].to_string())
            .and_modify(|dealings| {
                let mut last = dealings.pop().unwrap();
                last.0.pop();
                dealings.push(last);
            });

        for (dkg_client, state) in clients_and_states.iter_mut().skip(1) {
            let filtered = deterministic_filter_dealers(dkg_client, state, 2)
                .await
                .unwrap();
            assert!(derive_partial_keypair(state, 2, filtered).is_ok());
        }
    }

    #[tokio::test]
    async fn submit_verification_key() {
        let dealer_details_db = Arc::new(RwLock::new(HashMap::new()));
        let dealings_db = Arc::new(RwLock::new(HashMap::new()));
        let proposal_db = Arc::new(RwLock::new(HashMap::new()));
        let verification_share_db = Arc::new(RwLock::new(HashMap::new()));
        let mut clients_and_states = prepare_clients_and_states_with_submission(
            &dealer_details_db,
            &dealings_db,
            &proposal_db,
            &verification_share_db,
        )
        .await;

        for (_, state) in clients_and_states.iter_mut() {
            assert!(proposal_db
                .read()
                .unwrap()
                .contains_key(&state.proposal_id_value().unwrap()));
            assert!(state.coconut_keypair_is_some().await);
        }
    }

    #[tokio::test]
    async fn validate_verification_key() {
        let dealer_details_db = Arc::new(RwLock::new(HashMap::new()));
        let dealings_db = Arc::new(RwLock::new(HashMap::new()));
        let proposal_db = Arc::new(RwLock::new(HashMap::new()));
        let verification_share_db = Arc::new(RwLock::new(HashMap::new()));
        let mut clients_and_states = prepare_clients_and_states_with_validation(
            &dealer_details_db,
            &dealings_db,
            &proposal_db,
            &verification_share_db,
        )
        .await;
        for (_, state) in clients_and_states.iter_mut() {
            let proposal = proposal_db
                .read()
                .unwrap()
                .get(&state.proposal_id_value().unwrap())
                .unwrap()
                .clone();
            assert_eq!(proposal.status, Status::Passed);
        }
    }

    #[tokio::test]
    async fn validate_verification_key_malformed_share() {
        let dealer_details_db = Arc::new(RwLock::new(HashMap::new()));
        let dealings_db = Arc::new(RwLock::new(HashMap::new()));
        let proposal_db = Arc::new(RwLock::new(HashMap::new()));
        let verification_share_db = Arc::new(RwLock::new(HashMap::new()));
        let mut clients_and_states = prepare_clients_and_states_with_submission(
            &dealer_details_db,
            &dealings_db,
            &proposal_db,
            &verification_share_db,
        )
        .await;

        verification_share_db
            .write()
            .unwrap()
            .entry(TEST_VALIDATORS_ADDRESS[0].to_string())
            .and_modify(|share| share.share.push('x'));

        for (dkg_client, state) in clients_and_states.iter_mut() {
            verification_key_validation(dkg_client, state)
                .await
                .unwrap();
        }

        for (idx, (_, state)) in clients_and_states.iter().enumerate() {
            let proposal = proposal_db
                .read()
                .unwrap()
                .get(&state.proposal_id_value().unwrap())
                .unwrap()
                .clone();
            if idx == 0 {
                assert_eq!(proposal.status, Status::Rejected);
            } else {
                assert_eq!(proposal.status, Status::Passed);
            }
        }
    }

    #[tokio::test]
    async fn validate_verification_key_unpaired_share() {
        let dealer_details_db = Arc::new(RwLock::new(HashMap::new()));
        let dealings_db = Arc::new(RwLock::new(HashMap::new()));
        let proposal_db = Arc::new(RwLock::new(HashMap::new()));
        let verification_share_db = Arc::new(RwLock::new(HashMap::new()));
        let mut clients_and_states = prepare_clients_and_states_with_submission(
            &dealer_details_db,
            &dealings_db,
            &proposal_db,
            &verification_share_db,
        )
        .await;

        let second_share = verification_share_db
            .write()
            .unwrap()
            .get(TEST_VALIDATORS_ADDRESS[1])
            .unwrap()
            .share
            .clone();
        verification_share_db
            .write()
            .unwrap()
            .entry(TEST_VALIDATORS_ADDRESS[0].to_string())
            .and_modify(|share| share.share = second_share);

        for (dkg_client, state) in clients_and_states.iter_mut() {
            verification_key_validation(dkg_client, state)
                .await
                .unwrap();
        }

        for (idx, (_, state)) in clients_and_states.iter().enumerate() {
            let proposal = proposal_db
                .read()
                .unwrap()
                .get(&state.proposal_id_value().unwrap())
                .unwrap()
                .clone();
            if idx == 0 {
                assert_eq!(proposal.status, Status::Rejected);
            } else {
                assert_eq!(proposal.status, Status::Passed);
            }
        }
    }

    #[tokio::test]
    async fn finalize_verification_key() {
        let dealer_details_db = Arc::new(RwLock::new(HashMap::new()));
        let dealings_db = Arc::new(RwLock::new(HashMap::new()));
        let proposal_db = Arc::new(RwLock::new(HashMap::new()));
        let verification_share_db = Arc::new(RwLock::new(HashMap::new()));
        let clients_and_states = prepare_clients_and_states_with_finalization(
            &dealer_details_db,
            &dealings_db,
            &proposal_db,
            &verification_share_db,
        )
        .await;

        for (_, state) in clients_and_states.iter() {
            let proposal = proposal_db
                .read()
                .unwrap()
                .get(&state.proposal_id_value().unwrap())
                .unwrap()
                .clone();
            assert_eq!(proposal.status, Status::Executed);
        }
    }
}