//! Durable, owner-authoritative note-share state.
//!
//! A note share is deliberately a capability to read remote content, not an
//! exported copy.  The only content that may live in this record is an
//! explicitly requested frozen summary.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::remote_identity::{
    InstanceId, InstancePublicIdentity, InstanceSigningIdentity, RemoteIdentityError,
    SignedEnvelope,
};

/// The content representation disclosed by a note share.
///
/// `FrozenSummary` is a deliberate copy selected by the owner at share time;
/// it must never cause a fallback fetch of the live note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoteShareContent {
    LiveNote,
    FrozenSummary(String),
}

/// Read-only policy for one recipient's note capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteSharePolicy {
    pub expires_at: DateTime<Utc>,
    pub successful_read_limit: Option<u32>,
    pub re_share_allowed: bool,
}

impl NoteSharePolicy {
    pub fn expires_at(expires_at: DateTime<Utc>) -> Self {
        Self {
            expires_at,
            successful_read_limit: None,
            re_share_allowed: false,
        }
    }

    pub fn with_read_limit(expires_at: DateTime<Utc>, successful_read_limit: u32) -> Self {
        Self {
            expires_at,
            successful_read_limit: Some(successful_read_limit),
            re_share_allowed: false,
        }
    }
}

/// An owner-authoritative, recipient-bound remote note share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteShare {
    pub share_id: Uuid,
    pub note_id: Uuid,
    pub recipient_account_id: Uuid,
    pub content: NoteShareContent,
    pub policy: NoteSharePolicy,
    revoked: bool,
    successful_reads: u32,
    in_flight_reads: HashSet<Uuid>,
}

/// Owner-side record kept by the note-share service. The owner account is
/// intentionally separate from the recipient-bound share so revocation can
/// never be delegated to the recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedNoteShare {
    pub owner_account_id: Uuid,
    pub share: NoteShare,
}

/// Process-local ledger used by the MCP adapter while the CQL adapter is
/// connected. It has the same atomic begin/complete semantics as the durable
/// implementation and is deliberately keyed by opaque share ids.
#[derive(Debug, Default)]
pub struct NoteShareLedger {
    shares: HashMap<Uuid, OwnedNoteShare>,
}

