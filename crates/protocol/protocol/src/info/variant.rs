//! Contains the `L1BlockInfoTx` enum, containing different variants of the L1 block info
//! transaction.

use alloy_consensus::Header;
use alloy_eips::{BlockNumHash, eip7840::BlobParams};
use alloy_primitives::{Address, B256, Bytes, Sealable, Sealed, TxKind, U256, address};
use kona_genesis::{RollupConfig, SystemConfig};
use op_alloy_consensus::{DepositSourceDomain, L1InfoDepositSource, TxDeposit};

use crate::{
    BlockInfoError, DecodeError, L1BlockInfoBedrock, L1BlockInfoEcotone, L1BlockInfoIsthmus,
    Predeploys,
};

/// The system transaction gas limit post-Regolith
const REGOLITH_SYSTEM_TX_GAS: u64 = 1_000_000;

/// The depositor address of the L1 info transaction
pub(crate) const L1_INFO_DEPOSITOR_ADDRESS: Address =
    address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001");

/// The [`L1BlockInfoTx`] enum contains variants for the different versions of the L1 block info
/// transaction on OP Stack chains.
///
/// This transaction always sits at the top of the block, and alters the `L1 Block` contract's
/// knowledge of the L1 chain.
#[derive(Debug, Clone, Eq, PartialEq, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum L1BlockInfoTx {
    /// A Bedrock L1 info transaction
    Bedrock(L1BlockInfoBedrock),
    /// An Ecotone L1 info transaction
    Ecotone(L1BlockInfoEcotone),
    /// An Isthmus L1 info transaction
    Isthmus(L1BlockInfoIsthmus),
}

impl L1BlockInfoTx {
    /// Creates a new [`L1BlockInfoTx`] from the given information.
    pub fn try_new(
        rollup_config: &RollupConfig,
        system_config: &SystemConfig,
        sequence_number: u64,
        l1_header: &Header,
        l2_block_time: u64,
    ) -> Result<Self, BlockInfoError> {
        // In the first block of Ecotone, the L1Block contract has not been upgraded yet due to the
        // upgrade transactions being placed after the L1 info transaction. Because of this,
        // for the first block of Ecotone, we send a Bedrock style L1 block info transaction
        if !rollup_config.is_ecotone_active(l2_block_time) ||
            rollup_config.is_first_ecotone_block(l2_block_time)
        {
            return Ok(Self::Bedrock(L1BlockInfoBedrock {
                number: l1_header.number,
                time: l1_header.timestamp,
                base_fee: l1_header.base_fee_per_gas.unwrap_or(0),
                block_hash: l1_header.hash_slow(),
                sequence_number,
                batcher_address: system_config.batcher_address,
                l1_fee_overhead: system_config.overhead,
                l1_fee_scalar: system_config.scalar,
            }));
        }

        // --- Post-Ecotone Operations ---

        let scalar = system_config.scalar.to_be_bytes::<32>();
        let blob_base_fee_scalar = (scalar[0] == L1BlockInfoEcotone::L1_SCALAR)
            .then(|| {
                Ok::<u32, BlockInfoError>(u32::from_be_bytes(
                    scalar[24..28].try_into().map_err(|_| BlockInfoError::L1BlobBaseFeeScalar)?,
                ))
            })
            .transpose()?
            .unwrap_or_default();
        let base_fee_scalar = u32::from_be_bytes(
            scalar[28..32].try_into().map_err(|_| BlockInfoError::BaseFeeScalar)?,
        );

        // Use the `requests_hash` presence in the L1 header to determine if pectra has activated on
        // L1.
        //
        // There was an incident on OP Stack Sepolia chains (03-05-2025) when L1 activated pectra,
        // where the sequencer followed the incorrect chain, using the legacy Cancun blob fee
        // schedule instead of the new Prague blob fee schedule. This portion of the chain was
        // chosen to be canonicalized in favor of the prospect of a deep reorg imposed by the
        // sequencers of the testnet chains. An optional hardfork was introduced for Sepolia only,
        // where if present, activates the use of the Prague blob fee schedule. If the hardfork is
        // not present, and L1 has activated pectra, the Prague blob fee schedule is used
        // immediately.
        let blob_fee_config = l1_header
            .requests_hash
            .and_then(|_| {
                (rollup_config.hardforks.pectra_blob_schedule_time.is_none() ||
                    rollup_config.is_pectra_blob_schedule_active(l1_header.timestamp))
                .then_some(BlobParams::prague())
            })
            .unwrap_or(BlobParams::cancun());

        if rollup_config.is_isthmus_active(l2_block_time) &&
            !rollup_config.is_first_isthmus_block(l2_block_time)
        {
            let operator_fee_scalar = system_config.operator_fee_scalar.unwrap_or_default();
            let operator_fee_constant = system_config.operator_fee_constant.unwrap_or_default();
            return Ok(Self::Isthmus(L1BlockInfoIsthmus {
                number: l1_header.number,
                time: l1_header.timestamp,
                base_fee: l1_header.base_fee_per_gas.unwrap_or(0),
                block_hash: l1_header.hash_slow(),
                sequence_number,
                batcher_address: system_config.batcher_address,
                blob_base_fee: l1_header.blob_fee(blob_fee_config).unwrap_or(1),
                blob_base_fee_scalar,
                base_fee_scalar,
                operator_fee_scalar,
                operator_fee_constant,
            }));
        }

        Ok(Self::Ecotone(L1BlockInfoEcotone {
            number: l1_header.number,
            time: l1_header.timestamp,
            base_fee: l1_header.base_fee_per_gas.unwrap_or(0),
            block_hash: l1_header.hash_slow(),
            sequence_number,
            batcher_address: system_config.batcher_address,
            blob_base_fee: l1_header.blob_fee(blob_fee_config).unwrap_or(1),
            blob_base_fee_scalar,
            base_fee_scalar,
            empty_scalars: false,
            l1_fee_overhead: U256::ZERO,
        }))
    }

