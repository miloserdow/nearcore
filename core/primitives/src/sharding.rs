use crate::bandwidth_scheduler::BandwidthRequests;
use crate::congestion_info::CongestionInfo;
use crate::hash::{CryptoHash, hash};
use crate::merkle::{MerklePath, combine_hash, merklize, verify_path};
use crate::receipt::Receipt;
use crate::transaction::SignedTransaction;
use crate::types::validator_stake::{ValidatorStake, ValidatorStakeIter, ValidatorStakeV1};
use crate::types::{Balance, BlockHeight, Gas, MerkleHash, ShardId, StateRoot};
use crate::validator_signer::{EmptyValidatorSigner, ValidatorSigner};
use crate::version::ProtocolVersion;
use borsh::{BorshDeserialize, BorshSerialize};
use near_crypto::Signature;
use near_fmt::AbbrBytes;
use near_primitives_core::version::{PROTOCOL_VERSION, ProtocolFeature};
use near_schema_checker_lib::ProtocolSchema;
use shard_chunk_header_inner::ShardChunkHeaderInnerV4;
use std::cmp::Ordering;
use std::sync::Arc;
use tracing::debug_span;

#[derive(
    BorshSerialize,
    BorshDeserialize,
    Hash,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Clone,
    Debug,
    Default,
    serde::Serialize,
    serde::Deserialize,
    ProtocolSchema,
)]
pub struct ChunkHash(pub CryptoHash);

impl ChunkHash {
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

impl AsRef<[u8]> for ChunkHash {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl From<ChunkHash> for Vec<u8> {
    fn from(chunk_hash: ChunkHash) -> Self {
        chunk_hash.0.into()
    }
}

impl From<CryptoHash> for ChunkHash {
    fn from(crypto_hash: CryptoHash) -> Self {
        Self(crypto_hash)
    }
}

/// This version of the type is used in the old state sync, where we sync to the state right before the new epoch
#[derive(Clone, Debug, PartialEq, BorshSerialize, BorshDeserialize)]
pub struct StateSyncInfoV0 {
    /// The "sync_hash" block referred to in the state sync algorithm. This is the first block of the
    /// epoch we want to state sync for. This field is not strictly required since this struct is keyed
    /// by this hash in the database, but it's a small amount of data that makes the info in this type more complete.
    pub sync_hash: CryptoHash,
    /// Shards to fetch state
    pub shards: Vec<ShardId>,
}

/// This version of the type is used when syncing to the current epoch's state, and `sync_hash` is an
/// Option because it is not known at the beginning of the epoch, but only until a few more blocks are produced.
#[derive(Clone, Debug, PartialEq, BorshSerialize, BorshDeserialize)]
pub struct StateSyncInfoV1 {
    /// The first block of the epoch we want to state sync for. This field is not strictly required since
    /// this struct is keyed by this hash in the database, but it's a small amount of data that makes
    /// the info in this type more complete.
    pub epoch_first_block: CryptoHash,
    /// The block we'll use as the "sync_hash" when state syncing. Previously, state sync
    /// used the first block of an epoch as the "sync_hash", and synced state to the epoch before.
    /// Now that state sync downloads the state of the current epoch, we need to wait a few blocks
    /// after applying the first block in an epoch to know what "sync_hash" we'll use, so this field
    /// is first set to None until we find the right "sync_hash".
    pub sync_hash: Option<CryptoHash>,
    /// Shards to fetch state
    pub shards: Vec<ShardId>,
}

/// Contains the information that is used to sync state for shards as epochs switch
/// Currently there is only one version possible, but an improvement we might want to make in the future
/// is that when syncing to the current epoch's state, we currently wait for two new chunks in each shard, but
/// with some changes to the meaning of the "sync_hash", we should only need to wait for one. So this is included
/// in order to allow for this change in the future without needing another database migration.
#[derive(Clone, Debug, PartialEq, BorshSerialize, BorshDeserialize)]
pub enum StateSyncInfo {
    /// Old state sync: sync to the state right before the new epoch
    V0(StateSyncInfoV0),
    /// New state sync: sync to the state right after the new epoch
    V1(StateSyncInfoV1),
}

impl StateSyncInfo {
    pub fn new(epoch_first_block: CryptoHash, shards: Vec<ShardId>) -> Self {
        Self::V1(StateSyncInfoV1 { epoch_first_block, sync_hash: None, shards })
    }

    /// Block hash that identifies this state sync struct on disk
    pub fn epoch_first_block(&self) -> &CryptoHash {
        match self {
            Self::V0(info) => &info.sync_hash,
            Self::V1(info) => &info.epoch_first_block,
        }
    }

    pub fn shards(&self) -> &[ShardId] {
        match self {
            Self::V0(info) => &info.shards,
            Self::V1(info) => &info.shards,
        }
    }
}

pub mod shard_chunk_header_inner;
pub use shard_chunk_header_inner::{
    ShardChunkHeaderInner, ShardChunkHeaderInnerV1, ShardChunkHeaderInnerV2,
    ShardChunkHeaderInnerV3,
};

#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug, ProtocolSchema)]
#[borsh(init=init)]
pub struct ShardChunkHeaderV1 {
    pub inner: ShardChunkHeaderInnerV1,

    pub height_included: BlockHeight,

    /// Signature of the chunk producer.
    pub signature: Signature,

    #[borsh(skip)]
    pub hash: ChunkHash,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug, ProtocolSchema)]
#[borsh(init=init)]
pub struct ShardChunkHeaderV2 {
    pub inner: ShardChunkHeaderInnerV1,

    pub height_included: BlockHeight,

    /// Signature of the chunk producer.
    pub signature: Signature,

    #[borsh(skip)]
    pub hash: ChunkHash,
}

impl ShardChunkHeaderV2 {
    pub fn init(&mut self) {
        self.hash = Self::compute_hash(&self.inner);
    }

