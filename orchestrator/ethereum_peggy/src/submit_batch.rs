use crate::utils::get_tx_batch_nonce;
use clarity::{Address as EthAddress, Transaction};
use clarity::PrivateKey as EthPrivateKey;
use num256::Uint256;
use peggy_utils::error::PeggyError;
use peggy_utils::types::*;
use std::time::Duration;
use web30::client::Web3;
use web30::types::{SendTxOption, TransactionRequest};
use clarity::utils::bytes_to_hex_str;

/// this function generates an appropriate Ethereum transaction
/// to submit the provided transaction batch and validator set update.
pub async fn send_eth_transaction_batch(
    current_valset: Valset,
    batch: TransactionBatch,
    confirms: &[BatchConfirmResponse],
    web3: &Web3,
    timeout: Duration,
    peggy_contract_address: EthAddress,
    our_eth_key: EthPrivateKey,
    nonce: Uint256,
) -> Result<(), PeggyError> {
    let (current_addresses, current_powers) = current_valset.filter_empty_addresses();
    let current_valset_nonce = current_valset.nonce;
    let new_batch_nonce = batch.nonce;
    //assert!(new_valset_nonce > old_valset_nonce);
    let eth_address = our_eth_key.to_public_key().unwrap();
    info!(
        "Ordering signatures and submitting TransacqtionBatch {}:{} to Ethereum",
        batch.token_contract, new_batch_nonce
    );
    trace!("Batch {:?}", batch);

    let sig_data = current_valset.order_batch_sigs(confirms)?;
    let sig_arrays = to_arrays(sig_data);
    let (amounts, destinations) = batch.get_checkpoint_values();

    // Solidity function signature
    // function submitBatch(
    // // The validators that approve the batch and new valset
    // address[] memory _currentValidators,
    // uint256[] memory _currentPowers,
    // uint256 _currentValsetNonce,
    // // These are arrays of the parts of the validators signatures
    // uint8[] memory _v,
    // bytes32[] memory _r,
    // bytes32[] memory _s,
    // // The batch of transactions
    // uint256[] memory _amounts,
    // address[] memory _destinations,
    // uint256 _batchNonce,
    // address _tokenContract
    let tokens = &[
        current_addresses.into(),
        current_powers.into(),
        current_valset_nonce.into(),
        sig_arrays.v,
        sig_arrays.r,
        sig_arrays.s,
        amounts,
        destinations,
        new_batch_nonce.clone().into(),
        batch.token_contract.into(),
    ];
    let payload = clarity::abi::encode_call("submitBatch(address[],uint256[],uint256,uint8[],bytes32[],bytes32[],uint256[],address[],uint256,address)",
    tokens).unwrap();
    trace!("Tokens {:?}", tokens);

    let before_nonce = get_tx_batch_nonce(
        peggy_contract_address,
        batch.token_contract,
        eth_address,
        &web3,
    )
    .await?;
    if before_nonce >= new_batch_nonce {
        info!(
            "Someone else updated the batch to {}, exiting early",
            before_nonce
        );
        return Ok(());
    }

    info!("Sending ethereum tx");

    let transaction = Transaction {
        to: peggy_contract_address,
        nonce: nonce.clone(),
        gas_price: web3.eth_gas_price().await?,
        gas_limit: 1_000_000u32.into(),
        value: 0u32.into(),
        data: payload.clone(),
        signature: None,
    };

    info!("tx: {}", bytes_to_hex_str(&transaction.sign(&our_eth_key, Some(web3.net_version().await?)).to_bytes().unwrap()));

    let estimate_result = web3.eth_estimate_gas(TransactionRequest {
        from: Some(eth_address),
        to: transaction.to,
        nonce: None,
        gas_price: None,
        gas: None,
        value: Some(0u64.into()),
        data: Some(payload.clone().into()),
    }).await;

    match estimate_result {
        Ok(gas) => {
            if gas.gt(&1_000_000u64.into()) {
                error!("Error while sending tx: gas limit is too high, possibly trying to send failing tx {}", gas);
            }
        }
        Err(e) => {
            error!("Error while sending tx: {}", e);
        }
    }

    let tx_result = web3
        .send_transaction(
            peggy_contract_address,
            payload,
            0u32.into(),
            eth_address,
            our_eth_key,
            vec![
                SendTxOption::GasLimit(1_000_000u32.into()),
                SendTxOption::Nonce(nonce),
            ],
        )
        .await;

    let tx = match tx_result {
        Ok(t) => t,
        Err(e) => {
            error!("Error while sending tx: {}", e);

            return Ok(());
        }
    };

    info!("Sent batch update with txid {:#066x}", tx);

    // TODO this segment of code works around the race condition for submitting batches mostly
    // by not caring if our own submission reverts and only checking if the valset has been updated
    // period not if our update succeeded in particular. This will require some further consideration
    // in the future as many independent relayers racing to update the same thing will hopefully
    // be the common case.
    web3.wait_for_transaction(tx, timeout, None).await?;

    let last_nonce = get_tx_batch_nonce(
        peggy_contract_address,
        batch.token_contract,
        eth_address,
        &web3,
    )
    .await?;
    if last_nonce != new_batch_nonce {
        error!(
            "Current nonce is {} expected to update to nonce {}",
            last_nonce, new_batch_nonce
        );
    } else {
        info!("Successfully updated Batch with new Nonce {:?}", last_nonce);
    }
    Ok(())
}