    /// Creates a new [`L1BlockInfoTx`] from the given information and returns a typed [`TxDeposit`]
    /// to include at the top of a block.
    pub fn try_new_with_deposit_tx(
        rollup_config: &RollupConfig,
        system_config: &SystemConfig,
        sequence_number: u64,
        l1_header: &Header,
        l2_block_time: u64,
    ) -> Result<(Self, Sealed<TxDeposit>), BlockInfoError> {
        let l1_info =
            Self::try_new(rollup_config, system_config, sequence_number, l1_header, l2_block_time)?;

        let source = DepositSourceDomain::L1Info(L1InfoDepositSource {
            l1_block_hash: l1_info.block_hash(),
            seq_number: sequence_number,
        });

        let mut deposit_tx = TxDeposit {
            source_hash: source.source_hash(),
            from: L1_INFO_DEPOSITOR_ADDRESS,
            to: TxKind::Call(Predeploys::L1_BLOCK_INFO),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 150_000_000,
            is_system_transaction: true,
            input: l1_info.encode_calldata(),
        };

        // With the regolith hardfork, system transactions were deprecated, and we allocate
        // a constant amount of gas for special transactions like L1 block info.
        if rollup_config.is_regolith_active(l2_block_time) {
            deposit_tx.is_system_transaction = false;
            deposit_tx.gas_limit = REGOLITH_SYSTEM_TX_GAS;
        }

        Ok((l1_info, deposit_tx.seal_slow()))
    }

    /// Decodes the [`L1BlockInfoEcotone`] object from Ethereum transaction calldata.
    pub fn decode_calldata(r: &[u8]) -> Result<Self, DecodeError> {
        if r.len() < 4 {
            return Err(DecodeError::MissingSelector);
        }
        // SAFETY: The length of `r` must be at least 4 bytes.
        let mut selector = [0u8; 4];
        selector.copy_from_slice(&r[0..4]);
        match selector {
            L1BlockInfoBedrock::L1_INFO_TX_SELECTOR => {
                L1BlockInfoBedrock::decode_calldata(r).map(Self::Bedrock)
            }
            L1BlockInfoEcotone::L1_INFO_TX_SELECTOR => {
                L1BlockInfoEcotone::decode_calldata(r).map(Self::Ecotone)
            }
            L1BlockInfoIsthmus::L1_INFO_TX_SELECTOR => {
                L1BlockInfoIsthmus::decode_calldata(r).map(Self::Isthmus)
            }
            _ => Err(DecodeError::InvalidSelector),
        }
    }

    /// Returns whether the scalars are empty.
    pub const fn empty_scalars(&self) -> bool {
        match self {
            Self::Bedrock(_) | Self::Isthmus(..) => false,
            Self::Ecotone(L1BlockInfoEcotone { empty_scalars, .. }) => *empty_scalars,
        }
    }

    /// Returns the block hash for the [`L1BlockInfoTx`].
    pub const fn block_hash(&self) -> B256 {
        match self {
            Self::Bedrock(tx) => tx.block_hash,
            Self::Ecotone(tx) => tx.block_hash,
            Self::Isthmus(tx) => tx.block_hash,
        }
    }

    /// Encodes the [`L1BlockInfoTx`] object into Ethereum transaction calldata.
    pub fn encode_calldata(&self) -> Bytes {
        match self {
            Self::Bedrock(bedrock_tx) => bedrock_tx.encode_calldata(),
            Self::Ecotone(ecotone_tx) => ecotone_tx.encode_calldata(),
            Self::Isthmus(isthmus_tx) => isthmus_tx.encode_calldata(),
        }
    }

