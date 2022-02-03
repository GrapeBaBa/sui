// Copyright (c) Facebook, Inc. and its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crate::{authority_client::AuthorityAPI, downloader::*};
use async_trait::async_trait;
use fastx_framework::build_move_package_to_bytes;
use fastx_types::object::Object;
use fastx_types::{
    base_types::*, committee::Committee, error::FastPayError, fp_ensure, messages::*,
};
use futures::{future, StreamExt, TryFutureExt};
use itertools::Itertools;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use rand::seq::SliceRandom;
use typed_store::rocks::open_cf;
use typed_store::Map;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::timeout;

mod client_store;
use self::client_store::ClientStore;
const OBJECT_DOWNLOAD_CHANNEL_BOUND: usize = 1024;

#[cfg(test)]
use fastx_types::FASTX_FRAMEWORK_ADDRESS;

#[cfg(test)]
#[path = "unit_tests/client_tests.rs"]
mod client_tests;

// TODO: Make timeout duration configurable.
const AUTHORITY_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub type AsyncResult<'a, T, E> = future::BoxFuture<'a, Result<T, E>>;

pub struct ClientState<AuthorityAPI> {
    /// Our FastPay address.
    address: FastPayAddress,
    /// Our signature key.
    secret: KeyPair,
    /// Our FastPay committee.
    committee: Committee,
    /// How to talk to this committee.
    authority_clients: BTreeMap<AuthorityName, AuthorityAPI>,
    /// Persistent store for client
    store: ClientStore,
}

// Operations are considered successful when they successfully reach a quorum of authorities.
#[async_trait]
pub trait Client {
    /// Send object to a FastX account.
    async fn transfer_object(
        &mut self,
        object_id: ObjectID,
        gas_payment: ObjectID,
        recipient: FastPayAddress,
    ) -> Result<CertifiedOrder, anyhow::Error>;

    /// Receive object from FastX.
    async fn receive_object(&mut self, certificate: &CertifiedOrder) -> Result<(), anyhow::Error>;

    /// Send object to a FastX account.
    /// Do not confirm the transaction, however this locks the objects until confirmation
    async fn transfer_to_fastx_unsafe_unconfirmed(
        &mut self,
        object_id: ObjectID,
        gas_payment: ObjectID,
        recipient: FastPayAddress,
    ) -> Result<CertifiedOrder, anyhow::Error>;

    /// Try to complete all pending orders once. Return if any fails
    async fn try_complete_pending_orders(&mut self) -> Result<(), FastPayError>;

    /// Synchronise client state with a random authorities, updates all object_ids and certificates, request only goes out to one authority.
    /// this method doesn't guarantee data correctness, client will have to handle potential byzantine authority
    async fn sync_client_state_with_random_authority(
        &mut self,
    ) -> Result<AuthorityName, anyhow::Error>;

    /// Call move functions in the module in the given package, with args supplied
    async fn move_call(
        &mut self,
        package_object_ref: ObjectRef,
        module: Identifier,
        function: Identifier,
        type_arguments: Vec<TypeTag>,
        gas_object_ref: ObjectRef,
        object_arguments: Vec<ObjectRef>,
        pure_arguments: Vec<Vec<u8>>,
        gas_budget: u64,
    ) -> Result<(CertifiedOrder, OrderEffects), anyhow::Error>;

    /// Publish Move modules
    async fn publish(
        &mut self,
        package_source_files_path: String,
        gas_object_ref: ObjectRef,
    ) -> Result<(CertifiedOrder, OrderEffects), anyhow::Error>;

    /// Get the object information
    async fn get_object_info(
        &mut self,
        object_info_req: ObjectInfoRequest,
    ) -> Result<ObjectInfoResponse, anyhow::Error>;

    /// Get all object we own.
    async fn get_owned_objects(&self) -> Vec<ObjectID>;

    async fn download_owned_objects_from_all_authorities(
        &self,
    ) -> Result<BTreeSet<ObjectRef>, FastPayError>;
}

impl<A> ClientState<A> {
    /// It is recommended that one call sync and download_owned_objects_from_all_authorities
    /// right after constructor to fetch missing info form authorities
    /// TODO: client should manage multiple addresses instead of each addr having DBs
    /// https://github.com/MystenLabs/fastnft/issues/332
    pub fn new(
        path: PathBuf,
        address: FastPayAddress,
        secret: KeyPair,
        committee: Committee,
        authority_clients: BTreeMap<AuthorityName, A>,
        certificates: BTreeMap<TransactionDigest, CertifiedOrder>,
        object_refs: BTreeMap<ObjectID, ObjectRef>,
    ) -> Result<Self, FastPayError> {
        let client_state = ClientState {
            address,
            secret,
            committee,
            authority_clients,
            store: ClientStore::new(path),
        };

        // Backfill the DB
        client_state.store.populate(object_refs, certificates)?;
        Ok(client_state)
    }

    pub fn address(&self) -> FastPayAddress {
        self.address
    }

    pub fn next_sequence_number(
        &self,
        object_id: &ObjectID,
    ) -> Result<SequenceNumber, FastPayError> {
        if self.store.object_sequence_numbers.contains_key(object_id)? {
            Ok(self
                .store
                .object_sequence_numbers
                .get(object_id)?
                .expect("Unable to get sequence number"))
        } else {
            Err(FastPayError::ObjectNotFound {
                object_id: *object_id,
            })
        }
    }
    pub fn object_ref(&self, object_id: ObjectID) -> Result<ObjectRef, FastPayError> {
        self.store
            .object_refs
            .get(&object_id)?
            .ok_or(FastPayError::ObjectNotFound { object_id })
    }

    pub fn object_refs(&self) -> BTreeMap<ObjectID, ObjectRef> {
        self.store.object_refs.iter().collect()
    }

    /// Need to remove unwraps. Found this tricky due to iterator requirements of downloader and not being able to exit from closure to top fn
    /// https://github.com/MystenLabs/fastnft/issues/307
    pub fn certificates(&self, object_id: &ObjectID) -> impl Iterator<Item = CertifiedOrder> + '_ {
        self.store
            .object_certs
            .get(object_id)
            .unwrap()
            .into_iter()
            .flat_map(|cert_digests| {
                self.store
                    .certificates
                    .multi_get(&cert_digests[..])
                    .unwrap()
                    .into_iter()
                    .flatten()
            })
    }

    pub fn all_certificates(&self) -> BTreeMap<TransactionDigest, CertifiedOrder> {
        self.store.certificates.iter().collect()
    }

    pub fn insert_object_info(
        &mut self,
        object_ref: &ObjectRef,
        parent_tx_digest: &TransactionDigest,
    ) -> Result<(), FastPayError> {
        let (object_id, seq, _) = object_ref;
        let mut tx_digests = self.store.object_certs.get(object_id)?.unwrap_or_default();
        tx_digests.push(*parent_tx_digest);

        // Multi table atomic insert using batches
        let batch = self
            .store
            .object_sequence_numbers
            .batch()
            .insert_batch(
                &self.store.object_sequence_numbers,
                std::iter::once((object_id, seq)),
            )?
            .insert_batch(
                &self.store.object_certs,
                std::iter::once((object_id, &tx_digests.to_vec())),
            )?
            .insert_batch(
                &self.store.object_refs,
                std::iter::once((object_id, object_ref)),
            )?;
        // Execute atomic write of opers
        batch.write()?;
        Ok(())
    }

    pub fn remove_object_info(&mut self, object_id: &ObjectID) -> Result<(), FastPayError> {
        // Multi table atomic delete using batches
        let batch = self
            .store
            .object_sequence_numbers
            .batch()
            .delete_batch(
                &self.store.object_sequence_numbers,
                std::iter::once(object_id),
            )?
            .delete_batch(&self.store.object_certs, std::iter::once(object_id))?
            .delete_batch(&self.store.object_refs, std::iter::once(object_id))?;
        // Execute atomic write of opers
        batch.write()?;
        Ok(())
    }
}

