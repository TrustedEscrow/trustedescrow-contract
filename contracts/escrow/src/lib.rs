#![no_std]
//! TrustEscrow escrow contract. One instance per trade, deployed by the factory.
//!
//! The seller is paid only when both sides have spoken — seller proof on-chain
//! plus the buyer's delivery code or signature — or when the arbitrator rules.
//! No timeout pays the seller.

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error, token,
    Address, Bytes, BytesN, Env, String,
};

pub use trustescrow_types::{
    Dispute, DisputeOrigin, DisputeRecord, Escrow, EscrowParams, Order, Outcome, Proof, ProofKind,
    ProofRecord, RefundPath, ReleasePath, Settlement, State, MAX_FEE_BPS,
};

pub const MAX_URI_LEN: u32 = 256;
pub const MIN_WINDOW: u64 = 60 * 60;
pub const MAX_WINDOW: u64 = 365 * 24 * 60 * 60;

const BPS_DENOMINATOR: i128 = 10_000;
const ALLOWED_SCHEMES: [&[u8]; 3] = [b"https://", b"ipfs://", b"ar://"];

const DAY_IN_LEDGERS: u32 = 17_280;
const TTL_THRESHOLD: u32 = 30 * DAY_IN_LEDGERS;
const TTL_EXTEND_TO: u32 = 120 * DAY_IN_LEDGERS;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    InvalidState = 1,
    NotParticipant = 2,
    InvalidCode = 3,
    ProofAlreadySubmitted = 4,
    ProofRequired = 5,
    DeadlinePassed = 6,
    DeadlineNotReached = 7,
    InvalidParties = 8,
    InvalidAmount = 9,
    InvalidFee = 10,
    InvalidWindow = 11,
    InvalidUri = 12,
    Overflow = 13,
    NoFeeToSweep = 14,
}

#[contracttype]
enum DataKey {
    Escrow,
}

#[contractevent(topics = ["created"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Created {
    #[topic]
    pub buyer: Address,
    #[topic]
    pub seller: Address,
    pub token: Address,
    pub amount: i128,
}

#[contractevent(topics = ["funded"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Funded {
    pub amount: i128,
    pub delivery_deadline: u64,
}

#[contractevent(topics = ["proof"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProofSubmitted {
    pub kind: ProofKind,
    pub hash: BytesN<32>,
    pub receipt_deadline: u64,
}

#[contractevent(topics = ["disputed"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Disputed {
    pub opened_by: DisputeOrigin,
    pub deadline: u64,
}

#[contractevent(topics = ["released"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Released {
    pub path: ReleasePath,
    pub payout: i128,
    pub fee: i128,
}

#[contractevent(topics = ["refunded"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refunded {
    pub path: RefundPath,
    pub amount: i128,
}

#[contractevent(topics = ["cancelled"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cancelled {
    pub by: Address,
}

#[contractevent(topics = ["fee_swept"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeeSwept {
    pub fee: i128,
}

/// Which deadline `Extended` pushed back.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtendedKind {
    Delivery,
    Receipt,
}

#[contractevent(topics = ["extended"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Extended {
    #[topic]
    pub kind: ExtendedKind,
    pub new_deadline: u64,
}

#[contract]
pub struct EscrowContract;

