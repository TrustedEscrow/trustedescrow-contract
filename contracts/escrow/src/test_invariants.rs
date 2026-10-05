//! Randomised state-machine test.
//!
//! Drives many escrows through random sequences of calls, from random callers
//! at random times, and checks the contract's invariants after every step.
//! Most calls are chosen from the ones that make sense in the current state,
//! but a quarter are arbitrary, so invalid calls are exercised too. The point
//! is that no sequence, however odd, can reach a state that breaks the
//! invariants. The generator is seeded, so a failure message names the
//! sequence and step that reproduce it.

extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Bytes, BytesN, Env, String,
};
use std::collections::BTreeSet;

const AMOUNT: i128 = 1_000_000_000;
const FEE_BPS: u32 = 150;
const START: u64 = 1_700_000_000;
const HOUR: u64 = 3_600;
const DAY: u64 = 24 * HOUR;
const SEQUENCES: u64 = 96;
const STEPS: usize = 16;
/// Calls attempted after reaching a terminal state, all of which must fail.
const STEPS_AFTER_TERMINAL: usize = 3;
const CODE: &[u8] = b"K7M29XQF4TBNR3WD";
const WRONG_CODE: &[u8] = b"K7M29XQF4TBNR3WE";

/// xorshift64*: tiny, deterministic, and good enough to pick test actions.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick(&mut self, options: &[Op]) -> Op {
        options[self.below(options.len() as u64) as usize]
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Fund,
    Cancel,
    SubmitProof,
    SubmitProofWithCode,
    ReleaseWithCode,
    Confirm,
    Dispute,
    Escalate,
    Resolve,
    RefundAfterDeliveryTimeout,
    RefundAfterArbitrationTimeout,
    SellerRefund,
    SweepFee,
    ExtendDelivery,
    ExtendReceipt,
}

const ALL_OPS: [Op; 15] = [
    Op::Fund,
    Op::Cancel,
    Op::SubmitProof,
    Op::SubmitProofWithCode,
    Op::ReleaseWithCode,
    Op::Confirm,
    Op::Dispute,
    Op::Escalate,
    Op::Resolve,
    Op::RefundAfterDeliveryTimeout,
    Op::RefundAfterArbitrationTimeout,
    Op::SellerRefund,
    Op::SweepFee,
    Op::ExtendDelivery,
    Op::ExtendReceipt,
];

/// Calls that can plausibly succeed from `state`. Timeouts appear even though
/// they only succeed after their deadline, so early attempts are rejected.
fn relevant(state: State) -> &'static [Op] {
    match state {
        State::Created => &[Op::Fund, Op::Fund, Op::Cancel],
        State::Funded => &[
            Op::SubmitProof,
            Op::SubmitProofWithCode,
            Op::Dispute,
            Op::RefundAfterDeliveryTimeout,
            Op::SellerRefund,
            Op::ExtendDelivery,
        ],
        State::Delivered => &[
            Op::ReleaseWithCode,
            Op::Confirm,
            Op::Dispute,
            Op::Escalate,
            Op::SellerRefund,
            Op::ExtendReceipt,
        ],
        State::Disputed => &[
            Op::Resolve,
            Op::Resolve,
            Op::RefundAfterArbitrationTimeout,
            Op::SellerRefund,
        ],
        State::Released | State::Refunded | State::Cancelled => &ALL_OPS,
    }
}

/// Whether a generated `try_` call went through.
fn succeeded<T, E, F>(result: Result<Result<T, E>, F>) -> bool {
    matches!(result, Ok(Ok(_)))
}

struct World<'a> {
    env: Env,
    buyer: Address,
    seller: Address,
    stranger: Address,
    fee_recipient: Address,
    token: TokenClient<'a>,
    escrow: EscrowContractClient<'a>,
}

