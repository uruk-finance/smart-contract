// ═══════════════════════════════════════════════════════════════════
//  CONTRACT 3 (FIXED): Synthetic Engine
//
//  Key fix: uses SyntheticTokenClient (from shared-interfaces) for
//  mint/burn calls. soroban_sdk::token::Client only covers the
//  standard SEP-41 read + transfer interface — it has no `mint`.
//
//  Cross-contract call map:
//    mint/burn       → SyntheticTokenClient   (sep41-token contract)
//    price feeds     → OracleClient           (oracle-twap contract)
//    KYC checks      → ComplianceClient       (compliance-registry)
//    fee routing     → EarningsClient         (earnings-distributor)
//    collateral tok  → token::Client          (standard SAC/SEP-41)
// ═══════════════════════════════════════════════════════════════════
#![no_std]
    use soroban_sdk::{
        contract, contractimpl, contracttype, contractevent, symbol_short,
        token, Address, BytesN, Env, Symbol, Vec,
    };
use shared_interfaces::{
    SyntheticTokenClient, OracleClient, ComplianceClient, EarningsClient,
};

// ── Constants ────────────────────────────────────────────────────
const PRECISION:        i128 = 10_000;
const WAD:              i128 = 10_000_000;

// ── Storage key symbols ──────────────────────────────────────────
const ADMIN:      Symbol = symbol_short!("ADMIN");
const ORACLE:     Symbol = symbol_short!("ORACLE");
const COMPLIANCE: Symbol = symbol_short!("COMPL");
const EARNINGS:   Symbol = symbol_short!("EARN");
const DAO:        Symbol = symbol_short!("DAO");
const STABPOOL:   Symbol = symbol_short!("STABPOOL");
const PAUSED:     Symbol = symbol_short!("PAUSED");

// ── Per-asset configuration ───────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug)]
pub struct AssetConfig {
    pub token:           Address,
    pub oracle_symbol:   Symbol,
    pub coll_oracle_sym: Symbol,
    pub min_coll_ratio:  i128,
    pub liq_ratio:       i128,
    pub liq_penalty:     i128,
    /// One-shot mint fee in bps (not an annual rate).
    pub stab_fee_bps:    i128,
    pub debt_ceiling:    i128,
    pub enabled:         bool,
}

// ── CDP Vault ─────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug)]
pub struct Vault {
    pub owner:          Address,
    pub collateral:     i128,
    pub collateral_tok: Address,
    pub synth_asset:    Symbol,
    pub debt:           i128,
    pub opened_at:      u64,
    pub last_update:    u64,
}

// ── Storage keys ──────────────────────────────────────────────────
#[contracttype]
pub enum DataKey {
    AssetConfig(Symbol),
    Vault(Address, Symbol),
    DebtTotal(Symbol),
    CollateralAllowed(Address),
    VaultsByOwner(Address),
}

// ─────────────────────────────────────────────────────────────────
//  Events (Protocol 23: using #[contractevent] macro)
// ─────────────────────────────────────────────────────────────────
#[contractevent(topics = ["open"])]
pub struct OpenEvent {
    #[topic]
    pub owner: Address,
    pub synth_asset: Symbol,
    pub collateral_token: Address,
    pub collateral_amount: i128,
}

#[contractevent(topics = ["deposit"])]
pub struct DepositEvent {
    #[topic]
    pub owner: Address,
    pub synth_asset: Symbol,
    pub amount: i128,
}

#[contractevent(topics = ["mint"])]
pub struct MintEvent {
    #[topic]
    pub owner: Address,
    pub synth_asset: Symbol,
    pub amount: i128,
}

#[contractevent(topics = ["burn"])]
pub struct BurnEvent {
    #[topic]
    pub owner: Address,
    pub synth_asset: Symbol,
    pub amount: i128,
}

#[contractevent(topics = ["liq"])]
pub struct LiquidationEvent {
    #[topic]
    pub owner: Address,
    pub synth_asset: Symbol,
    pub collateral_seized: i128,
}