#[allow(dead_code)]
#[derive(Clone)]
struct CertificateRequester<A> {
    committee: Committee,
    authority_clients: Vec<A>,
    sender: Option<FastPayAddress>,
}

impl<A> CertificateRequester<A> {
    fn new(
        committee: Committee,
        authority_clients: Vec<A>,
        sender: Option<FastPayAddress>,
    ) -> Self {
        Self {
            committee,
            authority_clients,
            sender,
        }
    }
}

#[async_trait]
impl<A> Requester for CertificateRequester<A>
where
    A: AuthorityAPI + Send + Sync + 'static + Clone,
{
    type Key = (ObjectID, SequenceNumber);
    type Value = Result<CertifiedOrder, FastPayError>;

    /// Try to find a certificate for the given sender, object_id and sequence number.
    async fn query(
        &mut self,
        (object_id, sequence_number): (ObjectID, SequenceNumber),
    ) -> Result<CertifiedOrder, FastPayError> {
        // BUG(https://github.com/MystenLabs/fastnft/issues/290): This function assumes that requesting the parent cert of object seq+1 will give the cert of
        //        that creates the object. This is not true, as objects may be deleted and may not have a seq+1
        //        to look up.
        //
        //        The authority `handle_object_info_request` is now fixed to return the parent at seq, and not
        //        seq+1. But a lot of the client code makes the above wrong assumption, and the line above reverts
        //        query to the old (incorrect) behavious to not break tests everywhere.
        let inner_sequence_number = sequence_number.increment();

        let request = ObjectInfoRequest {
            object_id,
            request_sequence_number: Some(inner_sequence_number),
        };
        // Sequentially try each authority in random order.
        // TODO: Improve shuffle, different authorities might different amount of stake.
        self.authority_clients.shuffle(&mut rand::thread_rng());
        for client in self.authority_clients.iter_mut() {
            let result = client.handle_object_info_request(request.clone()).await;
            if let Ok(response) = result {
                let certificate = response
                    .parent_certificate
                    .expect("Unable to get certificate");
                if certificate.check(&self.committee).is_ok() {
                    // BUG (https://github.com/MystenLabs/fastnft/issues/290): Orders do not have a sequence number any more, objects do.
                    /*
                    let order = &certificate.order;
                    if let Some(sender) = self.sender {

                        if order.sender() == &sender && order.sequence_number() == inner_sequence_number {
                            return Ok(certificate.clone());
                        }
                    } else {
                        return Ok(certificate.clone());
                    }
                    */
                    return Ok(certificate);
                }
            }
        }
        Err(FastPayError::ErrorWhileRequestingCertificate)
    }
}

