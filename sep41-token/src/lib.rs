// ═══════════════════════════════════════════════════════════════════
//  CONTRACT 1: URUK Synthetic Asset Token
// ═══════════════════════════════════════════════════════════════════
#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, contractevent, contractmeta,
    symbol_short, Address, Env, String, Symbol,
};

// SEP-41 required metadata
contractmeta!(
    key   = "Description",
    val   = "URUK Synthetic Asset — SEP-41 Compliant"
);

// ── Storage keys ─────────────────────────────────────────────────
const ADMIN:      Symbol = symbol_short!("ADMIN");
const MINTER:     Symbol = symbol_short!("MINTER");    // synthetic engine
const PAUSED:     Symbol = symbol_short!("PAUSED");
const DECIMALS:   Symbol = symbol_short!("DECIMALS");
const NAME:       Symbol = symbol_short!("NAME");
const SYMBOL_KEY: Symbol = symbol_short!("SYMBOL");

// ── Data keys ────────────────────────────────────────────────────
#[contracttype]
pub enum DataKey {
    Balance(Address),
    Allowance(Address, Address), // (owner, spender)
    TotalSupply,
    FrozenAccount(Address),      // compliance: freeze individual accounts
}

// ─────────────────────────────────────────────────────────────────
//  Events (Protocol 23: SEP-41 mandated events using #[contractevent])
// ─────────────────────────────────────────────────────────────────

#[contractevent(topics = ["transfer"])]
pub struct TransferEvent {
    #[topic]
    pub from: Address,
    #[topic]
    pub to: Address,
    pub amount: i128,
}

#[contractevent(topics = ["approve"])]
pub struct ApproveEvent {
    #[topic]
    pub from: Address,
    #[topic]
    pub spender: Address,
    pub amount: i128,
    pub expiration_ledger: u32,
}

#[contractevent(topics = ["mint"])]
pub struct MintEvent {
    #[topic]
    pub to: Address,
    pub amount: i128,
}

#[contractevent(topics = ["burn"])]
pub struct BurnEvent {
    #[topic]
    pub from: Address,
    pub amount: i128,
}

#[contractevent(topics = ["freeze"])]
pub struct FreezeEvent {
    #[topic]
    pub account: Address,
    pub frozen: bool,
}

#[contractevent(topics = ["clawback"])]
pub struct ClawbackEvent {
    #[topic]
    pub from: Address,
    #[topic]
    pub to: Address,
    pub amount: i128,
}

#[contract]
pub struct SyntheticToken;

#[contractimpl]
impl SyntheticToken {
    // ── Initialise (deploy once per synthetic asset) ──────────────
    pub fn initialize(
        env:      Env,
        admin:    Address,
        minter:   Address,   // address of SyntheticEngine contract
        name:     String,    // e.g. "Stellar Bitcoin"
        symbol:   String,    // e.g. "sBTC"
        decimals: u32,       // 7 (Stellar standard)
    ) {
        assert!(!env.storage().instance().has(&ADMIN), "already initialised");
        env.storage().instance().set(&ADMIN,      &admin);
        env.storage().instance().set(&MINTER,     &minter);
        env.storage().instance().set(&NAME,       &name);
        env.storage().instance().set(&SYMBOL_KEY, &symbol);
        env.storage().instance().set(&DECIMALS,   &decimals);
        env.storage().instance().set(&PAUSED,     &false);
        env.storage().persistent().set(&DataKey::TotalSupply, &0_i128);
    }

    // ════════════════════════════════════════════════════════════
    //  SEP-41 CORE INTERFACE
    // ════════════════════════════════════════════════════════════

    /// Transfer tokens between accounts
    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();
        Self::_assert_active(&env);
        Self::_assert_not_frozen(&env, &from);
        Self::_assert_not_frozen(&env, &to);
        assert!(amount > 0, "amount must be positive");

