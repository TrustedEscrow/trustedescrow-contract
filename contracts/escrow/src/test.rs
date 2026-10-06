#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{
        storage::Instance as _, Address as _, Events, IssuerFlags, Ledger, MockAuth, MockAuthInvoke,
    },
    token::{StellarAssetClient, TokenClient},
    xdr::ContractEvent,
    Bytes, BytesN, Env, Event, IntoVal, String,
};

const AMOUNT: i128 = 1_000_000_000;
const FEE_BPS: u32 = 150;
const START: u64 = 1_700_000_000;
const DAY: u64 = 86_400;
const DELIVERY_WINDOW: u64 = 7 * DAY;
const RECEIPT_WINDOW: u64 = 3 * DAY;
const ARBITRATION_WINDOW: u64 = 30 * DAY;
const CODE: &[u8] = b"K7M29XQF4TBNR3WD";
const ONE_BYTE_OFF: &[u8] = b"K7M29XQF4TBNR3WE";
const TRACKING_URI: &str = "https://track.example/ABC123";
const SALT: [u8; 32] = [9; 32];

struct Setup<'a> {
    env: Env,
    buyer: Address,
    seller: Address,
    arbitrator: Address,
    fee_recipient: Address,
    token: TokenClient<'a>,
    escrow: EscrowContractClient<'a>,
}

fn new_env() -> Env {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(START);
    env
}

fn default_params(env: &Env) -> EscrowParams {
    let sac = env.register_stellar_asset_contract_v2(Address::generate(env));
    // Lets tests simulate the fee recipient losing authorisation (a frozen
    // account), the same as a real issuer with AUTH_REVOCABLE set.
    sac.issuer().set_flag(IssuerFlags::RevocableFlag);
    let token = sac.address();
    EscrowParams {
        order: Order {
            buyer: Address::generate(env),
            seller: Address::generate(env),
            token,
            amount: AMOUNT,
            terms_hash: BytesN::from_array(env, &[1; 32]),
            release_code_hash: env.crypto().sha256(&Bytes::from_slice(env, CODE)).into(),
            funding_deadline: START + DAY,
            delivery_window: DELIVERY_WINDOW,
            receipt_window: RECEIPT_WINDOW,
            arbitration_window: ARBITRATION_WINDOW,
        },
        arbitrator: Address::generate(env),
        fee_bps: FEE_BPS,
        fee_recipient: Address::generate(env),
        salt: BytesN::from_array(env, &SALT),
    }
}

fn setup_from<'a>(env: Env, params: EscrowParams) -> Setup<'a> {
    let order = &params.order;
    StellarAssetClient::new(&env, &order.token).mint(&order.buyer, &order.amount);
    let id = env.register(EscrowContract, (params.clone(),));
    Setup {
        token: TokenClient::new(&env, &order.token),
        escrow: EscrowContractClient::new(&env, &id),
        buyer: order.buyer.clone(),
        seller: order.seller.clone(),
        arbitrator: params.arbitrator.clone(),
        fee_recipient: params.fee_recipient.clone(),
        env,
    }
}

fn setup<'a>() -> Setup<'a> {
    let env = new_env();
    let params = default_params(&env);
    setup_from(env, params)
}

/// Entry points panic with a contract error rather than returning `Result`, so
/// the generated `try_` client surfaces it as a generic `soroban_sdk::Error`.
fn assert_err<T: core::fmt::Debug, E: core::fmt::Debug>(
    result: Result<T, Result<soroban_sdk::Error, E>>,
    expected: Error,
) {
    match result {
        Err(Ok(got)) => assert_eq!(got, soroban_sdk::Error::from(expected)),
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}

impl Setup<'_> {
    fn at(&self, timestamp: u64) {
        self.env.ledger().set_timestamp(timestamp);
    }

    fn get(&self) -> Escrow {
        self.escrow.get()
    }

    fn state(&self) -> State {
        self.get().state
    }

    fn balance(&self, who: &Address) -> i128 {
        self.token.balance(who)
    }

    /// (De-)authorise `who` to use its balance of the settlement token —
    /// simulates a frozen account or a missing trustline.
    fn set_authorized(&self, who: &Address, authorized: bool) {
        StellarAssetClient::new(&self.env, &self.token.address).set_authorized(who, &authorized);
    }

    fn code(&self) -> Bytes {
        Bytes::from_slice(&self.env, CODE)
    }

    fn wrong_code(&self) -> Bytes {
        Bytes::from_slice(&self.env, ONE_BYTE_OFF)
    }

    fn uri(&self, s: &str) -> String {
        String::from_str(&self.env, s)
    }

    fn hash(&self) -> BytesN<32> {
        BytesN::from_array(&self.env, &[7; 32])
    }

    fn deliver(&self) {
        self.escrow
            .submit_proof(&ProofKind::Tracking, &self.uri(TRACKING_URI), &self.hash());
    }

    fn funded(self) -> Self {
        self.escrow.fund();
        self
    }

    fn delivered(self) -> Self {
        self.escrow.fund();
        self.deliver();
        self
    }

    fn assert_only_auth(&self, who: &Address) {
        let auths = self.env.auths();
        assert_eq!(auths.len(), 1, "expected exactly one signer, got {auths:?}");
        assert_eq!(&auths[0].0, who);
    }

    fn assert_seller_paid(&self) {
        let fee = AMOUNT * FEE_BPS as i128 / 10_000;
        assert_eq!(self.state(), State::Released);
        assert_eq!(self.balance(&self.seller), AMOUNT - fee);
        assert_eq!(self.balance(&self.fee_recipient), fee);
        assert_eq!(self.balance(&self.buyer), 0);
        assert_eq!(self.balance(&self.escrow.address), 0);
    }

    fn assert_buyer_refunded(&self) {
        assert_eq!(self.state(), State::Refunded);
        assert_eq!(self.balance(&self.buyer), AMOUNT);
        assert_eq!(self.balance(&self.seller), 0);
        assert_eq!(self.balance(&self.fee_recipient), 0);
        assert_eq!(self.balance(&self.escrow.address), 0);
    }

    /// Every mutating entry point must reject a terminal escrow.
    fn assert_closed(&self) {
        let (uri, hash, code) = (self.uri(TRACKING_URI), self.hash(), self.code());
        assert_err(self.escrow.try_fund(), Error::InvalidState);
        assert_err(self.escrow.try_cancel(&self.buyer), Error::InvalidState);
        assert!(self
            .escrow
            .try_submit_proof(&ProofKind::Tracking, &uri, &hash)
            .is_err());
        assert!(self
            .escrow
            .try_submit_proof_with_code(&ProofKind::Tracking, &uri, &hash, &code)
            .is_err());
        assert_err(
            self.escrow.try_release_with_code(&code),
            Error::InvalidState,
        );
        assert_err(self.escrow.try_confirm(), Error::InvalidState);
        assert_err(self.escrow.try_dispute(&self.buyer), Error::InvalidState);
        assert_err(self.escrow.try_escalate(), Error::InvalidState);
        assert_err(
            self.escrow.try_resolve(&Outcome::Refund),
            Error::InvalidState,
        );
        assert_err(
            self.escrow.try_refund_after_delivery_timeout(),
            Error::InvalidState,
        );
        assert_err(
            self.escrow.try_refund_after_arbitration_timeout(),
            Error::InvalidState,
        );
        assert_err(self.escrow.try_seller_refund(), Error::InvalidState);
        assert_err(
            self.escrow
                .try_extend_delivery(&(self.get().delivery_deadline + DAY)),
            Error::InvalidState,
        );
        assert_err(
            self.escrow
                .try_extend_receipt(&(self.get().receipt_deadline + DAY)),
            Error::InvalidState,
        );
    }

    /// The escrow's own events from the last invocation, in emission order.
    /// Token transfer events from the asset contract are filtered out.
    fn escrow_events(&self) -> std::vec::Vec<ContractEvent> {
        self.env
            .events()
            .all()
            .filter_by_contract(&self.escrow.address)
            .events()
            .to_vec()
    }

    fn event(&self, event: &impl Event) -> ContractEvent {
        event.to_xdr(&self.env, &self.escrow.address)
    }
}