    pub fn compute_hash(inner: &ShardChunkHeaderInnerV1) -> ChunkHash {
        let inner_bytes = borsh::to_vec(&inner).expect("Failed to serialize");
        let inner_hash = hash(&inner_bytes);

        ChunkHash(combine_hash(&inner_hash, &inner.encoded_merkle_root))
    }

    pub fn new(
        prev_block_hash: CryptoHash,
        prev_state_root: StateRoot,
        prev_outcome_root: CryptoHash,
        encoded_merkle_root: CryptoHash,
        encoded_length: u64,
        height: BlockHeight,
        shard_id: ShardId,
        prev_gas_used: Gas,
        gas_limit: Gas,
        prev_balance_burnt: Balance,
        prev_outgoing_receipts_root: CryptoHash,
        tx_root: CryptoHash,
        prev_validator_proposals: Vec<ValidatorStakeV1>,
        signer: &ValidatorSigner,
    ) -> Self {
        let inner = ShardChunkHeaderInnerV1 {
            prev_block_hash,
            prev_state_root,
            prev_outcome_root,
            encoded_merkle_root,
            encoded_length,
            height_created: height,
            shard_id,
            prev_gas_used,
            gas_limit,
            prev_balance_burnt,
            prev_outgoing_receipts_root,
            tx_root,
            prev_validator_proposals,
        };
        let hash = Self::compute_hash(&inner);
        let signature = signer.sign_bytes(hash.as_ref());
        Self { inner, height_included: 0, signature, hash }
    }
}

// V2 -> V3: Use versioned ShardChunkHeaderInner structure
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug, ProtocolSchema)]
#[borsh(init=init)]
pub struct ShardChunkHeaderV3 {
    pub inner: ShardChunkHeaderInner,

    pub height_included: BlockHeight,

    /// Signature of the chunk producer.
    pub signature: Signature,

    #[borsh(skip)]
    pub hash: ChunkHash,
}

impl ShardChunkHeaderV3 {
    pub fn init(&mut self) {
        self.hash = Self::compute_hash(&self.inner);
    }

    pub fn compute_hash(inner: &ShardChunkHeaderInner) -> ChunkHash {
        let inner_bytes = borsh::to_vec(&inner).expect("Failed to serialize");
        let inner_hash = hash(&inner_bytes);

        ChunkHash(combine_hash(&inner_hash, inner.encoded_merkle_root()))
    }

    pub fn new(
        protocol_version: ProtocolVersion,
        prev_block_hash: CryptoHash,
        prev_state_root: StateRoot,
        prev_outcome_root: CryptoHash,
        encoded_merkle_root: CryptoHash,
        encoded_length: u64,
        height: BlockHeight,
        shard_id: ShardId,
        prev_gas_used: Gas,
        gas_limit: Gas,
        prev_balance_burnt: Balance,
        prev_outgoing_receipts_root: CryptoHash,
        tx_root: CryptoHash,
        prev_validator_proposals: Vec<ValidatorStake>,
        congestion_info: Option<CongestionInfo>,
        bandwidth_requests: Option<BandwidthRequests>,
        signer: &ValidatorSigner,
    ) -> Self {
        let inner = if let Some(bandwidth_requests) = bandwidth_requests {
            // `bandwidth_requests` can only be `Some` when bandwidth scheduler is enabled.
            assert!(ProtocolFeature::BandwidthScheduler.enabled(protocol_version));

            // Congestion control has to be enabled before bandwidth scheduler
            assert!(ProtocolFeature::CongestionControl.enabled(protocol_version));
            ShardChunkHeaderInner::V4(ShardChunkHeaderInnerV4 {
                prev_block_hash,
                prev_state_root,
                prev_outcome_root,
                encoded_merkle_root,
                encoded_length,
                height_created: height,
                shard_id,
                prev_gas_used,
                gas_limit,
                prev_balance_burnt,
                prev_outgoing_receipts_root,
                tx_root,
                prev_validator_proposals,
                congestion_info: congestion_info
                    .expect("Congestion info must exist when bandwidth scheduler is enabled"),
                bandwidth_requests,
            })
        } else if let Some(congestion_info) = congestion_info {
            // `congestion_info`` can only be `Some` when congestion control is enabled.
            assert!(ProtocolFeature::CongestionControl.enabled(protocol_version));
            ShardChunkHeaderInner::V3(ShardChunkHeaderInnerV3 {
                prev_block_hash,
                prev_state_root,
                prev_outcome_root,
                encoded_merkle_root,
                encoded_length,
                height_created: height,
                shard_id,
                prev_gas_used,
                gas_limit,
                prev_balance_burnt,
                prev_outgoing_receipts_root,
                tx_root,
                prev_validator_proposals,
                congestion_info,
            })
        } else {
            ShardChunkHeaderInner::V2(ShardChunkHeaderInnerV2 {
                prev_block_hash,
                prev_state_root,
                prev_outcome_root,
                encoded_merkle_root,
                encoded_length,
                height_created: height,
                shard_id,
                prev_gas_used,
                gas_limit,
                prev_balance_burnt,
                prev_outgoing_receipts_root,
                tx_root,
                prev_validator_proposals,
            })
        };
        Self::from_inner(inner, signer)
    }

    pub fn from_inner(inner: ShardChunkHeaderInner, signer: &ValidatorSigner) -> Self {
        let hash = Self::compute_hash(&inner);
        let signature = signer.sign_bytes(hash.as_ref());
        Self { inner, height_included: 0, signature, hash }
    }
}

#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug, ProtocolSchema)]
pub enum ShardChunkHeader {
    V1(ShardChunkHeaderV1),
    V2(ShardChunkHeaderV2),
    V3(ShardChunkHeaderV3),
}