#[derive(Debug, thiserror::Error)]
pub enum NoteShareLedgerError {
    #[error("note share already exists")]
    AlreadyExists,
    #[error("note share not found")]
    NotFound,
    #[error("signed entitlement does not match the requested share")]
    EntitlementMismatch,
    #[error("owner account is not authorized")]
    OwnerMismatch,
    #[error(transparent)]
    Share(#[from] NoteShareError),
    #[error(transparent)]
    Identity(#[from] RemoteIdentityError),
}

impl NoteShareLedger {
    pub fn record(&self, share_id: Uuid) -> Option<&OwnedNoteShare> {
        self.shares.get(&share_id)
    }

    pub fn insert(
        &mut self,
        owner_account_id: Uuid,
        share: NoteShare,
    ) -> Result<(), NoteShareLedgerError> {
        if self.shares.contains_key(&share.share_id) {
            return Err(NoteShareLedgerError::AlreadyExists);
        }
        self.shares.insert(
            share.share_id,
            OwnedNoteShare {
                owner_account_id,
                share,
            },
        );
        Ok(())
    }

    /// Replace a durable snapshot before a read decision. This lets a revoke
    /// made on another owner device take effect immediately instead of waiting
    /// for a process-local cache to expire.
    pub fn replace(&mut self, owner_account_id: Uuid, share: NoteShare) {
        let mut share = share;
        // The durable snapshot deliberately omits in-flight reservations
        // because they cannot survive a restart. Preserve reservations that
        // belong to this live process, otherwise a concurrent read can be
        // stranded between begin and complete when another device's revoke or
        // counter refresh replaces the cache entry.
        if let Some(existing) = self.shares.get(&share.share_id) {
            share.in_flight_reads = existing.share.in_flight_reads.clone();
        }
        self.shares.insert(
            share.share_id,
            OwnedNoteShare {
                owner_account_id,
                share,
            },
        );
    }

    /// Bind a pending share to the account that accepted its gateway
    /// activation. A share created for the mobile flow starts with a nil
    /// recipient and cannot be read until this owner-side step succeeds.
    pub fn bind_recipient(
        &mut self,
        share_id: Uuid,
        owner_account_id: Uuid,
        recipient_account_id: Uuid,
    ) -> Result<(), NoteShareLedgerError> {
        let record = self
            .shares
            .get_mut(&share_id)
            .ok_or(NoteShareLedgerError::NotFound)?;
        if record.owner_account_id != owner_account_id {
            return Err(NoteShareLedgerError::OwnerMismatch);
        }
        record
            .share
            .bind_recipient(recipient_account_id)
            .map_err(NoteShareLedgerError::Share)
    }

    pub fn begin_read(
        &mut self,
        share_id: Uuid,
        recipient_account_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(NoteShareContent, NoteReadAttempt), NoteShareLedgerError> {
        let record = self
            .shares
            .get_mut(&share_id)
            .ok_or(NoteShareLedgerError::NotFound)?;
        let attempt = record.share.begin_read(recipient_account_id, now)?;
        Ok((record.share.content.clone(), attempt))
    }

    /// Verify an entitlement at the receiving peer before reserving a read.
    /// The peer is never trusted to infer scope from a token: every signed
    /// field is checked against the owner record and the authenticated account.
    pub fn begin_read_with_entitlement(
        &mut self,
        share_id: Uuid,
        entitlement: &SignedEnvelope<NoteShareEntitlement>,
        owner_identity: &InstancePublicIdentity,
        recipient_account_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(NoteShareContent, NoteReadAttempt), NoteShareLedgerError> {
        entitlement.verify(owner_identity)?;
        let record = self
            .shares
            .get(&share_id)
            .ok_or(NoteShareLedgerError::NotFound)?;
        let payload = &entitlement.payload;
        if payload.share_id != share_id
            || payload.note_id != record.share.note_id
            || payload.recipient_account_id != recipient_account_id
            || payload.owner_instance_id != owner_identity.instance_id
            || payload.expires_at != record.share.policy.expires_at
            || payload.successful_read_limit != record.share.policy.successful_read_limit
        {
            return Err(NoteShareLedgerError::EntitlementMismatch);
        }
        self.begin_read(share_id, recipient_account_id, now)
    }

    pub fn complete_read(
        &mut self,
        share_id: Uuid,
        attempt: NoteReadAttempt,
        now: DateTime<Utc>,
    ) -> Result<(), NoteShareLedgerError> {
        let record = self
            .shares
            .get_mut(&share_id)
            .ok_or(NoteShareLedgerError::NotFound)?;
        record.share.complete_read(attempt, now)?;
        Ok(())
    }

    pub fn abandon_read(
        &mut self,
        share_id: Uuid,
        attempt: NoteReadAttempt,
    ) -> Result<(), NoteShareLedgerError> {
        let record = self
            .shares
            .get_mut(&share_id)
            .ok_or(NoteShareLedgerError::NotFound)?;
        record.share.abandon_read(attempt)?;
        Ok(())
    }

    pub fn revoke(
        &mut self,
        share_id: Uuid,
        owner_account_id: Uuid,
    ) -> Result<(), NoteShareLedgerError> {
        let record = self
            .shares
            .get_mut(&share_id)
            .ok_or(NoteShareLedgerError::NotFound)?;
        if record.owner_account_id != owner_account_id {
            return Err(NoteShareLedgerError::OwnerMismatch);
        }
        record.share.revoke();
        Ok(())
    }

    pub fn entitlement(
        &self,
        share_id: Uuid,
        recipient_account_id: Uuid,
        signer: &InstanceSigningIdentity,
        now: DateTime<Utc>,
    ) -> Result<SignedEnvelope<NoteShareEntitlement>, NoteShareLedgerError> {
        let record = self
            .shares
            .get(&share_id)
            .ok_or(NoteShareLedgerError::NotFound)?;
        record
            .share
            .authorize(recipient_account_id, now)
            .map_err(NoteShareLedgerError::Share)?;
        record
            .share
            .issue_entitlement(signer)
            .map_err(NoteShareLedgerError::Identity)
    }
}

static GLOBAL_NOTE_SHARES: OnceLock<Mutex<NoteShareLedger>> = OnceLock::new();

/// Return the owner-server ledger used by the MCP serving process.
pub fn global_ledger() -> &'static Mutex<NoteShareLedger> {
    GLOBAL_NOTE_SHARES.get_or_init(|| Mutex::new(NoteShareLedger::default()))
}

/// The signed, recipient-bound assertion released only after activation and
/// acceptance. It contains no note title, body, or personal profile data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteShareEntitlement {
    pub version: u8,
    pub share_id: Uuid,
    pub note_id: Uuid,
    pub owner_instance_id: InstanceId,
    pub recipient_account_id: Uuid,
    pub content_is_frozen_summary: bool,
    pub expires_at: DateTime<Utc>,
    pub successful_read_limit: Option<u32>,
}

/// A non-exportable, single-attempt read reservation.
///
/// Persistence adapters must atomically create and finalize this reservation.
/// The model makes it explicit that transport handoff is not a successful read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteReadAttempt(Uuid);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NoteShareError {
    #[error("the authenticated account is not this share's recipient")]
    RecipientMismatch,
    #[error("the share has expired")]
    Expired,
    #[error("the share has been revoked")]
    Revoked,
    #[error("the share's successful-read limit has been reached")]
    ReadLimitReached,
    #[error("the read attempt is not active")]
    UnknownReadAttempt,
}

impl NoteShare {
    pub fn new(
        share_id: Uuid,
        note_id: Uuid,
        recipient_account_id: Uuid,
        content: NoteShareContent,
        policy: NoteSharePolicy,
    ) -> Self {
        Self {
            share_id,
            note_id,
            recipient_account_id,
            content,
            policy,
            revoked: false,
            successful_reads: 0,
            in_flight_reads: HashSet::new(),
        }
    }