// --- Construction ------------------------------------------------------------

#[test]
fn constructor_records_order_and_operator_config() {
    let s = setup();
    let e = s.get();
    assert_eq!(e.buyer, s.buyer);
    assert_eq!(e.seller, s.seller);
    assert_eq!(e.arbitrator, s.arbitrator);
    assert_eq!(e.fee_recipient, s.fee_recipient);
    assert_eq!(e.fee_bps, FEE_BPS);
    assert_eq!(e.amount, AMOUNT);
    assert_eq!(e.state, State::Created);
    assert_eq!(e.created_at, START);
    assert_eq!(e.funding_deadline, START + DAY);
    assert_eq!(e.delivery_window, DELIVERY_WINDOW);
    assert_eq!(e.receipt_window, RECEIPT_WINDOW);
    assert_eq!(e.arbitration_window, ARBITRATION_WINDOW);
    assert_eq!(
        (e.funded_at, e.delivery_deadline, e.receipt_deadline),
        (0, 0, 0)
    );
    assert!(e.proof().is_none());
    assert!(e.dispute().is_none());
    assert_eq!(e.settlement, Settlement::Open);
    assert_eq!(e.unswept_fee, 0);
    assert_eq!(e.salt, BytesN::from_array(&s.env, &SALT));
    assert_eq!(s.balance(&s.escrow.address), 0);
    assert_eq!(s.balance(&s.buyer), AMOUNT);
}

#[test]
fn bump_is_permissionless() {
    let s = setup();
    s.escrow.bump();
    assert!(s.env.auths().is_empty());
}

fn register_with(mutate: impl FnOnce(&mut EscrowParams)) {
    let env = new_env();
    let mut params = default_params(&env);
    mutate(&mut params);
    env.register(EscrowContract, (params,));
}

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn buyer_cannot_be_seller() {
    register_with(|p| p.order.seller = p.order.buyer.clone());
}

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn arbitrator_cannot_be_a_party() {
    register_with(|p| p.arbitrator = p.order.seller.clone());
}

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn amount_must_be_positive() {
    register_with(|p| p.order.amount = 0);
}

#[test]
#[should_panic(expected = "Error(Contract, #10)")]
fn fee_is_capped() {
    register_with(|p| p.fee_bps = MAX_FEE_BPS + 1);
}

#[test]
#[should_panic(expected = "Error(Contract, #11)")]
fn windows_have_a_minimum() {
    register_with(|p| p.order.receipt_window = MIN_WINDOW - 1);
}

#[test]
#[should_panic(expected = "Error(Contract, #11)")]
fn funding_deadline_must_be_in_the_future() {
    register_with(|p| p.order.funding_deadline = START);
}

// --- Funding and cancellation ------------------------------------------------

#[test]
fn fund_moves_tokens_and_starts_delivery_window() {
    let s = setup();
    s.escrow.fund();
    s.assert_only_auth(&s.buyer);
    let e = s.get();
    assert_eq!(e.state, State::Funded);
    assert_eq!(e.funded_at, START);
    assert_eq!(e.delivery_deadline, START + DELIVERY_WINDOW);
    assert_eq!(s.balance(&s.buyer), 0);
    assert_eq!(s.balance(&s.escrow.address), AMOUNT);
}

#[test]
fn fund_twice_is_rejected() {
    let s = setup().funded();
    assert_err(s.escrow.try_fund(), Error::InvalidState);
}

#[test]
fn fund_is_rejected_at_funding_deadline() {
    let s = setup();
    s.at(START + DAY);
    assert_err(s.escrow.try_fund(), Error::DeadlinePassed);
}