impl ShardChunkHeader {
    pub fn new_dummy(height: BlockHeight, shard_id: ShardId, prev_block_hash: CryptoHash) -> Self {
        let congestion_info = ProtocolFeature::CongestionControl
            .enabled(PROTOCOL_VERSION)
            .then_some(CongestionInfo::default());

        ShardChunkHeader::V3(ShardChunkHeaderV3::new(
            PROTOCOL_VERSION,
            prev_block_hash,
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            height,
            shard_id,
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            congestion_info,
            BandwidthRequests::default_for_protocol_version(PROTOCOL_VERSION),
            &EmptyValidatorSigner::default().into(),
        ))
    }

    #[inline]
    pub fn take_inner(self) -> ShardChunkHeaderInner {
        match self {
            Self::V1(header) => ShardChunkHeaderInner::V1(header.inner),
            Self::V2(header) => ShardChunkHeaderInner::V1(header.inner),
            Self::V3(header) => header.inner,
        }
    }

    pub fn inner_header_hash(&self) -> CryptoHash {
        let inner_bytes = match self {
            Self::V1(header) => borsh::to_vec(&header.inner),
            Self::V2(header) => borsh::to_vec(&header.inner),
            Self::V3(header) => borsh::to_vec(&header.inner),
        };
        hash(&inner_bytes.expect("Failed to serialize"))
    }

    /// Height at which the chunk was created.
    /// TODO: this is always `height(prev_block_hash) + 1`. Consider using
    /// `prev_block_height` instead as this is more explicit and
    /// `height_created` also conflicts with `height_included`.
    #[inline]
    pub fn height_created(&self) -> BlockHeight {
        match self {
            Self::V1(header) => header.inner.height_created,
            Self::V2(header) => header.inner.height_created,
            Self::V3(header) => header.inner.height_created(),
        }
    }

    #[inline]
    pub fn signature(&self) -> &Signature {
        match self {
            Self::V1(header) => &header.signature,
            Self::V2(header) => &header.signature,
            Self::V3(header) => &header.signature,
        }
    }

    #[inline]
    pub fn height_included(&self) -> BlockHeight {
        match self {
            Self::V1(header) => header.height_included,
            Self::V2(header) => header.height_included,
            Self::V3(header) => header.height_included,
        }
    }

    #[inline]
    pub fn height_included_mut(&mut self) -> &mut BlockHeight {
        match self {
            Self::V1(header) => &mut header.height_included,
            Self::V2(header) => &mut header.height_included,
            Self::V3(header) => &mut header.height_included,
        }
    }

    pub fn is_new_chunk(&self, block_height: BlockHeight) -> bool {
        self.height_included() == block_height
    }

    #[inline]
    pub fn prev_validator_proposals(&self) -> ValidatorStakeIter {
        match self {
            Self::V1(header) => ValidatorStakeIter::v1(&header.inner.prev_validator_proposals),
            Self::V2(header) => ValidatorStakeIter::v1(&header.inner.prev_validator_proposals),
            Self::V3(header) => header.inner.prev_validator_proposals(),
        }
    }

    #[inline]
    pub fn prev_state_root(&self) -> StateRoot {
        match self {
            Self::V1(header) => header.inner.prev_state_root,
            Self::V2(header) => header.inner.prev_state_root,
            Self::V3(header) => *header.inner.prev_state_root(),
        }
    }

    #[inline]
    pub fn prev_block_hash(&self) -> &CryptoHash {
        match self {
            Self::V1(header) => &header.inner.prev_block_hash,
            Self::V2(header) => &header.inner.prev_block_hash,
            Self::V3(header) => header.inner.prev_block_hash(),
        }
    }

    #[inline]
    pub fn is_genesis(&self) -> bool {
        self.prev_block_hash() == &CryptoHash::default()
    }

    #[inline]
    pub fn encoded_merkle_root(&self) -> CryptoHash {
        match self {
            Self::V1(header) => header.inner.encoded_merkle_root,
            Self::V2(header) => header.inner.encoded_merkle_root,
            Self::V3(header) => *header.inner.encoded_merkle_root(),
        }
    }

    #[inline]
    pub fn shard_id(&self) -> ShardId {
        match self {
            Self::V1(header) => header.inner.shard_id,
            Self::V2(header) => header.inner.shard_id,
            Self::V3(header) => header.inner.shard_id(),
        }
    }

    #[inline]
    pub fn encoded_length(&self) -> u64 {
        match self {
            Self::V1(header) => header.inner.encoded_length,
            Self::V2(header) => header.inner.encoded_length,
            Self::V3(header) => header.inner.encoded_length(),
        }
    }

    #[inline]
    pub fn prev_gas_used(&self) -> Gas {
        match &self {
            ShardChunkHeader::V1(header) => header.inner.prev_gas_used,
            ShardChunkHeader::V2(header) => header.inner.prev_gas_used,
            ShardChunkHeader::V3(header) => header.inner.prev_gas_used(),
        }
    }

    #[inline]
    pub fn gas_limit(&self) -> Gas {
        match &self {
            ShardChunkHeader::V1(header) => header.inner.gas_limit,
            ShardChunkHeader::V2(header) => header.inner.gas_limit,
            ShardChunkHeader::V3(header) => header.inner.gas_limit(),
        }
    }

    #[inline]
    pub fn prev_balance_burnt(&self) -> Balance {
        match &self {
            ShardChunkHeader::V1(header) => header.inner.prev_balance_burnt,
            ShardChunkHeader::V2(header) => header.inner.prev_balance_burnt,
            ShardChunkHeader::V3(header) => header.inner.prev_balance_burnt(),
        }
    }