fn world<'a>() -> World<'a> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(START);

    let token = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let buyer = Address::generate(&env);
    let seller = Address::generate(&env);
    let fee_recipient = Address::generate(&env);
    StellarAssetClient::new(&env, &token).mint(&buyer, &AMOUNT);

    // Short windows so that random time steps cross every deadline.
    let params = EscrowParams {
        order: Order {
            buyer: buyer.clone(),
            seller: seller.clone(),
            token: token.clone(),
            amount: AMOUNT,
            terms_hash: BytesN::from_array(&env, &[1; 32]),
            release_code_hash: env.crypto().sha256(&Bytes::from_slice(&env, CODE)).into(),
            funding_deadline: START + DAY,
            delivery_window: 2 * DAY,
            receipt_window: DAY,
            arbitration_window: DAY,
        },
        arbitrator: Address::generate(&env),
        fee_bps: FEE_BPS,
        fee_recipient: fee_recipient.clone(),
        salt: BytesN::from_array(&env, &[9; 32]),
    };
    let id = env.register(EscrowContract, (params,));

    World {
        token: TokenClient::new(&env, &token),
        escrow: EscrowContractClient::new(&env, &id),
        stranger: Address::generate(&env),
        buyer,
        seller,
        fee_recipient,
        env,
    }
}

impl World<'_> {
    fn anyone(&self, rng: &mut Rng) -> Address {
        match rng.below(3) {
            0 => self.buyer.clone(),
            1 => self.seller.clone(),
            _ => self.stranger.clone(),
        }
    }

    fn some_code(&self, rng: &mut Rng) -> Bytes {
        let code = if rng.below(4) == 0 { WRONG_CODE } else { CODE };
        Bytes::from_slice(&self.env, code)
    }

    /// Maybe let time pass, then attempt one call. Returns whether it
    /// succeeded; rejections are expected.
    fn step(&self, rng: &mut Rng, state: State) -> bool {
        if rng.below(2) == 0 {
            let now = self.env.ledger().timestamp();
            self.env.ledger().set_timestamp(now + rng.below(36 * HOUR));
        }
        let op = if rng.below(4) == 0 {
            rng.pick(&ALL_OPS)
        } else {
            rng.pick(relevant(state))
        };

        let uri = String::from_str(&self.env, "https://track.example/1");
        let hash = BytesN::from_array(&self.env, &[7; 32]);
        match op {
            Op::Fund => succeeded(self.escrow.try_fund()),
            Op::Cancel => succeeded(self.escrow.try_cancel(&self.anyone(rng))),
            Op::SubmitProof => succeeded(self.escrow.try_submit_proof(
                &ProofKind::Tracking,
                &uri,
                &hash,
            )),
            Op::SubmitProofWithCode => succeeded(self.escrow.try_submit_proof_with_code(
                &ProofKind::Attestation,
                &uri,
                &hash,
                &self.some_code(rng),
            )),
            Op::ReleaseWithCode => {
                succeeded(self.escrow.try_release_with_code(&self.some_code(rng)))
            }
            Op::Confirm => succeeded(self.escrow.try_confirm()),
            Op::Dispute => succeeded(self.escrow.try_dispute(&self.anyone(rng))),
            Op::Escalate => succeeded(self.escrow.try_escalate()),
            Op::Resolve => {
                let outcome = if rng.below(2) == 0 {
                    Outcome::Release
                } else {
                    Outcome::Refund
                };
                succeeded(self.escrow.try_resolve(&outcome))
            }
            Op::RefundAfterDeliveryTimeout => {
                succeeded(self.escrow.try_refund_after_delivery_timeout())
            }
            Op::RefundAfterArbitrationTimeout => {
                succeeded(self.escrow.try_refund_after_arbitration_timeout())
            }
            Op::SellerRefund => succeeded(self.escrow.try_seller_refund()),
            Op::SweepFee => succeeded(self.escrow.try_sweep_fee()),
            Op::ExtendDelivery => {
                // Sometimes equal to the current deadline (correctly
                // rejected), usually later (accepted).
                let new_deadline = self.escrow.get().delivery_deadline + rng.below(2 * DAY);
                succeeded(self.escrow.try_extend_delivery(&new_deadline))
            }
            Op::ExtendReceipt => {
                let new_deadline = self.escrow.get().receipt_deadline + rng.below(2 * DAY);
                succeeded(self.escrow.try_extend_receipt(&new_deadline))
            }
        }
    }

    fn check(&self, previous: State, ctx: &str) -> Escrow {
        let e = self.escrow.get();
        let held = self.token.balance(&self.escrow.address);
        let buyer = self.token.balance(&self.buyer);
        let seller = self.token.balance(&self.seller);
        let fee = self.token.balance(&self.fee_recipient);

        // Conservation: tokens only ever move between these four.
        assert_eq!(held + buyer + seller + fee, AMOUNT, "{ctx}");

        // Terminal states are final.
        if matches!(
            previous,
            State::Released | State::Refunded | State::Cancelled
        ) {
            assert_eq!(e.state, previous, "{ctx}");
        }

        match e.state {
            State::Created | State::Cancelled => {
                assert_eq!((held, buyer), (0, AMOUNT), "{ctx}");
                assert!(e.proof().is_none() && e.dispute().is_none(), "{ctx}");
            }
            State::Funded => {
                assert_eq!(held, AMOUNT, "{ctx}");
                assert!(e.proof().is_none() && e.dispute().is_none(), "{ctx}");
            }
            State::Delivered => {
                assert_eq!(held, AMOUNT, "{ctx}");
                assert!(e.proof().is_some() && e.dispute().is_none(), "{ctx}");
            }
            State::Disputed => {
                assert_eq!(held, AMOUNT, "{ctx}");
                assert!(e.dispute().is_some(), "{ctx}");
            }
            State::Released => {
                // Two-sided release: buyer evidence on top of seller proof,
                // or the arbitrator's ruling. Nothing else pays the seller.
                match e.released_via().unwrap() {
                    ReleasePath::Code | ReleasePath::Confirmation => {
                        assert!(e.proof().is_some() && e.dispute().is_none(), "{ctx}")
                    }
                    ReleasePath::Arbitration => assert!(e.dispute().is_some(), "{ctx}"),
                }
                // A terminal escrow holds no tokens except an unswept fee:
                // the seller is paid in full regardless of whether the fee
                // transfer to `fee_recipient` succeeded.
                let expected_fee = AMOUNT * FEE_BPS as i128 / 10_000;
                assert_eq!(held, e.unswept_fee, "{ctx}");
                assert_eq!(
                    (buyer, seller, fee + e.unswept_fee),
                    (0, AMOUNT - expected_fee, expected_fee),
                    "{ctx}"
                );
            }
            State::Refunded => {
                match e.refunded_via().unwrap() {
                    RefundPath::DeliveryTimeout => assert!(e.proof().is_none(), "{ctx}"),
                    RefundPath::Arbitration | RefundPath::ArbitrationTimeout => {
                        assert!(e.dispute().is_some(), "{ctx}")
                    }
                    RefundPath::SellerRefund => {}
                }
                // Refunds are whole and fee-free.
                assert_eq!((held, buyer, seller, fee), (0, AMOUNT, 0, 0), "{ctx}");
            }
        }
        if !matches!(e.state, State::Released | State::Refunded) {
            assert_eq!(e.settlement, Settlement::Open, "{ctx}");
        }
        // Only a release can ever leave a fee unswept.
        if e.state != State::Released {
            assert_eq!(e.unswept_fee, 0, "{ctx}");
        }
        e
    }
}

