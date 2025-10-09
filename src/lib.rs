//! Certificate Transparency Log client suitable for monitoring, quick
//! SCT validation, gossiping, etc.
//!
//! The source code of this project contains some best-effort explanation
//! comments for others trying to implement such a client. As of 2019,
//! the documentation that exists out there are (in my opinion) pretty lacking,
//! and I had some bad time trying to implement this.
//!
//! All `pub_key` are in DER format, which is the format returned (in base64)
//! by google's trusted log list. `signature`s are *Digitally-signed structs*, and
//! `raw_signature`s are ASN1-encoded signatures.
//!
//! Best effort are made to catch misbehavior by CT logs or invalid certificates. It is up
//! to the user of this library to decide what to do when logs don't behave corrctly.
//!
//! This project is not intended to be a beginner friendly tutorial on how a
//! CT log works. To learn more about CT, you can read [my blog article](https://blog.maowtm.org/ct/en.html)
//! or [the RFC](https://tools.ietf.org/html/rfc6962).
//!
//! API calls are currently all blocking. If anyone is interested in rewriting them in Futures, PR is welcome.

// todo: gossiping

#[macro_use(lazy_static)]
extern crate lazy_static;

use std::{fmt, io, path};

use futures::StreamExt;
use futures::pin_mut;
use log::{info, warn};
use openssl::pkey::PKey;
use openssl::x509::X509;

use internal::new_http_client;
pub use sct::{SctEntry, SignedCertificateTimestamp};
pub use sth::SignedTreeHead;

use crate::internal::openssl_ffi::{x509_clone, x509_make_a_looks_like_issued_by_b};
use crate::internal::{
    Leaf, check_consistency_proof, check_inclusion_proof, fetch_inclusion_proof,
};

mod sct;
mod sth;

pub mod certutils;
pub mod google_log_list;
pub mod internal;
pub mod jsons;
pub mod utils;

#[cfg(not(any(feature = "native-tls", feature = "rustls-tls")))]
compile_error!("You must enable either the `native-tls` or `rustls-tls` feature.");

#[cfg(all(feature = "native-tls", feature = "rustls-tls"))]
compile_error!("You must enable only one of the `native-tls` and `rustls-tls` features, not both.");

/// Errors that this library could produce.
#[derive(Debug)]
pub enum Error {
    /// Something strange happened.
    Unknown(String),

    /// You provided something bad.
    InvalidArgument(String),

    /// File IO error
    FileIO(path::PathBuf, io::Error),

    /// Network IO error
    NetIO(reqwest::Error),

    /// The CT server provided us with invalid signature.
    InvalidSignature(String),

    /// The CT server responded with something other than 200.
    InvalidResponseStatus(reqwest::StatusCode),

    /// Server responded with something bad (e.g. malformed JSON)
    MalformedResponseBody(String),

    /// Server returned an invalid consistency proof.
    InvalidConsistencyProof {
        prev_size: u64,
        new_size: u64,
        desc: String,
    },

    /// ConsistencyProofPart::verify failed
    CannotVerifyTreeData(String),

    /// Something's wrong with the certificate.
    BadCertificate(String),

    /// Server returned an invalid inclusion proof.
    InvalidInclusionProof {
        tree_size: u64,
        leaf_index: u64,
        desc: String,
    },

    /// A malformed SCT is given.
    BadSct(String),

    /// We asked for a certain entry expecting it to be there, but the server gave us nothing.
    ExpectedEntry(u64),
}

/// Either a fetched and checked [`SignedTreeHead`], or a [`SignedTreeHead`] that has a valid signature
/// but did not pass some internal checks, or just an [`Error`].
#[derive(Debug)]
pub enum SthResult {
    /// Got the new tree head.
    Ok(SignedTreeHead),

    /// Something went wrong and no tree head was received.
    Err(Error),

    /// Something went wrong, but the server returned a valid signed tree head.
    /// The underlying error is wrapped inside. You may wish to log this.
    ErrWithSth(Error, SignedTreeHead),
}

impl SthResult {
    /// Return a signed tree head, if there is one received.
    ///
    /// This can return a `Some` even when there is error, if for example, the server returned a valid signed
    /// tree head but failed to provide a consistency proof. You may wish to log this.
    pub fn tree_head(&self) -> Option<&SignedTreeHead> {
        match self {
            SthResult::Ok(sth) => Some(sth),
            SthResult::Err(_) => None,
            SthResult::ErrWithSth(_, sth) => Some(sth),
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, SthResult::Ok(_))
    }