#[test]
fn either_party_can_cancel_before_funding() {
    for party in [0, 1] {
        let s = setup();
        let who = if party == 0 {
            s.buyer.clone()
        } else {
            s.seller.clone()
        };
        s.escrow.cancel(&who);
        assert_eq!(s.state(), State::Cancelled);
    }
}

#[test]
fn stranger_can_cancel_only_after_funding_deadline() {
    let s = setup();
    let stranger = Address::generate(&s.env);
    s.at(START + DAY - 1);
    assert_err(s.escrow.try_cancel(&stranger), Error::NotParticipant);
    s.at(START + DAY);
    s.escrow.cancel(&stranger);
    assert_eq!(s.state(), State::Cancelled);
}

#[test]
fn funded_escrow_cannot_be_cancelled() {
    let s = setup().funded();
    assert_err(s.escrow.try_cancel(&s.buyer), Error::InvalidState);
}

// --- Proof -------------------------------------------------------------------

#[test]
fn seller_signs_proof_and_starts_receipt_window() {
    let s = setup().funded();
    s.at(START + DAY);
    s.deliver();
    s.assert_only_auth(&s.seller);
    let e = s.get();
    assert_eq!(e.state, State::Delivered);
    assert_eq!(e.receipt_deadline, START + DAY + RECEIPT_WINDOW);
    let proof = e.proof().unwrap();
    assert_eq!(proof.submitted_at, START + DAY);
    assert_eq!(proof.uri, s.uri(TRACKING_URI));
}

#[test]
fn proof_requires_a_funded_escrow() {
    let s = setup();
    assert_err(
        s.escrow
            .try_submit_proof(&ProofKind::Tracking, &s.uri(TRACKING_URI), &s.hash()),
        Error::InvalidState,
    );
}

#[test]
fn proof_is_rejected_at_delivery_deadline() {
    let s = setup().funded();
    s.at(s.get().delivery_deadline);
    assert_err(
        s.escrow
            .try_submit_proof(&ProofKind::Tracking, &s.uri(TRACKING_URI), &s.hash()),
        Error::DeadlinePassed,
    );
    assert_eq!(s.state(), State::Funded);
}

#[test]
fn proof_is_single_shot() {
    let s = setup().delivered();
    let original = s.get().proof;
    assert_err(
        s.escrow.try_submit_proof(
            &ProofKind::Content,
            &s.uri("ipfs://other"),
            &BytesN::from_array(&s.env, &[8; 32]),
        ),
        Error::ProofAlreadySubmitted,
    );
    assert_err(
        s.escrow.try_submit_proof_with_code(
            &ProofKind::Attestation,
            &s.uri(""),
            &s.hash(),
            &s.code(),
        ),
        Error::ProofAlreadySubmitted,
    );
    assert_eq!(s.get().proof, original);
}

#[test]
fn invalid_proof_uris_are_rejected() {
    let s = setup().funded();
    let too_long = std::format!("https://{}", "a".repeat(249));
    assert_eq!(too_long.len(), 257);
    let cases = [
        (ProofKind::Tracking, ""),
        (ProofKind::Content, ""),
        (ProofKind::Tracking, "http://track.example/1"),
        (ProofKind::Tracking, "ftp://track.example/1"),
        (ProofKind::Tracking, "https://"),
        (ProofKind::Tracking, "https://track.example/a b"),
        (ProofKind::Tracking, too_long.as_str()),
    ];
    for (kind, uri) in cases {
        assert_err(
            s.escrow.try_submit_proof(&kind, &s.uri(uri), &s.hash()),
            Error::InvalidUri,
        );
    }
    assert_eq!(s.state(), State::Funded);
}