fn is_terminal(state: State) -> bool {
    matches!(state, State::Released | State::Refunded | State::Cancelled)
}

#[test]
fn random_call_sequences_preserve_invariants() {
    let mut endings = BTreeSet::new();
    for seq in 0..SEQUENCES {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (seq + 1));
        let w = world();
        let mut state = State::Created;
        let mut after_terminal = 0;
        for step in 0..STEPS {
            let ctx = std::format!("sequence {seq}, step {step}, from {state:?}");
            if w.step(&mut rng, state) {
                state = w.check(state, &ctx).state;
            } else {
                // A rejected call reverts entirely, so nothing may change.
                assert_eq!(
                    w.escrow.get().state,
                    state,
                    "{ctx}: rejected call changed state"
                );
            }
            if is_terminal(state) {
                after_terminal += 1;
                if after_terminal > STEPS_AFTER_TERMINAL {
                    break;
                }
            }
        }
        let e = w.check(state, &std::format!("sequence {seq}, end"));
        endings.insert(match e.state {
            State::Cancelled => std::string::String::from("Cancelled"),
            _ => std::format!("{:?}", e.settlement),
        });
    }

    // Guard against a degenerate walk: every way an escrow can end must have
    // been reached at least once, or the invariants above were never tested
    // against it.
    for ending in [
        "Cancelled",
        "Released(Code)",
        "Released(Confirmation)",
        "Released(Arbitration)",
        "Refunded(SellerRefund)",
        "Refunded(DeliveryTimeout)",
        "Refunded(Arbitration)",
        "Refunded(ArbitrationTimeout)",
    ] {
        assert!(
            endings.contains(ending),
            "{ending} never reached; reached {endings:?}"
        );
    }
}