        Self::_spend_balance(&env, &from, amount);
        Self::_receive_balance(&env, &to, amount);
        TransferEvent { from, to, amount }.publish(&env);
    }

    /// Transfer using an approved allowance
    pub fn transfer_from(
        env:    Env,
        spender: Address,
        from:   Address,
        to:     Address,
        amount: i128,
    ) {
        spender.require_auth();
        Self::_assert_active(&env);
        Self::_assert_not_frozen(&env, &from);
        Self::_assert_not_frozen(&env, &to);
        assert!(amount > 0, "amount must be positive");

        let key = DataKey::Allowance(from.clone(), spender.clone());
        let allowance: i128 = env.storage().temporary().get(&key).unwrap_or(0);
        assert!(allowance >= amount, "allowance exceeded");
        env.storage().temporary().set(&key, &(allowance - amount));

        Self::_spend_balance(&env, &from, amount);
        Self::_receive_balance(&env, &to, amount);
        TransferEvent { from, to, amount }.publish(&env);
    }

    /// Approve a spender on behalf of owner
    pub fn approve(
        env:             Env,
        from:            Address,
        spender:         Address,
        amount:          i128,
        expiration_ledger: u32,
    ) {
        from.require_auth();
        assert!(amount >= 0, "amount must be non-negative");
        assert!(
            expiration_ledger >= env.ledger().sequence(),
            "expiration must be in future"
        );
        env.storage().temporary().set(
            &DataKey::Allowance(from.clone(), spender.clone()),
            &amount,
        );
        env.storage().temporary()
            .extend_ttl(&DataKey::Allowance(from.clone(), spender.clone()), 0, expiration_ledger);
        ApproveEvent { from, spender, amount, expiration_ledger }.publish(&env);
    }

    // ════════════════════════════════════════════════════════════
    //  MINT / BURN — called by SyntheticEngine only
    // ════════════════════════════════════════════════════════════

    pub fn mint(env: Env, to: Address, amount: i128) {
        let minter: Address = env.storage().instance().get(&MINTER).unwrap();
        minter.require_auth();
        Self::_assert_active(&env);
        Self::_assert_not_frozen(&env, &to);
        assert!(amount > 0, "amount must be positive");

        Self::_receive_balance(&env, &to, amount);
        let supply: i128 = env.storage().persistent()
            .get(&DataKey::TotalSupply).unwrap_or(0);
        env.storage().persistent().set(&DataKey::TotalSupply, &(supply + amount));
        MintEvent { to, amount }.publish(&env);
    }

    pub fn burn(env: Env, from: Address, amount: i128) {
        from.require_auth();
        Self::_assert_active(&env);
        assert!(amount > 0, "amount must be positive");

        Self::_spend_balance(&env, &from, amount);
        let supply: i128 = env.storage().persistent()
            .get(&DataKey::TotalSupply).unwrap_or(0);
        env.storage().persistent().set(&DataKey::TotalSupply, &(supply - amount));
        BurnEvent { from, amount }.publish(&env);
    }

    pub fn burn_from(env: Env, spender: Address, from: Address, amount: i128) {
        spender.require_auth();
        let key = DataKey::Allowance(from.clone(), spender.clone());
        let allowance: i128 = env.storage().temporary().get(&key).unwrap_or(0);
        assert!(allowance >= amount, "allowance exceeded");
        env.storage().temporary().set(&key, &(allowance - amount));

        Self::_spend_balance(&env, &from, amount);
        let supply: i128 = env.storage().persistent()
            .get(&DataKey::TotalSupply).unwrap_or(0);
        env.storage().persistent().set(&DataKey::TotalSupply, &(supply - amount));
        BurnEvent { from, amount }.publish(&env);
    }

    // ════════════════════════════════════════════════════════════
    //  COMPLIANCE — admin only
    // ════════════════════════════════════════════════════════════

    /// Freeze / unfreeze an account (regulatory compliance)
    pub fn set_frozen(env: Env, admin: Address, account: Address, frozen: bool) {
        Self::_require_admin(&env, &admin);
        env.storage().persistent()
            .set(&DataKey::FrozenAccount(account.clone()), &frozen);
        FreezeEvent { account, frozen }.publish(&env);
    }

    /// Regulatory clawback — admin can recover from any frozen account
    pub fn clawback(env: Env, admin: Address, from: Address, amount: i128) {
        Self::_require_admin(&env, &admin);
        let frozen: bool = env.storage().persistent()
            .get(&DataKey::FrozenAccount(from.clone())).unwrap_or(false);
        assert!(frozen, "account must be frozen before clawback");

        let admin_addr: Address = env.storage().instance().get(&ADMIN).unwrap();
        Self::_spend_balance(&env, &from, amount);
        Self::_receive_balance(&env, &admin_addr, amount);
        ClawbackEvent { from, to: admin_addr, amount }.publish(&env);
    }

    /// Emergency pause (halts all transfers/mints)
    pub fn set_paused(env: Env, admin: Address, paused: bool) {
        Self::_require_admin(&env, &admin);
        env.storage().instance().set(&PAUSED, &paused);
    }

    /// Transfer admin role
    pub fn set_admin(env: Env, current: Address, new_admin: Address) {
        Self::_require_admin(&env, &current);
        env.storage().instance().set(&ADMIN, &new_admin);
    }

    /// Update minter (e.g. after engine upgrade)
    pub fn set_minter(env: Env, admin: Address, minter: Address) {
        Self::_require_admin(&env, &admin);
        env.storage().instance().set(&MINTER, &minter);
    }

    // ════════════════════════════════════════════════════════════
    //  SEP-41 VIEW INTERFACE (all required)
    // ════════════════════════════════════════════════════════════

    pub fn balance(env: Env, id: Address) -> i128 {
        env.storage().persistent()
            .get(&DataKey::Balance(id)).unwrap_or(0)
    }

    pub fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        env.storage().temporary()
            .get(&DataKey::Allowance(from, spender)).unwrap_or(0)
    }

    pub fn total_supply(env: Env) -> i128 {
        env.storage().persistent()
            .get(&DataKey::TotalSupply).unwrap_or(0)
    }

    pub fn decimals(env: Env) -> u32 {
        env.storage().instance().get(&DECIMALS).unwrap()
    }

    pub fn name(env: Env) -> String {
        env.storage().instance().get(&NAME).unwrap()
    }

    pub fn symbol(env: Env) -> String {
        env.storage().instance().get(&SYMBOL_KEY).unwrap()
    }

    pub fn is_frozen(env: Env, account: Address) -> bool {
        env.storage().persistent()
            .get(&DataKey::FrozenAccount(account)).unwrap_or(false)
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage().instance().get(&PAUSED).unwrap_or(false)
    }

    // ── Internals ─────────────────────────────────────────────────
    fn _spend_balance(env: &Env, from: &Address, amount: i128) {
        let key = DataKey::Balance(from.clone());
        let bal: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        assert!(bal >= amount, "insufficient balance");
        env.storage().persistent().set(&key, &(bal - amount));
    }

    fn _receive_balance(env: &Env, to: &Address, amount: i128) {
        let key = DataKey::Balance(to.clone());
        let bal: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(bal + amount));
    }

    fn _assert_active(env: &Env) {
        let paused: bool = env.storage().instance().get(&PAUSED).unwrap_or(false);
        assert!(!paused, "token is paused");
    }

    fn _assert_not_frozen(env: &Env, account: &Address) {
        let frozen: bool = env.storage().persistent()
            .get(&DataKey::FrozenAccount(account.clone())).unwrap_or(false);
        assert!(!frozen, "account is frozen");
    }

    fn _require_admin(env: &Env, caller: &Address) {
        let admin: Address = env.storage().instance().get(&ADMIN).unwrap();
        assert!(*caller == admin, "admin only");
        caller.require_auth();
    }
}