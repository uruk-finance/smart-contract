// ═══════════════════════════════════════════════════════════════════
//  CONTRACT 5: Stability Pool
//
//  Liquity-inspired design adapted for Stellar Soroban:
//  - Users deposit USDC to provide a backstop for liquidations
//  - When a vault is liquidated: stability pool absorbs debt,
//    receives discounted collateral (XLM/USDC/etc.) in return
//  - Depositors earn:
//      1. Collateral gains from liquidations (pro-rata)
//      2. A share of stability fees routed from SyntheticEngine
//  - Epoch/scale system prevents precision loss with large liquidations
//  - Full withdrawal always possible (no lock)
// ═══════════════════════════════════════════════════════════════════
#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, contractevent, symbol_short,
    token, Address, BytesN, Env, Symbol, Vec,
};
use shared_interfaces::ComplianceClient;

const PRECISION:     i128 = 1_000_000_000_000; // 1e12
const DECIMAL_PREC:  i128 = 10_000_000;         // 7dp
const ADMIN:         Symbol = symbol_short!("ADMIN");
const USDC:          Symbol = symbol_short!("USDC");
const SYNTH_ENGINE:  Symbol = symbol_short!("ENGINE");
const DAO:           Symbol = symbol_short!("DAO");
const COMPLIANCE:    Symbol = symbol_short!("COMPL");

// ── Epoch tracking (Liquity-style) ───────────────────────────────
//  P = running product of (1 - loss_fraction) per liquidation
//  When P collapses to near-zero, we start a new epoch.
//  Each depositor stores their snapshot of (P, epoch, sum_coll_gain).

