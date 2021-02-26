use std::{
    convert::TryInto,
    ops::{AddAssign, Sub},
};

use chrono::{DateTime, Utc};
use num::{BigUint, FromPrimitive};
use tokio::time;

use zksync_config::ZkSyncConfig;
use zksync_storage::{
    chain::operations_ext::records::TxReceiptResponse, ConnectionPool, StorageProcessor,
};
use zksync_types::{
    forced_exit_requests::{ForcedExitRequest, ForcedExitRequestId},
    tx::TimeRange,
    tx::TxHash,
    AccountId, Address, Nonce, TokenId, ZkSyncTx,
};

use zksync_api::core_api_client::CoreApiClient;
use zksync_types::ForcedExit;
use zksync_types::SignedZkSyncTx;

use super::utils::{Engine, PrivateKey};
use crate::utils::read_signing_key;

// We try to process a request 3 times before sending warnings in the console
const PROCESSING_ATTEMPTS: u8 = 3;

pub struct ForcedExitSender {
    core_api_client: CoreApiClient,
    connection_pool: ConnectionPool,
    config: ZkSyncConfig,
    forced_exit_sender_account_id: AccountId,
    sender_private_key: PrivateKey<Engine>,
}

async fn get_forced_exit_sender_account_id(
    connection_pool: ConnectionPool,
    config: &ZkSyncConfig,
) -> anyhow::Result<AccountId> {
    let mut storage = connection_pool.access_storage().await?;
    let mut accounts_schema = storage.chain().account_schema();

    let account_id = accounts_schema
        .account_id_by_address(config.forced_exit_requests.sender_account_address)
        .await?;

    account_id.ok_or_else(|| anyhow::Error::msg("Failed to get the forced_exit_sender account id"))
}

impl ForcedExitSender {
    pub async fn new(
        core_api_client: CoreApiClient,
        connection_pool: ConnectionPool,
        config: ZkSyncConfig,
    ) -> Self {
        let forced_exit_sender_account_id =
            get_forced_exit_sender_account_id(connection_pool.clone(), &config)
                .await
                .expect("Failed to get the sender id");

        let sender_private_key =
            hex::decode(&config.clone().forced_exit_requests.sender_private_key[2..])
                .expect("Decoding private key failed");
        let sender_private_key =
            read_signing_key(&sender_private_key).expect("Reading private key failed");

        Self {
            core_api_client,
            connection_pool,
            forced_exit_sender_account_id,
            config,
            sender_private_key,
        }
    }

    pub fn extract_id_from_amount(&self, amount: BigUint) -> (i64, BigUint) {
        let id_space_size: i64 = 10_i64.pow(self.config.forced_exit_requests.digits_in_id as u32);

        let id_space_size = BigUint::from_i64(id_space_size).unwrap();

        // Taking to the power of 1 and finding mod
        // is the only way to find mod of BigUint
        let one = BigUint::from_u8(1u8).unwrap();
        let id = amount.modpow(&one, &id_space_size);

        // After extracting the id we need to delete it
        // to make sure that amount is the same as in the db
        let amount = amount.sub(&id);

        (id.try_into().unwrap(), amount)
    }

    pub fn build_forced_exit(
        &self,
        nonce: Nonce,
        target: Address,
        token: TokenId,
    ) -> SignedZkSyncTx {
        let tx = ForcedExit::new_signed(
            self.forced_exit_sender_account_id,
            target,
            token,
            BigUint::from(0u32),
            nonce,
            TimeRange::default(),
            &self.sender_private_key,
        )
        .expect("Failed to create signed ForcedExit transaction");

        SignedZkSyncTx {
            tx: ZkSyncTx::ForcedExit(Box::new(tx)),
            eth_sign_data: None,
        }
    }

    pub async fn build_transactions(
        &self,
        storage: &mut StorageProcessor<'_>,
        fe_request: ForcedExitRequest,
    ) -> anyhow::Result<Vec<SignedZkSyncTx>> {
        let mut account_schema = storage.chain().account_schema();

        let sender_state = account_schema
            .last_committed_state_for_account(self.forced_exit_sender_account_id)
            .await?
            .expect("The forced exit sender account has no committed state");

        let mut sender_nonce = sender_state.nonce;
        let mut transactions: Vec<SignedZkSyncTx> = vec![];

        for token in fe_request.tokens.into_iter() {
            transactions.push(self.build_forced_exit(sender_nonce, fe_request.target, token));
            sender_nonce.add_assign(1);
        }

        Ok(transactions)
    }

    // Returns the id the request if it should be fulfilled,
    // error otherwise
    pub fn check_request(
        &self,
        amount: BigUint,
        submission_time: DateTime<Utc>,
        request: Option<ForcedExitRequest>,
    ) -> bool {
        let request = match request {
            Some(r) => r,
            None => {
                // The request does not exit, we should not process it
                return false;
            }
        };

        if request.fulfilled_at.is_some() {
            // We should not re-process requests that were fulfilled before
            return false;
        }

        request.valid_until > submission_time && request.price_in_wei == amount
    }

    // Awaits until the request is complete
    pub async fn await_unconfirmed_request(
        &self,
        storage: &mut StorageProcessor<'_>,
        request: &ForcedExitRequest,
    ) -> anyhow::Result<()> {
        let hashes = request.fulfilled_by.clone();

        if let Some(hashes) = hashes {
            for hash in hashes.into_iter() {
                self.wait_until_comitted(storage, hash).await?;
                self.set_fulfilled_at(storage, request.id).await?;
            }
        }
        Ok(())
    }