    #[inline]
    pub fn prev_outgoing_receipts_root(&self) -> CryptoHash {
        match &self {
            ShardChunkHeader::V1(header) => header.inner.prev_outgoing_receipts_root,
            ShardChunkHeader::V2(header) => header.inner.prev_outgoing_receipts_root,
            ShardChunkHeader::V3(header) => *header.inner.prev_outgoing_receipts_root(),
        }
    }

    #[inline]
    pub fn prev_outcome_root(&self) -> CryptoHash {
        match &self {
            ShardChunkHeader::V1(header) => header.inner.prev_outcome_root,
            ShardChunkHeader::V2(header) => header.inner.prev_outcome_root,
            ShardChunkHeader::V3(header) => *header.inner.prev_outcome_root(),
        }
    }

    #[inline]
    pub fn tx_root(&self) -> CryptoHash {
        match &self {
            ShardChunkHeader::V1(header) => header.inner.tx_root,
            ShardChunkHeader::V2(header) => header.inner.tx_root,
            ShardChunkHeader::V3(header) => *header.inner.tx_root(),
        }
    }

    #[inline]
    pub fn chunk_hash(&self) -> ChunkHash {
        match &self {
            ShardChunkHeader::V1(header) => header.hash.clone(),
            ShardChunkHeader::V2(header) => header.hash.clone(),
            ShardChunkHeader::V3(header) => header.hash.clone(),
        }
    }

    /// Congestion info, if the feature is enabled on the chunk, `None` otherwise.
    #[inline]
    pub fn congestion_info(&self) -> Option<CongestionInfo> {
        match self {
            ShardChunkHeader::V1(_) => None,
            ShardChunkHeader::V2(_) => None,
            ShardChunkHeader::V3(header) => header.inner.congestion_info(),
        }
    }

    #[inline]
    pub fn bandwidth_requests(&self) -> Option<&BandwidthRequests> {
        match self {
            ShardChunkHeader::V1(_) | ShardChunkHeader::V2(_) => None,
            ShardChunkHeader::V3(header) => header.inner.bandwidth_requests(),
        }
    }

    /// Returns whether the header is valid for given `ProtocolVersion`.
    pub fn validate_version(
        &self,
        version: ProtocolVersion,
    ) -> Result<(), BadHeaderForProtocolVersionError> {
        const CONGESTION_CONTROL_VERSION: ProtocolVersion =
            ProtocolFeature::CongestionControl.protocol_version();
        const BANDWIDTH_SCHEDULER_VERSION: ProtocolVersion =
            ProtocolFeature::BandwidthScheduler.protocol_version();

        let is_valid = match &self {
            ShardChunkHeader::V1(_) => false,
            ShardChunkHeader::V2(_) => false,
            ShardChunkHeader::V3(header) => match header.inner {
                ShardChunkHeaderInner::V1(_) => version < CONGESTION_CONTROL_VERSION,
                // Note that we allow V2 in the congestion control version.
                // That is because the first chunk where this feature is
                // enabled does not have the congestion info.
                // In bandwidth scheduler version v3 and v4 are allowed. The first chunk in
                // the bandwidth scheduler version will be v3 because the chunk extra for the
                // last chunk of previous version doesn't have bandwidth requests.
                // v2 is also allowed in the bandwidth scheduler version because there
                // are multiple tests which upgrade from an old version directly to the
                // latest version. TODO(#12328) - don't allow InnerV2 in bandwidth scheduler version.
                ShardChunkHeaderInner::V2(_) => true,
                ShardChunkHeaderInner::V3(_) => version >= CONGESTION_CONTROL_VERSION,
                ShardChunkHeaderInner::V4(_) => version >= BANDWIDTH_SCHEDULER_VERSION,
            },
        };

        if is_valid {
            Ok(())
        } else {
            Err(BadHeaderForProtocolVersionError {
                protocol_version: version,
                header_version: self.header_version_number(),
                header_inner_version: self.inner_version_number(),
            })
        }
    }

    /// Used for error messages, use `match` for other code.
    #[inline]
    pub(crate) fn header_version_number(&self) -> u64 {
        match self {
            ShardChunkHeader::V1(_) => 1,
            ShardChunkHeader::V2(_) => 2,
            ShardChunkHeader::V3(_) => 3,
        }
    }

    /// Used for error messages, use `match` for other code.
    #[inline]
    pub(crate) fn inner_version_number(&self) -> u64 {
        match self {
            ShardChunkHeader::V1(v1) => {
                // Shows that Header V1 contains Inner V1
                let _inner_v1: &ShardChunkHeaderInnerV1 = &v1.inner;
                1
            }
            ShardChunkHeader::V2(v2) => {
                // Shows that Header V2 also contains Inner V1, not Inner V2
                let _inner_v1: &ShardChunkHeaderInnerV1 = &v2.inner;
                1
            }
            ShardChunkHeader::V3(v3) => {
                let inner_enum: &ShardChunkHeaderInner = &v3.inner;
                inner_enum.version_number()
            }
        }
    }

    pub fn compute_hash(&self) -> ChunkHash {
        match self {
            ShardChunkHeader::V1(header) => ShardChunkHeaderV1::compute_hash(&header.inner),
            ShardChunkHeader::V2(header) => ShardChunkHeaderV2::compute_hash(&header.inner),
            ShardChunkHeader::V3(header) => ShardChunkHeaderV3::compute_hash(&header.inner),
        }
    }
}

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "Invalid chunk header version for protocol version {protocol_version}. (header: {header_version}, inner: {header_inner_version})"
)]
pub struct BadHeaderForProtocolVersionError {
    pub protocol_version: ProtocolVersion,
    pub header_version: u64,
    pub header_inner_version: u64,
}

#[derive(
    BorshSerialize, BorshDeserialize, Hash, Eq, PartialEq, Clone, Debug, Default, ProtocolSchema,
)]
pub struct ChunkHashHeight(pub ChunkHash, pub BlockHeight);