    pub fn begin_read(
        &mut self,
        authenticated_account_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<NoteReadAttempt, NoteShareError> {
        self.authorize(authenticated_account_id, now)?;
        let attempt = NoteReadAttempt(Uuid::new_v4());
        self.in_flight_reads.insert(attempt.0);
        Ok(attempt)
    }

    pub fn bind_recipient(&mut self, recipient_account_id: Uuid) -> Result<(), NoteShareError> {
        if self.recipient_account_id != Uuid::nil() {
            return Err(NoteShareError::RecipientMismatch);
        }
        if recipient_account_id == Uuid::nil() {
            return Err(NoteShareError::RecipientMismatch);
        }
        self.recipient_account_id = recipient_account_id;
        Ok(())
    }

    pub fn complete_read(
        &mut self,
        attempt: NoteReadAttempt,
        now: DateTime<Utc>,
    ) -> Result<(), NoteShareError> {
        if self.revoked {
            self.in_flight_reads.remove(&attempt.0);
            return Err(NoteShareError::Revoked);
        }
        if now >= self.policy.expires_at {
            self.in_flight_reads.remove(&attempt.0);
            return Err(NoteShareError::Expired);
        }
        if !self.in_flight_reads.remove(&attempt.0) {
            return Err(NoteShareError::UnknownReadAttempt);
        }
        self.successful_reads = self.successful_reads.saturating_add(1);
        Ok(())
    }

    pub fn abandon_read(&mut self, attempt: NoteReadAttempt) -> Result<(), NoteShareError> {
        self.in_flight_reads
            .remove(&attempt.0)
            .then_some(())
            .ok_or(NoteShareError::UnknownReadAttempt)
    }

    pub fn revoke(&mut self) {
        self.revoked = true;
        self.in_flight_reads.clear();
    }

    pub fn successful_reads(&self) -> u32 {
        self.successful_reads
    }

    /// Restore the durable success counter after loading a share from CQL.
    /// In-flight reservations are intentionally never restored.
    pub fn restore_successful_reads(&mut self, successful_reads: u32) {
        self.successful_reads = successful_reads;
    }

    pub fn frozen_summary(&self) -> Option<&str> {
        match &self.content {
            NoteShareContent::LiveNote => None,
            NoteShareContent::FrozenSummary(summary) => Some(summary),
        }
    }

    pub fn permits_live_note(&self) -> bool {
        matches!(self.content, NoteShareContent::LiveNote)
    }

    /// Produce the owner-server assertion the recipient presents on each
    /// remote read. The owner remains authoritative for revocation and read
    /// budget checks, so this never grants an export of the note.
    pub fn issue_entitlement(
        &self,
        signer: &InstanceSigningIdentity,
    ) -> Result<SignedEnvelope<NoteShareEntitlement>, RemoteIdentityError> {
        signer.sign(NoteShareEntitlement {
            version: 1,
            share_id: self.share_id,
            note_id: self.note_id,
            owner_instance_id: signer.instance_id,
            recipient_account_id: self.recipient_account_id,
            content_is_frozen_summary: !self.permits_live_note(),
            expires_at: self.policy.expires_at,
            successful_read_limit: self.policy.successful_read_limit,
        })
    }

    fn authorize(
        &self,
        authenticated_account_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), NoteShareError> {
        if self.revoked {
            return Err(NoteShareError::Revoked);
        }
        if authenticated_account_id != self.recipient_account_id {
            return Err(NoteShareError::RecipientMismatch);
        }
        if now >= self.policy.expires_at {
            return Err(NoteShareError::Expired);
        }
        if self.policy.successful_read_limit.is_some_and(|limit| {
            self.successful_reads
                .saturating_add(self.in_flight_reads.len() as u32)
                >= limit
        }) {
            return Err(NoteShareError::ReadLimitReached);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use uuid::Uuid;

    use crate::remote_identity::{InstanceId, InstanceSigningIdentity};

    use super::{NoteShare, NoteShareContent, NoteShareError, NoteShareLedger, NoteSharePolicy};

    #[test]
    fn entitlement_is_signed_and_carries_no_note_content() {
        let recipient = Uuid::new_v4();
        let signer = InstanceSigningIdentity::generate(InstanceId::new());
        let share = NoteShare::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            recipient,
            NoteShareContent::FrozenSummary("summary that is stored only on the owner".to_owned()),
            NoteSharePolicy::with_read_limit(Utc::now() + Duration::hours(1), 2),
        );

        let entitlement = share
            .issue_entitlement(&signer)
            .expect("owner signs entitlement");
        entitlement
            .verify(&signer.public_identity())
            .expect("recipient can verify owner");
        assert_eq!(entitlement.payload.recipient_account_id, recipient);
        assert!(entitlement.payload.content_is_frozen_summary);
        let encoded = serde_json::to_string(&entitlement).expect("serializable envelope");
        assert!(!encoded.contains("summary that is stored only on the owner"));
    }

    #[test]
    fn ledger_enforces_owner_revoke_and_successful_read_budget() {
        let owner = Uuid::new_v4();
        let recipient = Uuid::new_v4();
        let share_id = Uuid::new_v4();
        let mut ledger = NoteShareLedger::default();
        ledger
            .insert(
                owner,
                NoteShare::new(
                    share_id,
                    Uuid::new_v4(),
                    recipient,
                    NoteShareContent::FrozenSummary("summary".into()),
                    NoteSharePolicy::with_read_limit(Utc::now() + Duration::hours(1), 1),
                ),
            )
            .unwrap();

        assert!(matches!(
            ledger.revoke(share_id, Uuid::new_v4()),
            Err(super::NoteShareLedgerError::OwnerMismatch)
        ));
        let now = Utc::now();
        let (_, attempt) = ledger.begin_read(share_id, recipient, now).unwrap();
        ledger.complete_read(share_id, attempt, Utc::now()).unwrap();
        assert!(matches!(
            ledger.begin_read(share_id, recipient, Utc::now()),
            Err(super::NoteShareLedgerError::Share(
                NoteShareError::ReadLimitReached
            ))
        ));
        ledger.revoke(share_id, owner).unwrap();
    }

    #[test]
    fn peer_must_verify_and_match_signed_entitlement_before_reading() {
        let owner = Uuid::new_v4();
        let recipient = Uuid::new_v4();
        let signer = InstanceSigningIdentity::generate(InstanceId::new());
        let share = NoteShare::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            recipient,
            NoteShareContent::FrozenSummary("peer-safe summary".into()),
            NoteSharePolicy::expires_at(Utc::now() + Duration::hours(1)),
        );
        let share_id = share.share_id;
        let entitlement = share.issue_entitlement(&signer).unwrap();
        let mut ledger = NoteShareLedger::default();
        ledger.insert(owner, share).unwrap();
        let (content, attempt) = ledger
            .begin_read_with_entitlement(
                share_id,
                &entitlement,
                &signer.public_identity(),
                recipient,
                Utc::now(),
            )
            .unwrap();
        assert_eq!(
            content,
            NoteShareContent::FrozenSummary("peer-safe summary".into())
        );
        ledger.complete_read(share_id, attempt, Utc::now()).unwrap();
    }

    #[test]
    fn pending_share_must_be_bound_before_read() {
        let owner = Uuid::new_v4();
        let recipient = Uuid::new_v4();
        let share_id = Uuid::new_v4();
        let mut ledger = NoteShareLedger::default();
        ledger
            .insert(
                owner,
                NoteShare::new(
                    share_id,
                    Uuid::new_v4(),
                    Uuid::nil(),
                    NoteShareContent::FrozenSummary("pending".into()),
                    NoteSharePolicy::expires_at(Utc::now() + Duration::hours(1)),
                ),
            )
            .unwrap();
        assert!(ledger.begin_read(share_id, recipient, Utc::now()).is_err());
        ledger.bind_recipient(share_id, owner, recipient).unwrap();
        assert!(ledger.begin_read(share_id, recipient, Utc::now()).is_ok());
    }

    #[test]
    fn durable_refresh_does_not_strand_an_in_flight_read() {
        let owner = Uuid::new_v4();
        let recipient = Uuid::new_v4();
        let share_id = Uuid::new_v4();
        let expires_at = Utc::now() + Duration::hours(1);
        let share = NoteShare::new(
            share_id,
            Uuid::new_v4(),
            recipient,
            NoteShareContent::FrozenSummary("refresh-safe".into()),
            NoteSharePolicy::expires_at(expires_at),
        );
        let mut ledger = NoteShareLedger::default();
        ledger.insert(owner, share.clone()).unwrap();
        let (_, attempt) = ledger.begin_read(share_id, recipient, Utc::now()).unwrap();

        // A remote device's counter/revocation poll refreshes the local
        // snapshot while this read is still being delivered.
        ledger.replace(owner, share);
        ledger.complete_read(share_id, attempt, Utc::now()).unwrap();
        assert_eq!(ledger.record(share_id).unwrap().share.successful_reads(), 1);
    }

    #[test]
    fn full_create_bind_signed_read_revoke_lifecycle() {
        let owner = Uuid::new_v4();
        let recipient = Uuid::new_v4();
        let signer = InstanceSigningIdentity::generate(InstanceId::new());
        let share_id = Uuid::new_v4();
        let mut ledger = NoteShareLedger::default();
        ledger
            .insert(
                owner,
                NoteShare::new(
                    share_id,
                    Uuid::new_v4(),
                    Uuid::nil(),
                    NoteShareContent::FrozenSummary("a bounded note summary".into()),
                    NoteSharePolicy::with_read_limit(Utc::now() + Duration::hours(1), 1),
                ),
            )
            .unwrap();
        ledger.bind_recipient(share_id, owner, recipient).unwrap();
        let entitlement = ledger
            .entitlement(share_id, recipient, &signer, Utc::now())
            .unwrap();
        let (_, attempt) = ledger
            .begin_read_with_entitlement(
                share_id,
                &entitlement,
                &signer.public_identity(),
                recipient,
                Utc::now(),
            )
            .unwrap();
        ledger.complete_read(share_id, attempt, Utc::now()).unwrap();
        ledger.revoke(share_id, owner).unwrap();
        assert!(
            ledger
                .begin_read_with_entitlement(
                    share_id,
                    &entitlement,
                    &signer.public_identity(),
                    recipient,
                    Utc::now(),
                )
                .is_err()
        );
    }

    #[test]
    fn only_the_named_recipient_can_consume_a_live_note_share() {
        let recipient = Uuid::new_v4();
        let mut share = NoteShare::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            recipient,
            NoteShareContent::LiveNote,
            NoteSharePolicy::expires_at(Utc::now() + Duration::hours(1)),
        );

        assert_eq!(
            share.begin_read(Uuid::new_v4(), Utc::now()),
            Err(NoteShareError::RecipientMismatch)
        );
        let read = share
            .begin_read(recipient, Utc::now())
            .expect("recipient may read");
        share
            .complete_read(read, Utc::now())
            .expect("successful read is audited");
        assert_eq!(share.successful_reads(), 1);
    }

