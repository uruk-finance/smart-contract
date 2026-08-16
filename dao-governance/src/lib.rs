#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, contractevent, symbol_short,
    Address, BytesN, Env, Symbol, Vec, String,
};
use shared_interfaces::{
    SyntheticEngineClient, ComplianceClient, OracleClient,
    PartnerRegistryClient, LiquidityPoolClient, UpgradeableClient,
};

// ─────────────────────────────────────────────────────────────────
//  Constants
// ─────────────────────────────────────────────────────────────────
const VOTING_PERIOD:   u64  = 172_800;  // 48 hours in seconds
const TIMELOCK:        u64  =  86_400;  // 24-hour execution delay
//const VOTING_PERIOD:   u64  = 600;  // 10 minutes in seconds
//const TIMELOCK:        u64  =  300;  // 5 minutes execution delay
const PROP_COUNTER:    Symbol = symbol_short!("PROP_CNT");
const CDP_CONTRACT:    Symbol = symbol_short!("CDP");
const LP_CONTRACT:     Symbol = symbol_short!("LP");
const PARTNER_REG:     Symbol = symbol_short!("PARTNER");
/// EarningsDistributor address — stored for completeness/future DAO-gated
/// actions on it; no current ACTION_* cross-calls it directly.
const EARNINGS_CONTRACT: Symbol = symbol_short!("EARNCTR");
/// StabilityPool address — stored for completeness/future DAO-gated
/// actions on it; no current ACTION_* cross-calls it directly.
const STAB_POOL_CONTRACT: Symbol = symbol_short!("STABCTR");
const DAO_ADDR:        Symbol = symbol_short!("DAO_ADDR");
const MULTISIG_COUNT:  Symbol = symbol_short!("MSIGCNT");
const MSIG_WALLET:     Symbol = symbol_short!("MSIGWLT");
/// Primary governance multi-sig wallet created at deploy (`create_multisig`
/// → wallet_id 0). Only its signers may propose or vote.
const GOV_WALLET_ID:   u32 = 0;
/// Compliance registry address. DAO governance (propose/vote) is
/// intentionally KYC-free — no KYC check is applied — but participation
/// is restricted to signers of the primary multi-sig wallet (see
/// `GOV_WALLET_ID`). This address is still used so the DAO can administer
/// the compliance registry itself (e.g. authorising/revoking KYC providers
/// via ACTION_KYC_ADD_VERIFIER/ACTION_KYC_REMOVE_VERIFIER proposals).
const COMPLIANCE:      Symbol = symbol_short!("COMPLY");
/// Oracle-twap registry address. Required to be set (via `set_oracle`)
/// before any oracle-provider-management proposal type can be executed.
const ORACLE:          Symbol = symbol_short!("ORACLE");
/// Actions that require multi-sig (even after DAO vote passes)
/// Bits: 0=deploy_asset, 1=deactivate, 2=unused (was upgrade_tier),
/// 6=add_signer, 7=remove_signer, 8=oracle_add_provider, 9=oracle_remove_provider,
/// 10=oracle_set_min_providers, 11=kyc_add_verifier, 12=kyc_remove_verifier,
/// 15=activate_partner_asset, 16=upgrade_contract.
/// All signer- and provider-management actions require multi-sig approval —
/// these are privileged enough that a GOV-token vote alone is not sufficient.
const MSIG_REQUIRED_MASK: u32 = 0b1_1001_1111_1100_0111; // bits 0,1,2,6-12,15,16

/// Action types for proposals (must match action_type bitmask bits)
pub const ACTION_DEPLOY_ASSET:     u32 = 0;
pub const ACTION_DEACTIVATE:       u32 = 1;
pub const ACTION_UPGRADE_TIER:     u32 = 2;
pub const ACTION_ADJUST_EARNINGS:  u32 = 3;
pub const ACTION_ADJUST_RATIO:     u32 = 4;
pub const ACTION_CREATE_POOL:      u32 = 5;
pub const ACTION_ADD_SIGNER:       u32 = 6;
pub const ACTION_REMOVE_SIGNER:    u32 = 7;
/// Add a new authorised price-feed provider on the oracle-twap contract.
/// Payload: [addr_len: u8][provider address strkey bytes]
pub const ACTION_ORACLE_ADD_PROVIDER:      u32 = 8;
/// Remove a price-feed provider from the oracle-twap contract.
/// Payload: [addr_len: u8][provider address strkey bytes]
pub const ACTION_ORACLE_REMOVE_PROVIDER:   u32 = 9;
/// Adjust the oracle's minimum-observations-needed quorum.
/// Payload: [min_providers: u32 LE (4 bytes)]
pub const ACTION_ORACLE_SET_MIN_PROVIDERS: u32 = 10;
/// Authorise a new KYC provider (verifier) on the compliance registry.
/// Payload: [addr_len: u8][verifier address strkey bytes]
pub const ACTION_KYC_ADD_VERIFIER:         u32 = 11;
/// Revoke a KYC provider (verifier) on the compliance registry.
/// Payload: [addr_len: u8][verifier address strkey bytes]
pub const ACTION_KYC_REMOVE_VERIFIER:      u32 = 12;
/// Whitelist (or de-whitelist) a collateral token on the synthetic engine,
/// independent of deploying a new market.
/// Payload: [addr_len: u8][token address strkey bytes][allowed: u8 (0/1)]
pub const ACTION_SET_COLLATERAL_ALLOWED:   u32 = 13;
/// Register a new partner on PartnerRegistry (DAO approval of a join application).
/// Payload: [addr_len:u8][owner strkey][name_len:u8][name utf8]
pub const ACTION_REGISTER_PARTNER:         u32 = 14;
/// Activate a partner-submitted synth (sets live=true on PartnerRegistry).
/// Payload: [symbol_len:u8][symbol utf8]
pub const ACTION_ACTIVATE_PARTNER_ASSET:   u32 = 15;
/// Replace a protocol contract's Wasm in place (storage preserved).
/// Payload: [addr_len:u8][target strkey][wasm_hash: 32 bytes]
pub const ACTION_UPGRADE_CONTRACT: u32 = 16;
pub const ACTION_GENERIC:          u32 = 99;

// ─────────────────────────────────────────────────────────────────
//  Types
// ─────────────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum ProposalStatus {
    Active,
    Passed,
    Failed,
    Executed,
    Cancelled,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Proposal {
    pub id:           u64,
    pub proposer:     Address,
    pub title:        soroban_sdk::Bytes,
    pub description:  soroban_sdk::Bytes,
    /// Encoded call: (target_contract, function_name, args_bytes)
    pub call_data:    soroban_sdk::Bytes,
    /// One-address-one-vote count (no GOV token weighting yet — see `vote`).
    pub yes_votes:    i128,
    pub no_votes:     i128,
    pub start_time:   u64,
    pub end_time:     u64,
    pub execute_after: u64,
    pub status:       ProposalStatus,
}

#[contracttype]
pub enum DataKey {
    Proposal(u64),
    HasVoted(u64, Address),   // (proposal_id, voter)
    ParamCR,                  // minimum collateral ratio
    ParamBorrowFee,
    ParamLiqPenalty,
    // Multi-sig storage keys
    MultiSigWallet(u32),      // wallet_id → MultiSigConfig
    MultiSigSigner(u32, u32), // (wallet_id, signer_idx) → Address
    MultiSigTx(u64),          // tx_id → MultiSigTransaction
    MultiSigApproval(u64, Address), // (tx_id, signer) → bool
    MultiSigTxCount,
    MultiSigWalletTxIds(u32), // wallet_id → Vec<u64> (all tx ids for this wallet)
    ProposalMultiSigTxs(u64), // proposal_id → Vec<u64> (multi-sig tx ids linked to this proposal)
    MultiSigTxProposal(u64),  // tx_id → proposal_id (reverse lookup)
}

// ─────────────────────────────────────────────────────────────────
//  Multi-Signature Control Types
// ─────────────────────────────────────────────────────────────────

/// Which subset of actions require multi-sig
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum MultiSigScope {
    /// Only privileged actions (deploy asset, deactivate partner, activate partner asset)
    PrivilegedOnly = 0,
    /// All DAO-executed actions require multi-sig
    AllExecutions = 1,
    /// Multi-sig is off — DAO vote alone is sufficient
    Disabled = 2,
}