    pub fn is_err(&self) -> bool {
        !self.is_ok()
    }

    /// Return the [`SignedTreeHead`], if this is a Ok. Otherwise panic.
    pub fn unwrap(self) -> SignedTreeHead {
        match self {
            SthResult::Ok(sth) => sth,
            _ => {
                panic!(
                    "unwrap called on SthResult with error: {}",
                    self.unwrap_err()
                )
            }
        }
    }

    /// Return the [`Error`], if this is an `Err` or `ErrWithSth`. Otherwise panic.
    pub fn unwrap_err(self) -> Error {
        match self {
            SthResult::ErrWithSth(e, _) => e,
            SthResult::Err(e) => e,
            _ => panic!("unwrap_err called on SthResult that is ok."),
        }
    }

    /// Return the [`SignedTreeHead`], if this is a `Ok` or `ErrWithSth`. Otherwise panic.
    pub fn unwrap_tree_head(self) -> SignedTreeHead {
        match self {
            SthResult::Ok(sth) => sth,
            SthResult::ErrWithSth(_, sth) => sth,
            SthResult::Err(e) => panic!("unwrap_tree_head called on SthResult with error: {}", e),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unknown(desc) => write!(f, "{}", desc),
            Error::InvalidArgument(desc) => write!(f, "Invalid argument: {}", desc),
            Error::FileIO(path, e) => write!(f, "{}: {}", path.to_string_lossy(), &e),
            Error::NetIO(e) => write!(f, "Network IO error: {}", &e),
            Error::InvalidSignature(desc) => write!(f, "Invalid signature received: {}", &desc),
            Error::InvalidResponseStatus(response_code) => write!(
                f,
                "Server responded with {} {}",
                response_code.as_u16(),
                response_code.as_str()
            ),
            Error::MalformedResponseBody(desc) => {
                write!(f, "Unable to parse server response: {}", &desc)
            }
            Error::InvalidConsistencyProof {
                prev_size,
                new_size,
                desc,
            } => write!(
                f,
                "Server provided an invalid consistency proof from {} to {}: {}",
                prev_size, new_size, &desc
            ),
            Error::CannotVerifyTreeData(desc) => write!(
                f,
                "The certificates returned by the server is inconsistent with the previously provided consistency proof: {}",
                &desc
            ),
            Error::BadCertificate(desc) => write!(
                f,
                "The certificate returned by the server has a problem: {}",
                &desc
            ),
            Error::InvalidInclusionProof {
                tree_size,
                leaf_index,
                desc,
            } => write!(
                f,
                "Server provided an invalid inclusion proof of {} in tree with size {}: {}",
                leaf_index, tree_size, desc
            ),
            Error::BadSct(desc) => write!(f, "The SCT received is invalid: {}", desc),
            Error::ExpectedEntry(leaf_index) => write!(
                f,
                "The server did not return the leaf with index {}, even though we believe it should be there.",
                leaf_index
            ),
        }
    }
}

/// A stateful CT monitor.
///
/// One instance of this struct only concerns with one particular log. To monitor multiple
/// logs, you can create multiple such instances and run them on different threads.
///
/// It remembers a last checked tree root, so that it only checks the newly added
/// certificates in the log each time you call [`update`](Self::update).
pub struct CTClient {
    base_url: reqwest::Url,
    pub_key: PKey<openssl::pkey::Public>,
    http_client: reqwest::Client,
    latest_size: u64,
    latest_tree_hash: [u8; 32],
}

impl fmt::Debug for CTClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CT log {}: current root = {}, size = {}",
            self.base_url,
            utils::u8_to_hex(&self.latest_tree_hash[..]),
            self.latest_size
        )
    }
}