impl ShardChunkHeaderV1 {
    pub fn init(&mut self) {
        self.hash = Self::compute_hash(&self.inner);
    }

    pub fn chunk_hash(&self) -> ChunkHash {
        self.hash.clone()
    }

    pub fn compute_hash(inner: &ShardChunkHeaderInnerV1) -> ChunkHash {
        let inner_bytes = borsh::to_vec(&inner).expect("Failed to serialize");
        let inner_hash = hash(&inner_bytes);

        ChunkHash(inner_hash)
    }

    pub fn new(
        prev_block_hash: CryptoHash,
        prev_state_root: StateRoot,
        prev_outcome_root: CryptoHash,
        encoded_merkle_root: CryptoHash,
        encoded_length: u64,
        height: BlockHeight,
        shard_id: ShardId,
        prev_gas_used: Gas,
        gas_limit: Gas,
        prev_balance_burnt: Balance,
        prev_outgoing_receipts_root: CryptoHash,
        tx_root: CryptoHash,
        prev_validator_proposals: Vec<ValidatorStakeV1>,
        signer: &ValidatorSigner,
    ) -> Self {
        let inner = ShardChunkHeaderInnerV1 {
            prev_block_hash,
            prev_state_root,
            prev_outcome_root,
            encoded_merkle_root,
            encoded_length,
            height_created: height,
            shard_id,
            prev_gas_used,
            gas_limit,
            prev_balance_burnt,
            prev_outgoing_receipts_root,
            tx_root,
            prev_validator_proposals,
        };
        let hash = Self::compute_hash(&inner);
        let signature = signer.sign_bytes(hash.as_ref());
        Self { inner, height_included: 0, signature, hash }
    }
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Eq, PartialEq, ProtocolSchema)]
pub enum PartialEncodedChunk {
    V1(PartialEncodedChunkV1),
    V2(PartialEncodedChunkV2),
}

impl PartialEncodedChunk {
    pub fn new(
        header: ShardChunkHeader,
        parts: Vec<PartialEncodedChunkPart>,
        prev_outgoing_receipts: Vec<ReceiptProof>,
    ) -> Self {
        match header {
            ShardChunkHeader::V1(header) => {
                Self::V1(PartialEncodedChunkV1 { header, parts, prev_outgoing_receipts })
            }
            header => Self::V2(PartialEncodedChunkV2 { header, parts, prev_outgoing_receipts }),
        }
    }

    pub fn cloned_header(&self) -> ShardChunkHeader {
        match self {
            Self::V1(chunk) => ShardChunkHeader::V1(chunk.header.clone()),
            Self::V2(chunk) => chunk.header.clone(),
        }
    }

    pub fn chunk_hash(&self) -> ChunkHash {
        match self {
            Self::V1(chunk) => chunk.header.hash.clone(),
            Self::V2(chunk) => chunk.header.chunk_hash(),
        }
    }

    pub fn height_included(&self) -> BlockHeight {
        match self {
            Self::V1(chunk) => chunk.header.height_included,
            Self::V2(chunk) => chunk.header.height_included(),
        }
    }

    #[inline]
    pub fn parts(&self) -> &[PartialEncodedChunkPart] {
        match self {
            Self::V1(chunk) => &chunk.parts,
            Self::V2(chunk) => &chunk.parts,
        }
    }

    #[inline]
    pub fn prev_outgoing_receipts(&self) -> &[ReceiptProof] {
        match self {
            Self::V1(chunk) => &chunk.prev_outgoing_receipts,
            Self::V2(chunk) => &chunk.prev_outgoing_receipts,
        }
    }

    #[inline]
    pub fn prev_block(&self) -> &CryptoHash {
        match &self {
            PartialEncodedChunk::V1(chunk) => &chunk.header.inner.prev_block_hash,
            PartialEncodedChunk::V2(chunk) => chunk.header.prev_block_hash(),
        }
    }

    pub fn height_created(&self) -> BlockHeight {
        match self {
            Self::V1(chunk) => chunk.header.inner.height_created,
            Self::V2(chunk) => chunk.header.height_created(),
        }
    }
    pub fn shard_id(&self) -> ShardId {
        match self {
            Self::V1(chunk) => chunk.header.inner.shard_id,
            Self::V2(chunk) => chunk.header.shard_id(),
        }
    }
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Eq, PartialEq, ProtocolSchema)]
pub struct PartialEncodedChunkV2 {
    pub header: ShardChunkHeader,
    pub parts: Vec<PartialEncodedChunkPart>,
    pub prev_outgoing_receipts: Vec<ReceiptProof>,
}

impl From<PartialEncodedChunk> for PartialEncodedChunkV2 {
    fn from(pec: PartialEncodedChunk) -> Self {
        match pec {
            PartialEncodedChunk::V1(chunk) => PartialEncodedChunkV2 {
                header: ShardChunkHeader::V1(chunk.header),
                parts: chunk.parts,
                prev_outgoing_receipts: chunk.prev_outgoing_receipts,
            },
            PartialEncodedChunk::V2(chunk) => chunk,
        }
    }
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Eq, PartialEq, ProtocolSchema)]
pub struct PartialEncodedChunkV1 {
    pub header: ShardChunkHeaderV1,
    pub parts: Vec<PartialEncodedChunkPart>,
    pub prev_outgoing_receipts: Vec<ReceiptProof>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PartialEncodedChunkWithArcReceipts {
    pub header: ShardChunkHeader,
    pub parts: Vec<PartialEncodedChunkPart>,
    pub prev_outgoing_receipts: Vec<Arc<ReceiptProof>>,
}

impl From<PartialEncodedChunkWithArcReceipts> for PartialEncodedChunk {
    fn from(pec: PartialEncodedChunkWithArcReceipts) -> Self {
        Self::V2(PartialEncodedChunkV2 {
            header: pec.header,
            parts: pec.parts,
            prev_outgoing_receipts: pec
                .prev_outgoing_receipts
                .into_iter()
                .map(|r| ReceiptProof::clone(&r))
                .collect(),
        })
    }
}

#[derive(
    BorshSerialize,
    BorshDeserialize,
    Debug,
    Clone,
    Eq,
    PartialEq,
    serde::Deserialize,
    ProtocolSchema,
)]
pub struct ShardProof {
    pub from_shard_id: ShardId,
    pub to_shard_id: ShardId,
    pub proof: MerklePath,
}