#[contractevent(topics = ["withdraw"])]
pub struct WithdrawEvent {
    #[topic]
    pub owner: Address,
    pub synth_asset: Symbol,
    pub amount: i128,
}

#[contractevent(topics = ["market_created"])]
pub struct MarketCreatedEvent {
    #[topic]
    pub symbol: Symbol,
    pub token: Address,
    pub oracle_symbol: Symbol,
    pub coll_oracle_symbol: Symbol,
    pub min_coll_ratio: i128,
    pub liq_ratio: i128,
    pub liq_penalty: i128,
    pub stab_fee_bps: i128,
    pub debt_ceiling: i128,
}

#[contract]
pub struct SyntheticEngine;

#[contractimpl]
impl SyntheticEngine {

    // ════════════════════════════════════════════════════════════
    //  INIT
    // ════════════════════════════════════════════════════════════

    pub fn initialize(
        env: Env, admin: Address, oracle: Address,
        compliance: Address, earnings: Address, dao: Address,
    ) {
        admin.require_auth();
        assert!(!env.storage().instance().has(&ADMIN), "already initialised");
        env.storage().instance().set(&ADMIN,      &admin);
        env.storage().instance().set(&ORACLE,     &oracle);
        env.storage().instance().set(&COMPLIANCE, &compliance);
        env.storage().instance().set(&EARNINGS,   &earnings);
        env.storage().instance().set(&DAO,        &dao);
        env.storage().instance().set(&PAUSED,     &false);
    }

    pub fn register_asset(
        env: Env, dao: Address, symbol: Symbol,
        token: Address, oracle_sym: Symbol, coll_oracle: Symbol,
        min_cr: i128, liq_cr: i128, liq_penalty: i128,
        stab_fee_bps: i128, debt_ceiling: i128,
    ) {
        Self::_require_dao(&env, &dao);
        assert!(min_cr > liq_cr, "min_cr must exceed liq_cr");
        assert!(liq_penalty <= 2_000, "penalty capped at 20%");
        env.storage().persistent().set(&DataKey::AssetConfig(symbol.clone()), &AssetConfig {
            token: token.clone(), oracle_symbol: oracle_sym.clone(),
            coll_oracle_sym: coll_oracle.clone(), min_coll_ratio: min_cr,
            liq_ratio: liq_cr, liq_penalty, stab_fee_bps, debt_ceiling,
            enabled: true,
        });

        MarketCreatedEvent {
            symbol: symbol.clone(),
            token,
            oracle_symbol: oracle_sym,
            coll_oracle_symbol: coll_oracle,
            min_coll_ratio: min_cr,
            liq_ratio: liq_cr,
            liq_penalty,
            stab_fee_bps,
            debt_ceiling,
        }.publish(&env);
    }

    pub fn set_collateral_allowed(env: Env, dao: Address, token: Address, allowed: bool) {
        Self::_require_dao(&env, &dao);
        if allowed {
            env.storage().persistent().set(&DataKey::CollateralAllowed(token), &true);
        } else {
            env.storage().persistent().remove(&DataKey::CollateralAllowed(token));
        }
    }

    /// DAO-governed kill switch for a single synthetic market (independent
    /// of the global `set_paused` circuit breaker) — disabled assets block
    /// new vault opens/mints but still allow repay/withdraw/liquidate.
    pub fn set_asset_enabled(env: Env, dao: Address, symbol: Symbol, enabled: bool) {
        Self::_require_dao(&env, &dao);
        let mut cfg = Self::_get_cfg(&env, &symbol);
        cfg.enabled = enabled;
        env.storage().persistent().set(&DataKey::AssetConfig(symbol), &cfg);
    }

    /// DAO-governed per-asset debt ceiling adjustment.
    pub fn set_debt_ceiling(env: Env, dao: Address, symbol: Symbol, ceiling: i128) {
        Self::_require_dao(&env, &dao);
        let mut cfg = Self::_get_cfg(&env, &symbol);
        cfg.debt_ceiling = ceiling;
        env.storage().persistent().set(&DataKey::AssetConfig(symbol), &cfg);
    }