impl CTClient {
    /// Construct a new `CTClient` instance, and fetch the latest tree root.
    ///
    /// Previous certificates in this log will not be checked.
    ///
    /// # Errors
    ///
    /// * If `base_url` does not ends with `/`.
    ///
    /// # Example
    ///
    /// ```
    /// use ctclient_async::CTClient;
    /// use base64::decode;
    /// # tokio_test::block_on(async {
    /// // URL and public key copy-pasted from https://www.gstatic.com/ct/log_list/v3/all_logs_list.json .
    /// let public_key = decode("MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEGoAaFRkZI3m0+qB5jo3VwdzCtZaSfpTgw34UfAoNLUaonRuxQWUMX5jEWhd5gVtKFEHsr6ldDqsSGXHNQ++7lw==").unwrap();
    /// let client = CTClient::new_from_latest_th("https://ct.cloudflare.com/logs/nimbus2025/", &public_key).await.unwrap();
    /// # });
    /// ```
    pub async fn new_from_latest_th(base_url: &str, pub_key: &[u8]) -> Result<Self, Error> {
        if !base_url.ends_with('/') {
            return Err(Error::InvalidArgument("baseUrl must end with /".to_owned()));
        }
        let base_url = reqwest::Url::parse(base_url)
            .map_err(|e| Error::InvalidArgument(format!("Unable to parse url: {}", &e)))?;
        let http_client = new_http_client()?;
        let evp_pkey = PKey::public_key_from_der(pub_key)
            .map_err(|e| Error::InvalidArgument(format!("Error parsing public key: {}", &e)))?;
        let sth = internal::check_tree_head(&http_client, &base_url, &evp_pkey).await?;
        Ok(CTClient {
            base_url,
            pub_key: evp_pkey,
            http_client,
            latest_size: sth.tree_size,
            latest_tree_hash: sth.root_hash,
        })
    }

    /// Construct a new `CTClient` that will check all certificates included after
    /// the given tree state.
    ///
    /// Previous certificates in this log before the provided tree hash will not be checked.
    ///
    /// # Example
    ///
    /// ```
    /// use ctclient_async::{CTClient, utils};
    /// use base64::decode;
    /// // URL and public key copy-pasted from https://www.gstatic.com/ct/log_list/v3/all_logs_list.json .
    /// let public_key = decode("MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE01EAhx4o0zPQrXTcYjgCt4MVFsT0Pwjzb1RwrM0lhWDlxAYPP6/gyMCXNkOn/7KFsjL7rwk78tHMpY8rXn8AYg==").unwrap();
    /// use std::convert::TryInto;
    /// // Tree captured on 2020-05-12 15:34:11 UTC
    /// let th: [u8; 32] = (&utils::hex_to_u8("63875e88a3e37dc5b6cdbe213fe1df490d40193e4777f79467958ee157de70d6")[..]).try_into().unwrap();
    /// let client = CTClient::new_from_perv_tree_hash("https://ct.cloudflare.com/logs/nimbus2020/", &public_key, th, 299304276).unwrap();
    /// ```
    pub fn new_from_perv_tree_hash(
        base_url: &str,
        pub_key: &[u8],
        tree_hash: [u8; 32],
        tree_size: u64,
    ) -> Result<Self, Error> {
        if !base_url.ends_with('/') {
            return Err(Error::InvalidArgument("baseUrl must end with /".to_owned()));
        }
        let base_url = reqwest::Url::parse(base_url)
            .map_err(|e| Error::InvalidArgument(format!("Unable to parse url: {}", &e)))?;
        let http_client = new_http_client()?;
        let evp_pkey = PKey::public_key_from_der(pub_key)
            .map_err(|e| Error::InvalidArgument(format!("Error parsing public key: {}", &e)))?;
        Ok(CTClient {
            base_url,
            pub_key: evp_pkey,
            http_client,
            latest_size: tree_size,
            latest_tree_hash: tree_hash,
        })
    }

    /// Get the last checked tree head. Returns `(tree_size, root_hash)`.
    pub fn get_checked_tree_head(&self) -> (u64, [u8; 32]) {
        (self.latest_size, self.latest_tree_hash)
    }

    /// Get the underlying http client used to call CT APIs.
    pub fn get_reqwest_client(&self) -> &reqwest::Client {
        &self.http_client
    }

    /// Get the base_url of the log currently being monitored by this client.
    ///
    /// This is the url that was passed to the constructor.
    pub fn get_base_url(&self) -> &reqwest::Url {
        &self.base_url
    }

    /// Calls `self.update()` with `None` as `cert_handler`.
    pub async fn light_update(&mut self) -> SthResult {
        self.update(None::<fn(&[X509])>).await
    }

