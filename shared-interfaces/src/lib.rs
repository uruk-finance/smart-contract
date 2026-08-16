// ═══════════════════════════════════════════════════════════════════
//  shared-interfaces/src/lib.rs
//
//  Custom cross-contract client definitions for every contract in
//  the Stellar Synthetic DAO system.
//
//  Why this file exists:
//  soroban_sdk::token::Client only exposes the SEP-41 *read* interface
//  (balance, allowance, decimals, name, symbol, total_supply) plus
//  transfer/transfer_from/approve/burn/burn_from.
//  It does NOT expose mint, set_admin, set_minter, freeze, etc.
//
//  For custom methods we must declare the interface with
//  soroban_sdk::contractclient! or write a manual client struct.
//  The idiomatic Soroban approach is to import the contract's
//  generated client via the workspace, but for a multi-contract
//  workspace we use contractimport! or a manual interface declaration.
//
//  This crate provides:
//    1. SyntheticTokenClient    – full SEP-41 + mint/burn/freeze
//    2. OracleClient            – get_price / get_twap
//    3. ComplianceClient        – assert_transfer_allowed
//    4. StabilityPoolClient     – absorb_liquidation / get_compounded_deposit / get_total_deposits
//    5. EarningsClient          – receive_revenue
//    6. PartnerRegistryClient   – credit_partner_revenue / get_partner / deactivate_partner / set_partner_share / dao_register_partner
//    7. LiquidityPoolClient     – create_pool
//    8. SyntheticEngineClient   – register_asset / set_asset_enabled / set_debt_ceiling / set_collateral_allowed
// ═══════════════════════════════════════════════════════════════════
#![no_std]
use soroban_sdk::{contractclient, Address, BytesN, Env, Symbol, String, Vec};

// ─────────────────────────────────────────────────────────────────
//  Full SEP-41 + admin interface for the sep41-token contract
// ─────────────────────────────────────────────────────────────────
#[contractclient(name = "SyntheticTokenClient")]
pub trait SyntheticTokenInterface {
    // SEP-41 core
    fn transfer(env: Env, from: Address, to: Address, amount: i128);
    fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128);
    fn approve(env: Env, from: Address, spender: Address, amount: i128, expiration_ledger: u32);
    fn balance(env: Env, id: Address) -> i128;
    fn allowance(env: Env, from: Address, spender: Address) -> i128;
    fn total_supply(env: Env) -> i128;
    fn decimals(env: Env) -> u32;
    fn name(env: Env) -> String;
    fn symbol(env: Env) -> String;
    // Admin / minter
    fn mint(env: Env, to: Address, amount: i128);
    fn burn(env: Env, from: Address, amount: i128);
    fn burn_from(env: Env, spender: Address, from: Address, amount: i128);
    fn set_frozen(env: Env, admin: Address, account: Address, frozen: bool);
    fn clawback(env: Env, admin: Address, from: Address, amount: i128);
    fn set_paused(env: Env, admin: Address, paused: bool);
    fn set_admin(env: Env, current: Address, new_admin: Address);
    fn set_minter(env: Env, admin: Address, minter: Address);
    fn is_frozen(env: Env, account: Address) -> bool;
    fn is_paused(env: Env) -> bool;
}

// ─────────────────────────────────────────────────────────────────
//  Oracle interface
// ─────────────────────────────────────────────────────────────────
#[contractclient(name = "OracleClient")]
pub trait OracleInterface {
    fn get_price(env: Env, asset: Symbol) -> i128;
    fn get_twap(env: Env, asset: Symbol) -> i128;
    fn is_stale(env: Env, asset: Symbol) -> bool;
    fn submit_price(env: Env, provider: Address, asset: Symbol, price: i128);
    fn submit_prices(env: Env, provider: Address, assets: Vec<Symbol>, prices: Vec<i128>);
    fn update_twap(env: Env, asset: Symbol) -> i128;
    fn is_provider(env: Env, addr: Address) -> bool;
    fn provider_count(env: Env) -> u32;
    // DAO-governed provider management
    fn add_provider(env: Env, dao: Address, provider: Address);
    fn remove_provider(env: Env, dao: Address, provider: Address);
    fn set_min_providers(env: Env, dao: Address, min_providers: u32);
    fn get_min_providers(env: Env) -> u32;
}

// ─────────────────────────────────────────────────────────────────
//  Compliance registry interface
// ─────────────────────────────────────────────────────────────────
#[contractclient(name = "ComplianceClient")]
pub trait ComplianceInterface {
    fn assert_transfer_allowed(
        env:    Env,
        caller: Address,
        from:   Address,
        to:     Address,
        asset:  Symbol,
        amount: i128,
    ) -> bool;
    fn is_transfer_allowed(
        env:    Env,
        from:   Address,
        to:     Address,
        asset:  Symbol,
        amount: i128,
    ) -> bool;
    /// Generic usage gate: every user-facing contract (CDP engine, liquidity
    /// pool, stability pool, ...) must call this before letting `user`
    /// deposit, borrow, swap, or withdraw. Panics if not compliant
    /// (no/expired KYC, frozen, suspended, sanctioned, or jurisdiction is
    /// blocked).
    fn assert_user_compliant(env: Env, caller: Address, user: Address) -> bool;
    /// Read-only check — returns false instead of panicking.
    fn is_user_compliant(env: Env, user: Address) -> bool;
    /// Simple yes/no: is this address Verified *right now*?
    /// Shorthand for status==Verified && not expired.
    fn is_verified(env: Env, user: Address) -> bool;
    fn is_frozen(env: Env, addr: Address) -> bool;
    /// Is `user`'s on-file jurisdiction currently allowed? Never reveals
    /// which jurisdiction it actually is.
    fn is_jurisdiction_ok(env: Env, user: Address) -> bool;
    // DAO multi-sig controlled KYC provider management — multiple
    // providers (Sumsub, Persona, Veriff, ...) can be authorised at once;
    // users choose which one to complete KYC with.
    fn set_verifier(env: Env, dao: Address, verifier: Address);
    fn add_verifier(env: Env, dao: Address, verifier: Address);
    fn remove_verifier(env: Env, dao: Address, verifier: Address);
    fn get_verifier(env: Env) -> Option<Address>;
    fn list_verifiers(env: Env) -> soroban_sdk::Vec<Address>;
    // Admin: status management
    fn suspend_account(env: Env, admin: Address, account: Address, reason: Symbol);
    fn unsuspend_account(env: Env, admin: Address, account: Address);
    fn sanction_account(env: Env, admin: Address, account: Address, reason: Symbol);
    // Signed attestation flow
    fn register_signer(env: Env, provider: Address, signer: Address);
    fn submit_attestation(
        env: Env, subject: Address, signer: Address,
        jurisdiction: u32, expires_at: u64,
    );
}