    /// Returns the L1 [`BlockNumHash`] for the info transaction.
    pub const fn id(&self) -> BlockNumHash {
        match self {
            Self::Ecotone(L1BlockInfoEcotone { number, block_hash, .. }) |
            Self::Bedrock(L1BlockInfoBedrock { number, block_hash, .. }) |
            Self::Isthmus(L1BlockInfoIsthmus { number, block_hash, .. }) => {
                BlockNumHash { number: *number, hash: *block_hash }
            }
        }
    }

    /// Returns the operator fee scalar.
    pub const fn operator_fee_scalar(&self) -> u32 {
        match self {
            Self::Isthmus(L1BlockInfoIsthmus { operator_fee_scalar, .. }) => *operator_fee_scalar,
            _ => 0,
        }
    }

    /// Returns the operator fee constant.
    pub const fn operator_fee_constant(&self) -> u64 {
        match self {
            Self::Isthmus(L1BlockInfoIsthmus { operator_fee_constant, .. }) => {
                *operator_fee_constant
            }
            _ => 0,
        }
    }

    /// Returns the l1 base fee.
    pub fn l1_base_fee(&self) -> U256 {
        match self {
            Self::Bedrock(L1BlockInfoBedrock { base_fee, .. }) |
            Self::Ecotone(L1BlockInfoEcotone { base_fee, .. }) |
            Self::Isthmus(L1BlockInfoIsthmus { base_fee, .. }) => U256::from(*base_fee),
        }
    }

    /// Returns the l1 fee scalar.
    pub fn l1_fee_scalar(&self) -> U256 {
        match self {
            Self::Bedrock(L1BlockInfoBedrock { l1_fee_scalar, .. }) => *l1_fee_scalar,
            Self::Ecotone(L1BlockInfoEcotone { base_fee_scalar, .. }) |
            Self::Isthmus(L1BlockInfoIsthmus { base_fee_scalar, .. }) => {
                U256::from(*base_fee_scalar)
            }
        }
    }

    /// Returns the blob base fee.
    pub fn blob_base_fee(&self) -> U256 {
        match self {
            Self::Bedrock(_) => U256::ZERO,
            Self::Ecotone(L1BlockInfoEcotone { blob_base_fee, .. }) |
            Self::Isthmus(L1BlockInfoIsthmus { blob_base_fee, .. }) => U256::from(*blob_base_fee),
        }
    }

    /// Returns the blob base fee scalar.
    pub fn blob_base_fee_scalar(&self) -> U256 {
        match self {
            Self::Bedrock(_) => U256::ZERO,
            Self::Ecotone(L1BlockInfoEcotone { blob_base_fee_scalar, .. }) |
            Self::Isthmus(L1BlockInfoIsthmus { blob_base_fee_scalar, .. }) => {
                U256::from(*blob_base_fee_scalar)
            }
        }
    }

    /// Returns the L1 fee overhead for the info transaction. After ecotone, this value is ignored.
    pub const fn l1_fee_overhead(&self) -> U256 {
        match self {
            Self::Bedrock(L1BlockInfoBedrock { l1_fee_overhead, .. }) => *l1_fee_overhead,
            Self::Ecotone(L1BlockInfoEcotone { l1_fee_overhead, .. }) => *l1_fee_overhead,
            Self::Isthmus(_) => U256::ZERO,
        }
    }

    /// Returns the batcher address for the info transaction
    pub const fn batcher_address(&self) -> Address {
        match self {
            Self::Bedrock(L1BlockInfoBedrock { batcher_address, .. }) |
            Self::Ecotone(L1BlockInfoEcotone { batcher_address, .. }) |
            Self::Isthmus(L1BlockInfoIsthmus { batcher_address, .. }) => *batcher_address,
        }
    }