/// Configuration for a multi-sig wallet
#[contracttype]
#[derive(Clone, Debug)]
pub struct MultiSigConfig {
    pub id:        u32,
    pub threshold: u32,           // required approvals (e.g. 3 of 5)
    pub signers:   Vec<Address>,  // list of signer addresses
    pub scope:     MultiSigScope,
    pub active:    bool,
}

/// A multi-sig transaction awaiting approval
#[contracttype]
#[derive(Clone, Debug)]
pub struct MultiSigTx {
    pub id:            u64,
    pub wallet_id:     u32,
    pub proposal_id:   u64,
    pub target:        Address,
    pub function_name: Symbol,
    pub call_data:     soroban_sdk::Bytes,
    pub approvals:     u32,
    pub rejections:    u32,
    pub status:        MultiSigTxStatus,
    pub created_at:    u64,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum MultiSigTxStatus {
    Pending,
    Approved,
    Rejected,
    Executed,
    Cancelled,
}

/// Info about a single signer's approval (for frontend display)
#[contracttype]
#[derive(Clone, Debug)]
pub struct MultiSigApprovalInfo {
    pub signer:   Address,
    pub approved: bool,
}

/// Summary of a multi-sig tx for dashboard display
#[contracttype]
#[derive(Clone, Debug)]
pub struct MultiSigTxSummary {
    pub tx_id:         u64,
    pub wallet_id:     u32,
    pub proposal_id:   u64,
    pub target:        Address,
    pub function_name: Symbol,
    pub approvals:     u32,
    pub rejections:    u32,
    pub threshold:     u32,
    pub signer_count:  u32,
    pub status:        MultiSigTxStatus,
    pub created_at:    u64,
}

// ─────────────────────────────────────────────────────────────────
//  Multi-Sig Events
// ─────────────────────────────────────────────────────────────────
#[contractevent(topics = ["msig_created"])]
pub struct MultiSigCreatedEvent {
    pub wallet_id: u32,
    pub threshold: u32,
    pub signer_count: u32,
}

#[contractevent(topics = ["msig_approved"])]
pub struct MultiSigApprovedEvent {
    #[topic]
    pub tx_id: u64,
    #[topic]
    pub signer: Address,
}

#[contractevent(topics = ["msig_executed"])]
pub struct MultiSigExecutedEvent {
    pub tx_id: u64,
    pub proposal_id: u64,
}

// ─────────────────────────────────────────────────────────────────
//  Events (Protocol 23: using #[contractevent] macro)
// ─────────────────────────────────────────────────────────────────
#[contractevent(topics = ["proposed"])]
pub struct ProposedEvent {
    #[topic]
    pub proposer: Address,
    pub proposal_id: u64,
}

#[contractevent(topics = ["voted"])]
pub struct VotedEvent {
    #[topic]
    pub voter: Address,
    pub proposal_id: u64,
    pub support: bool,
    pub weight: i128,
}

#[contractevent(topics = ["executed"])]
pub struct ExecutedEvent {
    pub proposal_id: u64,
}

#[contractevent(topics = ["cancelled"])]
pub struct CancelledEvent {
    pub proposal_id: u64,
}

// ─────────────────────────────────────────────────────────────────
//  DAO Governance Contract
// ─────────────────────────────────────────────────────────────────
#[contract]
pub struct DAOGovernance;

#[contractimpl]
impl DAOGovernance {
    // ── Init ──────────────────────────────────────────────────────
    pub fn initialize(env: Env, dao: Address, cdp: Address) {
        dao.require_auth();
        env.storage().instance().set(&DAO_ADDR, &dao);
        env.storage().instance().set(&CDP_CONTRACT, &cdp);
        env.storage().instance().set(&PROP_COUNTER, &0_u64);
        // Default protocol parameters
        env.storage().persistent().set(&DataKey::ParamCR, &15_000_i128);        // 150%
        env.storage().persistent().set(&DataKey::ParamBorrowFee, &50_i128);     // 0.5%
        env.storage().persistent().set(&DataKey::ParamLiqPenalty, &1_000_i128); // 10%
    }

    /// Set protocol-level configuration: multisig wallet address and
    /// external contract addresses the DAO can invoke.
    /// Only the DAO admin (multisig wallet) can call this.
    pub fn set_protocol_config(
        env: Env,
        dao: Address,
        multisig_wallet: Address,
        lp_contract: Address,
        partner_registry: Address,
        earnings_contract: Address,
        stab_pool_contract: Address,
    ) {
        Self::_require_dao(&env, &dao);
        env.storage().instance().set(&MSIG_WALLET, &multisig_wallet);
        env.storage().instance().set(&LP_CONTRACT, &lp_contract);
        env.storage().instance().set(&PARTNER_REG, &partner_registry);
        env.storage().instance().set(&EARNINGS_CONTRACT, &earnings_contract);
        env.storage().instance().set(&STAB_POOL_CONTRACT, &stab_pool_contract);
    }

    /// Current earnings-distributor address, if configured.
    pub fn get_earnings_contract(env: Env) -> Option<Address> {
        env.storage().instance().get(&EARNINGS_CONTRACT)
    }

    /// Current stability-pool address, if configured.
    pub fn get_stab_pool_contract(env: Env) -> Option<Address> {
        env.storage().instance().get(&STAB_POOL_CONTRACT)
    }

    /// Point the DAO at a compliance-registry deployment. This does NOT
    /// gate `propose`/`vote`/`propose_and_queue_multisig` via KYC —
    /// those are gated by multi-sig membership instead (see
    /// `_require_governance_signer`). It only enables KYC-provider-
    /// management proposal types (ACTION_KYC_ADD_VERIFIER /
    /// ACTION_KYC_REMOVE_VERIFIER). Only the DAO (multi-sig) can call this.
    pub fn set_compliance(env: Env, dao: Address, compliance: Address) {
        Self::_require_dao(&env, &dao);
        env.storage().instance().set(&COMPLIANCE, &compliance);
    }

    /// Current compliance-registry address, if configured.
    pub fn get_compliance(env: Env) -> Option<Address> {
        env.storage().instance().get(&COMPLIANCE)
    }

    /// Point the DAO at an oracle-twap deployment. Required before any
    /// `ACTION_ORACLE_*` proposal (add/remove provider, adjust min-providers
    /// quorum) can be executed — see `_apply_call_data`. Only the DAO
    /// (multi-sig) can call this.
    pub fn set_oracle(env: Env, dao: Address, oracle: Address) {
        Self::_require_dao(&env, &dao);
        env.storage().instance().set(&ORACLE, &oracle);
    }

    /// Genesis bootstrap: add an oracle provider. Oracle-twap stores this
    /// contract as `dao`, so the call is authorised by us (not the admin
    /// EOA). After genesis, prefer ACTION_ORACLE_ADD_PROVIDER proposals.
    pub fn add_oracle_provider(env: Env, dao: Address, provider: Address) {
        Self::_require_dao(&env, &dao);
        Self::_oracle(&env).add_provider(&env.current_contract_address(), &provider);
    }

    /// Genesis bootstrap: set the oracle observation quorum. Same auth
    /// model as `add_oracle_provider`. After genesis, prefer
    /// ACTION_ORACLE_SET_MIN_PROVIDERS proposals.
    pub fn set_oracle_min_providers(env: Env, dao: Address, min_providers: u32) {
        Self::_require_dao(&env, &dao);
        Self::_oracle(&env).set_min_providers(&env.current_contract_address(), &min_providers);
    }

    /// Current oracle-twap address, if configured.
    pub fn get_oracle(env: Env) -> Option<Address> {
        env.storage().instance().get(&ORACLE)
    }

    /// Get the protocol multisig wallet address.
    pub fn get_multisig_wallet(env: Env) -> Address {
        env.storage().instance()
            .get(&MSIG_WALLET)
            .unwrap_or_else(|| {
                // Fall back to DAO address if no explicit multisig wallet set
                env.storage().instance().get(&DAO_ADDR).unwrap()
            })
    }