#[test]
fn valid_proof_uris_are_accepted() {
    let max_len = std::format!("https://{}", "a".repeat(248));
    assert_eq!(max_len.len(), 256);
    let cases = [
        (ProofKind::Attestation, ""),
        (
            ProofKind::Content,
            "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        ),
        (
            ProofKind::Content,
            "ar://bNbA3TEQVL60xlgCcqdz4ZPHFZ711cZ3hmkpGttDt_U",
        ),
        (ProofKind::Tracking, max_len.as_str()),
    ];
    for (kind, uri) in cases {
        let s = setup().funded();
        s.escrow.submit_proof(&kind, &s.uri(uri), &s.hash());
        assert_eq!(s.get().proof().unwrap().uri, s.uri(uri));
    }
}

// --- Release on the buyer's code ---------------------------------------------

#[test]
fn code_after_proof_releases_to_seller_minus_fee() {
    let s = setup().delivered();
    s.escrow.release_with_code(&s.code());
    s.assert_seller_paid();
    assert_eq!(s.get().released_via(), Some(ReleasePath::Code));
}

#[test]
fn code_release_needs_no_signature() {
    let s = setup().delivered();
    s.escrow.release_with_code(&s.code());
    assert!(s.env.auths().is_empty());
}

#[test]
fn code_is_rejected_before_seller_proof() {
    let s = setup().funded();
    assert_err(
        s.escrow.try_release_with_code(&s.code()),
        Error::ProofRequired,
    );
    assert_eq!(s.state(), State::Funded);
}

#[test]
fn one_byte_off_code_is_rejected() {
    let s = setup().delivered();
    assert_err(
        s.escrow.try_release_with_code(&s.wrong_code()),
        Error::InvalidCode,
    );
    assert_eq!(s.state(), State::Delivered);
    assert_eq!(s.balance(&s.escrow.address), AMOUNT);
}

#[test]
fn buyer_can_give_receipt_after_receipt_deadline() {
    let s = setup().delivered();
    s.at(s.get().receipt_deadline + DAY);
    s.escrow.release_with_code(&s.code());
    s.assert_seller_paid();
}

#[test]
fn release_conserves_amount_for_any_fee() {
    for fee_bps in [0, 1, 150, 999, MAX_FEE_BPS] {
        for amount in [1i128, 7, 9_999, 10_001, 1_000_000_007] {
            let env = new_env();
            let mut params = default_params(&env);
            params.fee_bps = fee_bps;
            params.order.amount = amount;
            let s = setup_from(env, params).delivered();
            s.escrow.release_with_code(&s.code());

            let fee = s.balance(&s.fee_recipient);
            assert_eq!(fee, amount * fee_bps as i128 / 10_000);
            assert_eq!(s.balance(&s.seller) + fee, amount);
            assert_eq!(s.balance(&s.escrow.address), 0);
        }
    }
}

// --- Fee sweep -----------------------------------------------------------------

#[test]
fn a_frozen_fee_recipient_does_not_block_the_seller() {
    let s = setup().delivered();
    s.set_authorized(&s.fee_recipient, false);

    s.escrow.release_with_code(&s.code());

    assert_eq!(s.state(), State::Released);
    assert_eq!(s.balance(&s.seller), AMOUNT - FEE);
    assert_eq!(s.balance(&s.fee_recipient), 0);
    assert_eq!(s.balance(&s.escrow.address), FEE);
    assert_eq!(s.get().unswept_fee, FEE);
}

#[test]
fn sweep_fee_recovers_it_once_the_recipient_can_receive_again() {
    let s = setup().delivered();
    s.set_authorized(&s.fee_recipient, false);
    s.escrow.release_with_code(&s.code());

    s.set_authorized(&s.fee_recipient, true);
    s.escrow.sweep_fee();

    assert_eq!(s.get().unswept_fee, 0);
    assert_eq!(s.balance(&s.fee_recipient), FEE);
    assert_eq!(s.balance(&s.escrow.address), 0);
}

#[test]
fn sweep_fee_is_permissionless() {
    let s = setup().delivered();
    s.set_authorized(&s.fee_recipient, false);
    s.escrow.release_with_code(&s.code());
    s.set_authorized(&s.fee_recipient, true);

    s.escrow.sweep_fee();
    assert!(s.env.auths().is_empty());
}

#[test]
fn sweeping_with_nothing_due_is_rejected() {
    let s = setup().delivered();
    s.escrow.release_with_code(&s.code());
    assert_eq!(s.get().unswept_fee, 0);
    assert_err(s.escrow.try_sweep_fee(), Error::NoFeeToSweep);
}

#[test]
fn sweep_fee_emits_fee_swept() {
    let s = setup().delivered();
    s.set_authorized(&s.fee_recipient, false);
    s.escrow.release_with_code(&s.code());
    s.set_authorized(&s.fee_recipient, true);

    s.escrow.sweep_fee();
    assert_eq!(
        s.escrow_events(),
        std::vec![s.event(&FeeSwept { fee: FEE })]
    );
}

// --- Buyer confirmation ------------------------------------------------------

#[test]
fn confirm_releases_after_proof() {
    let s = setup().delivered();
    s.escrow.confirm();
    s.assert_only_auth(&s.buyer);
    s.assert_seller_paid();
    assert_eq!(s.get().released_via(), Some(ReleasePath::Confirmation));
}

#[test]
fn confirm_is_rejected_before_seller_proof() {
    let s = setup().funded();
    assert_err(s.escrow.try_confirm(), Error::ProofRequired);
}

// --- In-person handover ------------------------------------------------------

#[test]
fn proof_with_code_releases_in_one_call() {
    let s = setup().funded();
    s.escrow
        .submit_proof_with_code(&ProofKind::Attestation, &s.uri(""), &s.hash(), &s.code());
    s.assert_only_auth(&s.seller);
    s.assert_seller_paid();
    let e = s.get();
    assert_eq!(e.released_via(), Some(ReleasePath::Code));
    assert_eq!(e.proof().unwrap().kind, ProofKind::Attestation);
}

#[test]
fn proof_with_wrong_code_records_nothing() {
    let s = setup().funded();
    assert_err(
        s.escrow.try_submit_proof_with_code(
            &ProofKind::Attestation,
            &s.uri(""),
            &s.hash(),
            &s.wrong_code(),
        ),
        Error::InvalidCode,
    );
    let e = s.get();
    assert_eq!(e.state, State::Funded);
    assert!(e.proof().is_none());
}

#[test]
fn proof_with_code_is_accepted_after_delivery_deadline() {
    let s = setup().funded();
    s.at(s.get().delivery_deadline + DAY);
    s.escrow
        .submit_proof_with_code(&ProofKind::Attestation, &s.uri(""), &s.hash(), &s.code());
    s.assert_seller_paid();
}

// --- Disputes and the silent buyer -------------------------------------------

#[test]
fn silent_buyer_escalates_instead_of_paying_seller() {
    let s = setup().delivered();
    let receipt_deadline = s.get().receipt_deadline;
    assert_eq!(receipt_deadline, START + RECEIPT_WINDOW);

    s.at(receipt_deadline - 1);
    assert_err(s.escrow.try_escalate(), Error::DeadlineNotReached);

    s.at(receipt_deadline);
    s.escrow.escalate();
    assert!(s.env.auths().is_empty());

    let e = s.get();
    assert_eq!(e.state, State::Disputed);
    let dispute = e.dispute().unwrap();
    assert_eq!(dispute.opened_by, DisputeOrigin::ReceiptTimeout);
    assert_eq!(dispute.from_state, State::Delivered);
    assert_eq!(dispute.deadline, receipt_deadline + ARBITRATION_WINDOW);
    assert_eq!(s.balance(&s.seller), 0);
    assert_eq!(s.balance(&s.escrow.address), AMOUNT);
}

#[test]
fn code_is_not_an_automatic_release_once_disputed() {
    let s = setup().delivered();
    s.escrow.dispute(&s.buyer);
    assert_err(
        s.escrow.try_release_with_code(&s.code()),
        Error::InvalidState,
    );
    assert_err(s.escrow.try_confirm(), Error::InvalidState);
    assert_eq!(s.state(), State::Disputed);
}

#[test]
fn escalate_then_dispute_in_same_ledger() {
    let s = setup().delivered();
    s.at(s.get().receipt_deadline);
    s.escrow.escalate();
    assert_err(s.escrow.try_dispute(&s.buyer), Error::InvalidState);
    assert_eq!(
        s.get().dispute().unwrap().opened_by,
        DisputeOrigin::ReceiptTimeout
    );
}

#[test]
fn dispute_then_escalate_in_same_ledger() {
    let s = setup().delivered();
    s.at(s.get().receipt_deadline);
    s.escrow.dispute(&s.seller);
    assert_err(s.escrow.try_escalate(), Error::InvalidState);
    assert_eq!(s.get().dispute().unwrap().opened_by, DisputeOrigin::Seller);
}

#[test]
fn either_party_can_dispute_from_funded_before_delivery_deadline() {
    let s = setup().funded();
    s.escrow.dispute(&s.seller);
    s.assert_only_auth(&s.seller);
    let dispute = s.get().dispute().unwrap();
    assert_eq!(dispute.opened_by, DisputeOrigin::Seller);
    assert_eq!(dispute.from_state, State::Funded);
}

#[test]
fn stranger_cannot_dispute() {
    let s = setup().delivered();
    let stranger = Address::generate(&s.env);
    assert_err(s.escrow.try_dispute(&stranger), Error::NotParticipant);
}

// --- Events ------------------------------------------------------------------
//
// The indexer builds its escrow list from these, so each transition must emit
// exactly one well-formed event (two for the in-person path) and a rejected
// call must emit none.

const FEE: i128 = AMOUNT * FEE_BPS as i128 / 10_000;

#[test]
fn construction_emits_created_with_both_parties() {
    let s = setup();
    let expected = Created {
        buyer: s.buyer.clone(),
        seller: s.seller.clone(),
        token: s.token.address.clone(),
        amount: AMOUNT,
    };
    assert_eq!(s.escrow_events(), std::vec![s.event(&expected)]);
}

#[test]
fn fund_emits_funded() {
    let s = setup().funded();
    let expected = Funded {
        amount: AMOUNT,
        delivery_deadline: START + DELIVERY_WINDOW,
    };
    assert_eq!(s.escrow_events(), std::vec![s.event(&expected)]);
}

#[test]
fn proof_emits_receipt_deadline() {
    let s = setup().delivered();
    let expected = ProofSubmitted {
        kind: ProofKind::Tracking,
        hash: s.hash(),
        receipt_deadline: START + RECEIPT_WINDOW,
    };
    assert_eq!(s.escrow_events(), std::vec![s.event(&expected)]);
}

#[test]
fn code_release_emits_released_with_fee_split() {
    let s = setup().delivered();
    s.escrow.release_with_code(&s.code());
    let expected = Released {
        path: ReleasePath::Code,
        payout: AMOUNT - FEE,
        fee: FEE,
    };
    assert_eq!(s.escrow_events(), std::vec![s.event(&expected)]);
}

#[test]
fn proof_with_code_emits_proof_then_release() {
    let s = setup().funded();
    s.escrow
        .submit_proof_with_code(&ProofKind::Attestation, &s.uri(""), &s.hash(), &s.code());
    let proof = ProofSubmitted {
        kind: ProofKind::Attestation,
        hash: s.hash(),
        receipt_deadline: START,
    };
    let release = Released {
        path: ReleasePath::Code,
        payout: AMOUNT - FEE,
        fee: FEE,
    };
    assert_eq!(
        s.escrow_events(),
        std::vec![s.event(&proof), s.event(&release)]
    );
}

#[test]
fn escalation_emits_disputed_with_receipt_timeout_origin() {
    let s = setup().delivered();
    let receipt_deadline = s.get().receipt_deadline;
    s.at(receipt_deadline);
    s.escrow.escalate();
    let expected = Disputed {
        opened_by: DisputeOrigin::ReceiptTimeout,
        deadline: receipt_deadline + ARBITRATION_WINDOW,
    };
    assert_eq!(s.escrow_events(), std::vec![s.event(&expected)]);
}

#[test]
fn refund_emits_refunded_with_path_and_full_amount() {
    let s = setup().funded();
    s.escrow.seller_refund();
    let expected = Refunded {
        path: RefundPath::SellerRefund,
        amount: AMOUNT,
    };
    assert_eq!(s.escrow_events(), std::vec![s.event(&expected)]);
}

#[test]
fn cancel_emits_cancelled_with_caller() {
    let s = setup();
    s.escrow.cancel(&s.seller);
    let expected = Cancelled {
        by: s.seller.clone(),
    };
    assert_eq!(s.escrow_events(), std::vec![s.event(&expected)]);
}

#[test]
fn rejected_call_emits_nothing() {
    let s = setup().delivered();
    assert!(s.escrow.try_release_with_code(&s.wrong_code()).is_err());
    assert!(s.escrow_events().is_empty());
}

// --- Same-ledger races -------------------------------------------------------
//
// Transactions in one ledger are applied in order, so when two conflicting
// calls are both valid the first to land decides and the second must fail
// cleanly rather than act on a settled escrow.

#[test]
fn dispute_landing_before_code_blocks_the_release() {
    let s = setup().delivered();
    s.escrow.dispute(&s.buyer);
    assert_err(
        s.escrow.try_release_with_code(&s.code()),
        Error::InvalidState,
    );
    assert_eq!(s.balance(&s.seller), 0);
    assert_eq!(s.balance(&s.escrow.address), AMOUNT);
}

#[test]
fn code_landing_before_dispute_settles_first() {
    let s = setup().delivered();
    s.escrow.release_with_code(&s.code());
    assert_err(s.escrow.try_dispute(&s.buyer), Error::InvalidState);
    s.assert_seller_paid();
}

#[test]
fn confirmation_and_escalation_race_at_receipt_deadline() {
    // At receipt_deadline exactly, the buyer may still confirm and anyone may
    // already escalate. Whichever lands first decides.
    let confirmed = setup().delivered();
    confirmed.at(confirmed.get().receipt_deadline);
    confirmed.escrow.confirm();
    assert_err(confirmed.escrow.try_escalate(), Error::InvalidState);
    confirmed.assert_seller_paid();

    let escalated = setup().delivered();
    escalated.at(escalated.get().receipt_deadline);
    escalated.escrow.escalate();
    assert_err(escalated.escrow.try_confirm(), Error::InvalidState);
    assert_eq!(escalated.state(), State::Disputed);
}

// --- Delivery code format ----------------------------------------------------
//
// The contract hashes exactly the bytes it is given. These tests tie it to the
// reference implementation in trustescrow-code that every client must match.

#[test]
fn generated_code_round_trips_through_the_contract() {
    let env = new_env();
    let mut params = default_params(&env);
    let code = trustescrow_code::encode(&[0x5a; 10]);
    params.order.release_code_hash =
        BytesN::from_array(&env, &trustescrow_code::release_code_hash(&code));
    let s = setup_from(env, params).delivered();

    // The buyer reads the display form aloud; the seller types it in
    // lowercase. After normalisation it is the committed code.
    let typed = trustescrow_code::display(&code).to_lowercase();
    let canonical = trustescrow_code::normalise(&typed).unwrap();
    s.escrow
        .release_with_code(&Bytes::from_slice(&s.env, &canonical));
    s.assert_seller_paid();
}

#[test]
fn unnormalised_display_form_is_rejected() {
    let s = setup().delivered();
    assert_err(
        s.escrow
            .try_release_with_code(&Bytes::from_slice(&s.env, b"K7M2-9XQF-4TBN-R3WD")),
        Error::InvalidCode,
    );
}

#[test]
fn reference_hash_matches_the_contract() {
    let s = setup();
    let code: &[u8; 16] = CODE.try_into().unwrap();
    assert_eq!(
        s.get().release_code_hash.to_array(),
        trustescrow_code::release_code_hash(code)
    );
}

// --- Storage TTL -------------------------------------------------------------
//
// An archived escrow cannot be touched until restored, which for users looks
// like frozen funds. Every state change, and the public bump, must push the
// instance TTL back to full.

impl Setup<'_> {
    fn ttl(&self) -> u32 {
        self.env.as_contract(&self.escrow.address, || {
            self.env.storage().instance().get_ttl()
        })
    }

    fn advance_ledgers(&self, ledgers: u32) {
        let sequence = self.env.ledger().sequence();
        self.env.ledger().set_sequence_number(sequence + ledgers);
    }
}