    /// Fetch the latest tree root, check all the new certificates if `cert_handler` is a Some, and update our
    /// internal "last checked tree root".
    ///
    /// This function should never panic, no matter what the server does to us.
    ///
    /// Return the latest [`SignedTreeHead`] (STH) returned by the server, even if
    /// it is the same as last time, or if it rolled back (new tree_size < current tree_size).
    ///
    /// To log the behavior of CT logs, store the returned tree head and signature in some kind
    /// of database (even when error). This can be used to prove a misconduct (such as a non-extending-only tree)
    /// in the future.
    ///
    /// Will only update the stored latest tree head if an [`Ok`](SthResult::Ok) is returned.
    pub async fn update<H>(&mut self, mut cert_handler: Option<H>) -> SthResult
    where
        H: FnMut(&[X509]),
    {
        let mut delaycheck = std::time::Instant::now();
        let sth = match internal::check_tree_head(&self.http_client, &self.base_url, &self.pub_key)
            .await
        {
            Ok(s) => s,
            Err(e) => return SthResult::Err(e),
        };
        let new_tree_size = sth.tree_size;
        let new_tree_root = sth.root_hash;
        use std::cmp::Ordering;
        match new_tree_size.cmp(&self.latest_size) {
            Ordering::Equal => {
                if new_tree_root == self.latest_tree_hash {
                    info!("{} remained the same.", self.base_url.as_str());
                    SthResult::Ok(sth)
                } else {
                    SthResult::ErrWithSth(
                        Error::InvalidConsistencyProof {
                            prev_size: self.latest_size,
                            new_size: new_tree_size,
                            desc: format!(
                                "Server forked! {} and {} both correspond to tree_size {}",
                                &utils::u8_to_hex(&self.latest_tree_hash),
                                &utils::u8_to_hex(&new_tree_root),
                                new_tree_size
                            ),
                        },
                        sth,
                    )
                }
            }
            Ordering::Less => {
                // Make sure server isn't doing trick with us.
                match internal::check_consistency_proof(
                    &self.http_client,
                    &self.base_url,
                    new_tree_size,
                    self.latest_size,
                    &new_tree_root,
                    &self.latest_tree_hash,
                )
                .await
                {
                    Ok(_) => {
                        warn!(
                            "{} rolled back? {} -> {}",
                            self.base_url.as_str(),
                            self.latest_size,
                            new_tree_size
                        );
                        SthResult::Ok(sth)
                    }
                    Err(e) => SthResult::ErrWithSth(
                        Error::InvalidConsistencyProof {
                            prev_size: new_tree_size,
                            new_size: self.latest_size,
                            desc: format!(
                                "Server rolled back, and can't provide a consistency proof from the rolled back tree to the original tree: {}",
                                e
                            ),
                        },
                        sth,
                    ),
                }
            }
            Ordering::Greater => {
                let consistency_proof_parts = match internal::check_consistency_proof(
                    &self.http_client,
                    &self.base_url,
                    self.latest_size,
                    new_tree_size,
                    &self.latest_tree_hash,
                    &new_tree_root,
                )
                .await
                {
                    Ok(k) => k,
                    Err(e) => return SthResult::ErrWithSth(e, sth),
                };

                if cert_handler.is_some() {
                    let i_start = self.latest_size;
                    let leafs = internal::get_entries(
                        &self.http_client,
                        &self.base_url,
                        i_start..new_tree_size,
                        500,
                    );
                    // `get_entries` returns a stream backed by an async block which is !Unpin.
                    // Pin it on the stack so we can `.next().await` without requiring `Unpin`.
                    pin_mut!(leafs);
                    let mut leaf_hashes: Vec<[u8; 32]> =
                        Vec::with_capacity((new_tree_size - i_start) as usize);
                    for i in i_start..new_tree_size {
                        match leafs.next().await {
                            Some(Ok(leaf)) => {
                                leaf_hashes.push(leaf.hash);
                                if let Err(e) = self.check_leaf(&leaf, &mut cert_handler) {
                                    return SthResult::ErrWithSth(e, sth);
                                }
                            }
                            Some(Err(e)) => {
                                return SthResult::ErrWithSth(
                                    if let Error::MalformedResponseBody(inner_e) = e {
                                        Error::MalformedResponseBody(format!(
                                            "While parsing leaf #{}: {}",
                                            i, &inner_e
                                        ))
                                    } else {
                                        e
                                    },
                                    sth,
                                );
                            }
                            None => {
                                return SthResult::ErrWithSth(Error::ExpectedEntry(i), sth);
                            }
                        }
                        if delaycheck.elapsed() > std::time::Duration::from_secs(1) {
                            info!(
                                "{}: Catching up: {} / {} ({}%)",
                                self.base_url.as_str(),
                                i,
                                new_tree_size,
                                ((i - i_start) * 1000 / (new_tree_size - i_start)) as f32 / 10f32
                            );
                            delaycheck = std::time::Instant::now();
                        }
                    }
                    assert_eq!(leaf_hashes.len(), (new_tree_size - i_start) as usize);
                    for proof_part in consistency_proof_parts.into_iter() {
                        assert!(proof_part.subtree.0 >= i_start);
                        assert!(proof_part.subtree.1 <= new_tree_size);
                        if let Err(e) = proof_part.verify(
                            &leaf_hashes[(proof_part.subtree.0 - i_start) as usize
                                ..(proof_part.subtree.1 - i_start) as usize],
                        ) {
                            return SthResult::ErrWithSth(Error::CannotVerifyTreeData(e), sth);
                        }
                    }
                    info!(
                        "{} updated to {} {} (read {} leaves)",
                        self.base_url.as_str(),
                        new_tree_size,
                        &utils::u8_to_hex(&new_tree_root),
                        new_tree_size - i_start
                    );
                } else {
                    info!(
                        "{} light updated to {} {}",
                        self.base_url.as_str(),
                        new_tree_size,
                        &utils::u8_to_hex(&new_tree_root)
                    );
                }

                self.latest_size = new_tree_size;
                self.latest_tree_hash = new_tree_root;
                SthResult::Ok(sth)
            }
        }
    }