#[contractimpl]
impl EscrowContract {
    pub fn __constructor(env: Env, params: EscrowParams) {
        let now = now(&env);
        let order = params.order;

        if order.buyer == order.seller
            || params.arbitrator == order.buyer
            || params.arbitrator == order.seller
        {
            panic_with_error!(&env, Error::InvalidParties);
        }
        if order.amount <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        if params.fee_bps > MAX_FEE_BPS {
            panic_with_error!(&env, Error::InvalidFee);
        }
        if order.funding_deadline <= now || order.funding_deadline > add(&env, now, MAX_WINDOW) {
            panic_with_error!(&env, Error::InvalidWindow);
        }
        for window in [
            order.delivery_window,
            order.receipt_window,
            order.arbitration_window,
        ] {
            if !(MIN_WINDOW..=MAX_WINDOW).contains(&window) {
                panic_with_error!(&env, Error::InvalidWindow);
            }
        }

        let escrow = Escrow {
            buyer: order.buyer,
            seller: order.seller,
            arbitrator: params.arbitrator,
            token: order.token,
            amount: order.amount,
            fee_bps: params.fee_bps,
            fee_recipient: params.fee_recipient,
            terms_hash: order.terms_hash,
            release_code_hash: order.release_code_hash,
            state: State::Created,
            created_at: now,
            funding_deadline: order.funding_deadline,
            delivery_window: order.delivery_window,
            receipt_window: order.receipt_window,
            arbitration_window: order.arbitration_window,
            funded_at: 0,
            delivery_deadline: 0,
            receipt_deadline: 0,
            proof: ProofRecord::Pending,
            dispute: DisputeRecord::NotOpened,
            settlement: Settlement::Open,
            unswept_fee: 0,
            salt: params.salt,
        };
        save(&env, &escrow);

        Created {
            buyer: escrow.buyer,
            seller: escrow.seller,
            token: escrow.token,
            amount: escrow.amount,
        }
        .publish(&env);
    }

    /// Buyer deposits `amount`. Starts the delivery window.
    pub fn fund(env: Env) {
        let mut e = load(&env);
        require_state(&env, &e, State::Created);
        let now = now(&env);
        if now >= e.funding_deadline {
            panic_with_error!(&env, Error::DeadlinePassed);
        }
        e.buyer.require_auth();

        e.state = State::Funded;
        e.funded_at = now;
        e.delivery_deadline = add(&env, now, e.delivery_window);
        save(&env, &e);

        token::TokenClient::new(&env, &e.token).transfer(
            &e.buyer,
            env.current_contract_address(),
            &e.amount,
        );
        Funded {
            amount: e.amount,
            delivery_deadline: e.delivery_deadline,
        }
        .publish(&env);
    }

    /// Abandon an unfunded escrow. Either party may cancel before
    /// `funding_deadline`; anyone may after it. No funds are held.
    pub fn cancel(env: Env, caller: Address) {
        caller.require_auth();
        let mut e = load(&env);
        require_state(&env, &e, State::Created);
        let is_party = caller == e.buyer || caller == e.seller;
        if !is_party && now(&env) < e.funding_deadline {
            panic_with_error!(&env, Error::NotParticipant);
        }

        e.state = State::Cancelled;
        save(&env, &e);
        Cancelled { by: caller }.publish(&env);
    }

    /// Seller commits proof of delivery. Single-shot. Starts the buyer's
    /// receipt window; it does not by itself entitle the seller to anything.
    pub fn submit_proof(env: Env, kind: ProofKind, uri: String, hash: BytesN<32>) {
        let mut e = load(&env);
        if e.proof().is_some() {
            panic_with_error!(&env, Error::ProofAlreadySubmitted);
        }
        require_state(&env, &e, State::Funded);
        e.seller.require_auth();
        let now = now(&env);
        if now >= e.delivery_deadline {
            panic_with_error!(&env, Error::DeadlinePassed);
        }
        validate_uri(&env, kind, &uri);

        e.receipt_deadline = add(&env, now, e.receipt_window);
        e.proof = ProofRecord::Submitted(Proof {
            kind,
            uri,
            hash: hash.clone(),
            submitted_at: now,
        });
        e.state = State::Delivered;
        save(&env, &e);

        ProofSubmitted {
            kind,
            hash,
            receipt_deadline: e.receipt_deadline,
        }
        .publish(&env);
    }