#[test]
fn state_changes_and_bump_restore_the_full_ttl() {
    // Fund before advancing, so the buyer's token balance cannot expire.
    let s = setup().funded();
    let full = s.ttl();
    let max_ttl = s
        .env
        .as_contract(&s.escrow.address, || s.env.storage().max_ttl());
    assert_eq!(full, TTL_EXTEND_TO.min(max_ttl));

    // Let most of the TTL run down, then change state.
    s.advance_ledgers(full - 10);
    assert_eq!(s.ttl(), 10);
    s.deliver();
    assert_eq!(s.ttl(), full);

    // An idle escrow is kept alive by the permissionless bump.
    s.advance_ledgers(full - 10);
    assert_eq!(s.ttl(), 10);
    s.escrow.bump();
    assert_eq!(s.ttl(), full);
}

// --- Terminality -------------------------------------------------------------

#[test]
fn terminal_states_accept_no_transitions() {
    let released = setup().delivered();
    released.escrow.release_with_code(&released.code());
    released.assert_closed();

    let refunded = setup().funded();
    refunded.escrow.seller_refund();
    refunded.assert_closed();

    let cancelled = setup();
    cancelled.escrow.cancel(&cancelled.buyer);
    cancelled.assert_closed();
}

// --- Seller refund -----------------------------------------------------------