#[derive(
    BorshSerialize,
    BorshDeserialize,
    Debug,
    Clone,
    Eq,
    PartialEq,
    serde::Deserialize,
    ProtocolSchema,
)]
/// For each Merkle proof there is a subset of receipts which may be proven.
pub struct ReceiptProof(pub Vec<Receipt>, pub ShardProof);

// Implement ordering to ensure `ReceiptProofs` are ordered consistently,
// because we expect messages with ReceiptProofs to be deterministic.
impl PartialOrd<Self> for ReceiptProof {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ReceiptProof {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.1.from_shard_id, self.1.to_shard_id)
            .cmp(&(other.1.from_shard_id, other.1.to_shard_id))
    }
}

impl ReceiptProof {
    pub fn verify_against_receipt_root(&self, receipt_root: CryptoHash) -> bool {
        let ReceiptProof(shard_receipts, receipt_proof) = self;
        let receipt_hash =
            CryptoHash::hash_borsh(ReceiptList(receipt_proof.to_shard_id, shard_receipts));
        verify_path(receipt_root, &receipt_proof.proof, &receipt_hash)
    }
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Eq, PartialEq, ProtocolSchema)]
pub struct PartialEncodedChunkPart {
    pub part_ord: u64,
    pub part: Box<[u8]>,
    pub merkle_proof: MerklePath,
}

impl std::fmt::Debug for PartialEncodedChunkPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PartialEncodedChunkPart")
            .field("part_ord", &self.part_ord)
            .field("part", &format_args!("{}", AbbrBytes(self.part.as_ref())))
            .field("merkle_proof", &self.merkle_proof)
            .finish()
    }
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Eq, PartialEq, ProtocolSchema)]
pub struct ShardChunkV1 {
    pub chunk_hash: ChunkHash,
    pub header: ShardChunkHeaderV1,
    pub transactions: Vec<SignedTransaction>,
    pub prev_outgoing_receipts: Vec<Receipt>,
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Eq, PartialEq, ProtocolSchema)]
pub struct ShardChunkV2 {
    pub chunk_hash: ChunkHash,
    pub header: ShardChunkHeader,
    pub transactions: Vec<SignedTransaction>,
    pub prev_outgoing_receipts: Vec<Receipt>,
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, Eq, PartialEq, ProtocolSchema)]
pub enum ShardChunk {
    V1(ShardChunkV1),
    V2(ShardChunkV2),
}

impl ShardChunk {
    pub fn with_header(chunk: ShardChunk, header: ShardChunkHeader) -> Option<ShardChunk> {
        match chunk {
            Self::V1(chunk) => match header {
                ShardChunkHeader::V1(header) => Some(ShardChunk::V1(ShardChunkV1 {
                    chunk_hash: header.chunk_hash(),
                    header,
                    transactions: chunk.transactions,
                    prev_outgoing_receipts: chunk.prev_outgoing_receipts,
                })),
                ShardChunkHeader::V2(_) => None,
                ShardChunkHeader::V3(_) => None,
            },
            Self::V2(chunk) => Some(ShardChunk::V2(ShardChunkV2 {
                chunk_hash: header.chunk_hash(),
                header,
                transactions: chunk.transactions,
                prev_outgoing_receipts: chunk.prev_outgoing_receipts,
            })),
        }
    }

    pub fn set_height_included(&mut self, height: BlockHeight) {
        match self {
            Self::V1(chunk) => chunk.header.height_included = height,
            Self::V2(chunk) => *chunk.header.height_included_mut() = height,
        }
    }

    #[inline]
    pub fn height_included(&self) -> BlockHeight {
        match self {
            Self::V1(chunk) => chunk.header.height_included,
            Self::V2(chunk) => chunk.header.height_included(),
        }
    }

    #[inline]
    pub fn height_created(&self) -> BlockHeight {
        match self {
            Self::V1(chunk) => chunk.header.inner.height_created,
            Self::V2(chunk) => chunk.header.height_created(),
        }
    }

    #[inline]
    pub fn prev_block(&self) -> &CryptoHash {
        match &self {
            ShardChunk::V1(chunk) => &chunk.header.inner.prev_block_hash,
            ShardChunk::V2(chunk) => chunk.header.prev_block_hash(),
        }
    }

    #[inline]
    pub fn prev_state_root(&self) -> StateRoot {
        match self {
            Self::V1(chunk) => chunk.header.inner.prev_state_root,
            Self::V2(chunk) => chunk.header.prev_state_root(),
        }
    }

    #[inline]
    pub fn tx_root(&self) -> CryptoHash {
        match self {
            Self::V1(chunk) => chunk.header.inner.tx_root,
            Self::V2(chunk) => chunk.header.tx_root(),
        }
    }

    #[inline]
    pub fn prev_outgoing_receipts_root(&self) -> CryptoHash {
        match self {
            Self::V1(chunk) => chunk.header.inner.prev_outgoing_receipts_root,
            Self::V2(chunk) => chunk.header.prev_outgoing_receipts_root(),
        }
    }