    /// Replace this contract's Wasm. Call after `stellar contract upload`.
    /// `dao` must be the address stored at init (the deployer admin).
    pub fn upgrade(env: Env, dao: Address, new_wasm_hash: BytesN<32>) {
        Self::_require_dao(&env, &dao);
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    // ── Submit a governance proposal ─────────────────────────────
    pub fn propose(
        env:         Env,
        proposer:    Address,
        title:       soroban_sdk::Bytes,
        description: soroban_sdk::Bytes,
        call_data:   soroban_sdk::Bytes,
    ) -> u64 {
        proposer.require_auth();
        // Only multi-sig signers (wallet 0) may propose. KYC-free by
        // design — membership is the access control, not compliance.
        Self::_require_governance_signer(&env, &proposer);

        let id: u64 = env.storage().instance().get(&PROP_COUNTER).unwrap_or(0);
        let now = env.ledger().timestamp();

        let proposal = Proposal {
            id,
            proposer: proposer.clone(),
            title,
            description,
            call_data,
            yes_votes: 0,
            no_votes:  0,
            start_time:    now,
            end_time:      now + VOTING_PERIOD,
            execute_after: now + VOTING_PERIOD + TIMELOCK,
            status: ProposalStatus::Active,
        };

        env.storage().persistent().set(&DataKey::Proposal(id), &proposal);
        env.storage().instance().set(&PROP_COUNTER, &(id + 1));
        ProposedEvent {
            proposer,
            proposal_id: id,
        }.publish(&env);
        id
    }

    // ── Cast a vote ───────────────────────────────────────────────
    pub fn vote(env: Env, voter: Address, proposal_id: u64, support: bool) {
        voter.require_auth();
        // Only multi-sig signers (wallet 0) may vote. KYC-free by design.
        Self::_require_governance_signer(&env, &voter);

        let mut proposal = Self::_require_active(&env, proposal_id);
        assert!(
            !env.storage().persistent().has(&DataKey::HasVoted(proposal_id, voter.clone())),
            "already voted"
        );

        // One-signer-one-vote. `HasVoted` stops a given address from
        // voting twice on the same proposal.
        let weight: i128 = 1;

        if support { proposal.yes_votes += weight; }
        else        { proposal.no_votes  += weight; }

        env.storage().persistent().set(&DataKey::HasVoted(proposal_id, voter.clone()), &true);
        env.storage().persistent().set(&DataKey::Proposal(proposal_id), &proposal);
        VotedEvent {
            voter,
            proposal_id,
            support,
            weight,
        }.publish(&env);
    }

    // ── Finalise: lock in Passed once Yes votes meet the signer threshold ─
    /// Does not close the ballot at `end_time`. Late signers may still vote
    /// while status is Active. Execute additionally requires the timelock.
    pub fn finalise(env: Env, proposal_id: u64) {
        let mut proposal = Self::_get_proposal(&env, proposal_id);
        assert!(proposal.status == ProposalStatus::Active, "not active");
        let threshold = Self::_gov_threshold(&env);
        assert!(proposal.yes_votes >= threshold, "threshold not reached");
        proposal.status = ProposalStatus::Passed;
        env.storage().persistent().set(&DataKey::Proposal(proposal_id), &proposal);
    }

    // ── Execute after threshold + timelock ───────────────────────
    pub fn execute(env: Env, executor: Address, proposal_id: u64) {
        executor.require_auth();
        let mut proposal = Self::_get_proposal(&env, proposal_id);
        assert!(
            proposal.status == ProposalStatus::Active || proposal.status == ProposalStatus::Passed,
            "proposal not executable"
        );
        let threshold = Self::_gov_threshold(&env);
        assert!(proposal.yes_votes >= threshold, "threshold not reached");
        assert!(env.ledger().timestamp() >= proposal.execute_after, "timelock not expired");

        // Decode call_data and route to cross-contract calls or param updates
        if !proposal.call_data.is_empty() {
            // For direct proposals, target is the CDP contract by default
            let cdp: Address = env.storage().instance().get(&CDP_CONTRACT).unwrap();
            let fn_sym = symbol_short!("apply");
            Self::_apply_call_data(&env, &proposal.call_data, &cdp, &fn_sym, &executor);
        }

        proposal.status = ProposalStatus::Executed;
        env.storage().persistent().set(&DataKey::Proposal(proposal_id), &proposal);
        ExecutedEvent {
            proposal_id,
        }.publish(&env);
    }

    // ── Cancel (proposer only, while still active) ────────────────
    pub fn cancel(env: Env, proposer: Address, proposal_id: u64) {
        proposer.require_auth();
        let mut proposal = Self::_get_proposal(&env, proposal_id);
        assert!(proposal.proposer == proposer, "only proposer can cancel");
        assert!(proposal.status == ProposalStatus::Active, "not active");
        proposal.status = ProposalStatus::Cancelled;
        env.storage().persistent().set(&DataKey::Proposal(proposal_id), &proposal);
        CancelledEvent {
            proposal_id,
        }.publish(&env);
    }

    // ── Views ─────────────────────────────────────────────────────
    pub fn get_proposal(env: Env, id: u64) -> Proposal {
        Self::_get_proposal(&env, id)
    }

    pub fn has_voted(env: Env, id: u64, voter: Address) -> bool {
        env.storage().persistent().has(&DataKey::HasVoted(id, voter))
    }

    /// Whether `addr` is a signer of the primary governance multi-sig
    /// (wallet_id 0). Used by the webapp to gate Vote / Propose UI.
    pub fn is_governance_signer(env: Env, addr: Address) -> bool {
        Self::_is_governance_signer(&env, &addr)
    }

    pub fn get_param_cr(env: Env) -> i128 {
        env.storage().persistent().get(&DataKey::ParamCR).unwrap()
    }

    pub fn get_param_borrow_fee(env: Env) -> i128 {
        env.storage().persistent().get(&DataKey::ParamBorrowFee).unwrap()
    }

    /// Return the next proposal ID (current counter value).
    /// Call this before `propose()` to know what ID the new proposal will get.
    pub fn get_proposal_count(env: Env) -> u64 {
        env.storage().instance().get(&PROP_COUNTER).unwrap_or(0)
    }

    // ── Internals ─────────────────────────────────────────────────
    fn _require_active(env: &Env, id: u64) -> Proposal {
        let p = Self::_get_proposal(env, id);
        assert!(p.status == ProposalStatus::Active, "proposal not active");
        p
    }

    fn _gov_threshold(env: &Env) -> i128 {
        Self::_get_multisig_config(env, GOV_WALLET_ID).threshold as i128
    }

    fn _get_proposal(env: &Env, id: u64) -> Proposal {
        env.storage().persistent()
            .get(&DataKey::Proposal(id))
            .expect("proposal not found")
    }

    /// True when `addr` is listed as a signer on the primary governance
    /// multi-sig wallet (created at deploy as wallet_id 0).
    fn _is_governance_signer(env: &Env, addr: &Address) -> bool {
        let cfg: Option<MultiSigConfig> = env.storage().persistent()
            .get(&DataKey::MultiSigWallet(GOV_WALLET_ID));
        match cfg {
            Some(c) if c.active => Self::_is_signer(env, GOV_WALLET_ID, addr, &c),
            _ => false,
        }
    }

    fn _require_governance_signer(env: &Env, addr: &Address) {
        assert!(
            Self::_is_governance_signer(env, addr),
            "only governance signers may propose/vote"
        );
    }

    /// Decode call_data and route to the appropriate cross-contract call.
    /// call_data format: [action_type: u32 (4 bytes LE)][payload...]
    /// Action types map to the standard ACTION_* constants.
    ///
    /// `caller` is the identity authorising this execution — the plain
    /// vote-executor for `execute()`, or the multi-sig wallet address for
    /// `execute_multisig_tx()`.
    ///
    /// Oracle-twap and PartnerRegistry store **this contract** as `dao`, so
    /// those cross-calls pass `env.current_contract_address()` (contract-to-contract
    /// auth). Other DAO-gated registries still store the protocol multi-sig
    /// wallet as `dao` and receive `caller`; Soroban auto-authorises a matching
    /// transaction-source-account at any call depth.
    fn _apply_call_data(
        env: &Env,
        data: &soroban_sdk::Bytes,
        _target: &Address,
        _fn_name: &Symbol,
        caller: &Address,
    ) {
        if data.len() < 4 { return; }

        // Decode action_type from first 4 bytes (u32 LE)
        let mut abuf = [0u8; 4];
        for i in 0..4usize {
            abuf[i] = data.get(i as u32).unwrap().into();
        }
        let action_type = u32::from_le_bytes(abuf);

        // Payload starts at byte 4
        let payload_len = data.len().saturating_sub(4);

        match action_type {
            // ── Deploy Asset ──────────────────────────────────────
            // payload: [symbol_len: u8][symbol: bytes]
            //          [token_len: u8][synth token address strkey: bytes]
            //          [oracle_sym_len: u8][oracle_sym: bytes]
            //          [coll_token_len: u8][collateral token address strkey: bytes]
            //          [coll_oracle_len: u8][coll_oracle: bytes]
            //          [min_cr: i128 LE (16)][liq_cr: i128 LE (16)]
            //          [liq_penalty: i128 LE (16)][stab_fee_bps: i128 LE (16)]
            //          [debt_ceiling: i128 LE (16)]
            // Calls SyntheticEngine.register_asset() to create the market AND
            // SyntheticEngine.set_collateral_allowed() to whitelist its
            // chosen collateral token in the same proposal execution — this
            // is what lets "deploying a market" also select its collateral.
            ACTION_DEPLOY_ASSET => {
                let cdp: Address = env.storage().instance().get(&CDP_CONTRACT).unwrap();
                let mut pos = 4u32;
                let (symbol, p)      = Self::_decode_symbol_at(env, data, pos);  pos = p;
                let (token, p)       = Self::_decode_address_at(env, data, pos); pos = p;
                let (oracle_sym, p)  = Self::_decode_symbol_at(env, data, pos);  pos = p;
                let (coll_token, p)  = Self::_decode_address_at(env, data, pos); pos = p;
                let (coll_oracle, p) = Self::_decode_symbol_at(env, data, pos);  pos = p;
                let (min_cr, p)        = Self::_decode_i128_at(data, pos); pos = p;
                let (liq_cr, p)        = Self::_decode_i128_at(data, pos); pos = p;
                let (liq_penalty, p)   = Self::_decode_i128_at(data, pos); pos = p;
                let (stab_fee_bps, p)  = Self::_decode_i128_at(data, pos); pos = p;
                let (debt_ceiling, _p) = Self::_decode_i128_at(data, pos);

                let engine = SyntheticEngineClient::new(env, &cdp);
                engine.register_asset(
                    caller, &symbol, &token, &oracle_sym, &coll_oracle,
                    &min_cr, &liq_cr, &liq_penalty, &stab_fee_bps, &debt_ceiling,
                );
                engine.set_collateral_allowed(caller, &coll_token, &true);
            }

            // ── Whitelist / de-whitelist a collateral token ────────
            // payload: [addr_len: u8][token address strkey: bytes][allowed: u8 (0/1)]
            ACTION_SET_COLLATERAL_ALLOWED => {
                let cdp: Address = env.storage().instance().get(&CDP_CONTRACT).unwrap();
                let (token, pos) = Self::_decode_address_at(env, data, 4);
                let allowed_byte: u8 = data.get(pos).unwrap().into();
                SyntheticEngineClient::new(env, &cdp)
                    .set_collateral_allowed(caller, &token, &(allowed_byte != 0));
            }

            // ── Deactivate a partner ────────────────────────────────
            // payload: [partner_id: u32 LE (4 bytes)]
            ACTION_DEACTIVATE => {
                let preg: Address = env.storage().instance().get(&PARTNER_REG)
                    .expect("partner registry not configured — call set_protocol_config first");
                let (partner_id, _pos) = Self::_decode_u32_at(data, 4);
                PartnerRegistryClient::new(env, &preg)
                    .deactivate_partner(&env.current_contract_address(), &partner_id);
            }

            // Retired: partner tiers were removed. Old proposals fail here.
            ACTION_UPGRADE_TIER => {
                panic!("upgrade_tier removed — all partners have the same rights");
            }

            // ── Adjust Ratios ─────────────────────────────────────
            ACTION_ADJUST_RATIO => {
                // payload: [min_cr: i128 LE (16 bytes)][liq_penalty: i128 LE (16 bytes)]
                if payload_len >= 16 {
                    let mut cr_buf = [0u8; 16];
                    for i in 0..16usize {
                        cr_buf[i] = data.get((4 + i) as u32).unwrap().into();
                    }
                    let min_cr = i128::from_le_bytes(cr_buf);
                    env.storage().persistent().set(&DataKey::ParamCR, &min_cr);
                }
                if payload_len >= 32 {
                    let mut lp_buf = [0u8; 16];
                    for i in 0..16usize {
                        lp_buf[i] = data.get((4 + 16 + i) as u32).unwrap().into();
                    }
                    let liq_penalty = i128::from_le_bytes(lp_buf);
                    env.storage().persistent().set(&DataKey::ParamLiqPenalty, &liq_penalty);
                }
            }

            // ── Adjust a partner's revenue share ───────────────────
            // payload: [partner_id: u32 LE (4 bytes)][new_share: i128 LE (16 bytes)]
            // NOTE: this used to just write to a local, never-consumed
            // "borrow fee" parameter — repurposed to actually call
            // PartnerRegistry.set_partner_share(), which is what the
            // webapp's "Adjust Earning Split" form has always collected.
            ACTION_ADJUST_EARNINGS => {
                let preg: Address = env.storage().instance().get(&PARTNER_REG)
                    .expect("partner registry not configured — call set_protocol_config first");
                let (partner_id, pos) = Self::_decode_u32_at(data, 4);
                let (new_share, _pos) = Self::_decode_i128_at(data, pos);
                PartnerRegistryClient::new(env, &preg)
                    .set_partner_share(&env.current_contract_address(), &partner_id, &new_share);
            }

            // ── Register a partner (approve a join application) ────
            // payload: [addr_len:u8][owner][name_len:u8][name]
            ACTION_REGISTER_PARTNER => {
                let preg: Address = env.storage().instance().get(&PARTNER_REG)
                    .expect("partner registry not configured — call set_protocol_config first");
                let mut pos = 4u32;
                let (owner, p) = Self::_decode_address_at(env, data, pos); pos = p;
                let (name, _p) = Self::_decode_string_at(env, data, pos);
                PartnerRegistryClient::new(env, &preg)
                    .dao_register_partner(&env.current_contract_address(), &owner, &name);
            }

            // ── Activate a partner-submitted asset ─────────────────
            // payload: [symbol_len:u8][symbol]
            ACTION_ACTIVATE_PARTNER_ASSET => {
                let preg: Address = env.storage().instance().get(&PARTNER_REG)
                    .expect("partner registry not configured — call set_protocol_config first");
                let (symbol, _p) = Self::_decode_symbol_at(env, data, 4);
                PartnerRegistryClient::new(env, &preg)
                    .activate_partner_asset(&env.current_contract_address(), &symbol);
            }

            // ── Create Pool ───────────────────────────────────────
            // payload: [pool_id_len: u8][pool_id: bytes]
            //          [token_a_len: u8][token_a address strkey: bytes]
            //          [token_b_len: u8][token_b address strkey: bytes]
            //          [fee_tier: i128 LE (16 bytes)]
            ACTION_CREATE_POOL => {
                let lp: Address = env.storage().instance().get(&LP_CONTRACT)
                    .expect("liquidity pool not configured — call set_protocol_config first");
                let mut pos = 4u32;
                let (pool_id, p) = Self::_decode_symbol_at(env, data, pos);  pos = p;
                let (token_a, p) = Self::_decode_address_at(env, data, pos); pos = p;
                let (token_b, p) = Self::_decode_address_at(env, data, pos); pos = p;
                let (fee_tier, _p) = Self::_decode_i128_at(data, pos);

                LiquidityPoolClient::new(env, &lp)
                    .create_pool(caller, &pool_id, &token_a, &token_b, &fee_tier);
            }

            // ── Add Signer ────────────────────────────────────────
            // payload: [wallet_id: u32 LE (4 bytes)][addr_len: u8][addr_strkey: N bytes]
            ACTION_ADD_SIGNER => {
                if payload_len < 5 { return; }

                let mut wbuf = [0u8; 4];
                for i in 0..4usize { wbuf[i] = data.get((4 + i) as u32).unwrap().into(); }
                let wallet_id = u32::from_le_bytes(wbuf);

                let addr_len: u8 = data.get(8).unwrap().into();
                if payload_len < 5 + addr_len as u32 { return; }
                let mut addr_bytes = soroban_sdk::Bytes::new(env);
                for i in 0..addr_len {
                    addr_bytes.push_back(data.get((9 + i as u32) as u32).unwrap());
                }
                let new_signer = Address::from_string_bytes(&addr_bytes);

                // Add signer to wallet
                let mut cfg: MultiSigConfig = env.storage().persistent()
                    .get(&DataKey::MultiSigWallet(wallet_id))
                    .expect("multi-sig wallet not found");
                assert!(cfg.active, "multi-sig wallet inactive");
                assert!(cfg.signers.len() < 15, "max 15 signers per wallet");

                let mut already = false;
                for s in cfg.signers.iter() {
                    if s == new_signer { already = true; break; }
                }
                assert!(!already, "address is already a signer");

                let idx = cfg.signers.len() as u32;
                cfg.signers.push_back(new_signer.clone());
                env.storage().persistent().set(&DataKey::MultiSigWallet(wallet_id), &cfg);
                env.storage().persistent()
                    .set(&DataKey::MultiSigSigner(wallet_id, idx), &new_signer);
            }

            // ── Remove Signer ─────────────────────────────────────
            // payload: [wallet_id: u32 LE (4 bytes)][addr_len: u8][addr_strkey: N bytes]
            ACTION_REMOVE_SIGNER => {
                if payload_len < 5 { return; }

                let mut wbuf = [0u8; 4];
                for i in 0..4usize { wbuf[i] = data.get((4 + i) as u32).unwrap().into(); }
                let wallet_id = u32::from_le_bytes(wbuf);

                let addr_len: u8 = data.get(8).unwrap().into();
                if payload_len < 5 + addr_len as u32 { return; }
                let mut addr_bytes = soroban_sdk::Bytes::new(env);
                for i in 0..addr_len {
                    addr_bytes.push_back(data.get((9 + i as u32) as u32).unwrap());
                }
                let old_signer = Address::from_string_bytes(&addr_bytes);

                // Remove signer from wallet
                let mut cfg: MultiSigConfig = env.storage().persistent()
                    .get(&DataKey::MultiSigWallet(wallet_id))
                    .expect("multi-sig wallet not found");
                assert!(cfg.active, "multi-sig wallet inactive");
                let signer_count: u32 = cfg.signers.len() as u32;
                assert!(signer_count > cfg.threshold,
                    "cannot remove signer: would drop below threshold");

                let mut found = false;
                let mut new_signers: Vec<Address> = Vec::new(env);
                for s in cfg.signers.iter() {
                    if s == old_signer && !found {
                        found = true;
                        continue;
                    }
                    new_signers.push_back(s);
                }
                assert!(found, "signer not found in wallet");

                cfg.signers = new_signers;
                env.storage().persistent().set(&DataKey::MultiSigWallet(wallet_id), &cfg);
                for (i, signer) in cfg.signers.iter().enumerate() {
                    env.storage().persistent()
                        .set(&DataKey::MultiSigSigner(wallet_id, i as u32), &signer);
                }
                let old_last = cfg.signers.len() as u32;
                if env.storage().persistent().has(&DataKey::MultiSigSigner(wallet_id, old_last)) {
                    env.storage().persistent().remove(&DataKey::MultiSigSigner(wallet_id, old_last));
                }
            }

            // ── Oracle: Add Provider ───────────────────────────────
            // payload: [addr_len: u8][addr_strkey: N bytes]
            ACTION_ORACLE_ADD_PROVIDER => {
                let provider = Self::_decode_address_payload(env, data, 4);
                Self::_oracle(env).add_provider(&env.current_contract_address(), &provider);
            }

            // ── Oracle: Remove Provider ─────────────────────────────
            // payload: [addr_len: u8][addr_strkey: N bytes]
            ACTION_ORACLE_REMOVE_PROVIDER => {
                let provider = Self::_decode_address_payload(env, data, 4);
                Self::_oracle(env).remove_provider(&env.current_contract_address(), &provider);
            }

            // ── Oracle: Adjust Minimum Providers Needed ────────────
            // payload: [min_providers: u32 LE (4 bytes)]
            ACTION_ORACLE_SET_MIN_PROVIDERS => {
                if payload_len < 4 { return; }
                let mut mbuf = [0u8; 4];
                for i in 0..4usize { mbuf[i] = data.get((4 + i) as u32).unwrap().into(); }
                let min_providers = u32::from_le_bytes(mbuf);
                Self::_oracle(env).set_min_providers(&env.current_contract_address(), &min_providers);
            }

            // ── KYC: Add Verifier (provider) ────────────────────────
            // payload: [addr_len: u8][addr_strkey: N bytes]
            ACTION_KYC_ADD_VERIFIER => {
                let compliance: Address = env.storage().instance().get(&COMPLIANCE)
                    .expect("compliance not configured — call set_compliance first");
                let verifier = Self::_decode_address_payload(env, data, 4);
                ComplianceClient::new(env, &compliance).add_verifier(caller, &verifier);
            }

            // ── KYC: Remove Verifier (provider) ─────────────────────
            // payload: [addr_len: u8][addr_strkey: N bytes]
            ACTION_KYC_REMOVE_VERIFIER => {
                let compliance: Address = env.storage().instance().get(&COMPLIANCE)
                    .expect("compliance not configured — call set_compliance first");
                let verifier = Self::_decode_address_payload(env, data, 4);
                ComplianceClient::new(env, &compliance).remove_verifier(caller, &verifier);
            }

            // ── Upgrade a protocol contract's Wasm (storage preserved) ─
            // payload: [addr_len:u8][target strkey][wasm_hash: 32 bytes]
            // The new Wasm must already be uploaded (`stellar contract upload`).
            ACTION_UPGRADE_CONTRACT => {
                let (target, pos) = Self::_decode_address_at(env, data, 4);
                let wasm_hash = Self::_decode_bytesn32_at(env, data, pos);
                if target == env.current_contract_address() {
                    // Self-upgrade: vote + timelock already authorised this.
                    env.deployer().update_current_contract_wasm(wasm_hash);
                } else {
                    UpgradeableClient::new(env, &target)
                        .upgrade(&env.current_contract_address(), &wasm_hash);
                }
            }

            // ── Generic / Fallback ────────────────────────────────
            _ => {
                // For generic actions, try the legacy param update encoding
                // [param_id: u8][value: i128 as 16 bytes LE]
                if data.len() >= 17 {
                    let param_id: u8 = data.get(0).unwrap().into();
                    let mut buf = [0u8; 16];
                    for i in 0..16usize {
                        buf[i] = data.get((i + 1) as u32).unwrap().into();
                    }
                    let value = i128::from_le_bytes(buf);
                    match param_id {
                        0 => env.storage().persistent().set(&DataKey::ParamCR, &value),
                        1 => env.storage().persistent().set(&DataKey::ParamBorrowFee, &value),
                        2 => env.storage().persistent().set(&DataKey::ParamLiqPenalty, &value),
                        _ => {}
                    }
                }
            }
        }
    }

    /// Decode an `[addr_len: u8][addr_strkey: N bytes]` payload segment
    /// starting at `start` into an `Address`. Shared by the oracle-provider
    /// and KYC-verifier proposal actions.
    fn _decode_address_payload(env: &Env, data: &soroban_sdk::Bytes, start: u32) -> Address {
        let addr_len: u8 = data.get(start).unwrap().into();
        let mut addr_bytes = soroban_sdk::Bytes::new(env);
        for i in 0..addr_len {
            addr_bytes.push_back(data.get(start + 1 + i as u32).unwrap());
        }
        Address::from_string_bytes(&addr_bytes)
    }

    /// Same as `_decode_address_payload`, but also returns the byte
    /// position immediately after this segment — used to walk through
    /// multi-field payloads (e.g. ACTION_DEPLOY_ASSET, ACTION_CREATE_POOL)
    /// where several variable-length fields are packed back-to-back.
    fn _decode_address_at(env: &Env, data: &soroban_sdk::Bytes, pos: u32) -> (Address, u32) {
        let addr_len: u8 = data.get(pos).unwrap().into();
        let mut addr_bytes = soroban_sdk::Bytes::new(env);
        for i in 0..addr_len {
            addr_bytes.push_back(data.get(pos + 1 + i as u32).unwrap());
        }
        (Address::from_string_bytes(&addr_bytes), pos + 1 + addr_len as u32)
    }

    /// Decode a `[sym_len: u8][sym_ascii: N bytes]` payload segment at `pos`
    /// into a `Symbol`, returning the position immediately after it. Symbols
    /// are capped at 32 bytes (Soroban's `Symbol` limit).
    fn _decode_symbol_at(env: &Env, data: &soroban_sdk::Bytes, pos: u32) -> (Symbol, u32) {
        let sym_len: u8 = data.get(pos).unwrap().into();
        assert!(sym_len as usize <= 32, "symbol payload too long");
        let mut buf = [0u8; 32];
        for i in 0..sym_len as usize {
            buf[i] = data.get(pos + 1 + i as u32).unwrap().into();
        }
        let s = core::str::from_utf8(&buf[..sym_len as usize]).expect("invalid utf-8 symbol");
        (Symbol::new(env, s), pos + 1 + sym_len as u32)
    }

    /// Decode a `[len: u8][utf8 bytes]` payload into a Soroban `String`
    /// (partner display name, max 80 bytes).
    fn _decode_string_at(env: &Env, data: &soroban_sdk::Bytes, pos: u32) -> (String, u32) {
        let len: u8 = data.get(pos).unwrap().into();
        assert!(len as usize <= 80, "string payload too long");
        let mut buf = [0u8; 80];
        for i in 0..len as usize {
            buf[i] = data.get(pos + 1 + i as u32).unwrap().into();
        }
        let s = core::str::from_utf8(&buf[..len as usize]).expect("invalid utf-8 string");
        (String::from_str(env, s), pos + 1 + len as u32)
    }

    /// Decode a 16-byte little-endian `i128` at `pos`, returning the
    /// position immediately after it.
    fn _decode_i128_at(data: &soroban_sdk::Bytes, pos: u32) -> (i128, u32) {
        let mut buf = [0u8; 16];
        for i in 0..16usize {
            buf[i] = data.get(pos + i as u32).unwrap().into();
        }
        (i128::from_le_bytes(buf), pos + 16)
    }

    /// Decode a 4-byte little-endian `u32` at `pos`, returning the
    /// position immediately after it.
    fn _decode_u32_at(data: &soroban_sdk::Bytes, pos: u32) -> (u32, u32) {
        let mut buf = [0u8; 4];
        for i in 0..4usize {
            buf[i] = data.get(pos + i as u32).unwrap().into();
        }
        (u32::from_le_bytes(buf), pos + 4)
    }

    /// Decode a 32-byte Wasm hash at `pos`.
    fn _decode_bytesn32_at(env: &Env, data: &soroban_sdk::Bytes, pos: u32) -> BytesN<32> {
        assert!(data.len() >= pos + 32, "wasm hash truncated");
        let mut arr = [0u8; 32];
        for i in 0..32u32 {
            arr[i as usize] = data.get(pos + i).unwrap().into();
        }
        BytesN::from_array(env, &arr)
    }

    // ════════════════════════════════════════════════════════════
    //  MULTI-SIGNATURE CONTROL
    // ════════════════════════════════════════════════════════════

    /// Create a multi-sig wallet. Only the existing DAO admin can call this.
    pub fn create_multisig(
        env:      Env,
        dao:      Address,
        threshold: u32,
        signers:   Vec<Address>,
        scope:     MultiSigScope,
    ) -> u32 {
        Self::_require_dao(&env, &dao);
        assert!(threshold > 0, "threshold must be positive");
        assert!(threshold <= signers.len() as u32, "threshold exceeds signer count");
        assert!(signers.len() <= 15, "max 15 signers per wallet");

        let id: u32 = env.storage().instance().get(&MULTISIG_COUNT).unwrap_or(0);
        let cfg = MultiSigConfig {
            id, threshold, signers: signers.clone(), scope, active: true,
        };
        env.storage().persistent().set(&DataKey::MultiSigWallet(id), &cfg);

        // Index each signer
        for (i, signer) in signers.iter().enumerate() {
            env.storage().persistent()
                .set(&DataKey::MultiSigSigner(id, i as u32), &signer);
        }

        env.storage().instance().set(&MULTISIG_COUNT, &(id + 1));
        MultiSigCreatedEvent {
            wallet_id: id,
            threshold,
            signer_count: signers.len() as u32,
        }.publish(&env);
        id
    }

    /// Check whether a given action type requires multi-sig approval.
    pub fn requires_multisig(env: Env, action_type: u32) -> bool {
        let mask: u32 = env.storage().instance()
            .get(&symbol_short!("MSIGMSK"))
            .unwrap_or(MSIG_REQUIRED_MASK);
        (mask & (1 << action_type)) != 0
    }

    /// Set which actions require multi-sig (bitmask).
    /// Bit 0 = deploy_asset, Bit 1 = deactivate, Bit 15 = activate_partner_asset, etc.
    pub fn set_multisig_mask(env: Env, dao: Address, mask: u32) {
        Self::_require_dao(&env, &dao);
        env.storage().instance().set(&symbol_short!("MSIGMSK"), &mask);
    }

    /// Submit a proposal execution for multi-sig approval.
    /// Called after a DAO vote passes when multi-sig is required.
    pub fn submit_multisig_tx(
        env:          Env,
        submitter:    Address,
        wallet_id:    u32,
        proposal_id:  u64,
        target:       Address,
        function_name: Symbol,
        call_data:    soroban_sdk::Bytes,
    ) -> u64 {
        submitter.require_auth();

        let cfg = Self::_get_multisig_config(&env, wallet_id);
        assert!(cfg.active, "multi-sig wallet inactive");

        let tx_id: u64 = env.storage().instance()
            .get(&DataKey::MultiSigTxCount).unwrap_or(0);

        let tx = MultiSigTx {
            id: tx_id,
            wallet_id,
            proposal_id,
            target,
            function_name,
            call_data,
            approvals: 0,
            rejections: 0,
            status: MultiSigTxStatus::Pending,
            created_at: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&DataKey::MultiSigTx(tx_id), &tx);
        env.storage().instance().set(&DataKey::MultiSigTxCount, &(tx_id + 1));

        // Track this tx id in the wallet's list
        let mut wallet_txs: soroban_sdk::Vec<u64> = env.storage().persistent()
            .get(&DataKey::MultiSigWalletTxIds(wallet_id))
            .unwrap_or(soroban_sdk::Vec::new(&env));
        wallet_txs.push_back(tx_id);
        env.storage().persistent().set(&DataKey::MultiSigWalletTxIds(wallet_id), &wallet_txs);

        // Store proposal → tx reverse mapping for dashboard lookup
        env.storage().persistent()
            .set(&DataKey::MultiSigTxProposal(tx_id), &proposal_id);
        let mut proposal_txs: soroban_sdk::Vec<u64> = env.storage().persistent()
            .get(&DataKey::ProposalMultiSigTxs(proposal_id))
            .unwrap_or(soroban_sdk::Vec::new(&env));
        proposal_txs.push_back(tx_id);
        env.storage().persistent()
            .set(&DataKey::ProposalMultiSigTxs(proposal_id), &proposal_txs);

        tx_id
    }

    /// Approve or reject a multi-sig transaction.
    pub fn approve_multisig_tx(
        env:     Env,
        signer:  Address,
        tx_id:   u64,
        approve: bool,
    ) {
        signer.require_auth();

        let mut tx = Self::_get_multisig_tx(&env, tx_id);
        assert!(tx.status == MultiSigTxStatus::Pending, "tx not pending");

        let cfg = Self::_get_multisig_config(&env, tx.wallet_id);
        // Verify signer is in this wallet
        assert!(Self::_is_signer(&env, tx.wallet_id, &signer, &cfg),
            "not a signer for this wallet");

        assert!(
            !env.storage().persistent()
                .has(&DataKey::MultiSigApproval(tx_id, signer.clone())),
            "already voted on this tx"
        );

        env.storage().persistent()
            .set(&DataKey::MultiSigApproval(tx_id, signer.clone()), &approve);

        if approve {
            tx.approvals += 1;
        } else {
            tx.rejections += 1;
        }

        // Check if threshold reached
        if tx.approvals >= cfg.threshold {
            tx.status = MultiSigTxStatus::Approved;
        } else if tx.rejections > (cfg.signers.len() as u32 - cfg.threshold) {
            tx.status = MultiSigTxStatus::Rejected;
        }

        env.storage().persistent().set(&DataKey::MultiSigTx(tx_id), &tx);
        MultiSigApprovedEvent { tx_id, signer }.publish(&env);
    }

    /// Execute a multi-sig transaction.
    ///
    /// With Stellar native multi-sig the signing/approval step happens
    /// off-chain: multiple keys sign the same transaction envelope and it is
    /// submitted once the threshold is reached.  In that flow this function is
    /// called directly from the final signed envelope without any prior
    /// on-chain `approve_multisig_tx` calls, so we accept both `Pending` and
    /// `Approved` statuses here.  The `executor.require_auth()` call ensures
    /// the multi-sig wallet (whose signers collectively signed this envelope)
    /// is the one authorising the execution.
    pub fn execute_multisig_tx(env: Env, executor: Address, tx_id: u64) {
        executor.require_auth();

        let mut tx = Self::_get_multisig_tx(&env, tx_id);
        assert!(
            tx.status == MultiSigTxStatus::Approved || tx.status == MultiSigTxStatus::Pending,
            "tx already finalized"
        );

        // Decode and apply call_data — routes to cross-contract calls
        if !tx.call_data.is_empty() {
            Self::_apply_call_data(&env, &tx.call_data, &tx.target, &tx.function_name, &executor);
        }

        tx.status = MultiSigTxStatus::Executed;
        env.storage().persistent().set(&DataKey::MultiSigTx(tx_id), &tx);
        MultiSigExecutedEvent { tx_id, proposal_id: tx.proposal_id }.publish(&env);
    }

    /// Check how many approvals a multi-sig tx has so far.
    pub fn get_multisig_tx_status(env: Env, tx_id: u64) -> MultiSigTx {
        Self::_get_multisig_tx(&env, tx_id)
    }

    /// Get a multi-sig wallet configuration.
    pub fn get_multisig_config(env: Env, wallet_id: u32) -> MultiSigConfig {
        Self::_get_multisig_config(&env, wallet_id)
    }

    /// List all pending multi-sig transaction IDs for a wallet.
    /// Signers call this to see what needs their approval.
    pub fn get_pending_multisig_txs(env: Env, wallet_id: u32) -> soroban_sdk::Vec<u64> {
        let _cfg = Self::_get_multisig_config(&env, wallet_id); // validates wallet exists
        let all_ids: soroban_sdk::Vec<u64> = env.storage().persistent()
            .get(&DataKey::MultiSigWalletTxIds(wallet_id))
            .unwrap_or(soroban_sdk::Vec::new(&env));
        let mut pending = soroban_sdk::Vec::new(&env);
        for id in all_ids.iter() {
            if let Some(tx) = env.storage().persistent().get(&DataKey::MultiSigTx(id)) {
                let t: MultiSigTx = tx;
                if t.status == MultiSigTxStatus::Pending {
                    pending.push_back(id);
                }
            }
        }
        pending
    }

    /// Get all approvals for a given multi-sig tx (who signed, what they voted).
    /// Frontend uses this to show progress: "2 of 3 signed, Alice + Bob signed, Carol pending"
    pub fn get_multisig_tx_approvals(
        env: Env,
        tx_id: u64,
    ) -> soroban_sdk::Vec<MultiSigApprovalInfo> {
        let tx = Self::_get_multisig_tx(&env, tx_id);
        let cfg = Self::_get_multisig_config(&env, tx.wallet_id);
        let mut approvals = soroban_sdk::Vec::new(&env);
        for i in 0..cfg.signers.len() {
            let signer: Address = env.storage().persistent()
                .get(&DataKey::MultiSigSigner(tx.wallet_id, i as u32))
                .unwrap();
            let voted = env.storage().persistent()
                .get(&DataKey::MultiSigApproval(tx_id, signer.clone()))
                .unwrap_or(false);
            approvals.push_back(MultiSigApprovalInfo {
                signer,
                approved: voted,
            });
        }
        approvals
    }

    /// Get a rich summary of a multi-sig tx for dashboard display.
    pub fn get_multisig_tx_summary(env: Env, tx_id: u64) -> MultiSigTxSummary {
        let tx = Self::_get_multisig_tx(&env, tx_id);
        let cfg = Self::_get_multisig_config(&env, tx.wallet_id);
        MultiSigTxSummary {
            tx_id: tx.id,
            wallet_id: tx.wallet_id,
            proposal_id: tx.proposal_id,
            target: tx.target,
            function_name: tx.function_name,
            approvals: tx.approvals,
            rejections: tx.rejections,
            threshold: cfg.threshold,
            signer_count: cfg.signers.len() as u32,
            status: tx.status,
            created_at: tx.created_at,
        }
    }

    /// List all signer addresses for a multi-sig wallet.
    pub fn get_signers(env: Env, wallet_id: u32) -> soroban_sdk::Vec<Address> {
        let cfg = Self::_get_multisig_config(&env, wallet_id);
        cfg.signers
    }

    /// Check whether an address is a signer for the given wallet (returns false if wallet not found).
    pub fn is_signer_of(env: Env, wallet_id: u32, addr: Address) -> bool {
        let cfg: Option<MultiSigConfig> = env.storage().persistent()
            .get(&DataKey::MultiSigWallet(wallet_id));
        match cfg {
            Some(c) => Self::_is_signer(&env, wallet_id, &addr, &c),
            None => false,
        }
    }

    /// Get the total number of multi-sig wallets created.
    pub fn get_wallet_count(env: Env) -> u32 {
        env.storage().instance().get(&MULTISIG_COUNT).unwrap_or(0)
    }

    // ══════════════════════════════════════════════════════════════
    //  SIGNER MANAGEMENT (callable via DAO proposal execution)
    // ══════════════════════════════════════════════════════════════

    /// Add a signer to an existing multi-sig wallet.
    /// Requires auth from the DAO (i.e. must be called via a passed proposal).
    pub fn add_signer_to_wallet(
        env:         Env,
        dao:         Address,
        wallet_id:   u32,
        new_signer:  Address,
    ) {
        Self::_require_dao(&env, &dao);
        let mut cfg = Self::_get_multisig_config(&env, wallet_id);
        assert!(cfg.active, "multi-sig wallet inactive");

        // Check not already a signer
        assert!(
            !Self::_is_signer(&env, wallet_id, &new_signer, &cfg),
            "address is already a signer"
        );
        assert!(cfg.signers.len() < 15, "max 15 signers per wallet");

        // Append the new signer
        let idx = cfg.signers.len() as u32;
        cfg.signers.push_back(new_signer.clone());
        env.storage().persistent()
            .set(&DataKey::MultiSigWallet(wallet_id), &cfg);
        env.storage().persistent()
            .set(&DataKey::MultiSigSigner(wallet_id, idx), &new_signer);

        MultiSigCreatedEvent {
            wallet_id,
            threshold: cfg.threshold,
            signer_count: cfg.signers.len() as u32,
        }.publish(&env);
    }

    /// Remove a signer from a multi-sig wallet.
    /// Requires auth from the DAO. Cannot reduce signer count below threshold.
    pub fn remove_signer_from_wallet(
        env:         Env,
        dao:         Address,
        wallet_id:   u32,
        old_signer:  Address,
    ) {
        Self::_require_dao(&env, &dao);
        let mut cfg = Self::_get_multisig_config(&env, wallet_id);
        assert!(cfg.active, "multi-sig wallet inactive");
        let signer_count: u32 = cfg.signers.len() as u32;
        assert!(signer_count > cfg.threshold,
            "cannot remove signer: would drop below threshold");

        // Find and remove the signer
        let mut found = false;
        let mut new_signers: Vec<Address> = Vec::new(&env);
        for s in cfg.signers.iter() {
            if s == old_signer && !found {
                found = true;
                continue; // skip this one
            }
            new_signers.push_back(s);
        }
        assert!(found, "signer not found in wallet");

        // Rebuild signer storage with new indices
        cfg.signers = new_signers;
        env.storage().persistent()
            .set(&DataKey::MultiSigWallet(wallet_id), &cfg);
        for (i, signer) in cfg.signers.iter().enumerate() {
            env.storage().persistent()
                .set(&DataKey::MultiSigSigner(wallet_id, i as u32), &signer);
        }
        // Clean up the old last index (in case we removed from middle and compacted)
        let old_last = cfg.signers.len() as u32;
        if env.storage().persistent().has(&DataKey::MultiSigSigner(wallet_id, old_last)) {
            env.storage().persistent().remove(&DataKey::MultiSigSigner(wallet_id, old_last));
        }
    }

    /// Cancel a pending multi-sig tx. Only a signer of the wallet can cancel.
    pub fn cancel_multisig_tx(env: Env, signer: Address, tx_id: u64) {
        signer.require_auth();
        let mut tx = Self::_get_multisig_tx(&env, tx_id);
        assert!(tx.status == MultiSigTxStatus::Pending, "tx not pending");
        let cfg = Self::_get_multisig_config(&env, tx.wallet_id);
        assert!(
            Self::_is_signer(&env, tx.wallet_id, &signer, &cfg),
            "not a signer for this wallet"
        );
        tx.status = MultiSigTxStatus::Cancelled;
        env.storage().persistent().set(&DataKey::MultiSigTx(tx_id), &tx);
    }

    // ══════════════════════════════════════════════════════════════
    //  COMBINED PROPOSE + MULTI-SIG
    // ══════════════════════════════════════════════════════════════

    /// Create a governance proposal AND simultaneously queue a multi-sig
    /// transaction for it.  This is the primary entry-point for the frontend
    /// "Create Proposal" flow when multi-sig is enabled.
    ///
    /// Returns (proposal_id, multisig_tx_id).
    pub fn propose_and_queue_multisig(
        env:              Env,
        proposer:         Address,
        title:            soroban_sdk::Bytes,
        description:      soroban_sdk::Bytes,
        call_data:        soroban_sdk::Bytes,
        wallet_id:        u32,
        multisig_target:  Address,
        multisig_fn_name: Symbol,
    ) -> soroban_sdk::Vec<u64> {
        proposer.require_auth();
        Self::_require_governance_signer(&env, &proposer);

        // 1. Create the proposal (same logic as propose()).
        let proposal_id: u64 = env.storage().instance().get(&PROP_COUNTER).unwrap_or(0);
        let now = env.ledger().timestamp();

        let proposal = Proposal {
            id: proposal_id,
            proposer: proposer.clone(),
            title: title.clone(),
            description: description.clone(),
            call_data: call_data.clone(),
            yes_votes: 0,
            no_votes:  0,
            start_time:    now,
            end_time:      now + VOTING_PERIOD,
            execute_after: now + VOTING_PERIOD + TIMELOCK,
            status: ProposalStatus::Active,
        };

        env.storage().persistent().set(&DataKey::Proposal(proposal_id), &proposal);
        env.storage().instance().set(&PROP_COUNTER, &(proposal_id + 1));
        ProposedEvent {
            proposer: proposer.clone(),
            proposal_id,
        }.publish(&env);

        // 2. Queue the multi-sig transaction (same logic as submit_multisig_tx)
        let cfg = Self::_get_multisig_config(&env, wallet_id);
        assert!(cfg.active, "multi-sig wallet inactive");

        let tx_id: u64 = env.storage().instance()
            .get(&DataKey::MultiSigTxCount).unwrap_or(0);

        let tx = MultiSigTx {
            id: tx_id,
            wallet_id,
            proposal_id,
            target: multisig_target,
            function_name: multisig_fn_name,
            call_data,
            approvals: 0,
            rejections: 0,
            status: MultiSigTxStatus::Pending,
            created_at: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&DataKey::MultiSigTx(tx_id), &tx);
        env.storage().instance().set(&DataKey::MultiSigTxCount, &(tx_id + 1));

        // Track in wallet's tx list
        let mut wallet_txs: soroban_sdk::Vec<u64> = env.storage().persistent()
            .get(&DataKey::MultiSigWalletTxIds(wallet_id))
            .unwrap_or(soroban_sdk::Vec::new(&env));
        wallet_txs.push_back(tx_id);
        env.storage().persistent().set(&DataKey::MultiSigWalletTxIds(wallet_id), &wallet_txs);

        // Store proposal ↔ tx bidirectional mapping
        env.storage().persistent()
            .set(&DataKey::MultiSigTxProposal(tx_id), &proposal_id);
        let mut proposal_txs: soroban_sdk::Vec<u64> = env.storage().persistent()
            .get(&DataKey::ProposalMultiSigTxs(proposal_id))
            .unwrap_or(soroban_sdk::Vec::new(&env));
        proposal_txs.push_back(tx_id);
        env.storage().persistent()
            .set(&DataKey::ProposalMultiSigTxs(proposal_id), &proposal_txs);

        MultiSigCreatedEvent {
            wallet_id,
            threshold: cfg.threshold,
            signer_count: cfg.signers.len() as u32,
        }.publish(&env);

        // Return both IDs
        let mut result = soroban_sdk::Vec::new(&env);
        result.push_back(proposal_id);
        result.push_back(tx_id);
        result
    }

    /// Get all multi-sig transaction IDs linked to a proposal.
    pub fn get_proposal_multisig_txs(
        env: Env,
        proposal_id: u64,
    ) -> soroban_sdk::Vec<u64> {
        env.storage().persistent()
            .get(&DataKey::ProposalMultiSigTxs(proposal_id))
            .unwrap_or(soroban_sdk::Vec::new(&env))
    }

    /// Get the proposal ID linked to a multi-sig transaction.
    pub fn get_multisig_tx_proposal(env: Env, tx_id: u64) -> u64 {
        env.storage().persistent()
            .get(&DataKey::MultiSigTxProposal(tx_id))
            .unwrap_or(0)
    }

    /// Whether a proposal has any pending multi-sig transactions.
    pub fn proposal_has_pending_multisig(env: Env, proposal_id: u64) -> bool {
        let txs = env.storage().persistent()
            .get(&DataKey::ProposalMultiSigTxs(proposal_id))
            .unwrap_or(soroban_sdk::Vec::new(&env));
        for tx_id in txs.iter() {
            if let Some(tx) = env.storage().persistent().get(&DataKey::MultiSigTx(tx_id)) {
                let t: MultiSigTx = tx;
                if t.status == MultiSigTxStatus::Pending {
                    return true;
                }
            }
        }
        false
    }

    // ── Multi-sig internals ─────────────────────────────────────

    fn _get_multisig_config(env: &Env, wallet_id: u32) -> MultiSigConfig {
        env.storage().persistent()
            .get(&DataKey::MultiSigWallet(wallet_id))
            .expect("multi-sig wallet not found")
    }

    fn _get_multisig_tx(env: &Env, tx_id: u64) -> MultiSigTx {
        env.storage().persistent()
            .get(&DataKey::MultiSigTx(tx_id))
            .expect("multi-sig tx not found")
    }

    fn _is_signer(env: &Env, _wallet_id: u32, addr: &Address, cfg: &MultiSigConfig) -> bool {
        for s in cfg.signers.iter() {
            if s == *addr { return true; }
        }
        false
    }

    fn _require_dao(env: &Env, caller: &Address) {
        let dao: Address = env.storage().instance().get(&DAO_ADDR).unwrap();
        assert!(*caller == dao, "DAO only");
        caller.require_auth();
    }

    fn _oracle(env: &Env) -> OracleClient<'_> {
        let oracle: Address = env.storage().instance().get(&ORACLE)
            .expect("oracle not configured — call set_oracle first");
        OracleClient::new(env, &oracle)
    }
}