    /// In-person handover: the seller records proof and presents the buyer's
    /// code in one transaction. Accepted after `delivery_deadline` too — a
    /// buyer who hands over the code has accepted late delivery.
    pub fn submit_proof_with_code(
        env: Env,
        kind: ProofKind,
        uri: String,
        hash: BytesN<32>,
        code: Bytes,
    ) {
        let mut e = load(&env);
        if e.proof().is_some() {
            panic_with_error!(&env, Error::ProofAlreadySubmitted);
        }
        require_state(&env, &e, State::Funded);
        e.seller.require_auth();
        validate_uri(&env, kind, &uri);
        verify_code(&env, &e, &code);

        let now = now(&env);
        e.receipt_deadline = now;
        e.proof = ProofRecord::Submitted(Proof {
            kind,
            uri,
            hash: hash.clone(),
            submitted_at: now,
        });
        ProofSubmitted {
            kind,
            hash,
            receipt_deadline: now,
        }
        .publish(&env);
        release(&env, e, ReleasePath::Code);
    }

    /// Release on the buyer's delivery code. Callable by anyone holding it, but
    /// only once the seller's proof is on-chain.
    pub fn release_with_code(env: Env, code: Bytes) {
        let e = load(&env);
        match e.state {
            State::Delivered => {}
            State::Funded => panic_with_error!(&env, Error::ProofRequired),
            _ => panic_with_error!(&env, Error::InvalidState),
        }
        verify_code(&env, &e, &code);
        release(&env, e, ReleasePath::Code);
    }

    /// Buyer confirms receipt with their own signature — the fallback for a
    /// buyer who has lost their code.
    pub fn confirm(env: Env) {
        let e = load(&env);
        match e.state {
            State::Delivered => {}
            State::Funded => panic_with_error!(&env, Error::ProofRequired),
            _ => panic_with_error!(&env, Error::InvalidState),
        }
        e.buyer.require_auth();
        release(&env, e, ReleasePath::Confirmation);
    }

    /// Either party hands the escrow to the arbitrator. From `Funded` this is
    /// closed once `delivery_deadline` passes, so a seller cannot block the
    /// buyer's refund.
    pub fn dispute(env: Env, caller: Address) {
        caller.require_auth();
        let e = load(&env);
        let origin = if caller == e.buyer {
            DisputeOrigin::Buyer
        } else if caller == e.seller {
            DisputeOrigin::Seller
        } else {
            panic_with_error!(&env, Error::NotParticipant)
        };
        match e.state {
            State::Funded => {
                if now(&env) >= e.delivery_deadline {
                    panic_with_error!(&env, Error::DeadlinePassed);
                }
            }
            State::Delivered => {}
            _ => panic_with_error!(&env, Error::InvalidState),
        }
        open_dispute(&env, e, origin);
    }

    /// The buyer gave neither receipt nor objection by `receipt_deadline`.
    /// Anyone may hand the escrow to the arbitrator. This is the path that
    /// replaces paying the seller on a timeout.
    pub fn escalate(env: Env) {
        let e = load(&env);
        require_state(&env, &e, State::Delivered);
        if now(&env) < e.receipt_deadline {
            panic_with_error!(&env, Error::DeadlineNotReached);
        }
        open_dispute(&env, e, DisputeOrigin::ReceiptTimeout);
    }

    /// Arbitrator chooses one of two outcomes, before the arbitration deadline.
    pub fn resolve(env: Env, outcome: Outcome) {
        let e = load(&env);
        let deadline = dispute_deadline(&env, &e);
        e.arbitrator.require_auth();
        if now(&env) >= deadline {
            panic_with_error!(&env, Error::DeadlinePassed);
        }
        match outcome {
            Outcome::Release => release(&env, e, ReleasePath::Arbitration),
            Outcome::Refund => refund(&env, e, RefundPath::Arbitration),
        }
    }

    /// The seller never submitted proof. Anyone may refund the buyer.
    pub fn refund_after_delivery_timeout(env: Env) {
        let e = load(&env);
        require_state(&env, &e, State::Funded);
        if now(&env) < e.delivery_deadline {
            panic_with_error!(&env, Error::DeadlineNotReached);
        }
        refund(&env, e, RefundPath::DeliveryTimeout);
    }