    #[inline]
    pub fn shard_id(&self) -> ShardId {
        match self {
            Self::V1(chunk) => chunk.header.inner.shard_id,
            Self::V2(chunk) => chunk.header.shard_id(),
        }
    }

    #[inline]
    pub fn chunk_hash(&self) -> ChunkHash {
        match self {
            Self::V1(chunk) => chunk.chunk_hash.clone(),
            Self::V2(chunk) => chunk.chunk_hash.clone(),
        }
    }

    #[inline]
    pub fn prev_outgoing_receipts(&self) -> &[Receipt] {
        match self {
            Self::V1(chunk) => &chunk.prev_outgoing_receipts,
            Self::V2(chunk) => &chunk.prev_outgoing_receipts,
        }
    }

    #[inline]
    pub fn to_transactions(&self) -> &[SignedTransaction] {
        match self {
            Self::V1(chunk) => &chunk.transactions,
            Self::V2(chunk) => &chunk.transactions,
        }
    }

    pub fn into_transactions(self) -> Vec<SignedTransaction> {
        match self {
            Self::V1(chunk) => chunk.transactions,
            Self::V2(chunk) => chunk.transactions,
        }
    }

    #[inline]
    pub fn header_hash(&self) -> ChunkHash {
        match self {
            Self::V1(chunk) => chunk.header.chunk_hash(),
            Self::V2(chunk) => chunk.header.chunk_hash(),
        }
    }

    #[inline]
    pub fn prev_block_hash(&self) -> CryptoHash {
        match self {
            Self::V1(chunk) => chunk.header.inner.prev_block_hash,
            Self::V2(chunk) => *chunk.header.prev_block_hash(),
        }
    }

    #[inline]
    pub fn take_header(self) -> ShardChunkHeader {
        match self {
            Self::V1(chunk) => ShardChunkHeader::V1(chunk.header),
            Self::V2(chunk) => chunk.header,
        }
    }

    pub fn cloned_header(&self) -> ShardChunkHeader {
        match self {
            Self::V1(chunk) => ShardChunkHeader::V1(chunk.header.clone()),
            Self::V2(chunk) => chunk.header.clone(),
        }
    }

    pub fn compute_header_hash(&self) -> ChunkHash {
        match self {
            Self::V1(chunk) => ShardChunkHeaderV1::compute_hash(&chunk.header.inner),
            Self::V2(chunk) => chunk.header.compute_hash(),
        }
    }
}

#[derive(
    Default, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq, ProtocolSchema,
)]
pub struct EncodedShardChunkBody {
    pub parts: Vec<Option<Box<[u8]>>>,
}

impl EncodedShardChunkBody {
    pub fn num_fetched_parts(&self) -> usize {
        let mut fetched_parts: usize = 0;

        for part in self.parts.iter() {
            if part.is_some() {
                fetched_parts += 1;
            }
        }

        fetched_parts
    }

    pub fn get_merkle_hash_and_paths(&self) -> (MerkleHash, Vec<MerklePath>) {
        let parts: Vec<&[u8]> =
            self.parts.iter().map(|x| x.as_deref().unwrap()).collect::<Vec<_>>();
        merklize(&parts)
    }
}

#[derive(BorshSerialize, Debug, Clone, ProtocolSchema)]
pub struct ReceiptList<'a>(pub ShardId, pub &'a [Receipt]);

#[derive(BorshSerialize, BorshDeserialize, ProtocolSchema)]
pub struct TransactionReceipt(pub Vec<SignedTransaction>, pub Vec<Receipt>);

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq, ProtocolSchema)]
pub struct EncodedShardChunkV1 {
    pub header: ShardChunkHeaderV1,
    pub content: EncodedShardChunkBody,
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq, ProtocolSchema)]
pub struct EncodedShardChunkV2 {
    pub header: ShardChunkHeader,
    pub content: EncodedShardChunkBody,
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq, ProtocolSchema)]
pub enum EncodedShardChunk {
    V1(EncodedShardChunkV1),
    V2(EncodedShardChunkV2),
}

impl EncodedShardChunk {
    pub fn cloned_header(&self) -> ShardChunkHeader {
        match self {
            Self::V1(chunk) => ShardChunkHeader::V1(chunk.header.clone()),
            Self::V2(chunk) => chunk.header.clone(),
        }
    }

    #[inline]
    pub fn content(&self) -> &EncodedShardChunkBody {
        match self {
            Self::V1(chunk) => &chunk.content,
            Self::V2(chunk) => &chunk.content,
        }
    }

    #[inline]
    pub fn content_mut(&mut self) -> &mut EncodedShardChunkBody {
        match self {
            Self::V1(chunk) => &mut chunk.content,
            Self::V2(chunk) => &mut chunk.content,
        }
    }

    #[inline]
    pub fn shard_id(&self) -> ShardId {
        match self {
            Self::V1(chunk) => chunk.header.inner.shard_id,
            Self::V2(chunk) => chunk.header.shard_id(),
        }
    }

    #[inline]
    pub fn encoded_merkle_root(&self) -> CryptoHash {
        match self {
            Self::V1(chunk) => chunk.header.inner.encoded_merkle_root,
            Self::V2(chunk) => chunk.header.encoded_merkle_root(),
        }
    }

    #[inline]
    pub fn encoded_length(&self) -> u64 {
        match self {
            Self::V1(chunk) => chunk.header.inner.encoded_length,
            Self::V2(chunk) => chunk.header.encoded_length(),
        }
    }

    pub fn from_header(header: ShardChunkHeader, total_parts: usize) -> Self {
        let chunk = EncodedShardChunkV2 {
            header,
            content: EncodedShardChunkBody { parts: vec![None; total_parts] },
        };
        Self::V2(chunk)
    }