impl<A> ClientState<A>
where
    A: AuthorityAPI + Send + Sync + 'static + Clone,
{
    /// Sync a certificate and all its dependencies to a destination authority, using a
    /// source authority to get information about parent certificates.
    ///
    /// Note: Both source and destination may be byzantine, therefore one should always
    /// time limit the call to this function to avoid byzantine authorities consuming
    /// an unbounded amount of resources.
    async fn sync_authority_source_to_destination(
        &self,
        cert: ConfirmationOrder,
        source_authority: AuthorityName,
        destination_authority: AuthorityName,
    ) -> Result<(), FastPayError> {
        let mut source_client = self.authority_clients[&source_authority].clone();
        let mut destination_client = self.authority_clients[&destination_authority].clone();

        // This represents a stack of certificates that we need to register with the
        // destination authority. The stack is a LIFO queue, and therefore later insertions
        // represent certificates that earlier insertions depend on. Thus updating an
        // authority in the order we pop() the certificates from this stack should ensure
        // certificates are uploaded in causal order.
        let digest = cert.certificate.order.digest();
        let mut missing_certificates: Vec<_> = vec![cert.clone()];

        // We keep a list of certificates already processed to avoid duplicates
        let mut candidate_certificates: HashSet<TransactionDigest> =
            vec![digest].into_iter().collect();
        let mut attempted_certificates: HashSet<TransactionDigest> = HashSet::new();

        while let Some(target_cert) = missing_certificates.pop() {
            match destination_client
                .handle_confirmation_order(target_cert.clone())
                .await
            {
                Ok(_) => continue,
                Err(FastPayError::LockErrors { .. }) => {}
                Err(e) => return Err(e),
            }

            // If we are here it means that the destination authority is missing
            // the previous certificates, so we need to read them from the source
            // authority.

            // The first time we cannot find the cert from the destination authority
            // we try to get its dependencies. But the second time we have already tried
            // to update its dependencies, so we should just admit failure.
            let cert_digest = target_cert.certificate.order.digest();
            if attempted_certificates.contains(&cert_digest) {
                return Err(FastPayError::AuthorityInformationUnavailable);
            }
            attempted_certificates.insert(cert_digest);

            // TODO: Eventually the client will store more information, and we could
            // first try to read certificates and parents from a local cache before
            // asking an authority.
            // let input_objects = target_cert.certificate.order.input_objects();

            let order_info = if missing_certificates.is_empty() {
                // Here we cover a corner case due to the nature of using consistent
                // broadcast: it is possible for the client to have a certificate
                // signed by some authority, before the authority has processed the
                // certificate. This can only happen to a certificate for objects
                // not used in another certificicate, hence it can only be the case
                // for the very first certificate we try to sync. For this reason for
                // this one instead of asking for the effects of a previous execution
                // we send the cert for execution. Since execution is idempotent this
                // is ok.

                source_client
                    .handle_confirmation_order(target_cert.clone())
                    .await?
            } else {
                // Unlike the previous case if a certificate created an object that
                // was involved in the processing of another certificate the previous
                // cert must have been processed, so here we just ask for the effects
                // of such an execution.

                source_client
                    .handle_order_info_request(OrderInfoRequest {
                        transaction_digest: cert_digest,
                    })
                    .await?
            };

            // Put back the target cert
            missing_certificates.push(target_cert);
            let signed_effects = &order_info
                .signed_effects
                .ok_or(FastPayError::AuthorityInformationUnavailable)?;

            for returned_digest in &signed_effects.effects.dependencies {
                // We check that we are not processing twice the same certificate, as
                // it would be common if two objects used by one order, were also both
                // mutated by the same preceeding order.
                if !candidate_certificates.contains(returned_digest) {
                    // Add this cert to the set we have processed
                    candidate_certificates.insert(*returned_digest);

                    let inner_order_info = source_client
                        .handle_order_info_request(OrderInfoRequest {
                            transaction_digest: *returned_digest,
                        })
                        .await?;

                    let returned_certificate = inner_order_info
                        .certified_order
                        .ok_or(FastPayError::AuthorityInformationUnavailable)?;

                    // Check & Add it to the list of certificates to sync
                    returned_certificate.check(&self.committee).map_err(|_| {
                        FastPayError::ByzantineAuthoritySuspicion {
                            authority: source_authority,
                        }
                    })?;
                    missing_certificates.push(ConfirmationOrder::new(returned_certificate));
                }
            }
        }

        Ok(())
    }

    /// Sync a certificate to an authority.
    ///
    /// This function infers which authorities have the history related to
    /// a certificate and attempts `retries` number of them, sampled accoding to
    /// stake, in order to bring the destination authority up to date to accept
    /// the certificate. The time devoted to each attempt is bounded by
    /// `timeout_milliseconds`.
    pub async fn sync_certificate_to_authority_with_timeout(
        &self,
        cert: ConfirmationOrder,
        destination_authority: AuthorityName,
        timeout_milliseconds: u64,
        retries: usize,
    ) -> Result<(), FastPayError> {
        // Extract the set of authorities that should have this certificate
        // and its full history. We should be able to use these are source authorities.
        let mut candidate_source_authorties: HashSet<AuthorityName> = cert
            .certificate
            .signatures
            .iter()
            .map(|(name, _)| *name)
            .collect();

        // Sample a `retries` number of distinct authorities by stake.
        let mut source_authorities: Vec<AuthorityName> = Vec::new();
        while source_authorities.len() < retries && !candidate_source_authorties.is_empty() {
            // Here we do rejection sampling.
            //
            // TODO: add a filter parameter to sample, so that we can directly
            //       sample from a subset which is more efficient.
            let sample_authority = self.committee.sample();
            if candidate_source_authorties.contains(sample_authority) {
                candidate_source_authorties.remove(sample_authority);
                source_authorities.push(*sample_authority);
            }
        }

        // Now try to update the destination authority sequentially using
        // the source authorities we have sampled.
        for source_authority in source_authorities {
            // Note: here we could improve this function by passing into the
            //       `sync_authority_source_to_destination` call a cache of
            //       certificates and parents to avoid re-downloading them.
            if timeout(
                Duration::from_millis(timeout_milliseconds),
                self.sync_authority_source_to_destination(
                    cert.clone(),
                    source_authority,
                    destination_authority,
                ),
            )
            .await
            .is_ok()
            {
                // If the updates suceeds we return, since there is no need
                // to try other sources.
                return Ok(());
            }

            // If we are here it means that the update failed, either due to the
            // source being faulty or the destination being faulty.
            //
            // TODO: We should probably be keeping a record of suspected faults
            // upon failure to de-prioritize authorities that we have observed being
            // less reliable.
        }

        // Eventually we should add more information to this error about the destination
        // and maybe event the certificiate.
        Err(FastPayError::AuthorityUpdateFailure)
    }

    #[cfg(test)]
    async fn request_certificate(
        &mut self,
        sender: FastPayAddress,
        object_id: ObjectID,
        sequence_number: SequenceNumber,
    ) -> Result<CertifiedOrder, FastPayError> {
        CertificateRequester::new(
            self.committee.clone(),
            self.authority_clients.values().cloned().collect(),
            Some(sender),
        )
        .query((object_id, sequence_number))
        .await
    }

    /// Find the highest sequence number that is known to a quorum of authorities.
    /// NOTE: This is only reliable in the synchronous model, with a sufficient timeout value.
    #[cfg(test)]
    async fn get_strong_majority_sequence_number(&self, object_id: ObjectID) -> SequenceNumber {
        let request = ObjectInfoRequest {
            object_id,
            request_sequence_number: None,
        };
        let mut authority_clients = self.authority_clients.clone();
        let numbers: futures::stream::FuturesUnordered<_> = authority_clients
            .iter_mut()
            .map(|(name, client)| {
                let fut = client.handle_object_info_request(request.clone());
                async move {
                    match fut.await {
                        Ok(info) => info.object().map(|obj| (*name, obj.version())),
                        _ => None,
                    }
                }
            })
            .collect();
        self.committee.get_strong_majority_lower_bound(
            numbers.filter_map(|x| async move { x }).collect().await,
        )
    }

    /// Return owner address and sequence number of an object backed by a quorum of authorities.
    /// NOTE: This is only reliable in the synchronous model, with a sufficient timeout value.
    #[cfg(test)]
    async fn get_strong_majority_owner(
        &self,
        object_id: ObjectID,
    ) -> Option<(Authenticator, SequenceNumber)> {
        let request = ObjectInfoRequest {
            object_id,
            request_sequence_number: None,
        };
        let authority_clients = self.authority_clients.clone();
        let numbers: futures::stream::FuturesUnordered<_> = authority_clients
            .iter()
            .map(|(name, client)| {
                let fut = client.handle_object_info_request(request.clone());
                async move {
                    match fut.await {
                        Ok(ObjectInfoResponse {
                            object_and_lock: Some(ObjectResponse { object, .. }),
                            ..
                        }) => Some((*name, Some((object.owner, object.version())))),
                        _ => None,
                    }
                }
            })
            .collect();
        self.committee.get_strong_majority_lower_bound(
            numbers.filter_map(|x| async move { x }).collect().await,
        )
    }

    #[cfg(test)]
    async fn get_framework_object_ref(&mut self) -> Result<ObjectRef, anyhow::Error> {
        let info = self
            .get_object_info(ObjectInfoRequest {
                object_id: FASTX_FRAMEWORK_ADDRESS,
                request_sequence_number: None,
            })
            .await?;
        let reference = info
            .object_and_lock
            .ok_or(FastPayError::ObjectNotFound {
                object_id: FASTX_FRAMEWORK_ADDRESS,
            })?
            .object
            .to_object_reference();
        Ok(reference)
    }

    /// Execute a sequence of actions in parallel for a quorum of authorities.
    async fn communicate_with_quorum<'a, V, F>(
        &'a mut self,
        execute: F,
    ) -> Result<Vec<V>, FastPayError>
    where
        F: Fn(AuthorityName, &'a mut A) -> AsyncResult<'a, V, FastPayError> + Clone,
    {
        let committee = &self.committee;
        let authority_clients = &mut self.authority_clients;
        let mut responses: futures::stream::FuturesUnordered<_> = authority_clients
            .iter_mut()
            .map(|(name, client)| {
                let execute = execute.clone();
                async move { (*name, execute(*name, client).await) }
            })
            .collect();

        let mut values = Vec::new();
        let mut value_score = 0;
        let mut error_scores = HashMap::new();
        while let Some((name, result)) = responses.next().await {
            match result {
                Ok(value) => {
                    values.push(value);
                    value_score += committee.weight(&name);
                    if value_score >= committee.quorum_threshold() {
                        // Success!
                        return Ok(values);
                    }
                }
                Err(err) => {
                    let entry = error_scores.entry(err.clone()).or_insert(0);
                    *entry += committee.weight(&name);
                    if *entry >= committee.validity_threshold() {
                        // At least one honest node returned this error.
                        // No quorum can be reached, so return early.
                        return Err(FastPayError::QuorumNotReached {
                            errors: error_scores.into_keys().collect(),
                        });
                    }
                }
            }
        }
        Err(FastPayError::QuorumNotReached {
            errors: error_scores.into_keys().collect(),
        })
    }

    /// Broadcast missing confirmation orders and invoke handle_order on each authority client.
    async fn broadcast_and_handle_order(
        &mut self,
        sender: FastPayAddress,
        order: Order,
    ) -> Result<(Vec<(CertifiedOrder, OrderInfoResponse)>, CertifiedOrder), anyhow::Error> {
        for object_kind in &order.input_objects() {
            let object_id = object_kind.object_id();
            let next_sequence_number = self.next_sequence_number(&object_id).unwrap_or_default();
            fp_ensure!(
                object_kind.version() >= next_sequence_number,
                FastPayError::UnexpectedSequenceNumber {
                    object_id,
                    expected_sequence: next_sequence_number,
                }
                .into()
            );
        }

        let committee = self.committee.clone();
        let (responses, votes) = self
            .broadcast_and_execute(
                sender,
                order.input_objects(),
                Vec::new(),
                |name, authority| {
                    let order = order.clone();
                    let committee = committee.clone();
                    Box::pin(async move {
                        match authority.handle_order(order).await {
                            Ok(OrderInfoResponse {
                                signed_order: Some(inner_signed_order),
                                ..
                            }) => {
                                fp_ensure!(
                                    inner_signed_order.authority == name,
                                    FastPayError::ErrorWhileProcessingTransferOrder
                                );
                                inner_signed_order.check(&committee)?;
                                Ok((inner_signed_order.authority, inner_signed_order.signature))
                            }
                            Err(err) => Err(err),
                            _ => Err(FastPayError::ErrorWhileProcessingTransferOrder),
                        }
                    })
                },
            )
            .await?;
        let certificate = CertifiedOrder {
            order,
            signatures: votes,
        };
        // Certificate is valid because
        // * `communicate_with_quorum` ensured a sufficient "weight" of (non-error) answers were returned by authorities.
        // * each answer is a vote signed by the expected authority.
        Ok((responses, certificate))
    }

    /// Broadcast missing confirmation orders and execute provided authority action on each authority.
    // BUG(https://github.com/MystenLabs/fastnft/issues/290): This logic for
    // updating an authority that is behind is not correct, since we now have
    // potentially many dependencies that need to be satisfied, not just a
    // list.
    async fn broadcast_and_execute<'a, V, F: 'a>(
        &'a mut self,
        sender: FastPayAddress,
        inputs: Vec<InputObjectKind>,
        certificates_to_broadcast: Vec<CertifiedOrder>,
        action: F,
    ) -> Result<(Vec<(CertifiedOrder, OrderInfoResponse)>, Vec<V>), anyhow::Error>
    where
        F: Fn(AuthorityName, &'a mut A) -> AsyncResult<'a, V, FastPayError> + Send + Sync + Copy,
        V: Copy,
    {
        let requester = CertificateRequester::new(
            self.committee.clone(),
            self.authority_clients.values().cloned().collect(),
            Some(sender),
        );

        let known_certificates = inputs.iter().flat_map(|input_kind| {
            self.certificates(&input_kind.object_id())
                .filter_map(move |cert| {
                    if cert.order.sender() == &sender {
                        Some(((input_kind.object_id(), input_kind.version()), Ok(cert)))
                    } else {
                        None
                    }
                })
        });

        let (_, mut handle) = Downloader::start(requester, known_certificates);
        let result = self
            .communicate_with_quorum(|name, client| {
                let certificates_to_broadcast = certificates_to_broadcast.clone();
                let inputs = inputs.clone();
                let mut handle = handle.clone();
                Box::pin(async move {
                    // Sync certificate with authority
                    // Figure out which certificates this authority is missing.
                    let mut responses = Vec::new();
                    let mut missing_certificates = Vec::new();
                    for input_kind in inputs {
                        let object_id = input_kind.object_id();
                        let target_sequence_number = input_kind.version();
                        let request = ObjectInfoRequest {
                            object_id,
                            request_sequence_number: None,
                        };
                        let response = client.handle_object_info_request(request).await?;

                        let current_sequence_number = response
                            .object_and_lock
                            .ok_or(FastPayError::ObjectNotFound { object_id })?
                            .object
                            .version();

                        // Download each missing certificate in reverse order using the downloader.
                        let mut number = target_sequence_number.decrement();
                        while let Ok(seq) = number {
                            if seq < current_sequence_number {
                                break;
                            }
                            let certificate = handle
                                .query((object_id, seq))
                                .await
                                .map_err(|_| FastPayError::ErrorWhileRequestingCertificate)??;
                            missing_certificates.push(certificate);
                            number = seq.decrement();
                        }
                    }

                    // Send all missing confirmation orders.
                    missing_certificates.reverse();
                    missing_certificates.extend(certificates_to_broadcast.clone());
                    for certificate in missing_certificates {
                        responses.push((
                            certificate.clone(),
                            client
                                .handle_confirmation_order(ConfirmationOrder::new(certificate))
                                .await?,
                        ));
                    }
                    Ok((responses, action(name, client).await?))
                })
            })
            .await?;
        // Terminate downloader task and retrieve the content of the cache.
        handle.stop().await?;

        let action_results = result.iter().map(|(_, result)| *result).collect();

        // Assume all responses are the same, pick the first one.
        let order_response = result
            .iter()
            .map(|(response, _)| response.clone())
            .next()
            .unwrap_or_default();

        Ok((order_response, action_results))
    }

    /// Broadcast confirmation orders.
    /// The corresponding sequence numbers should be consecutive and increasing.
    async fn broadcast_confirmation_orders(
        &mut self,
        sender: FastPayAddress,
        inputs: Vec<InputObjectKind>,
        certificates_to_broadcast: Vec<CertifiedOrder>,
    ) -> Result<Vec<(CertifiedOrder, OrderInfoResponse)>, anyhow::Error> {
        self.broadcast_and_execute(sender, inputs, certificates_to_broadcast, |_, _| {
            Box::pin(async { Ok(()) })
        })
        .await
        .map(|(responses, _)| responses)
    }

    /// Make sure we have all our certificates with sequence number
    /// in the range 0..self.next_sequence_number
    pub async fn download_certificates(
        &mut self,
    ) -> Result<BTreeMap<ObjectID, Vec<CertifiedOrder>>, FastPayError> {
        let mut sent_certificates: BTreeMap<ObjectID, Vec<CertifiedOrder>> = BTreeMap::new();

        for (object_id, next_sequence_number) in self.store.object_sequence_numbers.iter() {
            let known_sequence_numbers: BTreeSet<_> = self
                .certificates(&object_id)
                .flat_map(|cert| cert.order.input_objects())
                .filter_map(|object_kind| {
                    if object_kind.object_id() == object_id {
                        Some(object_kind.version())
                    } else {
                        None
                    }
                })
                .collect();

            let mut requester = CertificateRequester::new(
                self.committee.clone(),
                self.authority_clients.values().cloned().collect(),
                None,
            );

            let entry = sent_certificates.entry(object_id).or_default();
            // TODO: it's inefficient to loop through sequence numbers to retrieve missing cert, rethink this logic when we change certificate storage in client.
            let mut number = SequenceNumber::from(0);
            while number < next_sequence_number {
                if !known_sequence_numbers.contains(&number) {
                    let certificate = requester.query((object_id, number)).await?;
                    entry.push(certificate);
                }
                number = number.increment();
            }
        }
        Ok(sent_certificates)
    }

    /// Update our view of certificates. Update the object_id and the next sequence number accordingly.
    /// NOTE: This is only useful in the eventuality of missing local data.
    /// We assume certificates to be valid and sent by us, and their sequence numbers to be unique.
    fn update_certificates(
        &mut self,
        object_id: &ObjectID,
        certificates: &[CertifiedOrder],
    ) -> Result<(), FastPayError> {
        for new_cert in certificates {
            // Try to get object's last seq number before the mutation, default to 0 for newly created object.
            let seq = new_cert
                .order
                .input_objects()
                .iter()
                .find_map(|object_kind| {
                    if object_id == &object_kind.object_id() {
                        Some(object_kind.version())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            let mut new_next_sequence_number = self.next_sequence_number(object_id)?;
            if seq >= new_next_sequence_number {
                new_next_sequence_number = seq.increment();
            }
            let new_cert_order_digest = new_cert.order.digest();
            // Multi table atomic insert using batches
            let mut batch = self
                .store
                .object_sequence_numbers
                .batch()
                .insert_batch(
                    &self.store.object_sequence_numbers,
                    std::iter::once((object_id, new_next_sequence_number)),
                )?
                .insert_batch(
                    &self.store.certificates,
                    std::iter::once((&new_cert_order_digest, new_cert)),
                )?;
            let mut certs = match self.store.object_certs.get(object_id)? {
                Some(c) => c.clone(),
                None => Vec::new(),
            };
            if !certs.contains(&new_cert_order_digest) {
                certs.push(new_cert_order_digest);
                batch = batch.insert_batch(
                    &self.store.object_certs,
                    std::iter::once((object_id, certs)),
                )?;
            }
            // Execute atomic write of opers
            batch.write()?;
        }
        // Sanity check
        let certificates_count = self.certificates(object_id).count();

        if certificates_count == usize::from(self.next_sequence_number(object_id)?) {
            Ok(())
        } else {
            Err(FastPayError::UnexpectedSequenceNumber {
                object_id: *object_id,
                expected_sequence: SequenceNumber::from(certificates_count as u64),
            })
        }
    }

    /// There are situations where a transaction failure does not have side effects in the authorities
    /// Hence after a failure, we can release the order lock locally
    /// This function tries to check if the error from a transaction is one of such errors
    /// If an error does not have sife effects, we unlock the objects and return the original error
    /// TODO: define other situations and error types where we can unlock objects after authority error
    /// https://github.com/MystenLabs/fastnft/issues/346
    fn handle_transaction_error_side_effects<T>(
        &self,
        val: Result<T, anyhow::Error>,
        _order: &Order,
    ) -> Result<T, anyhow::Error>
    where
        T: std::fmt::Debug,
    {
        // if let Err(err) = val {
        //     // Try convert to FP error
        //     let fp_error = err.downcast_ref::<FastPayError>();
        //     // TODO: define all such errors: https://github.com/MystenLabs/fastnft/issues/346
        //     // Try to match error variants
        //     let (conv, flag1) = matches_error!(
        //         fp_error,
        //         Some(FastPayError::UnexpectedSequenceNumber { .. })
        //             | Some(FastPayError::InvalidObjectDigest { .. })
        //             | Some(FastPayError::LockErrors { .. })
        //             | Some(FastPayError::ObjectNotFound { .. })
        //     );
        //     let (conv, flag2) = matches_error!(conv,
        //         Some(FastPayError::QuorumNotReached {errors, ..}) if matches!(errors.as_slice(),
        //         [FastPayError::LockErrors{..},..] | [FastPayError::ObjectNotFound{..},..]
        //         | [FastPayError::UnexpectedSequenceNumber{..},..] | [FastPayError::InvalidObjectDigest{..},..]));
        //     if flag1 || flag2 {
        //         // Execution failed but no side effects on authorities
        //         // Ensure we can unlock by this order
        //         fp_ensure!(
        //             self.can_lock_or_unlock(&order.clone())?,
        //             FastPayError::OverlappingOrderObjectsError.into()
        //         );
        //         // We can now unlock the input objects
        //         self.unlock_pending_order_objects(order)?;
        //         // All done
        //         return Err(conv.unwrap().clone().into());
        //     }
        //     return anyhow::private::Err(err);
        // }
        // Return the original error
        val
    }

    /// Execute (or retry) an order and subsequently execute the Confirmation Order.
    /// Update local object states using newly created certificate and ObjectInfoResponse from the Confirmation step.
    /// Unlocking objects from an order must only be performed at the end of confirmation
    /// If the authorities failed to execute the order due to the object not being found, we can unlock the object
    /// TODO: define other situations where we can unlock objects after authority error
    /// https://github.com/MystenLabs/fastnft/issues/346
    async fn execute_transaction(
        &mut self,
        order: Order,
    ) -> Result<(CertifiedOrder, OrderEffects), anyhow::Error> {
        // This call locks the input objects
        let tx_result = self
            .execute_transaction_without_confirmation(order.clone())
            .await;
        // Check the kinds of errors returned
        // Some errors can allow us unlock the objects
        // Due to Rust object ownership rules, one has to transfer the response back
        let new_certificate =
            self.handle_transaction_error_side_effects(tx_result, &order.clone())?;

        // Confirm last transfer certificate if needed.
        let conf_result = self
            .broadcast_confirmation_orders(
                self.address,
                new_certificate.order.input_objects(),
                vec![new_certificate.clone()],
            )
            .await;

        // Check the kinds of errors returned
        // Some errors can allow us unlock the objects
        // Due to Rust object ownership rules, one has to transfer the response back
        let responses = self.handle_transaction_error_side_effects(conf_result, &order.clone())?;

        // Find response for the current order from all the returned order responses.
        let (_, response) = responses
            .into_iter()
            .find(|(cert, _)| cert.order == new_certificate.order)
            .ok_or(FastPayError::ErrorWhileRequestingInformation)?;

        // Update local data using new order response.
        self.update_objects_from_order_info(response.clone())
            .await?;
        // Ensure we can unlock by this order
        fp_ensure!(
            self.can_lock_or_unlock(&order.clone())?,
            FastPayError::OverlappingOrderObjectsError.into()
        );

        // We can now unlock the input objects
        self.unlock_pending_order_objects(&order)?;
        // All done
        Ok((new_certificate, response.signed_effects.unwrap().effects))
    }

    /// This function verifies that the objects in the specfied order are locked by the given order
    /// We use this to ensure that an order can indeed unclock or lock certain objects in order
    /// This means either exactly all the objects are owned by this order, or by no order
    /// The caller has to explicitly find which objects are locked
    /// TODO: always return true for immutable objects https://github.com/MystenLabs/fastnft/issues/305
    fn can_lock_or_unlock(&self, order: &Order) -> Result<bool, FastPayError> {
        let iter_matches = self.store.pending_orders.multi_get(
            &order
                .input_objects()
                .iter()
                .map(|q| q.object_id())
                .collect_vec(),
        )?;
        for o in iter_matches {
            // If we find any order that isn't the given order, we cannot proceed
            if o.is_some() && o.unwrap() != *order {
                return Ok(false);
            }
        }
        // All the objects are either owned by this order or by no order
        Ok(true)
    }
    /// Locks the objects for the given order
    /// It is important to check that the object is not locked before locking again
    /// One should call has_pending_order_conflict before locking as this overwites the previous lock
    /// If the object is already locked, ensure it is unlocked by calling unlock_pending_order_objects
    /// Client runs sequentially right now so access to this is safe
    /// Double-locking can cause equivocation. TODO: https://github.com/MystenLabs/fastnft/issues/335
    fn lock_pending_order_objects(&self, order: &Order) -> Result<(), FastPayError> {
        if !self.can_lock_or_unlock(order)? {
            return Err(FastPayError::OverlappingOrderObjectsError);
        }
        self.store
            .pending_orders
            .multi_insert(
                order
                    .input_objects()
                    .iter()
                    .map(|e| (e.object_id(), order.clone())),
            )
            .map_err(|e| e.into())
    }
    /// Unlocks the objects for the given order
    /// Unlocking an already unlocked object, is a no-op and does not Err
    fn unlock_pending_order_objects(&self, order: &Order) -> Result<(), FastPayError> {
        if !self.can_lock_or_unlock(order)? {
            return Err(FastPayError::OverlappingOrderObjectsError);
        }
        self.store
            .pending_orders
            .multi_remove(order.input_objects().iter().map(|e| e.object_id()))
            .map_err(|e| e.into())
    }

    /// Execute (or retry) an order without confirmation. Update local object states using newly created certificate.
    /// At the end of this function, the input objects are locked but can only be unlocked after confirmation
    async fn execute_transaction_without_confirmation(
        &mut self,
        order: Order,
    ) -> Result<CertifiedOrder, anyhow::Error> {
        // Check if this order can lock the objects
        // Is it okay for an order to double-lock it's own objects since it may be recovering from a crash
        fp_ensure!(
            self.can_lock_or_unlock(&order)?,
            FastPayError::OverlappingOrderObjectsError.into()
        );
        // Lock the objects in this order
        // We should only unlock them after confirmation
        self.lock_pending_order_objects(&order)?;
        let tx_result = self
            .broadcast_and_handle_order(self.address, order.clone())
            .await;

        // Check the kinds of errors returned
        // Some errors can allow us unlock the objects
        // Due to Rust object ownership rules, one has to transfer the response back
        let result = self.handle_transaction_error_side_effects(tx_result, &order.clone())?;

        // order_info_response contains response from broadcasting old unconfirmed order, if any.
        let (order_info_responses, new_sent_certificate) = result;
        assert_eq!(&new_sent_certificate.order, &order);

        // Update local data using all order response.
        for (_, response) in order_info_responses {
            self.update_objects_from_order_info(response).await?;
        }
        Ok(new_sent_certificate)
    }

    async fn download_own_object_ids(
        &self,
    ) -> Result<(AuthorityName, Vec<ObjectRef>), FastPayError> {
        let request = AccountInfoRequest {
            account: self.address,
        };
        // Sequentially try each authority in random order.
        let mut authorities: Vec<&AuthorityName> = self.authority_clients.keys().collect();
        // TODO: implement sampling according to stake distribution and using secure RNG. https://github.com/MystenLabs/fastnft/issues/128
        authorities.shuffle(&mut rand::thread_rng());
        // Authority could be byzantine, add timeout to avoid waiting forever.
        for authority_name in authorities {
            let authority = self.authority_clients.get(authority_name).unwrap();
            let result = timeout(
                AUTHORITY_REQUEST_TIMEOUT,
                authority.handle_account_info_request(request.clone()),
            )
            .map_err(|_| FastPayError::ErrorWhileRequestingInformation)
            .await?;
            if let Ok(AccountInfoResponse { object_ids, .. }) = &result {
                return Ok((*authority_name, object_ids.clone()));
            }
        }
        Err(FastPayError::ErrorWhileRequestingInformation)
    }

    async fn update_objects_from_order_info(
        &mut self,
        order_info_resp: OrderInfoResponse,
    ) -> Result<(CertifiedOrder, OrderEffects), FastPayError> {
        if let Some(v) = order_info_resp.signed_effects {
            // The cert should be included in the response
            let cert = order_info_resp.certified_order.unwrap();
            let parent_tx_digest = cert.order.digest();
            self.store.certificates.insert(&parent_tx_digest, &cert)?;

            let mut objs_to_download = Vec::new();

            for &(object_ref, owner) in v.effects.all_mutated() {
                let (object_id, seq, _) = object_ref;
                let old_seq = self
                    .store
                    .object_sequence_numbers
                    .get(&object_id)?
                    .unwrap_or_default();
                // only update if data is new
                if old_seq < seq {
                    if owner.is_address(&self.address) {
                        self.insert_object_info(&object_ref, &parent_tx_digest)?;
                        objs_to_download.push(object_ref);
                    } else {
                        self.remove_object_info(&object_id)?;
                    }
                } else if old_seq == seq && owner.is_address(&self.address) {
                    // ObjectRef can be 1 version behind because it's only updated after confirmation.
                    self.store.object_refs.insert(&object_id, &object_ref)?;
                }
            }

            // TODO: decide what to do with failed object downloads
            // https://github.com/MystenLabs/fastnft/issues/331
            let _failed = self
                .download_owned_objects_from_all_authorities_helper(objs_to_download)
                .await?;

            for (object_id, seq, _) in &v.effects.deleted {
                let old_seq = self
                    .store
                    .object_sequence_numbers
                    .get(object_id)?
                    .unwrap_or_default();
                if old_seq < *seq {
                    self.remove_object_info(object_id)?;
                }
            }
            Ok((cert, v.effects))
        } else {
            Err(FastPayError::ErrorWhileRequestingInformation)
        }
    }

    async fn get_object_info_execute(
        &mut self,
        object_info_req: ObjectInfoRequest,
    ) -> Result<ObjectInfoResponse, anyhow::Error> {
        let votes = self
            .communicate_with_quorum(|_, client| {
                let req = object_info_req.clone();
                Box::pin(async move { client.handle_object_info_request(req).await })
            })
            .await?;

        votes
            .get(0)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No valid confirmation order votes"))
    }

    /// Fetch the objects at the given object id, which do not already exist in the db
    /// All authorities are polled for each object and their all assumed to be honest
    /// This always returns the latest object known to the authorities
    /// How it works: this function finds all object refs that are not in the DB
    /// then it runs a downloader and submits download requests
    /// Afterwards it persists objects returned by the downloader
    /// It returns a set of the object ids which failed to download
    /// TODO: return failed download errors along with the object id
    async fn download_owned_objects_from_all_authorities_helper(
        &self,
        object_refs: Vec<ObjectRef>,
    ) -> Result<BTreeSet<ObjectRef>, FastPayError> {
        // Check the DB
        // This could be expensive. Might want to use object_ref table
        // We want items that are NOT in the table
        let fresh_object_refs = self
            .store
            .objects
            .multi_get(&object_refs)?
            .iter()
            .zip(object_refs)
            .filter_map(|(object, ref_)| match object {
                Some(_) => None,
                None => Some(ref_),
            })
            .collect::<BTreeSet<_>>();

        // Send request to download
        let (sender, mut receiver) = tokio::sync::mpsc::channel(OBJECT_DOWNLOAD_CHANNEL_BOUND);

        // Now that we have all the fresh ids, dispatch fetches
        for object_ref in fresh_object_refs.clone() {
            let sender = sender.clone();
            tokio::spawn(ClientState::fetch_and_store_object(
                self.authority_clients.clone(),
                object_ref,
                AUTHORITY_REQUEST_TIMEOUT,
                sender,
            ));
        }
        // Close unused channel
        drop(sender);
        let mut err_object_refs = fresh_object_refs.clone();
        // Receive from the downloader
        while let Some(resp) = receiver.recv().await {
            // Persists them to disk
            if let Ok(o) = resp {
                self.store.objects.insert(&o.to_object_reference(), &o)?;
                err_object_refs.remove(&o.to_object_reference());
            }
        }
        Ok(err_object_refs)
    }

    /// This function fetches one object at a time, and sends back the result over the channel
    /// The object ids are also returned so the caller can determine which fetches failed
    /// NOTE: This function assumes all authorities are honest
    async fn fetch_and_store_object(
        authority_clients: BTreeMap<PublicKeyBytes, A>,
        object_ref: ObjectRef,
        timeout: Duration,
        sender: tokio::sync::mpsc::Sender<Result<Object, FastPayError>>,
    ) {
        let object_id = object_ref.0;
        // Prepare the request
        let request = ObjectInfoRequest {
            object_id,
            request_sequence_number: None,
        };

        // For now assume all authorities. Assume they're all honest
        // This assumption is woeful, and should be fixed
        // TODO: https://github.com/MystenLabs/fastnft/issues/320
        let results = future::join_all(authority_clients.iter().map(|(_, ac)| {
            tokio::time::timeout(timeout, ac.handle_object_info_request(request.clone()))
        }))
        .await;

        fn obj_fetch_err(id: ObjectID, err: &str) -> Result<Object, FastPayError> {
            Err(FastPayError::ObjectFetchFailed {
                object_id: id,
                err: err.to_owned(),
            })
        }

        let mut ret_val: Result<Object, FastPayError> = Err(FastPayError::ObjectFetchFailed {
            object_id: object_ref.0,
            err: "No authority returned object".to_string(),
        });
        // Find the first non-error value
        // There are multiple reasons why we might not have an object
        // We can timeout, or the authority returns an error or simply no object
        // When we get an object back, it also might not match the digest we want
        for result in results {
            // Check if the result of the call is successful
            ret_val = match result {
                Ok(res) => match res {
                    // Check if the authority actually had an object
                    Ok(resp) => match resp.object_and_lock {
                        Some(o) => {
                            // Check if this is the the object we want
                            if o.object.digest() == object_ref.2 {
                                Ok(o.object)
                            } else {
                                obj_fetch_err(object_id, "Object digest mismatch")
                            }
                        }
                        None => obj_fetch_err(object_id, "object_and_lock is None"),
                    },
                    // Something in FastX failed
                    Err(e) => Err(e),
                },
                // Took too long
                Err(e) => obj_fetch_err(object_id, e.to_string().as_str()),
            };
            // We found a value
            if ret_val.is_ok() {
                break;
            }
        }
        sender
            .send(ret_val)
            .await
            .expect("Cannot send object on channel after object fetch attempt");
    }
}

#[async_trait]
impl<A> Client for ClientState<A>
where
    A: AuthorityAPI + Send + Sync + Clone + 'static,
{
    async fn transfer_object(
        &mut self,
        object_id: ObjectID,
        gas_payment: ObjectID,
        recipient: FastPayAddress,
    ) -> Result<CertifiedOrder, anyhow::Error> {
        let object_ref = self
            .store
            .object_refs
            .get(&object_id)?
            .ok_or(FastPayError::ObjectNotFound { object_id })?;
        let gas_payment =
            self.store
                .object_refs
                .get(&gas_payment)?
                .ok_or(FastPayError::ObjectNotFound {
                    object_id: gas_payment,
                })?;

        let transfer = Transfer {
            object_ref,
            sender: self.address,
            recipient,
            gas_payment,
        };
        let order = Order::new_transfer(transfer, &self.secret);
        let (certificate, _) = self.execute_transaction(order).await?;

        // remove object from local storage if the recipient is not us.
        if recipient != self.address {
            self.remove_object_info(&object_id)?;
        }

        Ok(certificate)
    }

    async fn receive_object(&mut self, certificate: &CertifiedOrder) -> Result<(), anyhow::Error> {
        certificate.check(&self.committee)?;
        match &certificate.order.kind {
            OrderKind::Transfer(transfer) => {
                fp_ensure!(
                    transfer.recipient == self.address,
                    FastPayError::IncorrectRecipientError.into()
                );
                let responses = self
                    .broadcast_confirmation_orders(
                        transfer.sender,
                        certificate.order.input_objects(),
                        vec![certificate.clone()],
                    )
                    .await?;

                for (_, response) in responses {
                    self.update_objects_from_order_info(response).await?;
                }

                let response = self
                    .get_object_info(ObjectInfoRequest {
                        object_id: *certificate.order.object_id(),
                        // TODO(https://github.com/MystenLabs/fastnft/issues/290):
                        //        This function assumes that requesting the parent cert of object seq+1 will give the cert of
                        //        that creates the object. This is not true, as objects may be deleted and may not have a seq+1
                        //        to look up.
                        //
                        //        The authority `handle_object_info_request` is now fixed to return the parent at seq, and not
                        //        seq+1. But a lot of the client code makes the above wrong assumption, and the line above reverts
                        //        query to the old (incorrect) behavious to not break tests everywhere.
                        request_sequence_number: Some(transfer.object_ref.1.increment()),
                    })
                    .await?;

                let object = &response
                    .object_and_lock
                    .ok_or(FastPayError::ObjectNotFound {
                        object_id: *certificate.order.object_id(),
                    })?
                    .object;
                self.store
                    .object_refs
                    .insert(&object.id(), &object.to_object_reference())?;

                // Everything worked: update the local objects and certs.
                let cert_order_digest = certificate.order.digest();
                if !self.store.certificates.contains_key(&cert_order_digest)? {
                    self.store
                        .object_sequence_numbers
                        .insert(&transfer.object_ref.0, &transfer.object_ref.1.increment())?;
                    let mut tx_digests =
                        match self.store.object_certs.get(&transfer.object_ref.0)? {
                            Some(c) => c,
                            None => Vec::new(),
                        };
                    tx_digests.push(cert_order_digest);
                    self.store
                        .object_certs
                        .insert(&transfer.object_ref.0, &tx_digests.to_vec())?;
                    self.store
                        .certificates
                        .insert(&cert_order_digest, certificate)?;
                }

                Ok(())
            }
            OrderKind::Publish(_) | OrderKind::Call(_) => {
                unimplemented!("receiving (?) Move call or publish")
            }
        }
    }

    async fn transfer_to_fastx_unsafe_unconfirmed(
        &mut self,
        object_id: ObjectID,
        gas_payment: ObjectID,
        recipient: FastPayAddress,
    ) -> Result<CertifiedOrder, anyhow::Error> {
        let object_ref = self.object_ref(object_id)?;
        let gas_payment = self.object_ref(gas_payment)?;

        let transfer = Transfer {
            object_ref,
            sender: self.address,
            recipient,
            gas_payment,
        };
        let order = Order::new_transfer(transfer, &self.secret);
        // We need to ensure that confirmation is executed eventually otherwise all objects in this order will be locked
        let new_certificate = self.execute_transaction_without_confirmation(order).await?;

        // The new cert will not be updated by order effect without confirmation, the new unconfirmed cert need to be added temporally.
        let new_sent_certificates = vec![new_certificate.clone()];
        for object_kind in new_certificate.order.input_objects() {
            self.update_certificates(&object_kind.object_id(), &new_sent_certificates)?;
        }

        Ok(new_certificate)
    }

    /// Try to complete pending orders
    /// Order could have been locked due to tx failure or intentional tx without confirmation
    /// We always assume a pending order simply can be re-executed due to idempotence of orders
    async fn try_complete_pending_orders(&mut self) -> Result<(), FastPayError> {
        // Orders are idempotent so no need to prevent multiple executions
        let unique_pending_orders: HashSet<_> = self
            .store
            .pending_orders
            .iter()
            .map(|(_, ord)| ord)
            .collect();
        // Need some kind of timeout or max_trials here?
        // TODO: https://github.com/MystenLabs/fastnft/issues/330
        for order in unique_pending_orders {
            // Execution method handles locking and unlocking if successful
            self.execute_transaction(order.clone()).await.map_err(|e| {
                FastPayError::ErrorWhileProcessingTransactionOrder { err: e.to_string() }
            })?;
        }
        Ok(())
    }

    async fn sync_client_state_with_random_authority(
        &mut self,
    ) -> Result<AuthorityName, anyhow::Error> {
        if !self.store.pending_orders.is_empty()? {
            // Finish executing the previous orders
            self.try_complete_pending_orders().await?;
        }
        // update object_ids.
        self.store.object_sequence_numbers.clear()?;
        self.store.object_refs.clear()?;

        let (authority_name, object_refs) = self.download_own_object_ids().await?;
        for object_ref in object_refs {
            let (object_id, sequence_number, _) = object_ref;
            self.store
                .object_sequence_numbers
                .insert(&object_id, &sequence_number)?;
            self.store.object_refs.insert(&object_id, &object_ref)?;
        }
        // Recover missing certificates.
        let new_certificates = self.download_certificates().await?;

        for (id, certs) in new_certificates {
            self.update_certificates(&id, &certs)?;
        }
        Ok(authority_name)
    }

    async fn move_call(
        &mut self,
        package_object_ref: ObjectRef,
        module: Identifier,
        function: Identifier,
        type_arguments: Vec<TypeTag>,
        gas_object_ref: ObjectRef,
        object_arguments: Vec<ObjectRef>,
        pure_arguments: Vec<Vec<u8>>,
        gas_budget: u64,
    ) -> Result<(CertifiedOrder, OrderEffects), anyhow::Error> {
        let move_call_order = Order::new_move_call(
            self.address,
            package_object_ref,
            module,
            function,
            type_arguments,
            gas_object_ref,
            object_arguments,
            pure_arguments,
            gas_budget,
            &self.secret,
        );
        self.execute_transaction(move_call_order).await
    }

    async fn publish(
        &mut self,
        package_source_files_path: String,
        gas_object_ref: ObjectRef,
    ) -> Result<(CertifiedOrder, OrderEffects), anyhow::Error> {
        // Try to compile the package at the given path
        let compiled_modules = build_move_package_to_bytes(Path::new(&package_source_files_path))?;
        let move_publish_order =
            Order::new_module(self.address, gas_object_ref, compiled_modules, &self.secret);
        self.execute_transaction(move_publish_order).await
    }

    async fn get_object_info(
        &mut self,
        object_info_req: ObjectInfoRequest,
    ) -> Result<ObjectInfoResponse, anyhow::Error> {
        self.get_object_info_execute(object_info_req).await
    }

    async fn get_owned_objects(&self) -> Vec<ObjectID> {
        self.store.object_sequence_numbers.keys().collect()
    }

    async fn download_owned_objects_from_all_authorities(
        &self,
    ) -> Result<BTreeSet<ObjectRef>, FastPayError> {
        let object_refs = self.store.object_refs.iter().map(|q| q.1).collect();
        self.download_owned_objects_from_all_authorities_helper(object_refs)
            .await
    }
}
/// This macro extends the matches! macros but does also returns the input object to the owner
macro_rules! matches_error {
    ($expression:expr, $(|)? $( $pattern:pat_param )|+ $( if $guard: expr )? $(,)?) => {
        match $expression {
            $( $pattern )|+ $( if $guard )? => ($expression, true),
            _ => ($expression, false)
        }
    }
}
pub(crate) use matches_error;