    /// Called by [`Self::update`](crate::CTClient::update) for each leaf received
    /// to check the certificates. Usually no need to call yourself.
    pub fn check_leaf<H>(
        &self,
        leaf: &internal::Leaf,
        cert_handler: &mut Option<H>,
    ) -> Result<(), Error>
    where
        H: FnMut(&[X509]),
    {
        let chain: Vec<_> = leaf
            .x509_chain
            .iter()
            .map(|der| openssl::x509::X509::from_der(&der[..]))
            .collect();
        for rs in chain.iter() {
            if let Err(e) = rs {
                return Err(Error::BadCertificate(format!(
                    "While decoding certificate: {}",
                    e
                )));
            }
        }
        let chain: Vec<X509> = chain.into_iter().map(|x| x.unwrap()).collect();
        if chain.len() <= 1 {
            return Err(Error::BadCertificate("Empty certificate chain?".to_owned()));
        }
        for part in chain.windows(2) {
            let ca = &part[1];
            let target = &part[0];
            let ca_pkey = ca.public_key().map_err(|e| {
                Error::BadCertificate(format!("Can't get public key from ca: {}", e))
            })?;
            let verify_success = target
                .verify(&ca_pkey)
                .map_err(|e| Error::Unknown(format!("{}", e)))?;
            if !verify_success {
                return Err(Error::BadCertificate(
                    "Invalid certificate chain.".to_owned(),
                ));
            }
        }
        if let Some(tbs) = &leaf.tbs_cert {
            use internal::openssl_ffi::{x509_remove_poison, x509_to_tbs};
            let cert = chain[0].as_ref();
            let mut cert_clone = x509_clone(&cert)
                .map_err(|e| Error::Unknown(format!("Duplicating certificate: {}", e)))?;
            x509_remove_poison(&mut cert_clone)
                .map_err(|e| Error::Unknown(format!("While removing poison: {}", e)))?;
            let expected_tbs = x509_to_tbs(&cert_clone)
                .map_err(|e| Error::Unknown(format!("x509_to_tbs errored: {}", e)))?;
            if tbs != &expected_tbs {
                // Maybe the precert is signed with an intermediate precert signing CA. The TBS will nevertheless contain the
                // "true" CA as the issuer name.
                // In that case, chain[1] is the precert signing CA, and chain[2] is the "true" signing CA.
                let mut tbs_correct = false;
                if chain.len() > 2 {
                    x509_make_a_looks_like_issued_by_b(&mut cert_clone, &chain[2]).map_err(
                        |e| {
                            Error::Unknown(format!(
                                "x509_make_a_looks_like_issued_by_b failed: {}",
                                e
                            ))
                        },
                    )?;
                    let new_expected_tbs = x509_to_tbs(&cert_clone)
                        .map_err(|e| Error::Unknown(format!("x509_to_tbs errored: {}", e)))?;
                    if tbs == &new_expected_tbs {
                        tbs_correct = true;
                    }
                }
                if !tbs_correct {
                    return Err(Error::BadCertificate(
                        "TBS does not match pre-cert.".to_owned(),
                    ));
                }
            }
        }

        if let Some(handler) = cert_handler {
            handler(&chain);
        }
        Ok(())
    }