    fn decode_transaction_receipts(
        parts: &[Option<Box<[u8]>>],
        encoded_length: u64,
    ) -> Result<TransactionReceipt, std::io::Error> {
        let encoded_data = parts
            .iter()
            .flat_map(|option| option.as_ref().expect("Missing shard").iter())
            .cloned()
            .take(encoded_length as usize)
            .collect::<Vec<u8>>();

        TransactionReceipt::try_from_slice(&encoded_data)
    }

    #[cfg(feature = "solomon")]
    pub fn new(
        prev_block_hash: CryptoHash,
        prev_state_root: StateRoot,
        prev_outcome_root: CryptoHash,
        height: BlockHeight,
        shard_id: ShardId,
        rs: &reed_solomon_erasure::galois_8::ReedSolomon,
        prev_gas_used: Gas,
        gas_limit: Gas,
        prev_balance_burnt: Balance,
        tx_root: CryptoHash,
        prev_validator_proposals: Vec<ValidatorStake>,
        transactions: Vec<SignedTransaction>,
        prev_outgoing_receipts: &[Receipt],
        prev_outgoing_receipts_root: CryptoHash,
        congestion_info: Option<CongestionInfo>,
        bandwidth_requests: Option<BandwidthRequests>,
        signer: &ValidatorSigner,
        protocol_version: ProtocolVersion,
    ) -> (Self, Vec<MerklePath>) {
        let (transaction_receipts_parts, encoded_length) = crate::reed_solomon::reed_solomon_encode(
            rs,
            &TransactionReceipt(transactions, prev_outgoing_receipts.to_vec()),
        );
        let content = EncodedShardChunkBody { parts: transaction_receipts_parts };
        let (encoded_merkle_root, merkle_paths) = content.get_merkle_hash_and_paths();

        let header = ShardChunkHeaderV3::new(
            protocol_version,
            prev_block_hash,
            prev_state_root,
            prev_outcome_root,
            encoded_merkle_root,
            encoded_length as u64,
            height,
            shard_id,
            prev_gas_used,
            gas_limit,
            prev_balance_burnt,
            prev_outgoing_receipts_root,
            tx_root,
            prev_validator_proposals,
            congestion_info,
            bandwidth_requests,
            signer,
        );
        let chunk = EncodedShardChunkV2 { header: ShardChunkHeader::V3(header), content };
        (Self::V2(chunk), merkle_paths)
    }

    pub fn chunk_hash(&self) -> ChunkHash {
        match self {
            Self::V1(chunk) => chunk.header.chunk_hash(),
            Self::V2(chunk) => chunk.header.chunk_hash(),
        }
    }

    fn part_ords_to_parts(
        &self,
        part_ords: Vec<u64>,
        merkle_paths: &[MerklePath],
    ) -> Vec<PartialEncodedChunkPart> {
        let parts = match self {
            Self::V1(chunk) => &chunk.content.parts,
            Self::V2(chunk) => &chunk.content.parts,
        };
        part_ords
            .into_iter()
            .map(|part_ord| PartialEncodedChunkPart {
                part_ord,
                part: parts[part_ord as usize].clone().unwrap(),
                merkle_proof: merkle_paths[part_ord as usize].clone(),
            })
            .collect()
    }

    pub fn create_partial_encoded_chunk(
        &self,
        part_ords: Vec<u64>,
        prev_outgoing_receipts: Vec<ReceiptProof>,
        merkle_paths: &[MerklePath],
    ) -> PartialEncodedChunk {
        let parts = self.part_ords_to_parts(part_ords, merkle_paths);
        match self {
            Self::V1(chunk) => {
                let chunk = PartialEncodedChunkV1 {
                    header: chunk.header.clone(),
                    parts,
                    prev_outgoing_receipts,
                };
                PartialEncodedChunk::V1(chunk)
            }
            Self::V2(chunk) => {
                let chunk = PartialEncodedChunkV2 {
                    header: chunk.header.clone(),
                    parts,
                    prev_outgoing_receipts,
                };
                PartialEncodedChunk::V2(chunk)
            }
        }
    }

    pub fn create_partial_encoded_chunk_with_arc_receipts(
        &self,
        part_ords: Vec<u64>,
        prev_outgoing_receipts: Vec<Arc<ReceiptProof>>,
        merkle_paths: &[MerklePath],
    ) -> PartialEncodedChunkWithArcReceipts {
        let parts = self.part_ords_to_parts(part_ords, merkle_paths);
        let header = match self {
            Self::V1(chunk) => ShardChunkHeader::V1(chunk.header.clone()),
            Self::V2(chunk) => chunk.header.clone(),
        };
        PartialEncodedChunkWithArcReceipts { header, parts, prev_outgoing_receipts }
    }

    pub fn decode_chunk(&self, data_parts: usize) -> Result<ShardChunk, std::io::Error> {
        let _span = debug_span!(
            target: "sharding",
            "decode_chunk",
            data_parts,
            height_included = self.cloned_header().height_included(),
            shard_id = ?self.cloned_header().shard_id(),
            chunk_hash = ?self.chunk_hash())
        .entered();

        let transaction_receipts =
            Self::decode_transaction_receipts(&self.content().parts, self.encoded_length())?;
        match self {
            Self::V1(chunk) => Ok(ShardChunk::V1(ShardChunkV1 {
                chunk_hash: chunk.header.chunk_hash(),
                header: chunk.header.clone(),
                transactions: transaction_receipts.0,
                prev_outgoing_receipts: transaction_receipts.1,
            })),

            Self::V2(chunk) => Ok(ShardChunk::V2(ShardChunkV2 {
                chunk_hash: chunk.header.chunk_hash(),
                header: chunk.header.clone(),
                transactions: transaction_receipts.0,
                prev_outgoing_receipts: transaction_receipts.1,
            })),
        }
    }
}
