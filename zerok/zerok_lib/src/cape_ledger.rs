use crate::{
    cape_state::*,
    ledger::{
        open_aap_audit_memo, open_xfr_audit_memo, traits::*, AAPTransactionKind, AuditError,
        AuditMemoOpening,
    },
    state::ValidationError,
    util::commit::{Commitment, Committable, RawCommitmentBuilder},
};
use jf_aap::{
    keys::{AuditorKeyPair, AuditorPubKey},
    structs::{AssetCode, AssetDefinition, Nullifier, RecordCommitment, RecordOpening},
    TransactionNote,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::iter::repeat;

// A representation of an unauthenticated sparse set of nullifiers (it is "authenticated" by
// querying the ultimate source of truth, the CAPE smart contract). The HashMap maps any nullifier
// to one of 3 states:
//  * Some(true): definitely in the set
//  * Some(false): definitely not in the set
//  * None: outside the sparse domain of this set, query a full node for a definitive answer
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CapeNullifierSet(HashMap<Nullifier, bool>);

impl CapeNullifierSet {
    pub fn get(&self, n: Nullifier) -> Option<bool> {
        self.0.get(&n).cloned()
    }

    pub fn insert(&mut self, n: Nullifier, value: bool) {
        self.0.insert(n, value);
    }
}

impl NullifierSet for CapeNullifierSet {
    type Proof = ();

    fn multi_insert(&mut self, nullifiers: &[(Nullifier, Self::Proof)]) -> Result<(), Self::Proof> {
        for (n, _) in nullifiers {
            self.0.insert(*n, true);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, strum_macros::Display)]
pub enum CapeTransactionKind {
    AAP(AAPTransactionKind),
    Burn,
    Wrap,
}

impl TransactionKind for CapeTransactionKind {
    fn send() -> Self {
        Self::AAP(AAPTransactionKind::send())
    }

    fn receive() -> Self {
        Self::AAP(AAPTransactionKind::receive())
    }

    fn mint() -> Self {
        Self::AAP(AAPTransactionKind::mint())
    }

    fn freeze() -> Self {
        Self::AAP(AAPTransactionKind::freeze())
    }

    fn unfreeze() -> Self {
        Self::AAP(AAPTransactionKind::unfreeze())
    }

    fn unknown() -> Self {
        Self::AAP(AAPTransactionKind::unknown())
    }
}

// CapeTransition models all of the objects which can transition a CAPE ledger. This includes
// transactions, submitted from users to the validator via the relayer, as well as ERC20 wrap
// operations, which are submitted directly to the contract but whose outputs end up being included
// in the next committed block.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CapeTransition {
    Transaction(CapeTransaction),
    Wrap {
        erc20_code: Erc20Code,
        src_addr: EthereumAddr,
        ro: Box<RecordOpening>,
    },
}

impl Committable for CapeTransition {
    fn commit(&self) -> Commitment<Self> {
        RawCommitmentBuilder::new("CapeTransition")
            .var_size_bytes(&bincode::serialize(self).unwrap())
            .finalize()
    }
}

impl Transaction for CapeTransition {
    type NullifierSet = CapeNullifierSet;
    type Hash = Commitment<Self>;
    type Kind = CapeTransactionKind;

    fn aap(note: TransactionNote, _proofs: Vec<()>) -> Self {
        Self::Transaction(CapeTransaction::AAP(note))
    }

    fn open_audit_memo(
        &self,
        assets: &HashMap<AssetCode, AssetDefinition>,
        keys: &HashMap<AuditorPubKey, AuditorKeyPair>,
    ) -> Result<AuditMemoOpening, AuditError> {
        match self {
            Self::Transaction(CapeTransaction::AAP(note)) => {
                open_aap_audit_memo(assets, keys, note)
            }
            Self::Transaction(CapeTransaction::Burn { xfr, .. }) => {
                open_xfr_audit_memo(assets, keys, xfr)
            }
            _ => Err(AuditError::NoAuditMemos),
        }
    }

    fn proven_nullifiers(&self) -> Vec<(Nullifier, ())> {
        let nullifiers = match self {
            Self::Transaction(txn) => txn.nullifiers(),
            Self::Wrap { .. } => Vec::new(),
        };
        nullifiers.into_iter().zip(repeat(())).collect()
    }

    fn output_commitments(&self) -> Vec<RecordCommitment> {
        match self {
            Self::Transaction(txn) => txn.commitments(),
            Self::Wrap { ro, .. } => vec![RecordCommitment::from(&**ro)],
        }
    }

    fn output_openings(&self) -> Option<Vec<RecordOpening>> {
        match self {
            Self::Wrap { ro, .. } => Some(vec![(**ro).clone()]),
            _ => None,
        }
    }

    fn hash(&self) -> Self::Hash {
        self.commit()
    }

    fn kind(&self) -> CapeTransactionKind {
        match self {
            Self::Transaction(CapeTransaction::AAP(txn)) => match txn {
                TransactionNote::Transfer(..) => CapeTransactionKind::send(),
                TransactionNote::Mint(..) => CapeTransactionKind::mint(),
                TransactionNote::Freeze(..) => CapeTransactionKind::freeze(),
            },
            Self::Transaction(CapeTransaction::Burn { .. }) => CapeTransactionKind::Burn,
            Self::Wrap { .. } => CapeTransactionKind::Wrap,
        }
    }

    fn set_proofs(&mut self, _proofs: Vec<()>) {}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapeBlock(Vec<CapeTransition>);

impl Committable for CapeBlock {
    fn commit(&self) -> Commitment<Self> {
        RawCommitmentBuilder::new("CapeBlock")
            .array_field(
                "txns",
                &self.0.iter().map(|x| x.commit()).collect::<Vec<_>>(),
            )
            .finalize()
    }
}

impl Block for CapeBlock {
    type Transaction = CapeTransition;

    fn new(txns: Vec<CapeTransition>) -> Self {
        Self(txns)
    }

    fn txns(&self) -> Vec<CapeTransition> {
        self.0.clone()
    }

    fn add_transaction(&mut self, txn: CapeTransition) -> Result<(), ValidationError> {
        self.0.push(txn);
        Ok(())
    }
}

// In CAPE, we don't do local lightweight validation to check the results of queries. We trust the
// results of Ethereum query services, and our local validator stores just enough information to
// satisfy the Validator interface required by the wallet. Thus, the CAPE integration for the
// Validator interface is actually more Truster than Validator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapeTruster {
    // The current timestamp. The only requirement is that this is a monotonically increasing value,
    // but in this implementation it tracks the number of blocks committed.
    now: u64,
    // Number of records, for generating new UIDs.
    num_records: u64,
}

impl CapeTruster {
    pub fn new(now: u64, num_records: u64) -> Self {
        Self { now, num_records }
    }
}

impl Validator for CapeTruster {
    type StateCommitment = u64;
    type Block = CapeBlock;

    fn now(&self) -> u64 {
        self.now
    }

    fn commit(&self) -> Self::StateCommitment {
        // Our commitment is just the block height of the ledger. Since we are trusting a query
        // service anyways, this can be used to determine a unique ledger state by querying for the
        // state of the ledger at this block index.
        self.now
    }

    fn validate_and_apply(&mut self, block: Self::Block) -> Result<Vec<u64>, ValidationError> {
        // We don't actually do validation here, since in this implementation we trust the query
        // service to provide only valid blocks. Instead, just compute the UIDs of the new records
        // assuming the block successfully validates.
        let mut uids = vec![];
        let mut uid = self.num_records;
        for txn in block.0 {
            for _ in 0..txn.output_len() {
                uids.push(uid);
                uid += 1;
            }
        }
        self.num_records = uid;
        self.now += 1;

        Ok(uids)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CapeLedger;

impl Ledger for CapeLedger {
    type Validator = CapeTruster;
}