    /// Returns the sequence number for the info transaction
    pub const fn sequence_number(&self) -> u64 {
        match self {
            Self::Bedrock(L1BlockInfoBedrock { sequence_number, .. }) |
            Self::Ecotone(L1BlockInfoEcotone { sequence_number, .. }) |
            Self::Isthmus(L1BlockInfoIsthmus { sequence_number, .. }) => *sequence_number,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_utils::{RAW_BEDROCK_INFO_TX, RAW_ECOTONE_INFO_TX, RAW_ISTHMUS_INFO_TX};
    use alloc::{string::ToString, vec::Vec};
    use alloy_primitives::{address, b256};
    use kona_genesis::HardForkConfig;
    use rstest::rstest;

    #[test]
    fn test_l1_block_info_missing_selector() {
        let err = L1BlockInfoTx::decode_calldata(&[]);
        assert_eq!(err, Err(DecodeError::MissingSelector));
    }

    #[test]
    fn test_l1_block_info_tx_invalid_len() {
        let calldata = L1BlockInfoBedrock::L1_INFO_TX_SELECTOR
            .into_iter()
            .chain([0xde, 0xad])
            .collect::<Vec<u8>>();
        let err = L1BlockInfoTx::decode_calldata(&calldata);
        assert!(err.is_err());
        assert_eq!(
            err.err().unwrap().to_string(),
            "Invalid bedrock data length. Expected 260, got 6"
        );

        let calldata = L1BlockInfoEcotone::L1_INFO_TX_SELECTOR
            .into_iter()
            .chain([0xde, 0xad])
            .collect::<Vec<u8>>();
        let err = L1BlockInfoTx::decode_calldata(&calldata);
        assert!(err.is_err());
        assert_eq!(
            err.err().unwrap().to_string(),
            "Invalid ecotone data length. Expected 164, got 6"
        );

        let calldata = L1BlockInfoIsthmus::L1_INFO_TX_SELECTOR
            .into_iter()
            .chain([0xde, 0xad])
            .collect::<Vec<u8>>();
        let err = L1BlockInfoTx::decode_calldata(&calldata);
        assert!(err.is_err());
        assert_eq!(
            err.err().unwrap().to_string(),
            "Invalid isthmus data length. Expected 176, got 6"
        );
    }

    #[test]
    fn test_l1_block_info_tx_block_hash() {
        let bedrock = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock {
            block_hash: b256!("392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc"),
            ..Default::default()
        });
        assert_eq!(
            bedrock.block_hash(),
            b256!("392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc")
        );

        let ecotone = L1BlockInfoTx::Ecotone(L1BlockInfoEcotone {
            block_hash: b256!("1c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add3"),
            ..Default::default()
        });
        assert_eq!(
            ecotone.block_hash(),
            b256!("1c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add3")
        );
    }

    #[test]
    fn test_decode_calldata_invalid_selector() {
        let err = L1BlockInfoTx::decode_calldata(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(err, Err(DecodeError::InvalidSelector));
    }

    #[test]
    fn test_l1_block_info_id() {
        let bedrock = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock {
            number: 123,
            block_hash: b256!("392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc"),
            ..Default::default()
        });
        assert_eq!(
            bedrock.id(),
            BlockNumHash {
                number: 123,
                hash: b256!("392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc")
            }
        );

        let ecotone = L1BlockInfoTx::Ecotone(L1BlockInfoEcotone {
            number: 456,
            block_hash: b256!("1c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add3"),
            ..Default::default()
        });
        assert_eq!(
            ecotone.id(),
            BlockNumHash {
                number: 456,
                hash: b256!("1c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add3")
            }
        );

        let isthmus = L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus {
            number: 101112,
            block_hash: b256!("4f98b83baf52c498b49bfff33e59965b27da7febbea9a2fcc4719d06dc06932a"),
            ..Default::default()
        });
        assert_eq!(
            isthmus.id(),
            BlockNumHash {
                number: 101112,
                hash: b256!("4f98b83baf52c498b49bfff33e59965b27da7febbea9a2fcc4719d06dc06932a")
            }
        );
    }

    #[test]
    fn test_l1_block_info_sequence_number() {
        let bedrock = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock {
            sequence_number: 123,
            ..Default::default()
        });
        assert_eq!(bedrock.sequence_number(), 123);

        let ecotone = L1BlockInfoTx::Ecotone(L1BlockInfoEcotone {
            sequence_number: 456,
            ..Default::default()
        });
        assert_eq!(ecotone.sequence_number(), 456);