// ─────────────────────────────────────────────────────────────────
//  Stability pool interface
// ─────────────────────────────────────────────────────────────────
#[contractclient(name = "StabilityPoolClient")]
pub trait StabilityPoolInterface {
    fn absorb_liquidation(
        env:         Env,
        engine:      Address,
        debt_to_abs: i128,
        coll_reward: i128,
        coll_token:  Address,
    );
    /// A depositor's current USDC-equivalent balance (principal +/-
    /// compounding from absorbed liquidations). Used by EarningsDistributor
    /// to compute each claimer's pro-rata share of the "staker" bucket.
    fn get_compounded_deposit(env: Env, depositor: Address) -> i128;
    /// Total USDC currently deposited in the pool — the snapshot
    /// denominator EarningsDistributor uses at epoch finalisation.
    fn get_total_deposits(env: Env) -> i128;
}

// ─────────────────────────────────────────────────────────────────
//  Earnings distributor interface
// ─────────────────────────────────────────────────────────────────
#[contractclient(name = "EarningsClient")]
pub trait EarningsInterface {
    /// `partner_id` = 0 → full fee stays with protocol (B2C / untagged).
    /// `partner_id` > 0 → partner share is cut first, remainder to protocol.
    fn receive_revenue(
        env:        Env,
        sender:     Address,
        token:      Address,
        amount:     i128,
        source:     Symbol,
        partner_id: u32,
    );
}

// ─────────────────────────────────────────────────────────────────
//  Partner registry interface
// ─────────────────────────────────────────────────────────────────
#[contractclient(name = "PartnerRegistryClient")]
pub trait PartnerRegistryInterface {
    fn credit_partner_revenue(
        env:        Env,
        caller:     Address,
        partner_id: u32,
        amount:     i128,
        epoch:      u64,
    );
    /// Revenue share in bps for an active partner (falls back handled by caller).
    fn get_revenue_share(env: Env, partner_id: u32) -> i128;
    fn get_partner_owner(env: Env, partner_id: u32) -> Option<Address>;
    // DAO-governed partner management — cross-called from
    // dao-governance::_apply_call_data so the DAO can actually control
    // the partner-registry contract.
    fn deactivate_partner(env: Env, dao: Address, partner_id: u32);
    fn set_partner_share(env: Env, dao: Address, partner_id: u32, new_share: i128);
    fn dao_register_partner(
        env: Env,
        dao: Address,
        owner: Address,
        name: String,
    ) -> u32;
    fn activate_partner_asset(env: Env, dao: Address, symbol: Symbol);
}

// ─────────────────────────────────────────────────────────────────
//  Liquidity pool interface
// ─────────────────────────────────────────────────────────────────
#[contractclient(name = "LiquidityPoolClient")]
pub trait LiquidityPoolInterface {
    fn create_pool(
        env:      Env,
        creator:  Address,
        pool_id:  Symbol,
        token_a:  Address,
        token_b:  Address,
        fee_tier: i128,
    ) -> Symbol;
}

// ─────────────────────────────────────────────────────────────────
//  Synthetic Engine interface
// ─────────────────────────────────────────────────────────────────
#[contractclient(name = "SyntheticEngineClient")]
pub trait SyntheticEngineInterface {
    fn register_asset(
        env: Env, dao: Address, symbol: Symbol,
        token: Address, oracle_sym: Symbol, coll_oracle: Symbol,
        min_cr: i128, liq_cr: i128, liq_penalty: i128,
        stab_fee_bps: i128, debt_ceiling: i128,
    );
    fn set_asset_enabled(env: Env, dao: Address, symbol: Symbol, enabled: bool);
    fn set_debt_ceiling(env: Env, dao: Address, symbol: Symbol, ceiling: i128);
    fn set_collateral_allowed(env: Env, dao: Address, token: Address, allowed: bool);
    fn set_paused(env: Env, admin: Address, paused: bool);
    fn get_asset_config(env: Env, symbol: Symbol) -> Option<Address>; // returns token address
}

// ─────────────────────────────────────────────────────────────────
//  In-place Wasm upgrade (storage is preserved)
// ─────────────────────────────────────────────────────────────────
#[contractclient(name = "UpgradeableClient")]
pub trait UpgradeableInterface {
    fn upgrade(env: Env, dao: Address, new_wasm_hash: BytesN<32>);
}