    /// Given a [`SignedCertificateTimestamp`], check that the CT log monitored by this client can provide
    /// an inclusion proof that backs the sct, and return the leaf index.
    ///
    /// Does not check the signature on the sct, and also does not check that the maximum merge delay has passed.
    pub async fn check_inclusion_proof_for_sct(
        &self,
        sct: &SignedCertificateTimestamp,
    ) -> Result<u64, Error> {
        let th = self.get_checked_tree_head();
        check_inclusion_proof(
            self.get_reqwest_client(),
            &self.base_url,
            th.0,
            &th.1,
            &sct.derive_leaf_hash(),
        )
        .await
    }

    pub async fn first_leaf_after(&self, timestamp: u64) -> Result<Option<(u64, Leaf)>, Error> {
        let mut low = 0u64;
        let mut high = self.latest_size;
        let mut last_leaf: Option<(u64, Leaf)> = None;
        while low < high {
            let mid = (low + high - 1) / 2;
            let entries_iter =
                internal::get_entries(&self.http_client, &self.base_url, mid..mid + 1, 1);
            // Pin the async-stream-backed iterator so it can be polled across await points.
            pin_mut!(entries_iter);
            match entries_iter.next().await {
                None => return Err(Error::ExpectedEntry(mid)),
                Some(Err(e)) => return Err(e),
                Some(Ok(got_entry)) => {
                    let got_timestamp = got_entry.timestamp;
                    use std::cmp::Ordering::*;
                    match got_timestamp.cmp(&timestamp) {
                        Equal => return Ok(Some((mid, got_entry))),
                        Less => {
                            low = mid + 1;
                        }
                        Greater => {
                            last_leaf = Some((mid, got_entry));
                            high = mid;
                        }
                    }
                }
            }
        }
        if low > self.latest_size {
            Ok(None)
        } else {
            Ok(Some(last_leaf.unwrap()))
        }
    }

    pub async fn first_tree_head_after(
        &self,
        timestamp: u64,
    ) -> Result<Option<(u64, [u8; 32])>, Error> {
        let fla = self.first_leaf_after(timestamp).await?;
        if fla.is_none() {
            return Ok(None);
        }
        let fla = fla.unwrap();
        let tsize = fla.0 + 1;
        let inclusion_res =
            fetch_inclusion_proof(&self.http_client, &self.base_url, tsize, &fla.1.hash).await?;
        if inclusion_res.leaf_index != fla.0 {
            return Err(Error::Unknown(
                "inclusion result.leaf_index != expected".to_owned(),
            ));
        }
        Ok(Some((tsize, inclusion_res.calculated_tree_hash)))
    }

    pub async fn rollback_to_timestamp(&mut self, timestamp: u64) -> Result<(), Error> {
        let res = self.first_tree_head_after(timestamp).await?;
        if res.is_none() {
            return Ok(());
        }
        let (tsize, thash) = res.unwrap();
        if tsize < self.latest_size {
            check_consistency_proof(
                &self.http_client,
                &self.base_url,
                tsize,
                self.latest_size,
                &thash,
                &self.latest_tree_hash,
            )
            .await?;
            self.latest_size = tsize;
            self.latest_tree_hash = thash;
            info!(
                "{}: Rolled back to {} {}",
                self.base_url.as_str(),
                tsize,
                utils::u8_to_hex(&thash)
            );
        }
        Ok(())
    }

