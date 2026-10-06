//! Drives one escrow through a sequence of fuzzer-chosen actions and checks
//! the same invariants as `test_invariants.rs`'s randomised test — reusing
//! `trustescrow_escrow::invariants::check` rather than a second copy of it,
//! so this and that test can never silently say different things about
//! what "safe" means for an escrow. Unlike the seeded randomised test, this
//! is coverage-guided: libFuzzer mutates towards inputs that reach new code
//! paths, rather than sampling uniformly.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, Bytes, BytesN, Env, String,
};
use trustescrow_escrow::{
    invariants, EscrowContract, EscrowContractClient, EscrowParams, Order, Outcome, ProofKind,
    State,
};

const AMOUNT: i128 = 1_000_000_000;
const FEE_BPS: u32 = 150;
const START: u64 = 1_700_000_000;
const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
/// Short windows, like `test_invariants.rs`'s, so bounded time jumps cross
/// every deadline instead of always landing safely inside a window.
const DELIVERY_WINDOW: u64 = 2 * DAY;
const RECEIPT_WINDOW: u64 = DAY;
const ARBITRATION_WINDOW: u64 = DAY;
/// Real code; anything else is "wrong".
const CODE: &[u8] = b"K7M29XQF4TBNR3WD";
/// Bounds how many actions one fuzz input drives, so a single run stays
/// fast — depth comes from the corpus exploring many short sequences, not
/// from one input being huge.
const MAX_ACTIONS: usize = 32;

#[derive(Debug, Arbitrary)]
enum Caller {
    Buyer,
    Seller,
    Arbitrator,
    Stranger,
}

#[derive(Debug, Arbitrary)]
enum Call {
    Fund,
    Cancel,
    SubmitProof,
    SubmitProofWithCode { right_code: bool },
    ReleaseWithCode { right_code: bool },
    Confirm,
    Dispute,
    Escalate,
    Resolve { release: bool },
    RefundAfterDeliveryTimeout,
    RefundAfterArbitrationTimeout,
    SellerRefund,
    SweepFee,
    ExtendDelivery { extra_minutes: u16 },
    ExtendReceipt { extra_minutes: u16 },
}

#[derive(Debug, Arbitrary)]
struct Action {
    caller: Caller,
    call: Call,
    /// Minutes to advance the ledger before this action, up to ~45 days —
    /// deliberately allowed to run well past every deadline and past
    /// `MAX_WINDOW`'s edge cases over several steps.
    time_jump_minutes: u16,
}

#[derive(Debug, Arbitrary)]
struct Input {
    actions: std::vec::Vec<Action>,
}

fuzz_target!(|input: Input| {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(START);

    let token = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let buyer = Address::generate(&env);
    let seller = Address::generate(&env);
    let arbitrator = Address::generate(&env);
    let fee_recipient = Address::generate(&env);
    let stranger = Address::generate(&env);
    StellarAssetClient::new(&env, &token).mint(&buyer, &AMOUNT);

    let code = Bytes::from_slice(&env, CODE);
    let release_code_hash = env.crypto().sha256(&code).into();

    let params = EscrowParams {
        order: Order {
            buyer: buyer.clone(),
            seller: seller.clone(),
            token: token.clone(),
            amount: AMOUNT,
            terms_hash: BytesN::from_array(&env, &[1; 32]),
            release_code_hash,
            funding_deadline: START + DAY,
            delivery_window: DELIVERY_WINDOW,
            receipt_window: RECEIPT_WINDOW,
            arbitration_window: ARBITRATION_WINDOW,
        },
        arbitrator: arbitrator.clone(),
        fee_bps: FEE_BPS,
        fee_recipient: fee_recipient.clone(),
        salt: BytesN::from_array(&env, &[9; 32]),
    };
    let id = env.register(EscrowContract, (params,));
    let escrow = EscrowContractClient::new(&env, &id);
    let token_client = TokenClient::new(&env, &token);

    let mut state = State::Created;
    for action in input.actions.iter().take(MAX_ACTIONS) {
        let now = env.ledger().timestamp();
        env.ledger()
            .set_timestamp(now + action.time_jump_minutes as u64 * MINUTE);

        let caller = match action.caller {
            Caller::Buyer => buyer.clone(),
            Caller::Seller => seller.clone(),
            Caller::Arbitrator => arbitrator.clone(),
            Caller::Stranger => stranger.clone(),
        };
        let wrong_code = Bytes::from_slice(&env, &[0u8; 4]);
        let uri = String::from_str(&env, "");
        let hash = BytesN::from_array(&env, &[7; 32]);

        let _ = match &action.call {
            Call::Fund => escrow.try_fund().map(drop),
            Call::Cancel => escrow.try_cancel(&caller).map(drop),
            Call::SubmitProof => escrow
                .try_submit_proof(&ProofKind::Attestation, &uri, &hash)
                .map(drop),
            Call::SubmitProofWithCode { right_code } => {
                let c = if *right_code {
                    code.clone()
                } else {
                    wrong_code
                };
                escrow
                    .try_submit_proof_with_code(&ProofKind::Attestation, &uri, &hash, &c)
                    .map(drop)
            }
            Call::ReleaseWithCode { right_code } => {
                let c = if *right_code {
                    code.clone()
                } else {
                    wrong_code
                };
                escrow.try_release_with_code(&c).map(drop)
            }
            Call::Confirm => escrow.try_confirm().map(drop),
            Call::Dispute => escrow
                .try_dispute(&caller, &BytesN::from_array(&env, &[9; 32]))
                .map(drop),
            Call::Escalate => escrow.try_escalate().map(drop),
            Call::Resolve { release } => {
                let outcome = if *release {
                    Outcome::Release
                } else {
                    Outcome::Refund
                };
                escrow
                    .try_resolve(&outcome, &BytesN::from_array(&env, &[8; 32]))
                    .map(drop)
            }
            Call::RefundAfterDeliveryTimeout => {
                escrow.try_refund_after_delivery_timeout().map(drop)
            }
            Call::RefundAfterArbitrationTimeout => {
                escrow.try_refund_after_arbitration_timeout().map(drop)
            }
            Call::SellerRefund => escrow.try_seller_refund().map(drop),
            Call::SweepFee => escrow.try_sweep_fee().map(drop),
            Call::ExtendDelivery { extra_minutes } => {
                let new_deadline = escrow.get().delivery_deadline + *extra_minutes as u64 * MINUTE;
                escrow.try_extend_delivery(&new_deadline).map(drop)
            }
            Call::ExtendReceipt { extra_minutes } => {
                let new_deadline = escrow.get().receipt_deadline + *extra_minutes as u64 * MINUTE;
                escrow.try_extend_receipt(&new_deadline).map(drop)
            }
        };

        let e = escrow.get();
        let held = token_client.balance(&id);
        let b = token_client.balance(&buyer);
        let s = token_client.balance(&seller);
        let f = token_client.balance(&fee_recipient);
        invariants::check(&e, held, b, s, f, state, "escrow_state_machine fuzz target");
        state = e.state;
    }
});