#[test]
fn seller_can_refund_from_any_open_state() {
    for stage in 0..3 {
        let s = setup().funded();
        if stage >= 1 {
            s.deliver();
        }
        if stage == 2 {
            s.escrow.dispute(&s.buyer);
        }
        s.escrow.seller_refund();
        s.assert_only_auth(&s.seller);
        s.assert_buyer_refunded();
        assert_eq!(s.get().refunded_via(), Some(RefundPath::SellerRefund));
    }
}

#[test]
fn seller_cannot_refund_an_unfunded_escrow() {
    let s = setup();
    assert_err(s.escrow.try_seller_refund(), Error::InvalidState);
}

// --- Delivery timeout --------------------------------------------------------

#[test]
fn undelivered_escrow_refunds_buyer_at_delivery_deadline() {
    let s = setup().funded();
    let deadline = s.get().delivery_deadline;
    assert_eq!(deadline, START + DELIVERY_WINDOW);

    s.at(deadline - 1);
    assert_err(
        s.escrow.try_refund_after_delivery_timeout(),
        Error::DeadlineNotReached,
    );

    s.at(deadline);
    s.escrow.refund_after_delivery_timeout();
    assert!(s.env.auths().is_empty());
    s.assert_buyer_refunded();
    assert_eq!(s.get().refunded_via(), Some(RefundPath::DeliveryTimeout));
}

#[test]
fn nobody_can_dispute_from_funded_after_delivery_deadline() {
    let s = setup().funded();
    s.at(s.get().delivery_deadline);
    assert_err(s.escrow.try_dispute(&s.seller), Error::DeadlinePassed);
    assert_err(s.escrow.try_dispute(&s.buyer), Error::DeadlinePassed);
    s.escrow.refund_after_delivery_timeout();
    s.assert_buyer_refunded();
}