    pub fn set_stability_pool(env: Env, admin: Address, pool: Address) {
        Self::_require_admin(&env, &admin);
        env.storage().instance().set(&STABPOOL, &pool);
    }

    pub fn set_paused(env: Env, admin: Address, paused: bool) {
        Self::_require_admin(&env, &admin);
        env.storage().instance().set(&PAUSED, &paused);
    }

    /// Replace this contract's Wasm in place. Instance and persistent
    /// storage are preserved. Authorised by the stored DAO address.
    pub fn upgrade(env: Env, _dao: Address, new_wasm_hash: BytesN<32>) {
        let stored: Address = env.storage().instance().get(&DAO).unwrap();
        stored.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    // ════════════════════════════════════════════════════════════
    //  VAULT OPERATIONS
    // ════════════════════════════════════════════════════════════

    pub fn open_vault(
        env: Env, owner: Address, synth: Symbol,
        coll_token: Address, coll_amount: i128,
    ) {
        owner.require_auth();
        Self::_assert_not_paused(&env);
        Self::_require_kyc(&env, &owner);
        Self::_require_compliant(&env, &owner, &owner, &synth, 0);

        assert!(
            env.storage().persistent()
                .get::<_, bool>(&DataKey::CollateralAllowed(coll_token.clone()))
                .unwrap_or(false),
            "collateral token not whitelisted"
        );
        let cfg: AssetConfig = env.storage().persistent()
            .get(&DataKey::AssetConfig(synth.clone()))
            .expect("synth asset not registered");
        assert!(cfg.enabled, "asset is disabled");
        assert!(
            !env.storage().persistent().has(&DataKey::Vault(owner.clone(), synth.clone())),
            "vault already open — use deposit_collateral"
        );
        assert!(coll_amount > 0, "collateral amount must be positive");

        // Standard SEP-41 transfer for collateral — token::Client is correct here
        token::Client::new(&env, &coll_token)
            .transfer(&owner, &env.current_contract_address(), &coll_amount);

        env.storage().persistent().set(&DataKey::Vault(owner.clone(), synth.clone()), &Vault {
            owner: owner.clone(), collateral: coll_amount, collateral_tok: coll_token.clone(),
            synth_asset: synth.clone(), debt: 0,
            opened_at: env.ledger().timestamp(), last_update: env.ledger().timestamp(),
        });

        let mut owned: Vec<Symbol> = env.storage().persistent()
            .get(&DataKey::VaultsByOwner(owner.clone()))
            .unwrap_or(Vec::new(&env));
        owned.push_back(synth.clone());
        env.storage().persistent().set(&DataKey::VaultsByOwner(owner.clone()), &owned);

        OpenEvent {
            owner,
            synth_asset: synth,
            collateral_token: coll_token,
            collateral_amount: coll_amount,
        }.publish(&env);
    }

    pub fn deposit_collateral(env: Env, owner: Address, synth: Symbol, amount: i128) {
        owner.require_auth();
        Self::_assert_not_paused(&env);
        Self::_require_kyc(&env, &owner);
        assert!(amount > 0, "amount must be positive");
        let mut vault = Self::_get_vault(&env, &owner, &synth);
        token::Client::new(&env, &vault.collateral_tok)
            .transfer(&owner, &env.current_contract_address(), &amount);
        vault.collateral  += amount;
        vault.last_update  = env.ledger().timestamp();
        env.storage().persistent().set(&DataKey::Vault(owner.clone(), synth.clone()), &vault);
        DepositEvent {
            owner,
            synth_asset: synth,
            amount,
        }.publish(&env);
    }

    pub fn mint_synth(env: Env, owner: Address, synth: Symbol, amount: i128) {
        owner.require_auth();
        Self::_assert_not_paused(&env);
        assert!(amount > 0, "amount must be positive");
        Self::_require_kyc(&env, &owner);
        Self::_require_compliant(&env, &owner, &owner, &synth, amount);

        let mut vault = Self::_get_vault(&env, &owner, &synth);
        let cfg       = Self::_get_cfg(&env, &synth);
        assert!(cfg.enabled, "asset is disabled");

        let current_debt: i128 = env.storage().persistent()
            .get(&DataKey::DebtTotal(synth.clone())).unwrap_or(0);
        assert!(
            current_debt.saturating_add(amount) <= cfg.debt_ceiling,
            "debt ceiling exceeded"
        );

        let new_debt = vault.debt.saturating_add(amount);
        Self::_assert_cr(&env, vault.collateral, new_debt, &cfg);

        vault.debt        = new_debt;
        vault.last_update = env.ledger().timestamp();
        env.storage().persistent().set(&DataKey::Vault(owner.clone(), synth.clone()), &vault);
        env.storage().persistent()
            .set(&DataKey::DebtTotal(synth.clone()), &(current_debt + amount));

        // ✅ FIXED: SyntheticTokenClient has mint(); token::Client does not
        SyntheticTokenClient::new(&env, &cfg.token).mint(&owner, &amount);

        // One-shot mint fee collected in synthetic token (not annual)
        let fee = amount.saturating_mul(cfg.stab_fee_bps) / (PRECISION * 100);
        if fee > 0 {
            // Burn the fee from owner, log it in earnings distributor
            SyntheticTokenClient::new(&env, &cfg.token).burn(&owner, &fee);
            if let Some(earn_addr) = env.storage().instance().get::<_, Address>(&EARNINGS) {
                EarningsClient::new(&env, &earn_addr)
                    .receive_revenue(
                        &env.current_contract_address(),
                        &cfg.token,
                        &fee,
                        &symbol_short!("stabfee"),
                        &0_u32, // untagged B2C mint fee — full protocol share
                    );
            }
        }

        MintEvent {
            owner,
            synth_asset: synth,
            amount,
        }.publish(&env);
    }

    pub fn burn_synth(env: Env, owner: Address, synth: Symbol, amount: i128) {
        owner.require_auth();
        assert!(amount > 0, "amount must be positive");
        Self::_require_kyc(&env, &owner);
        let mut vault = Self::_get_vault(&env, &owner, &synth);
        let cfg       = Self::_get_cfg(&env, &synth);

        let repay = amount.min(vault.debt);

        // ✅ FIXED: SyntheticTokenClient for burn on the synth token
        SyntheticTokenClient::new(&env, &cfg.token).burn(&owner, &repay);

        vault.debt        -= repay;
        vault.last_update  = env.ledger().timestamp();
        env.storage().persistent().set(&DataKey::Vault(owner.clone(), synth.clone()), &vault);
        let debt: i128 = env.storage().persistent()
            .get(&DataKey::DebtTotal(synth.clone())).unwrap_or(0);
        env.storage().persistent()
            .set(&DataKey::DebtTotal(synth.clone()), &(debt - repay).max(0));

        BurnEvent {
            owner,
            synth_asset: synth,
            amount: repay,
        }.publish(&env);
    }

    pub fn withdraw_collateral(env: Env, owner: Address, synth: Symbol, amount: i128) {
        owner.require_auth();
        Self::_assert_not_paused(&env);
        assert!(amount > 0, "amount must be positive");
        Self::_require_kyc(&env, &owner);
        let mut vault = Self::_get_vault(&env, &owner, &synth);
        let cfg       = Self::_get_cfg(&env, &synth);
        assert!(vault.collateral >= amount, "insufficient collateral");
        let new_coll = vault.collateral - amount;
        if vault.debt > 0 {
            Self::_assert_cr(&env, new_coll, vault.debt, &cfg);
        }
        vault.collateral  = new_coll;
        vault.last_update = env.ledger().timestamp();
        env.storage().persistent().set(&DataKey::Vault(owner.clone(), synth.clone()), &vault);
        // Standard transfer back — collateral is not a synthetic
        token::Client::new(&env, &vault.collateral_tok)
            .transfer(&env.current_contract_address(), &owner, &amount);
        WithdrawEvent {
            owner,
            synth_asset: synth,
            amount,
        }.publish(&env);
    }

    // ════════════════════════════════════════════════════════════
    //  LIQUIDATION
    // ════════════════════════════════════════════════════════════

    pub fn liquidate(
        env: Env, liquidator: Address, owner: Address,
        synth: Symbol, repay_amt: i128,
    ) {
        liquidator.require_auth();
        Self::_assert_not_paused(&env);
        assert!(repay_amt > 0, "repay amount must be positive");
        Self::_require_kyc(&env, &liquidator);

        let mut vault = Self::_get_vault(&env, &owner, &synth);
        let cfg       = Self::_get_cfg(&env, &synth);

        let coll_price  = Self::_price(&env, &cfg.coll_oracle_sym);
        let synth_price = Self::_price(&env, &cfg.oracle_symbol);
        let coll_usd    = vault.collateral * coll_price / WAD;
        let debt_usd    = vault.debt       * synth_price / WAD;
        let cr_bps      = if debt_usd > 0 { coll_usd * PRECISION / debt_usd } else { i128::MAX };
        assert!(cr_bps < cfg.liq_ratio, "vault is not undercollateralised");

        let actual_repay = repay_amt.min(vault.debt);

        // ✅ FIXED: liquidator burns synthetic via SyntheticTokenClient
        SyntheticTokenClient::new(&env, &cfg.token).burn(&liquidator, &actual_repay);

        let repay_usd   = actual_repay * synth_price / WAD;
        let penalty_usd = repay_usd * cfg.liq_penalty / PRECISION;
        let award_coll  = ((repay_usd + penalty_usd) * WAD / coll_price).min(vault.collateral);

        token::Client::new(&env, &vault.collateral_tok)
            .transfer(&env.current_contract_address(), &liquidator, &award_coll);

        vault.collateral  -= award_coll;
        vault.debt        -= actual_repay;
        vault.last_update  = env.ledger().timestamp();

        if vault.debt == 0 && vault.collateral > 0 {
            if let Some(sp) = env.storage().instance().get::<_, Address>(&STABPOOL) {
                token::Client::new(&env, &vault.collateral_tok)
                    .transfer(&env.current_contract_address(), &sp, &vault.collateral);
            }
            vault.collateral = 0;
        }

        if vault.debt == 0 && vault.collateral == 0 {
            env.storage().persistent().remove(&DataKey::Vault(owner.clone(), synth.clone()));
        } else {
            env.storage().persistent().set(&DataKey::Vault(owner.clone(), synth.clone()), &vault);
        }

        let debt: i128 = env.storage().persistent()
            .get(&DataKey::DebtTotal(synth.clone())).unwrap_or(0);
        env.storage().persistent()
            .set(&DataKey::DebtTotal(synth.clone()), &(debt - actual_repay).max(0));

        LiquidationEvent {
            owner,
            synth_asset: synth,
            collateral_seized: award_coll,
        }.publish(&env);
    }

    // ════════════════════════════════════════════════════════════
    //  VIEWS
    // ════════════════════════════════════════════════════════════

    pub fn get_vault(env: Env, owner: Address, synth: Symbol) -> Option<Vault> {
        env.storage().persistent().get(&DataKey::Vault(owner, synth))
    }

    pub fn get_cr_bps(env: Env, owner: Address, synth: Symbol) -> i128 {
        let vault       = Self::_get_vault(&env, &owner, &synth);
        if vault.debt == 0 { return i128::MAX; }
        let cfg         = Self::_get_cfg(&env, &synth);
        let coll_price  = Self::_price(&env, &cfg.coll_oracle_sym);
        let synth_price = Self::_price(&env, &cfg.oracle_symbol);
        vault.collateral * coll_price / WAD * PRECISION / (vault.debt * synth_price / WAD)
    }

    pub fn is_liquidatable(env: Env, owner: Address, synth: Symbol) -> bool {
        let vault = match env.storage().persistent()
            .get::<_, Vault>(&DataKey::Vault(owner, synth.clone())) {
            Some(v) => v, None => return false,
        };
        if vault.debt == 0 { return false; }
        let cfg         = Self::_get_cfg(&env, &synth);
        let coll_price  = Self::_price(&env, &cfg.coll_oracle_sym);
        let synth_price = Self::_price(&env, &cfg.oracle_symbol);
        let cr          = vault.collateral * coll_price / WAD * PRECISION / (vault.debt * synth_price / WAD);
        cr < cfg.liq_ratio
    }

    pub fn get_debt_total(env: Env, synth: Symbol) -> i128 {
        env.storage().persistent().get(&DataKey::DebtTotal(synth)).unwrap_or(0)
    }

    pub fn get_asset_config(env: Env, synth: Symbol) -> Option<AssetConfig> {
        env.storage().persistent().get(&DataKey::AssetConfig(synth))
    }

    pub fn get_vaults_by_owner(env: Env, owner: Address) -> Vec<Symbol> {
        env.storage().persistent()
            .get(&DataKey::VaultsByOwner(owner))
            .unwrap_or(Vec::new(&env))
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage().instance().get(&PAUSED).unwrap_or(false)
    }

    // ════════════════════════════════════════════════════════════
    //  INTERNALS
    // ════════════════════════════════════════════════════════════

	fn _assert_cr(env: &Env, collateral: i128, debt: i128, cfg: &AssetConfig) {
        let coll_price  = Self::_price(env, &cfg.coll_oracle_sym);
        let synth_price = Self::_price(env, &cfg.oracle_symbol);
        // Compare without intermediate USD rounding:
        //   (coll * coll_price) / (debt * synth_price) >= min_cr / PRECISION
        // ⇔ coll * coll_price * PRECISION >= debt * synth_price * min_cr
        let left  = collateral.saturating_mul(coll_price).saturating_mul(PRECISION);
        let right = debt.saturating_mul(synth_price).saturating_mul(cfg.min_coll_ratio);
        assert!(left >= right, "collateral ratio too low");
    }

    fn _price(env: &Env, asset: &Symbol) -> i128 {
        let oracle_addr: Address = env.storage().instance().get(&ORACLE).unwrap();
        // get_twap returns the TWAP price; falls back to spot if TWAP not yet computed
        OracleClient::new(env, &oracle_addr).get_twap(asset)
    }

    fn _require_compliant(env: &Env, from: &Address, to: &Address, synth: &Symbol, amount: i128) {
        let comp_addr: Address = env.storage().instance().get(&COMPLIANCE).unwrap();
        ComplianceClient::new(env, &comp_addr).assert_transfer_allowed(
            &env.current_contract_address(), from, to, synth, &amount,
        );
    }

    /// Baseline KYC gate: no user may open/fund/borrow/repay a vault or
    /// liquidate one without a live, non-restricted-jurisdiction KYC
    /// attestation on file with the compliance registry.
    fn _require_kyc(env: &Env, user: &Address) {
        let comp_addr: Address = env.storage().instance().get(&COMPLIANCE).unwrap();
        ComplianceClient::new(env, &comp_addr)
            .assert_user_compliant(&env.current_contract_address(), user);
    }

    fn _get_vault(env: &Env, owner: &Address, synth: &Symbol) -> Vault {
        env.storage().persistent()
            .get(&DataKey::Vault(owner.clone(), synth.clone()))
            .expect("vault not found — open one first")
    }

    fn _get_cfg(env: &Env, synth: &Symbol) -> AssetConfig {
        env.storage().persistent()
            .get(&DataKey::AssetConfig(synth.clone()))
            .expect("asset not configured")
    }

    fn _assert_not_paused(env: &Env) {
        assert!(
            !env.storage().instance().get::<_, bool>(&PAUSED).unwrap_or(false),
            "protocol is paused"
        );
    }

    fn _require_admin(env: &Env, caller: &Address) {
        let admin: Address = env.storage().instance().get(&ADMIN).unwrap();
        assert!(*caller == admin, "admin only");
        caller.require_auth();
    }

    fn _require_dao(env: &Env, caller: &Address) {
        let dao: Address = env.storage().instance().get(&DAO).unwrap();
        assert!(*caller == dao, "DAO only");
        caller.require_auth();
    }
}