    /// The arbitrator never ruled. Anyone may refund the buyer.
    pub fn refund_after_arbitration_timeout(env: Env) {
        let e = load(&env);
        let deadline = dispute_deadline(&env, &e);
        if now(&env) < deadline {
            panic_with_error!(&env, Error::DeadlineNotReached);
        }
        refund(&env, e, RefundPath::ArbitrationTimeout);
    }

    /// Seller returns the funds voluntarily. Only ever benefits the buyer.
    pub fn seller_refund(env: Env) {
        let e = load(&env);
        match e.state {
            State::Funded | State::Delivered | State::Disputed => {}
            _ => panic_with_error!(&env, Error::InvalidState),
        }
        e.seller.require_auth();
        refund(&env, e, RefundPath::SellerRefund);
    }

    /// Give the seller more time to deliver. Only the buyer can call this,
    /// and it can only push `delivery_deadline` later, never earlier — it
    /// only ever benefits the seller. Callable even after the old deadline
    /// has passed, as long as nobody has claimed the delivery timeout yet:
    /// a buyer who chooses to keep waiting has accepted the delay, the same
    /// way `submit_proof_with_code` already treats a late code handover as
    /// acceptance of late delivery.
    pub fn extend_delivery(env: Env, new_deadline: u64) {
        let mut e = load(&env);
        require_state(&env, &e, State::Funded);
        e.buyer.require_auth();
        if new_deadline <= e.delivery_deadline || new_deadline > add(&env, e.funded_at, MAX_WINDOW)
        {
            panic_with_error!(&env, Error::InvalidWindow);
        }
        e.delivery_deadline = new_deadline;
        save(&env, &e);
        Extended {
            kind: ExtendedKind::Delivery,
            new_deadline,
        }
        .publish(&env);
    }

    /// Give the buyer more time to give receipt or dispute. Only the seller
    /// can call this, and it can only push `receipt_deadline` later, never
    /// earlier — it only ever benefits the buyer.
    pub fn extend_receipt(env: Env, new_deadline: u64) {
        let mut e = load(&env);
        require_state(&env, &e, State::Delivered);
        e.seller.require_auth();
        let submitted_at = e
            .proof()
            .expect("State::Delivered implies proof is submitted")
            .submitted_at;
        if new_deadline <= e.receipt_deadline || new_deadline > add(&env, submitted_at, MAX_WINDOW)
        {
            panic_with_error!(&env, Error::InvalidWindow);
        }
        e.receipt_deadline = new_deadline;
        save(&env, &e);
        Extended {
            kind: ExtendedKind::Receipt,
            new_deadline,
        }
        .publish(&env);
    }

    pub fn get(env: Env) -> Escrow {
        load(&env)
    }

    /// Retry the fee transfer to `fee_recipient` after it failed on release
    /// (the recipient had no trustline, or was frozen). Permissionless, and
    /// can only ever pay `fee_recipient`. Fails if there is nothing unswept.
    pub fn sweep_fee(env: Env) {
        let mut e = load(&env);
        if e.unswept_fee <= 0 {
            panic_with_error!(&env, Error::NoFeeToSweep);
        }
        let fee = e.unswept_fee;
        e.unswept_fee = 0;
        save(&env, &e);

        token::TokenClient::new(&env, &e.token).transfer(
            &env.current_contract_address(),
            &e.fee_recipient,
            &fee,
        );
        FeeSwept { fee }.publish(&env);
    }

    /// Extend the instance TTL. Public so a bumper job can keep idle escrows live.
    pub fn bump(env: Env) {
        extend_ttl(&env);
    }
}

fn now(env: &Env) -> u64 {
    env.ledger().timestamp()
}

fn add(env: &Env, a: u64, b: u64) -> u64 {
    a.checked_add(b)
        .unwrap_or_else(|| panic_with_error!(env, Error::Overflow))
}

fn load(env: &Env) -> Escrow {
    env.storage().instance().get(&DataKey::Escrow).unwrap()
}