    #[test]
    fn only_a_successful_read_consumes_the_budget() {
        let recipient = Uuid::new_v4();
        let mut share = NoteShare::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            recipient,
            NoteShareContent::LiveNote,
            NoteSharePolicy::with_read_limit(Utc::now() + Duration::hours(1), 1),
        );

        let abandoned = share.begin_read(recipient, Utc::now()).expect("may start");
        share
            .abandon_read(abandoned)
            .expect("failed delivery does not consume");
        let delivered = share.begin_read(recipient, Utc::now()).expect("may retry");
        share
            .complete_read(delivered, Utc::now())
            .expect("first successful delivery consumes budget");
        assert_eq!(
            share.begin_read(recipient, Utc::now()),
            Err(NoteShareError::ReadLimitReached)
        );
    }

    #[test]
    fn revocation_terminates_an_in_flight_read_and_future_reads() {
        let recipient = Uuid::new_v4();
        let mut share = NoteShare::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            recipient,
            NoteShareContent::LiveNote,
            NoteSharePolicy::expires_at(Utc::now() + Duration::hours(1)),
        );
        let read = share.begin_read(recipient, Utc::now()).expect("may start");
        share.revoke();

        assert_eq!(
            share.complete_read(read, Utc::now()),
            Err(NoteShareError::Revoked)
        );
        assert_eq!(
            share.begin_read(recipient, Utc::now()),
            Err(NoteShareError::Revoked)
        );
    }

    #[test]
    fn expiration_terminates_an_in_flight_read_without_consuming_budget() {
        let recipient = Uuid::new_v4();
        let expires_at = Utc::now() + Duration::seconds(1);
        let mut share = NoteShare::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            recipient,
            NoteShareContent::LiveNote,
            NoteSharePolicy::with_read_limit(expires_at, 1),
        );
        let read = share.begin_read(recipient, Utc::now()).expect("may start");

        assert_eq!(
            share.complete_read(read, expires_at),
            Err(NoteShareError::Expired)
        );
        assert_eq!(share.successful_reads(), 0);
    }

    #[test]
    fn frozen_summary_never_requires_or_exposes_the_live_note() {
        let summary = "A short, intentional disclosure.".to_owned();
        let share = NoteShare::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            NoteShareContent::FrozenSummary(summary.clone()),
            NoteSharePolicy::expires_at(Utc::now() + Duration::hours(1)),
        );

        assert_eq!(share.frozen_summary(), Some(summary.as_str()));
        assert!(!share.permits_live_note());
    }
}