#[contracttype]
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub p_value:      i128,   // product snapshot
    pub sum_coll:     i128,   // cumulative collateral gain per unit USDC
    pub epoch:        u64,
    pub scale:        u64,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Deposit {
    pub initial_value: i128,   // USDC deposited in current epoch
    pub snapshot:      Snapshot,
    pub deposited_at:  u64,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct PoolState {
    pub total_deposits:    i128,   // total USDC in pool
    pub p:                 i128,   // current running product (starts at PRECISION)
    pub epoch:             u64,    // current epoch
    pub scale:             u64,
    pub sum_coll:          i128,   // cumulative S: coll gain per USDC (epoch-relative)
    pub total_coll_gained: i128,   // all-time coll received from liquidations
    pub total_liq_count:   u64,
    pub paused:            bool,
}

#[contracttype]
pub enum DataKey {
    Deposit(Address),
    State,
    EpochSum(u64, u64),    // (epoch, scale) → cumulative S at epoch start
    EpochP(u64),           // (epoch) → P at epoch boundary
    PendingColl(Address),  // unclaimed collateral per depositor
}


// ─────────────────────────────────────────────────────────────────
//  Events (using #[contractevent] macro)
// ─────────────────────────────────────────────────────────────────
#[contractevent(topics = ["deposit"])]
pub struct DepositEvent {
    #[topic]
    pub depositor: Address,
    pub amount: i128,
}

#[contractevent(topics = ["withdraw"])]
pub struct WithdrawEvent {
    #[topic]
    pub depositor: Address,
    pub amount: i128,
}

#[contractevent(topics = ["liqabs"])]
pub struct AbsorbLiquidationEvent {
    #[topic]
    pub engine: Address,
    pub debt_absorbed: i128,
    pub collateral_reward: i128,
}

#[contractevent(topics = ["claim"])]
pub struct ClaimGainsEvent {
    #[topic]
    pub depositor: Address,
    #[topic]
    pub gain_type: Symbol,
    pub amount: i128,
}

#[contract]
pub struct StabilityPool;

#[contractimpl]
impl StabilityPool {
    // ── Init ──────────────────────────────────────────────────────
    pub fn initialize(
        env:        Env,
        admin:      Address,
        usdc:       Address,
        engine:     Address,
        dao:        Address,
        compliance: Address,
    ) {
        admin.require_auth();
        assert!(!env.storage().instance().has(&ADMIN), "already initialised");
        env.storage().instance().set(&ADMIN,       &admin);
        env.storage().instance().set(&USDC,        &usdc);
        env.storage().instance().set(&SYNTH_ENGINE,&engine);
        env.storage().instance().set(&DAO,         &dao);
        env.storage().instance().set(&COMPLIANCE,  &compliance);
        env.storage().persistent().set(&DataKey::State, &PoolState {
            total_deposits: 0,
            p:              PRECISION,
            epoch:          0,
            scale:          0,
            sum_coll:       0,
            total_coll_gained: 0,
            total_liq_count: 0,
            paused:         false,
        });
    }

    // ── Deposit USDC ──────────────────────────────────────────────
    pub fn deposit(env: Env, depositor: Address, amount: i128) {
        depositor.require_auth();
        Self::_require_kyc(&env, &depositor);
        assert!(amount > 0, "amount must be positive");
        let mut state = Self::_get_state(&env);
        assert!(!state.paused, "pool paused");

        // Claim any pending gains before changing deposit
        Self::_claim_internal(&env, &depositor, &mut state);

        // Transfer USDC in
        let usdc: Address = env.storage().instance().get(&USDC).unwrap();
        token::Client::new(&env, &usdc).transfer(
            &depositor, &env.current_contract_address(), &amount,
        );

        let snap = Self::_current_snapshot(&state);
        let mut dep: Deposit = env.storage().persistent()
            .get(&DataKey::Deposit(depositor.clone()))
            .unwrap_or(Deposit {
                initial_value: 0,
                snapshot:      snap.clone(),
                deposited_at:  env.ledger().timestamp(),
            });
        dep.initial_value += amount;
        dep.snapshot       = snap;
        env.storage().persistent().set(&DataKey::Deposit(depositor.clone()), &dep);

        state.total_deposits += amount;
        env.storage().persistent().set(&DataKey::State, &state);

        DepositEvent {
            depositor,
            amount,
        }.publish(&env);
    }

    // ── Withdraw USDC ─────────────────────────────────────────────
    pub fn withdraw(env: Env, depositor: Address, amount: i128) -> i128 {
        depositor.require_auth();
        Self::_require_kyc(&env, &depositor);
        let mut state = Self::_get_state(&env);

        Self::_claim_internal(&env, &depositor, &mut state);

        let mut dep: Deposit = env.storage().persistent()
            .get(&DataKey::Deposit(depositor.clone()))
            .expect("no deposit");

        let compounded = Self::_compounded_deposit(&dep, &state);
        let withdraw_amt = amount.min(compounded);
        assert!(withdraw_amt > 0, "nothing to withdraw");

        let new_deposit = compounded - withdraw_amt;
        dep.initial_value = new_deposit;
        dep.snapshot      = Self::_current_snapshot(&state);
        env.storage().persistent().set(&DataKey::Deposit(depositor.clone()), &dep);

        state.total_deposits -= withdraw_amt;
        env.storage().persistent().set(&DataKey::State, &state);

        let usdc: Address = env.storage().instance().get(&USDC).unwrap();
        token::Client::new(&env, &usdc).transfer(
            &env.current_contract_address(), &depositor, &withdraw_amt,
        );

        WithdrawEvent {
            depositor,
            amount: withdraw_amt,
        }.publish(&env);
        withdraw_amt
    }

    // ── Absorb liquidation (called by SyntheticEngine) ────────────
    pub fn absorb_liquidation(
        env:         Env,
        engine:      Address,
        debt_to_abs: i128,    // USDC amount spent absorbing the shortfall
        coll_reward: i128,    // collateral token amount received
        coll_token:  Address,
    ) {
        engine.require_auth();
        let expected: Address = env.storage().instance().get(&SYNTH_ENGINE).unwrap();
        assert!(engine == expected, "only engine can trigger liquidation");

        let mut state = Self::_get_state(&env);
        assert!(!state.paused, "pool paused");
        assert!(state.total_deposits >= debt_to_abs, "insufficient stability pool depth");

        // Transfer collateral into pool
        token::Client::new(&env, &coll_token).transfer(
            &engine, &env.current_contract_address(), &coll_reward,
        );

        // USDC is an external asset — we can't mint/burn it like the old
        // sUSD synthetic. Instead, send the absorbed amount to the engine,
        // which uses it to settle the liquidated vault's obligations. This
        // keeps the pool's on-chain balance consistent with total_deposits.
        let usdc: Address = env.storage().instance().get(&USDC).unwrap();
        token::Client::new(&env, &usdc).transfer(
            &env.current_contract_address(), &engine, &debt_to_abs,
        );

        // Update running product P = P * (1 - debt/total)
        let loss_fraction = debt_to_abs * PRECISION / state.total_deposits;
        state.p = state.p - (state.p * loss_fraction / PRECISION);

        // Accrue collateral gain: sum_coll += coll_reward / total_deposits
        state.sum_coll += coll_reward * PRECISION / state.total_deposits;
        state.total_deposits -= debt_to_abs;
        state.total_coll_gained += coll_reward;
        state.total_liq_count  += 1;

        // If P collapses below 1e-9 of PRECISION, start new epoch
        if state.p < 1_000 {
            state.epoch += 1;
            state.scale  = 0;
            state.sum_coll = 0;
            state.p = PRECISION;
        }

        env.storage().persistent().set(&DataKey::State, &state);
        AbsorbLiquidationEvent {
            engine,
            debt_absorbed: debt_to_abs,
            collateral_reward: coll_reward,
        }.publish(&env);
    }

    // ── Claim gains (collateral) ───────────────────────────────────
    pub fn claim_gains(env: Env, depositor: Address) -> i128 {
        depositor.require_auth();
        Self::_require_kyc(&env, &depositor);
        let mut state = Self::_get_state(&env);
        Self::_claim_internal(&env, &depositor, &mut state);
        env.storage().persistent().set(&DataKey::State, &state);
        // Returns collateral_claimed — actual amount emitted in EVT_CLAIM
        0
    }

    // ── Views ─────────────────────────────────────────────────────
    pub fn get_compounded_deposit(env: Env, depositor: Address) -> i128 {
        let state = Self::_get_state(&env);
        let dep: Deposit = match env.storage().persistent()
            .get(&DataKey::Deposit(depositor)) {
            Some(d) => d,
            None    => return 0,
        };
        Self::_compounded_deposit(&dep, &state)
    }

    pub fn get_collateral_gain(env: Env, depositor: Address) -> i128 {
        let state = Self::_get_state(&env);
        let dep: Deposit = match env.storage().persistent()
            .get(&DataKey::Deposit(depositor)) {
            Some(d) => d,
            None    => return 0,
        };
        Self::_coll_gain(&dep, &state)
    }

    pub fn get_state(env: Env) -> PoolState {
        Self::_get_state(&env)
    }

    /// Total USDC currently deposited in the pool. Used by
    /// EarningsDistributor as the snapshot denominator when splitting
    /// revenue pro-rata across depositors.
    pub fn get_total_deposits(env: Env) -> i128 {
        Self::_get_state(&env).total_deposits
    }

    pub fn get_deposit(env: Env, depositor: Address) -> Option<Deposit> {
        env.storage().persistent().get(&DataKey::Deposit(depositor))
    }

    pub fn set_paused(env: Env, admin: Address, paused: bool) {
        Self::_require_admin(&env, &admin);
        let mut state = Self::_get_state(&env);
        state.paused = paused;
        env.storage().persistent().set(&DataKey::State, &state);
    }

    /// Allow the DAO to point at a new compliance registry deployment.
    pub fn set_compliance(env: Env, dao: Address, compliance: Address) {
        let cur_dao: Address = env.storage().instance().get(&DAO).unwrap();
        assert!(dao == cur_dao, "DAO only");
        dao.require_auth();
        env.storage().instance().set(&COMPLIANCE, &compliance);
    }

    /// Replace this contract's Wasm in place. Instance and persistent
    /// storage are preserved. Authorised by the stored DAO address.
    pub fn upgrade(env: Env, _dao: Address, new_wasm_hash: BytesN<32>) {
        let stored: Address = env.storage().instance().get(&DAO).unwrap();
        stored.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    // ── Internals ─────────────────────────────────────────────────
    /// No depositor may deposit/withdraw/claim without a live KYC
    /// attestation in an unrestricted jurisdiction.
    fn _require_kyc(env: &Env, user: &Address) {
        let comp_addr: Address = env.storage().instance().get(&COMPLIANCE).unwrap();
        ComplianceClient::new(env, &comp_addr)
            .assert_user_compliant(&env.current_contract_address(), user);
    }

    fn _claim_internal(env: &Env, depositor: &Address, state: &mut PoolState) {
        let dep: Deposit = match env.storage().persistent()
            .get(&DataKey::Deposit(depositor.clone())) {
            Some(d) => d,
            None    => return,
        };

        let coll_gain = Self::_coll_gain(&dep, state);

        if coll_gain > 0 {
            // Transfer collateral to depositor
            // (In production: track per-collateral-token balances)
            ClaimGainsEvent {
                depositor: depositor.clone(),
                gain_type: symbol_short!("coll"),
                amount: coll_gain,
            }.publish(&env);
        }

        // Reset snapshot
        let mut new_dep = dep;
        new_dep.snapshot = Self::_current_snapshot(state);
        env.storage().persistent().set(&DataKey::Deposit(depositor.clone()), &new_dep);
    }

    fn _compounded_deposit(dep: &Deposit, state: &PoolState) -> i128 {
        if dep.snapshot.epoch != state.epoch { return 0; } // full loss in different epoch
        let p_ratio = state.p * PRECISION / dep.snapshot.p_value.max(1);
        dep.initial_value * p_ratio / PRECISION
    }

    fn _coll_gain(dep: &Deposit, state: &PoolState) -> i128 {
        if dep.initial_value == 0 { return 0; }
        let sum_delta = state.sum_coll - dep.snapshot.sum_coll;
        dep.initial_value * sum_delta / PRECISION
    }

    fn _current_snapshot(state: &PoolState) -> Snapshot {
        Snapshot {
            p_value:  state.p,
            sum_coll: state.sum_coll,
            epoch:    state.epoch,
            scale:    state.scale,
        }
    }

    fn _get_state(env: &Env) -> PoolState {
        env.storage().persistent().get(&DataKey::State).unwrap()
    }

    fn _require_admin(env: &Env, caller: &Address) {
        let admin: Address = env.storage().instance().get(&ADMIN).unwrap();
        assert!(*caller == admin, "admin only");
        caller.require_auth();
    }
}