    /// Serialize the state of this client into bytes
    pub fn as_bytes(&self) -> Result<Vec<u8>, Error> {
        // Scheme: (All integers are in big-endian, fixed array don't specify length)
        // [Version: u8] [base_url in UTF-8] 0x00 [tree_size: u64] [tree_hash: [u8; 32]] [len of pub_key: u32] [pub_key: [u8]: DER public key for this log] [sha256 of everything seen before: [u8; 32]]
        let mut v = Vec::new();
        v.push(0u8); // Version = development
        let url_bytes = self.base_url.as_str().as_bytes();
        assert!(!url_bytes.contains(&0u8));
        v.extend_from_slice(url_bytes);
        v.push(0u8);
        v.extend_from_slice(&u64::to_be_bytes(self.latest_size));
        assert_eq!(self.latest_tree_hash.len(), 32);
        v.extend_from_slice(&self.latest_tree_hash);
        let pub_key = self
            .pub_key
            .public_key_to_der()
            .map_err(|e| Error::Unknown(format!("While encoding public key: {}", &e)))?;
        assert!(pub_key.len() < u32::MAX as usize);
        v.extend_from_slice(&u32::to_be_bytes(pub_key.len() as u32));
        v.extend_from_slice(&pub_key);
        v.extend_from_slice(&utils::sha256(&v));
        Ok(v)
    }

    /// Parse a byte string returned by [`Self::as_bytes`](CTClient::as_bytes).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        use std::convert::TryInto;
        fn e_inval() -> Result<CTClient, Error> {
            Err(Error::InvalidArgument("The bytes are invalid.".to_owned()))
        }
        let mut input = bytes;
        if input.is_empty() {
            return e_inval();
        }
        let version = input[0];
        input = &input[1..];
        if version != 0 {
            return Err(Error::InvalidArgument(
                "The bytes are encoded by a ctclient of higher version.".to_owned(),
            ));
        }
        let base_url_len = match input.iter().position(|x| *x == 0) {
            Some(k) => k,
            None => return e_inval(),
        };
        let base_url = std::str::from_utf8(&input[..base_url_len])
            .map_err(|e| Error::InvalidArgument(format!("Invalid UTF-8 in base_url: {}", &e)))?;
        input = &input[base_url_len + 1..];
        if input.len() < 8 {
            return e_inval();
        }
        let tree_size = u64::from_be_bytes(input[..8].try_into().unwrap());
        input = &input[8..];
        if input.len() < 32 {
            return e_inval();
        }
        let tree_hash: [u8; 32] = input[..32].try_into().unwrap();
        input = &input[32..];
        if input.len() < 4 {
            return e_inval();
        }
        let len_pub_key = u32::from_be_bytes(input[..4].try_into().unwrap());
        input = &input[4..];
        if input.len() < len_pub_key as usize {
            return e_inval();
        }
        let pub_key = &input[..len_pub_key as usize];
        input = &input[len_pub_key as usize..];
        if input.len() < 32 {
            return e_inval();
        }
        let checksum: [u8; 32] = input[..32].try_into().unwrap();
        input = &input[32..];
        if !input.is_empty() {
            return e_inval();
        }
        let expect_checksum = utils::sha256(&bytes[..bytes.len() - 32]);
        #[cfg(not(fuzzing))]
        {
            if checksum != expect_checksum {
                return e_inval();
            }
        }
        let pub_key = openssl::pkey::PKey::<openssl::pkey::Public>::public_key_from_der(pub_key)
            .map_err(|e| Error::InvalidArgument(format!("Can't parse public key: {}", &e)))?;
        Ok(CTClient {
            base_url: reqwest::Url::parse(base_url)
                .map_err(|e| Error::InvalidArgument(format!("Unable to parse base_url: {}", &e)))?,
            pub_key,
            http_client: new_http_client()?,
            latest_size: tree_size,
            latest_tree_hash: tree_hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn as_bytes_test() {
        let c = CTClient::new_from_latest_th("https://ct.googleapis.com/logs/argon2019/", &utils::hex_to_u8("3059301306072a8648ce3d020106082a8648ce3d030107034200042373109be1f35ef6986b6995961078ce49dbb404fc712c5a92606825c04a1aa1b0612d1b8714a9baf00133591d0530e94215e755d72af8b4a2ba45c946918756")).await.unwrap();
        let mut bytes = c.as_bytes().unwrap();
        println!("bytes: {}", &base64::encode(&bytes));
        let mut c_clone = CTClient::from_bytes(&bytes).unwrap();
        assert_eq!(c.latest_size, c_clone.latest_size);
        assert_eq!(c.latest_tree_hash, c_clone.latest_tree_hash);
        assert_eq!(c.base_url, c_clone.base_url);
        c_clone.light_update().await.unwrap(); // test public key
        let len = bytes.len();
        bytes[len - 1] ^= 1;
        CTClient::from_bytes(&bytes).expect_err("");
    }
}

#[cfg(test)]
mod long_tests;