#[test]
fn delivered_escrow_has_no_delivery_timeout() {
    let s = setup().delivered();
    s.at(s.get().delivery_deadline + DAY);
    assert_err(
        s.escrow.try_refund_after_delivery_timeout(),
        Error::InvalidState,
    );
}

// --- Extending deadlines -------------------------------------------------------

#[test]
fn buyer_can_extend_the_delivery_deadline() {
    let s = setup().funded();
    let original = s.get().delivery_deadline;
    s.escrow.extend_delivery(&(original + DAY));
    s.assert_only_auth(&s.buyer);
    assert_eq!(s.get().delivery_deadline, original + DAY);

    // A seller who was about to time out can still deliver.
    s.at(original);
    s.deliver();
    assert_eq!(s.state(), State::Delivered);
}

#[test]
fn seller_can_extend_the_receipt_deadline() {
    let s = setup().delivered();
    let original = s.get().receipt_deadline;
    s.escrow.extend_receipt(&(original + DAY));
    s.assert_only_auth(&s.seller);
    assert_eq!(s.get().receipt_deadline, original + DAY);

    // Escalation that would have been due is no longer due.
    s.at(original);
    assert_err(s.escrow.try_escalate(), Error::DeadlineNotReached);
}

#[test]
fn an_extension_can_only_push_the_deadline_later() {
    let s = setup().funded();
    let deadline = s.get().delivery_deadline;
    assert_err(
        s.escrow.try_extend_delivery(&deadline),
        Error::InvalidWindow,
    );
    assert_err(
        s.escrow.try_extend_delivery(&(deadline - 1)),
        Error::InvalidWindow,
    );
}

#[test]
fn an_extension_is_bounded_by_the_maximum_window() {
    let s = setup().funded();
    let too_far = START + MAX_WINDOW + 1;
    assert_err(s.escrow.try_extend_delivery(&too_far), Error::InvalidWindow);
    // The boundary itself is fine.
    s.escrow.extend_delivery(&(START + MAX_WINDOW));
}

#[test]
fn extend_delivery_needs_funded_extend_receipt_needs_delivered() {
    // extend_receipt makes no sense before proof exists.
    let funded = setup().funded();
    assert_err(
        funded
            .escrow
            .try_extend_receipt(&(funded.get().delivery_deadline + DAY)),
        Error::InvalidState,
    );

    // extend_delivery makes no sense once delivery has already happened.
    let delivered = setup().delivered();
    assert_err(
        delivered
            .escrow
            .try_extend_delivery(&(delivered.get().delivery_deadline + DAY)),
        Error::InvalidState,
    );
}

#[test]
fn a_buyer_can_extend_delivery_even_after_the_old_deadline_passed() {
    // Choosing to keep waiting is the buyer's call to make, the same as
    // accepting a late code handover already is in submit_proof_with_code.
    // Nobody has claimed the delivery timeout yet, so there's still
    // something here for the buyer to extend.
    let s = setup().funded();
    let original = s.get().delivery_deadline;
    s.at(original + DAY);
    s.escrow.extend_delivery(&(original + 2 * DAY));
    assert_eq!(s.get().delivery_deadline, original + 2 * DAY);
}

#[test]
fn extensions_emit_extended_with_the_kind_and_new_deadline() {
    let s = setup().funded();
    let new_delivery = s.get().delivery_deadline + DAY;
    s.escrow.extend_delivery(&new_delivery);
    assert_eq!(
        s.escrow_events(),
        std::vec![s.event(&Extended {
            kind: ExtendedKind::Delivery,
            new_deadline: new_delivery,
        })]
    );

    s.deliver();
    let new_receipt = s.get().receipt_deadline + DAY;
    s.escrow.extend_receipt(&new_receipt);
    assert_eq!(
        s.escrow_events(),
        std::vec![s.event(&Extended {
            kind: ExtendedKind::Receipt,
            new_deadline: new_receipt,
        })]
    );
}

// --- Arbitration -------------------------------------------------------------

#[test]
fn arbitrator_can_release() {
    let s = setup().delivered();
    s.escrow.dispute(&s.buyer);
    s.escrow.resolve(&Outcome::Release);
    s.assert_only_auth(&s.arbitrator);
    s.assert_seller_paid();
    assert_eq!(s.get().released_via(), Some(ReleasePath::Arbitration));
}

#[test]
fn arbitrator_refund_returns_full_amount_without_fee() {
    let s = setup().delivered();
    s.escrow.dispute(&s.seller);
    s.escrow.resolve(&Outcome::Refund);
    s.assert_buyer_refunded();
    assert_eq!(s.get().refunded_via(), Some(RefundPath::Arbitration));
}

#[test]
fn only_the_arbitrator_can_resolve() {
    let s = setup().delivered();
    s.escrow.dispute(&s.buyer);
    for impostor in [&s.buyer, &s.seller] {
        s.env.mock_auths(&[MockAuth {
            address: impostor,
            invoke: &MockAuthInvoke {
                contract: &s.escrow.address,
                fn_name: "resolve",
                args: (Outcome::Release,).into_val(&s.env),
                sub_invokes: &[],
            },
        }]);
        assert!(s.escrow.try_resolve(&Outcome::Release).is_err());
    }
    s.env.mock_all_auths();
    assert_eq!(s.state(), State::Disputed);
    assert_eq!(s.balance(&s.escrow.address), AMOUNT);
}