    pub async fn get_unconfirmed_requests(
        &self,
        storage: &mut StorageProcessor<'_>,
    ) -> anyhow::Result<Vec<ForcedExitRequest>> {
        let mut forced_exit_requests_schema = storage.forced_exit_requests_schema();
        forced_exit_requests_schema.get_unconfirmed_requests().await
    }

    pub async fn set_fulfilled_by(
        &self,
        storage: &mut StorageProcessor<'_>,
        id: ForcedExitRequestId,
        value: Option<Vec<TxHash>>,
    ) -> anyhow::Result<()> {
        let mut forced_exit_requests_schema = storage.forced_exit_requests_schema();
        forced_exit_requests_schema
            .set_fulfilled_by(id, value)
            .await
    }

    pub async fn await_unconfirmed(&mut self) -> anyhow::Result<()> {
        let mut storage = self.connection_pool.access_storage().await?;
        let unfullied_requests = self.get_unconfirmed_requests(&mut storage).await?;

        for request in unfullied_requests.into_iter() {
            let await_result = self.await_unconfirmed_request(&mut storage, &request).await;

            if await_result.is_err() {
                // A transaction has failed. That is not intended.
                // We can safely cancel such transaction, since we will re-try to
                // send it again later
                vlog::error!(
                    "A previously sent forced exit transaction has failed. Canceling the tx."
                );
                self.set_fulfilled_by(&mut storage, request.id, None)
                    .await?;
            }
        }

        Ok(())
    }

    pub async fn get_request_by_id(
        &self,
        storage: &mut StorageProcessor<'_>,
        id: i64,
    ) -> anyhow::Result<Option<ForcedExitRequest>> {
        let mut fe_schema = storage.forced_exit_requests_schema();

        let request = fe_schema.get_request_by_id(id).await?;
        Ok(request)
    }

    pub async fn set_fulfilled_at(
        &self,
        storage: &mut StorageProcessor<'_>,
        id: i64,
    ) -> anyhow::Result<()> {
        let mut fe_schema = storage.forced_exit_requests_schema();

        fe_schema.set_fulfilled_at(id, Utc::now()).await?;

        vlog::info!("FE request with id {} was fulfilled", id);

        Ok(())
    }

    pub async fn get_receipt(
        &self,
        storage: &mut StorageProcessor<'_>,
        tx_hash: TxHash,
    ) -> anyhow::Result<Option<TxReceiptResponse>> {
        storage
            .chain()
            .operations_ext_schema()
            .tx_receipt(tx_hash.as_ref())
            .await
    }

    pub async fn send_transactions(
        &self,
        storage: &mut StorageProcessor<'_>,
        request: &ForcedExitRequest,
        txs: Vec<SignedZkSyncTx>,
    ) -> anyhow::Result<Vec<TxHash>> {
        let mut schema = storage.forced_exit_requests_schema();

        let hashes: Vec<TxHash> = txs.iter().map(|tx| tx.hash()).collect();
        self.core_api_client.send_txs_batch(txs, vec![]).await??;

        schema
            .set_fulfilled_by(request.id, Some(hashes.clone()))
            .await?;

        Ok(hashes)
    }

    pub async fn wait_until_comitted(
        &self,
        storage: &mut StorageProcessor<'_>,
        tx_hash: TxHash,
    ) -> anyhow::Result<()> {
        let timeout_millis: u64 = 120000;
        let poll_interval_millis: u64 = 200;
        let poll_interval = time::Duration::from_secs(poll_interval_millis);
        let mut timer = time::interval(poll_interval);

        let mut time_passed: u64 = 0;

        loop {
            if time_passed >= timeout_millis {
                // If a transaction takes more than 2 minutes to commit we consider the server
                // broken and panic
                panic!("Comitting ForcedExit transaction failed!");
            }

            let receipt = self.get_receipt(storage, tx_hash).await?;

            if let Some(tx_receipt) = receipt {
                if tx_receipt.success {
                    return Ok(());
                } else {
                    return Err(anyhow::Error::msg("ForcedExit transaction failed"));
                }
            }

            timer.tick().await;
            time_passed += poll_interval_millis;
        }
    }

    pub async fn try_process_request(
        &self,
        amount: BigUint,
        submission_time: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let (id, amount) = self.extract_id_from_amount(amount);

        let mut storage = self.connection_pool.access_storage().await?;

        let fe_request = self.get_request_by_id(&mut storage, id).await?;

        let fe_request = if self.check_request(amount, submission_time, fe_request.clone()) {
            // The self.check_request already checked that the fe_request is Some(_)
            fe_request.unwrap()
        } else {
            // The request was not valid, that's fine
            return Ok(());
        };

        let txs = self
            .build_transactions(&mut storage, fe_request.clone())
            .await?;
        let hashes = self
            .send_transactions(&mut storage, &fe_request, txs)
            .await?;

        // We wait only for the first transaction to complete since the transactions
        // are sent in a batch
        self.wait_until_comitted(&mut storage, hashes[0]).await?;
        self.set_fulfilled_at(&mut storage, id).await?;

        Ok(())
    }

    pub async fn process_request(&self, amount: BigUint, submission_time: DateTime<Utc>) {
        let mut attempts: u8 = 0;
        // Typically this should not run any longer than 1 iteration
        // In case something bad happens we do not want the server crush because
        // of the forced_exit_requests component
        loop {
            let processing_attempt = self
                .try_process_request(amount.clone(), submission_time)
                .await;

            if processing_attempt.is_ok() {
                return;
            } else {
                attempts += 1;
            }

            if attempts >= PROCESSING_ATTEMPTS {
                vlog::error!("Failed to process forced exit for the {} time", attempts);
            }
        }
    }
}