        let isthmus = L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus {
            sequence_number: 101112,
            ..Default::default()
        });
        assert_eq!(isthmus.sequence_number(), 101112);
    }

    #[test]
    fn test_operator_fee_constant() {
        let bedrock = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default());
        assert_eq!(bedrock.operator_fee_constant(), 0);

        let ecotone = L1BlockInfoTx::Ecotone(L1BlockInfoEcotone::default());
        assert_eq!(ecotone.operator_fee_constant(), 0);

        let isthmus = L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus {
            operator_fee_constant: 123,
            ..Default::default()
        });
        assert_eq!(isthmus.operator_fee_constant(), 123);
    }

    #[test]
    fn test_operator_fee_scalar() {
        let bedrock = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default());
        assert_eq!(bedrock.operator_fee_scalar(), 0);

        let ecotone = L1BlockInfoTx::Ecotone(L1BlockInfoEcotone::default());
        assert_eq!(ecotone.operator_fee_scalar(), 0);

        let isthmus = L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus {
            operator_fee_scalar: 123,
            ..Default::default()
        });
        assert_eq!(isthmus.operator_fee_scalar(), 123);
    }

    #[test]
    fn test_l1_base_fee() {
        let bedrock =
            L1BlockInfoTx::Bedrock(L1BlockInfoBedrock { base_fee: 123, ..Default::default() });
        assert_eq!(bedrock.l1_base_fee(), U256::from(123));

        let ecotone =
            L1BlockInfoTx::Ecotone(L1BlockInfoEcotone { base_fee: 456, ..Default::default() });
        assert_eq!(ecotone.l1_base_fee(), U256::from(456));

        let isthmus =
            L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus { base_fee: 101112, ..Default::default() });
        assert_eq!(isthmus.l1_base_fee(), U256::from(101112));
    }

    #[test]
    fn test_l1_fee_overhead() {
        let bedrock = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock {
            l1_fee_overhead: U256::from(123),
            ..Default::default()
        });
        assert_eq!(bedrock.l1_fee_overhead(), U256::from(123));

        let ecotone = L1BlockInfoTx::Ecotone(L1BlockInfoEcotone {
            l1_fee_overhead: U256::from(456),
            ..Default::default()
        });
        assert_eq!(ecotone.l1_fee_overhead(), U256::from(456));

        let isthmus = L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus::default());
        assert_eq!(isthmus.l1_fee_overhead(), U256::ZERO);
    }

    #[test]
    fn test_batcher_address() {
        let bedrock = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock {
            batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            ..Default::default()
        });
        assert_eq!(bedrock.batcher_address(), address!("6887246668a3b87f54deb3b94ba47a6f63f32985"));

        let ecotone = L1BlockInfoTx::Ecotone(L1BlockInfoEcotone {
            batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            ..Default::default()
        });
        assert_eq!(ecotone.batcher_address(), address!("6887246668a3b87f54deb3b94ba47a6f63f32985"));

        let isthmus = L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus {
            batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            ..Default::default()
        });
        assert_eq!(isthmus.batcher_address(), address!("6887246668a3b87f54deb3b94ba47a6f63f32985"));
    }

    #[test]
    fn test_l1_fee_scalar() {
        let bedrock = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock {
            l1_fee_scalar: U256::from(123),
            ..Default::default()
        });
        assert_eq!(bedrock.l1_fee_scalar(), U256::from(123));

        let ecotone = L1BlockInfoTx::Ecotone(L1BlockInfoEcotone {
            base_fee_scalar: 456,
            ..Default::default()
        });
        assert_eq!(ecotone.l1_fee_scalar(), U256::from(456));

        let isthmus = L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus {
            base_fee_scalar: 101112,
            ..Default::default()
        });
        assert_eq!(isthmus.l1_fee_scalar(), U256::from(101112));
    }

    #[test]
    fn test_blob_base_fee() {
        let bedrock = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock { ..Default::default() });
        assert_eq!(bedrock.blob_base_fee(), U256::ZERO);

        let ecotone =
            L1BlockInfoTx::Ecotone(L1BlockInfoEcotone { blob_base_fee: 456, ..Default::default() });
        assert_eq!(ecotone.blob_base_fee(), U256::from(456));

        let isthmus = L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus {
            blob_base_fee: 101112,
            ..Default::default()
        });
        assert_eq!(isthmus.blob_base_fee(), U256::from(101112));
    }

    #[test]
    fn test_blob_base_fee_scalar() {
        let bedrock = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock { ..Default::default() });
        assert_eq!(bedrock.blob_base_fee_scalar(), U256::ZERO);

        let ecotone = L1BlockInfoTx::Ecotone(L1BlockInfoEcotone {
            blob_base_fee_scalar: 456,
            ..Default::default()
        });
        assert_eq!(ecotone.blob_base_fee_scalar(), U256::from(456));

        let isthmus = L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus {
            blob_base_fee_scalar: 101112,
            ..Default::default()
        });
        assert_eq!(isthmus.blob_base_fee_scalar(), U256::from(101112));
    }

    #[test]
    fn test_empty_scalars() {
        let bedrock = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock { ..Default::default() });
        assert!(!bedrock.empty_scalars());

        let ecotone = L1BlockInfoTx::Ecotone(L1BlockInfoEcotone {
            empty_scalars: true,
            ..Default::default()
        });
        assert!(ecotone.empty_scalars());

        let ecotone = L1BlockInfoTx::Ecotone(L1BlockInfoEcotone::default());
        assert!(!ecotone.empty_scalars());

        let isthmus = L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus::default());
        assert!(!isthmus.empty_scalars());
    }

    #[test]
    fn test_isthmus_l1_block_info_tx_roundtrip() {
        let expected = L1BlockInfoIsthmus {
            number: 19655712,
            time: 1713121139,
            base_fee: 10445852825,
            block_hash: b256!("1c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add3"),
            sequence_number: 5,
            batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            blob_base_fee: 1,
            blob_base_fee_scalar: 810949,
            base_fee_scalar: 1368,
            operator_fee_scalar: 0xabcd,
            operator_fee_constant: 0xdcba,
        };

        let L1BlockInfoTx::Isthmus(decoded) =
            L1BlockInfoTx::decode_calldata(RAW_ISTHMUS_INFO_TX.as_ref()).unwrap()
        else {
            panic!("Wrong fork");
        };
        assert_eq!(expected, decoded);
        assert_eq!(L1BlockInfoTx::Isthmus(decoded).encode_calldata().as_ref(), RAW_ISTHMUS_INFO_TX);
    }

    #[test]
    fn test_bedrock_l1_block_info_tx_roundtrip() {
        let expected = L1BlockInfoBedrock {
            number: 18334955,
            time: 1697121143,
            base_fee: 10419034451,
            block_hash: b256!("392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc"),
            sequence_number: 4,
            batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            l1_fee_overhead: U256::from(0xbc),
            l1_fee_scalar: U256::from(0xa6fe0),
        };

        let L1BlockInfoTx::Bedrock(decoded) =
            L1BlockInfoTx::decode_calldata(RAW_BEDROCK_INFO_TX.as_ref()).unwrap()
        else {
            panic!("Wrong fork");
        };
        assert_eq!(expected, decoded);
        assert_eq!(L1BlockInfoTx::Bedrock(decoded).encode_calldata().as_ref(), RAW_BEDROCK_INFO_TX);
    }

    #[test]
    fn test_ecotone_l1_block_info_tx_roundtrip() {
        let expected = L1BlockInfoEcotone {
            number: 19655712,
            time: 1713121139,
            base_fee: 10445852825,
            block_hash: b256!("1c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add3"),
            sequence_number: 5,
            batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            blob_base_fee: 1,
            blob_base_fee_scalar: 810949,
            base_fee_scalar: 1368,
            empty_scalars: false,
            l1_fee_overhead: U256::ZERO,
        };

        let L1BlockInfoTx::Ecotone(decoded) =
            L1BlockInfoTx::decode_calldata(RAW_ECOTONE_INFO_TX.as_ref()).unwrap()
        else {
            panic!("Wrong fork");
        };
        assert_eq!(expected, decoded);
        assert_eq!(L1BlockInfoTx::Ecotone(decoded).encode_calldata().as_ref(), RAW_ECOTONE_INFO_TX);
    }

    #[test]
    fn test_try_new_bedrock() {
        let rollup_config = RollupConfig::default();
        let system_config = SystemConfig::default();
        let sequence_number = 0;
        let l1_header = Header::default();
        let l2_block_time = 0;

        let l1_info = L1BlockInfoTx::try_new(
            &rollup_config,
            &system_config,
            sequence_number,
            &l1_header,
            l2_block_time,
        )
        .unwrap();

        let L1BlockInfoTx::Bedrock(l1_info) = l1_info else {
            panic!("Wrong fork");
        };

        assert_eq!(l1_info.number, l1_header.number);
        assert_eq!(l1_info.time, l1_header.timestamp);
        assert_eq!(l1_info.base_fee, { l1_header.base_fee_per_gas.unwrap_or(0) });
        assert_eq!(l1_info.block_hash, l1_header.hash_slow());
        assert_eq!(l1_info.sequence_number, sequence_number);
        assert_eq!(l1_info.batcher_address, system_config.batcher_address);
        assert_eq!(l1_info.l1_fee_overhead, system_config.overhead);
        assert_eq!(l1_info.l1_fee_scalar, system_config.scalar);
    }

    #[test]
    fn test_try_new_ecotone() {
        let rollup_config = RollupConfig {
            hardforks: HardForkConfig { ecotone_time: Some(1), ..Default::default() },
            ..Default::default()
        };
        let system_config = SystemConfig::default();
        let sequence_number = 0;
        let l1_header = Header::default();
        let l2_block_time = 0xFF;

        let l1_info = L1BlockInfoTx::try_new(
            &rollup_config,
            &system_config,
            sequence_number,
            &l1_header,
            l2_block_time,
        )
        .unwrap();

        let L1BlockInfoTx::Ecotone(l1_info) = l1_info else {
            panic!("Wrong fork");
        };

        assert_eq!(l1_info.number, l1_header.number);
        assert_eq!(l1_info.time, l1_header.timestamp);
        assert_eq!(l1_info.base_fee, { l1_header.base_fee_per_gas.unwrap_or(0) });
        assert_eq!(l1_info.block_hash, l1_header.hash_slow());
        assert_eq!(l1_info.sequence_number, sequence_number);
        assert_eq!(l1_info.batcher_address, system_config.batcher_address);
        assert_eq!(l1_info.blob_base_fee, l1_header.blob_fee(BlobParams::cancun()).unwrap_or(1));

        let scalar = system_config.scalar.to_be_bytes::<32>();
        let blob_base_fee_scalar = (scalar[0] == L1BlockInfoEcotone::L1_SCALAR)
            .then(|| {
                u32::from_be_bytes(
                    scalar[24..28].try_into().expect("Failed to parse L1 blob base fee scalar"),
                )
            })
            .unwrap_or_default();
        let base_fee_scalar =
            u32::from_be_bytes(scalar[28..32].try_into().expect("Failed to parse base fee scalar"));
        assert_eq!(l1_info.blob_base_fee_scalar, blob_base_fee_scalar);
        assert_eq!(l1_info.base_fee_scalar, base_fee_scalar);
    }

    #[rstest]
    #[case::fork_active(true, false)]
    #[case::fork_inactive(false, false)]
    #[should_panic]
    #[case::fork_active_wrong_params(true, true)]
    #[should_panic]
    #[case::fork_inactive_wrong_params(false, true)]
    fn test_try_new_ecotone_with_optional_prague_fee_fork(
        #[case] fork_active: bool,
        #[case] use_wrong_params: bool,
    ) {
        let rollup_config = RollupConfig {
            hardforks: HardForkConfig {
                ecotone_time: Some(1),
                pectra_blob_schedule_time: Some(2),
                ..Default::default()
            },
            ..Default::default()
        };
        let system_config = SystemConfig::default();
        let sequence_number = 0;
        let l1_header = Header {
            timestamp: if fork_active { 2 } else { 1 },
            excess_blob_gas: Some(0x5080000),
            blob_gas_used: Some(0x100000),
            requests_hash: Some(B256::ZERO),
            ..Default::default()
        };
        let l2_block_time = 0xFF;

        let l1_info = L1BlockInfoTx::try_new(
            &rollup_config,
            &system_config,
            sequence_number,
            &l1_header,
            l2_block_time,
        )
        .unwrap();

        let L1BlockInfoTx::Ecotone(l1_info) = l1_info else {
            panic!("Wrong fork");
        };

        assert_eq!(l1_info.number, l1_header.number);
        assert_eq!(l1_info.time, l1_header.timestamp);
        assert_eq!(l1_info.base_fee, { l1_header.base_fee_per_gas.unwrap_or(0) });
        assert_eq!(l1_info.block_hash, l1_header.hash_slow());
        assert_eq!(l1_info.sequence_number, sequence_number);
        assert_eq!(l1_info.batcher_address, system_config.batcher_address);
        assert_eq!(
            l1_info.blob_base_fee,
            l1_header
                .blob_fee(if fork_active != use_wrong_params {
                    BlobParams::prague()
                } else {
                    BlobParams::cancun()
                })
                .unwrap_or(1)
        );

        let scalar = system_config.scalar.to_be_bytes::<32>();
        let blob_base_fee_scalar = (scalar[0] == L1BlockInfoEcotone::L1_SCALAR)
            .then(|| {
                u32::from_be_bytes(
                    scalar[24..28].try_into().expect("Failed to parse L1 blob base fee scalar"),
                )
            })
            .unwrap_or_default();
        let base_fee_scalar =
            u32::from_be_bytes(scalar[28..32].try_into().expect("Failed to parse base fee scalar"));
        assert_eq!(l1_info.blob_base_fee_scalar, blob_base_fee_scalar);
        assert_eq!(l1_info.base_fee_scalar, base_fee_scalar);
    }

    #[test]
    fn test_try_new_isthmus_before_pectra_blob_schedule() {
        let rollup_config = RollupConfig {
            hardforks: HardForkConfig {
                isthmus_time: Some(1),
                pectra_blob_schedule_time: Some(1713121140),
                ..Default::default()
            },
            ..Default::default()
        };
        let system_config = SystemConfig {
            batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            operator_fee_scalar: Some(0xabcd),
            operator_fee_constant: Some(0xdcba),
            ..Default::default()
        };
        let sequence_number = 0;
        let l1_header = Header {
            number: 19655712,
            timestamp: 1713121139,
            base_fee_per_gas: Some(10445852825),
            // Assume Pectra is active on L1
            requests_hash: Some(B256::ZERO),
            ..Default::default()
        };
        let l2_block_time = 0xFF;

        let l1_info = L1BlockInfoTx::try_new(
            &rollup_config,
            &system_config,
            sequence_number,
            &l1_header,
            l2_block_time,
        )
        .unwrap();

        assert!(matches!(l1_info, L1BlockInfoTx::Isthmus(_)));

        let scalar = system_config.scalar.to_be_bytes::<32>();
        let blob_base_fee_scalar = (scalar[0] == L1BlockInfoIsthmus::L1_SCALAR)
            .then(|| {
                u32::from_be_bytes(
                    scalar[24..28].try_into().expect("Failed to parse L1 blob base fee scalar"),
                )
            })
            .unwrap_or_default();
        let base_fee_scalar =
            u32::from_be_bytes(scalar[28..32].try_into().expect("Failed to parse base fee scalar"));

        assert_eq!(
            l1_info,
            L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus {
                number: l1_header.number,
                time: l1_header.timestamp,
                base_fee: l1_header.base_fee_per_gas.unwrap_or(0),
                block_hash: l1_header.hash_slow(),
                sequence_number,
                batcher_address: system_config.batcher_address,
                // Expect cancun blob schedule to be used, since pectra blob schedule is scheduled
                // but not active yet.
                blob_base_fee: l1_header.blob_fee(BlobParams::cancun()).unwrap_or(1),
                blob_base_fee_scalar,
                base_fee_scalar,
                operator_fee_scalar: system_config.operator_fee_scalar.unwrap_or_default(),
                operator_fee_constant: system_config.operator_fee_constant.unwrap_or_default(),
            })
        );
    }

    #[test]
    fn test_try_new_isthmus() {
        let rollup_config = RollupConfig {
            hardforks: HardForkConfig { isthmus_time: Some(1), ..Default::default() },
            ..Default::default()
        };
        let system_config = SystemConfig {
            batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            operator_fee_scalar: Some(0xabcd),
            operator_fee_constant: Some(0xdcba),
            ..Default::default()
        };
        let sequence_number = 0;
        let l1_header = Header {
            number: 19655712,
            timestamp: 1713121139,
            base_fee_per_gas: Some(10445852825),
            ..Default::default()
        };
        let l2_block_time = 0xFF;

        let l1_info = L1BlockInfoTx::try_new(
            &rollup_config,
            &system_config,
            sequence_number,
            &l1_header,
            l2_block_time,
        )
        .unwrap();

        assert!(matches!(l1_info, L1BlockInfoTx::Isthmus(_)));

        let scalar = system_config.scalar.to_be_bytes::<32>();
        let blob_base_fee_scalar = (scalar[0] == L1BlockInfoIsthmus::L1_SCALAR)
            .then(|| {
                u32::from_be_bytes(
                    scalar[24..28].try_into().expect("Failed to parse L1 blob base fee scalar"),
                )
            })
            .unwrap_or_default();
        let base_fee_scalar =
            u32::from_be_bytes(scalar[28..32].try_into().expect("Failed to parse base fee scalar"));

        assert_eq!(
            l1_info,
            L1BlockInfoTx::Isthmus(L1BlockInfoIsthmus {
                number: l1_header.number,
                time: l1_header.timestamp,
                base_fee: l1_header.base_fee_per_gas.unwrap_or(0),
                block_hash: l1_header.hash_slow(),
                sequence_number,
                batcher_address: system_config.batcher_address,
                blob_base_fee: l1_header.blob_fee(BlobParams::prague()).unwrap_or(1),
                blob_base_fee_scalar,
                base_fee_scalar,
                operator_fee_scalar: system_config.operator_fee_scalar.unwrap_or_default(),
                operator_fee_constant: system_config.operator_fee_constant.unwrap_or_default(),
            })
        );
    }

    #[test]
    fn test_try_new_with_deposit_tx() {
        let rollup_config = RollupConfig {
            hardforks: HardForkConfig { isthmus_time: Some(1), ..Default::default() },
            ..Default::default()
        };
        let system_config = SystemConfig {
            batcher_address: address!("6887246668a3b87f54deb3b94ba47a6f63f32985"),
            operator_fee_scalar: Some(0xabcd),
            operator_fee_constant: Some(0xdcba),
            ..Default::default()
        };
        let sequence_number = 0;
        let l1_header = Header {
            number: 19655712,
            timestamp: 1713121139,
            base_fee_per_gas: Some(10445852825),
            ..Default::default()
        };
        let l2_block_time = 0xFF;

        let (l1_info, deposit_tx) = L1BlockInfoTx::try_new_with_deposit_tx(
            &rollup_config,
            &system_config,
            sequence_number,
            &l1_header,
            l2_block_time,
        )
        .unwrap();

        assert!(matches!(l1_info, L1BlockInfoTx::Isthmus(_)));
        assert_eq!(deposit_tx.from, L1_INFO_DEPOSITOR_ADDRESS);
        assert_eq!(deposit_tx.to, TxKind::Call(Predeploys::L1_BLOCK_INFO));
        assert_eq!(deposit_tx.mint, 0);
        assert_eq!(deposit_tx.value, U256::ZERO);
        assert_eq!(deposit_tx.gas_limit, REGOLITH_SYSTEM_TX_GAS);
        assert!(!deposit_tx.is_system_transaction);
        assert_eq!(deposit_tx.input, l1_info.encode_calldata());
    }
}
