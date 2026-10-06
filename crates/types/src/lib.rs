#![no_std]
//! Types shared by the escrow and factory contracts.
//!
//! The factory deploys escrows with an [`EscrowParams`] constructor argument, so
//! both contracts must agree on its shape. Keeping the definitions here means
//! they cannot drift.

use soroban_sdk::{contracttype, Address, BytesN, String};

/// Upper bound on the platform fee, in basis points (10%).
pub const MAX_FEE_BPS: u32 = 1_000;

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Created,
    Funded,
    Delivered,
    Disputed,
    Released,
    Refunded,
    Cancelled,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProofKind {
    /// Carrier tracking reference — physical goods.
    Tracking,
    /// File or artifact hash — digital goods.
    Content,
    /// Seller statement — services and in-person handovers, weakest tier.
    Attestation,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Proof {
    pub kind: ProofKind,
    pub uri: String,
    pub hash: BytesN<32>,
    pub submitted_at: u64,
}

/// How an escrow reached `Released`. Every variant carries evidence from the
/// buyer (code or signature) or a ruling from the arbitrator.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleasePath {
    Code,
    Confirmation,
    Arbitration,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefundPath {
    SellerRefund,
    DeliveryTimeout,
    Arbitration,
    ArbitrationTimeout,
}

/// The only two outcomes an arbitrator can choose.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Release,
    Refund,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisputeOrigin {
    Buyer,
    Seller,
    /// The buyer gave no receipt and no objection before `receipt_deadline`.
    ReceiptTimeout,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dispute {
    pub opened_by: DisputeOrigin,
    pub opened_at: u64,
    pub from_state: State,
    /// The arbitrator must rule before this; afterwards the buyer is refunded.
    pub deadline: u64,
    /// sha256 of the opener's off-chain statement, committed when the
    /// dispute opened. Zero for `ReceiptTimeout`, which has no statement.
    pub statement_hash: BytesN<32>,
    /// sha256 of the arbitrator's written ruling. Zero until `resolve` sets
    /// it; stays zero if the arbitrator never rules and the dispute instead
    /// ends via the arbitration timeout.
    pub ruling_hash: BytesN<32>,
}

/// What the buyer asks the factory to create.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Order {
    pub buyer: Address,
    pub seller: Address,
    pub token: Address,
    pub amount: i128,
    pub terms_hash: BytesN<32>,
    pub release_code_hash: BytesN<32>,
    pub funding_deadline: u64,
    pub delivery_window: u64,
    pub receipt_window: u64,
    pub arbitration_window: u64,
}

/// Escrow constructor argument: the buyer's order plus the operator
/// configuration the factory copies in at creation.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EscrowParams {
    pub order: Order,
    pub arbitrator: Address,
    pub fee_bps: u32,
    pub fee_recipient: Address,
    /// The salt the buyer passed to `Factory::create`. Lets a client prove
    /// provenance: a real factory escrow satisfies
    /// `factory.escrow_address(buyer, salt) == this contract's address`.
    /// Only the factory's deployer address can produce that match, so a
    /// directly-deployed escrow can never pass the check, whatever salt it
    /// claims.
    pub salt: BytesN<32>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Escrow {
    pub buyer: Address,
    pub seller: Address,
    pub arbitrator: Address,
    pub token: Address,
    pub amount: i128,
    pub fee_bps: u32,
    pub fee_recipient: Address,
    pub terms_hash: BytesN<32>,
    pub release_code_hash: BytesN<32>,
    pub state: State,
    pub created_at: u64,
    pub funding_deadline: u64,
    pub delivery_window: u64,
    pub receipt_window: u64,
    pub arbitration_window: u64,
    /// Zero until funded.
    pub funded_at: u64,
    /// Zero until funded. The seller must submit proof before this.
    pub delivery_deadline: u64,
    /// Zero until proof is submitted. The buyer must give receipt or dispute
    /// before this, otherwise anyone may escalate to arbitration.
    pub receipt_deadline: u64,
    pub proof: ProofRecord,
    pub dispute: DisputeRecord,
    pub settlement: Settlement,
    /// Zero unless a fee transfer failed on release (the fee recipient had
    /// no trustline, or was frozen). The seller is still paid in full either
    /// way; this is only ever the platform's own fee, recoverable later with
    /// `sweep_fee`. A terminal escrow holds no tokens except this.
    pub unswept_fee: i128,
    /// The salt `Factory::create` used to deploy this escrow. A client
    /// checks provenance with `factory.escrow_address(buyer, salt) == this
    /// contract's address`; see [`EscrowParams::salt`].
    pub salt: BytesN<32>,
}

// The optional parts of an escrow are enums rather than `Option<T>`: soroban-sdk
// 27's `#[contracttype]` cannot derive XDR conversions for `Option<T>` when `T`
// is itself a contract type, which breaks every test build. The accessors on
// `Escrow` below give Rust callers `Option` ergonomics back.

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProofRecord {
    Pending,
    Submitted(Proof),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DisputeRecord {
    NotOpened,
    Opened(Dispute),
}

/// How the escrow ended. Released and refunded are mutually exclusive, so one
/// field records both.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Settlement {
    Open,
    Released(ReleasePath),
    Refunded(RefundPath),
}

impl Escrow {
    pub fn proof(&self) -> Option<Proof> {
        match &self.proof {
            ProofRecord::Submitted(proof) => Some(proof.clone()),
            ProofRecord::Pending => None,
        }
    }

    pub fn dispute(&self) -> Option<Dispute> {
        match &self.dispute {
            DisputeRecord::Opened(dispute) => Some(dispute.clone()),
            DisputeRecord::NotOpened => None,
        }
    }

    pub fn released_via(&self) -> Option<ReleasePath> {
        match self.settlement {
            Settlement::Released(path) => Some(path),
            _ => None,
        }
    }

    pub fn refunded_via(&self) -> Option<RefundPath> {
        match self.settlement {
            Settlement::Refunded(path) => Some(path),
            _ => None,
        }
    }
}