fn save(env: &Env, e: &Escrow) {
    env.storage().instance().set(&DataKey::Escrow, e);
    extend_ttl(env);
}

fn extend_ttl(env: &Env) {
    let extend_to = TTL_EXTEND_TO.min(env.storage().max_ttl());
    let threshold = TTL_THRESHOLD.min(extend_to);
    env.storage().instance().extend_ttl(threshold, extend_to);
}

fn require_state(env: &Env, e: &Escrow, state: State) {
    if e.state != state {
        panic_with_error!(env, Error::InvalidState);
    }
}

fn dispute_deadline(env: &Env, e: &Escrow) -> u64 {
    match (&e.state, &e.dispute) {
        (State::Disputed, DisputeRecord::Opened(d)) => d.deadline,
        _ => panic_with_error!(env, Error::InvalidState),
    }
}

fn verify_code(env: &Env, e: &Escrow, code: &Bytes) {
    let hash: BytesN<32> = env.crypto().sha256(code).into();
    if hash != e.release_code_hash {
        panic_with_error!(env, Error::InvalidCode);
    }
}

fn validate_uri(env: &Env, kind: ProofKind, uri: &String) {
    let len = uri.len();
    if len == 0 {
        if kind == ProofKind::Attestation {
            return;
        }
        panic_with_error!(env, Error::InvalidUri);
    }
    if len > MAX_URI_LEN {
        panic_with_error!(env, Error::InvalidUri);
    }
    let mut buf = [0u8; MAX_URI_LEN as usize];
    let bytes = &mut buf[..len as usize];
    uri.copy_into_slice(bytes);

    let printable = bytes.iter().all(|b| (0x21..=0x7e).contains(b));
    let has_scheme = ALLOWED_SCHEMES
        .iter()
        .any(|scheme| bytes.len() > scheme.len() && bytes.starts_with(scheme));
    if !printable || !has_scheme {
        panic_with_error!(env, Error::InvalidUri);
    }
}

fn open_dispute(env: &Env, mut e: Escrow, origin: DisputeOrigin) {
    let now = now(env);
    let deadline = add(env, now, e.arbitration_window);
    e.dispute = DisputeRecord::Opened(Dispute {
        opened_by: origin,
        opened_at: now,
        from_state: e.state,
        deadline,
    });
    e.state = State::Disputed;
    save(env, &e);
    Disputed {
        opened_by: origin,
        deadline,
    }
    .publish(env);
}

fn release(env: &Env, mut e: Escrow, path: ReleasePath) {
    let fee = e
        .amount
        .checked_mul(e.fee_bps as i128)
        .unwrap_or_else(|| panic_with_error!(env, Error::Overflow))
        / BPS_DENOMINATOR;
    let payout = e.amount - fee;

    e.state = State::Released;
    e.settlement = Settlement::Released(path);

    let token = token::TokenClient::new(env, &e.token);
    let this = env.current_contract_address();
    token.transfer(&this, &e.seller, &payout);

    // `fee_recipient` is shared by every escrow. If it has lost its
    // trustline or been frozen, that must never block the seller's payout
    // above: keep the fee in the escrow instead of reverting the whole
    // release. `sweep_fee` can retry once the recipient can receive it again.
    let fee_sent = fee == 0
        || matches!(
            token.try_transfer(&this, &e.fee_recipient, &fee),
            Ok(Ok(()))
        );
    e.unswept_fee = if fee_sent { 0 } else { fee };
    save(env, &e);

    Released { path, payout, fee }.publish(env);
}

fn refund(env: &Env, mut e: Escrow, path: RefundPath) {
    e.state = State::Refunded;
    e.settlement = Settlement::Refunded(path);
    save(env, &e);

    token::TokenClient::new(env, &e.token).transfer(
        &env.current_contract_address(),
        &e.buyer,
        &e.amount,
    );
    Refunded {
        path,
        amount: e.amount,
    }
    .publish(env);
}

#[cfg(test)]
mod test;
#[cfg(test)]
mod test_invariants;