#[test]
fn arbitration_deadline_refunds_buyer() {
    let s = setup().delivered();
    s.escrow.dispute(&s.buyer);
    let deadline = s.get().dispute().unwrap().deadline;

    s.at(deadline - 1);
    assert_err(
        s.escrow.try_refund_after_arbitration_timeout(),
        Error::DeadlineNotReached,
    );

    s.at(deadline);
    assert_err(
        s.escrow.try_resolve(&Outcome::Release),
        Error::DeadlinePassed,
    );
    s.escrow.refund_after_arbitration_timeout();
    assert!(s.env.auths().is_empty());
    s.assert_buyer_refunded();
    assert_eq!(s.get().refunded_via(), Some(RefundPath::ArbitrationTimeout));
}

#[test]
fn resolve_twice_is_rejected() {
    let s = setup().delivered();
    s.escrow.dispute(&s.buyer);
    s.escrow.resolve(&Outcome::Release);
    assert_err(s.escrow.try_resolve(&Outcome::Refund), Error::InvalidState);
    s.assert_seller_paid();
}

#[test]
fn resolve_outside_dispute_is_rejected() {
    let s = setup().delivered();
    assert_err(s.escrow.try_resolve(&Outcome::Release), Error::InvalidState);
    assert_err(
        s.escrow.try_refund_after_arbitration_timeout(),
        Error::InvalidState,
    );
}

// --- Settlement token edge cases (real SAC behaviour) -------------------------
//
// `default_params` uses the real built-in Stellar Asset Contract, not a mock,
// so these tests exercise its actual trustline/authorization/clawback
// semantics rather than guessing at them. In this contract there is no
// separate "trustline does not exist" state distinct from "deauthorized": a
// never-touched address defaults to authorized, and `AUTH_REQUIRED` is what
// makes a fresh address start deauthorized — so that flag is how these tests
// simulate "cannot hold this asset yet".

#[test]
fn seller_without_authorization_blocks_release_but_the_buyer_can_still_be_refunded() {
    let env = new_env();
    let mut params = default_params(&env);
    let sac = env.register_stellar_asset_contract_v2(Address::generate(&env));
    sac.issuer().set_flag(IssuerFlags::RequiredFlag);
    // AUTH_REQUIRED defaults every address to deauthorized — the buyer and
    // fee recipient included, and (since this contract's own balance is
    // gated the same way) the escrow itself. Authorize everyone but the
    // seller, who is the one under test.
    let admin = StellarAssetClient::new(&env, &sac.address());
    admin.set_authorized(&params.order.buyer, &true);
    admin.set_authorized(&params.fee_recipient, &true);
    params.order.token = sac.address();
    let s = setup_from(env, params);
    admin.set_authorized(&s.escrow.address, &true);
    let s = s.delivered();

    // The seller itself was never authorized, so every path that would pay
    // it fails — but it's the seller's own payout that's blocked, not the
    // escrow.
    assert!(s.escrow.try_release_with_code(&s.code()).is_err());
    assert!(s.escrow.try_confirm().is_err());
    assert_eq!(s.state(), State::Delivered);

    // The buyer still has an exit: dispute, and the arbitrator refunds.
    s.escrow.dispute(&s.buyer);
    s.escrow.resolve(&Outcome::Refund);
    s.assert_buyer_refunded();
}

#[test]
fn buyer_frozen_after_funding_blocks_seller_refund_but_release_still_works() {
    let env = new_env();
    let mut params = default_params(&env);
    let sac = env.register_stellar_asset_contract_v2(Address::generate(&env));
    sac.issuer().set_flag(IssuerFlags::RevocableFlag);
    params.order.token = sac.address();
    let s = setup_from(env, params).funded();
    s.set_authorized(&s.buyer, false);

    // seller_refund pays the buyer, so it now fails.
    assert!(s.escrow.try_seller_refund().is_err());
    assert_eq!(s.state(), State::Funded);

    // Release never touches the buyer, so it's still a live exit — as long
    // as the seller delivers. A seller who never does leaves this escrow
    // with no exit at all until the buyer is reauthorised (see the
    // follow-up issue opened for this).
    s.deliver();
    s.escrow.release_with_code(&s.code());
    s.assert_seller_paid();
}

#[test]
fn buyer_frozen_before_the_delivery_deadline_blocks_that_refund_too() {
    let env = new_env();
    let mut params = default_params(&env);
    let sac = env.register_stellar_asset_contract_v2(Address::generate(&env));
    sac.issuer().set_flag(IssuerFlags::RevocableFlag);
    params.order.token = sac.address();
    let s = setup_from(env, params).funded();
    s.set_authorized(&s.buyer, false);

    s.at(s.get().delivery_deadline);
    assert!(s.escrow.try_refund_after_delivery_timeout().is_err());
    assert_eq!(s.state(), State::Funded);
}

#[test]
fn clawback_from_a_funded_escrow_leaves_it_permanently_stuck() {
    let env = new_env();
    let mut params = default_params(&env);
    let sac = env.register_stellar_asset_contract_v2(Address::generate(&env));
    sac.issuer().set_flag(IssuerFlags::ClawbackEnabledFlag);
    params.order.token = sac.address();
    let s = setup_from(env, params).funded();

    StellarAssetClient::new(&s.env, &s.token.address).clawback(&s.escrow.address, &AMOUNT);
    assert_eq!(s.balance(&s.escrow.address), 0);

    // `get()` has no way to know: the record still claims the full amount
    // is held, because nothing re-derives it from the real token balance.
    let e = s.get();
    assert_eq!(e.state, State::Funded);
    assert_eq!(e.amount, AMOUNT);

    // Every exit tries to move the full (no longer existing) amount, so
    // every one of them now fails — permanently. This is why the "Token
    // requirements" section below tells the factory admin to never
    // allowlist a clawback-enabled asset: see the follow-up issue opened
    // for a possible on-chain reconciliation path.
    assert!(s.escrow.try_seller_refund().is_err());
    assert_eq!(s.state(), State::Funded);
    s.deliver();
    assert!(s.escrow.try_release_with_code(&s.code()).is_err());
    assert_eq!(s.state(), State::Delivered);
}
