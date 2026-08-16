// ═══════════════════════════════════════════════════════════════════
//  CONTRACT 4: Liquidity Pool (AMM)
//
//  - Constant-product AMM (x*y=k) with fee tier system
//  - LP share tokens minted on deposit (SEP-41 compatible)
//  - Multi-fee tiers: 0.05%, 0.3%, 1%
//  - Swap fees → split between LPs and earnings distributor
//  - Investors earn: swap fees + GOV emissions + stability fee share
//  - Price impact protection (max 1% per swap by default)
//  - Flash swap support for arbitrage
//  - Protocol fee switch (DAO-controlled, 0–25% of swap fee)
// ═══════════════════════════════════════════════════════════════════
#![no_std]
use soroban_sdk::{
    contract, contractevent, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env, Symbol,
};
use shared_interfaces::{ComplianceClient, EarningsClient};

// ── Fee tiers (bps) ───────────────────────────────────────────────
const FEE_TIER_LOW: i128 = 5; // 0.05% — stablecoin pairs
const FEE_TIER_MED: i128 = 30; // 0.3%  — major pairs
const FEE_TIER_HIGH: i128 = 100; // 1.0%  — exotic pairs
const PRECISION: i128 = 10_000;
const WAD: i128 = 10_000_000;
const MAX_PRICE_IMPACT: i128 = 100; // 1%
const MIN_LIQUIDITY: i128 = 1_000; // burned on first deposit (prevents inflation attack)

const ADMIN: Symbol = symbol_short!("ADMIN");
const DAO: Symbol = symbol_short!("DAO");
const EARNINGS: Symbol = symbol_short!("EARN");
const COMPLIANCE: Symbol = symbol_short!("COMPL");
const PROT_FEE: Symbol = symbol_short!("PROTFEE"); // protocol fee in bps of swap fee

// ── Pool state ────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug)]
pub struct Pool {
    pub token_a: Address,
    pub token_b: Address,
    pub reserve_a: i128,
    pub reserve_b: i128,
    pub total_shares: i128,
    pub fee_tier: i128,    // bps
    pub fee_a_accum: i128, // accumulated fees per share in token_a (scaled 1e12)
    pub fee_b_accum: i128,
    pub cumulative_vol_a: i128, // total volume traded (for analytics)
    pub cumulative_vol_b: i128,
    pub paused: bool,
}

/// Per-LP position tracking
#[contracttype]
#[derive(Clone, Debug)]
pub struct LpPosition {
    pub shares: i128,
    pub fee_a_debt: i128, // fee_a_accum snapshot at last interaction
    pub fee_b_debt: i128,
    pub pending_a: i128, // accrued but unclaimed fees
    pub pending_b: i128,
    pub deposited_at: u64,
}

#[contracttype]
pub enum DataKey {
    Pool(Symbol),              // pool_id → Pool
    LpPos(Symbol, Address),    // (pool_id, provider) → LpPosition
    LpShares(Symbol, Address), // total shares held per LP (mirrors LpPosition.shares)
    PoolList(u32),             // index → pool_id
    PoolCount,
}

// ─────────────────────────────────────────────────────────────────
//  Events (using #[contractevent] macro)
// ─────────────────────────────────────────────────────────────────
#[contractevent(topics = ["addliq"])]
pub struct AddLiquidityEvent {
    #[topic]
    pub provider: Address,
    #[topic]
    pub pool_id: Symbol,
    pub amount_a: i128,
    pub amount_b: i128,
    pub shares: i128,
}

#[contractevent(topics = ["remliq"])]
pub struct RemoveLiquidityEvent {
    #[topic]
    pub provider: Address,
    #[topic]
    pub pool_id: Symbol,
    pub out_a: i128,
    pub out_b: i128,
    pub shares: i128,
}

#[contractevent(topics = ["swap"])]
pub struct SwapEvent {
    #[topic]
    pub trader: Address,
    #[topic]
    pub pool_id: Symbol,
    pub amount_in: i128,
    pub amount_out: i128,
    pub a_to_b: bool,
    /// 0 = direct / B2C (full protocol earnings); >0 = partner referral tag
    pub partner_id: u32,
}

#[contractevent(topics = ["claim"])]
pub struct ClaimFeesEvent {
    #[topic]
    pub provider: Address,
    #[topic]
    pub pool_id: Symbol,
    pub claim_a: i128,
    pub claim_b: i128,
}

#[contract]
pub struct LiquidityPool;

#[contractimpl]
impl LiquidityPool {
    // ── Init ──────────────────────────────────────────────────────
    pub fn initialize(
        env: Env,
        admin: Address,
        dao: Address,
        earnings: Address,
        compliance: Address,
    ) {
        admin.require_auth();
        assert!(!env.storage().instance().has(&ADMIN), "already initialised");
        env.storage().instance().set(&ADMIN, &admin);
        env.storage().instance().set(&DAO, &dao);
        env.storage().instance().set(&EARNINGS, &earnings);
        env.storage().instance().set(&COMPLIANCE, &compliance);
        env.storage().instance().set(&PROT_FEE, &500_i128); // 5% of swap fee to protocol
        env.storage().instance().set(&DataKey::PoolCount, &0_u32);
    }

    /// Allow the DAO to point at a new compliance registry deployment.
    pub fn set_compliance(env: Env, dao: Address, compliance: Address) {
        Self::_require_dao(&env, &dao);
        env.storage().instance().set(&COMPLIANCE, &compliance);
    }

    // ── Create a new pool (admin/DAO) ─────────────────────────────
    pub fn create_pool(
        env: Env,
        creator: Address,
        pool_id: Symbol,
        token_a: Address,
        token_b: Address,
        fee_tier: i128,
    ) -> Symbol {
        creator.require_auth();
        let admin: Address = env.storage().instance().get(&ADMIN).unwrap();
        let dao: Address = env.storage().instance().get(&DAO).unwrap();
        assert!(creator == admin || creator == dao, "admin or DAO only");
        assert!(
            fee_tier == FEE_TIER_LOW || fee_tier == FEE_TIER_MED || fee_tier == FEE_TIER_HIGH,
            "invalid fee tier"
        );
        assert!(
            !env.storage()
                .persistent()
                .has(&DataKey::Pool(pool_id.clone())),
            "pool exists"
        );

        let pool = Pool {
            token_a,
            token_b,
            reserve_a: 0,
            reserve_b: 0,
            total_shares: 0,
            fee_tier,
            fee_a_accum: 0,
            fee_b_accum: 0,
            cumulative_vol_a: 0,
            cumulative_vol_b: 0,
            paused: false,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool_id.clone()), &pool);

        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::PoolCount)
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::PoolList(count), &pool_id);
        env.storage()
            .instance()
            .set(&DataKey::PoolCount, &(count + 1));

        pool_id
    }

    // ── Add liquidity ─────────────────────────────────────────────
    pub fn add_liquidity(
        env: Env,
        provider: Address,
        pool_id: Symbol,
        amount_a: i128,
        amount_b: i128,
        min_shares: i128,
    ) -> i128 {
        provider.require_auth();
        Self::_require_kyc(&env, &provider);
        let mut pool = Self::_get_pool(&env, &pool_id);
        assert!(!pool.paused, "pool paused");
        assert!(amount_a > 0 && amount_b > 0, "amounts must be positive");

        // Settle pending fees before changing position
        Self::_settle_fees(&env, &pool_id, &provider, &pool);

        let shares: i128;
        if pool.total_shares == 0 {
            // First deposit: geometric mean, burn MIN_LIQUIDITY
            shares = Self::_sqrt(amount_a * amount_b) - MIN_LIQUIDITY;
            assert!(shares > 0, "initial liquidity too small");
            // Burn MIN_LIQUIDITY permanently to this contract
            pool.total_shares = MIN_LIQUIDITY;
        } else {
            // Proportional: shares = min(amount_a/res_a, amount_b/res_b) * total
            let s_a = amount_a * pool.total_shares / pool.reserve_a;
            let s_b = amount_b * pool.total_shares / pool.reserve_b;
            shares = s_a.min(s_b);
        }
        assert!(shares >= min_shares, "slippage: shares below minimum");

        // Transfer tokens in
        let tok_a = token::Client::new(&env, &pool.token_a);
        let tok_b = token::Client::new(&env, &pool.token_b);
        tok_a.transfer(&provider, &env.current_contract_address(), &amount_a);
        tok_b.transfer(&provider, &env.current_contract_address(), &amount_b);

        pool.reserve_a += amount_a;
        pool.reserve_b += amount_b;
        pool.total_shares += shares;

        // Update LP position
        let mut pos: LpPosition = env
            .storage()
            .persistent()
            .get(&DataKey::LpPos(pool_id.clone(), provider.clone()))
            .unwrap_or(LpPosition {
                shares: 0,
                fee_a_debt: pool.fee_a_accum,
                fee_b_debt: pool.fee_b_accum,
                pending_a: 0,
                pending_b: 0,
                deposited_at: env.ledger().timestamp(),
            });
        pos.shares += shares;
        pos.fee_a_debt = pool.fee_a_accum;
        pos.fee_b_debt = pool.fee_b_accum;
        env.storage()
            .persistent()
            .set(&DataKey::LpPos(pool_id.clone(), provider.clone()), &pos);
        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool_id.clone()), &pool);

        AddLiquidityEvent {
            provider,
            pool_id,
            amount_a,
            amount_b,
            shares,
        }
        .publish(&env);
        shares
    }

    // ── Remove liquidity ──────────────────────────────────────────
    pub fn remove_liquidity(
        env: Env,
        provider: Address,
        pool_id: Symbol,
        shares: i128,
        min_a: i128,
        min_b: i128,
    ) -> (i128, i128) {
        provider.require_auth();
        Self::_require_kyc(&env, &provider);
        let mut pool = Self::_get_pool(&env, &pool_id);

        // Settle and claim fees first
        Self::_settle_fees(&env, &pool_id, &provider, &pool);
        Self::_do_claim(&env, &pool_id, &provider, &pool);

        let mut pos: LpPosition = env
            .storage()
            .persistent()
            .get(&DataKey::LpPos(pool_id.clone(), provider.clone()))
            .expect("no position");
        assert!(pos.shares >= shares, "insufficient shares");

        let out_a = shares * pool.reserve_a / pool.total_shares;
        let out_b = shares * pool.reserve_b / pool.total_shares;
        assert!(out_a >= min_a, "slippage: token_a below minimum");
        assert!(out_b >= min_b, "slippage: token_b below minimum");

        pool.reserve_a -= out_a;
        pool.reserve_b -= out_b;
        pool.total_shares -= shares;
        pos.shares -= shares;

        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool_id.clone()), &pool);
        env.storage()
            .persistent()
            .set(&DataKey::LpPos(pool_id.clone(), provider.clone()), &pos);

        let tok_a = token::Client::new(&env, &pool.token_a);
        let tok_b = token::Client::new(&env, &pool.token_b);
        tok_a.transfer(&env.current_contract_address(), &provider, &out_a);
        tok_b.transfer(&env.current_contract_address(), &provider, &out_b);

        RemoveLiquidityEvent {
            provider,
            pool_id,
            out_a,
            out_b,
            shares,
        }
        .publish(&env);
        (out_a, out_b)
    }

    // ── Swap ──────────────────────────────────────────────────────
    /// Exact-in swap. Pass `partner_id = 0` for untagged / B2C swaps
    /// (Uruk keeps the full protocol fee). Pass a registered partner id
    /// so earnings are split with that partner.
    pub fn swap_exact_in(
        env: Env,
        trader: Address,
        pool_id: Symbol,
        a_to_b: bool, // true = sell token_a, receive token_b
        amount_in: i128,
        min_out: i128,
        partner_id: u32,
    ) -> i128 {
        trader.require_auth();
        Self::_require_kyc(&env, &trader);
        let mut pool = Self::_get_pool(&env, &pool_id);
        assert!(!pool.paused, "pool paused");
        assert!(amount_in > 0, "amount_in must be positive");

        // Fee split: LP share + protocol share
        let prot_fee_bps: i128 = env.storage().instance().get(&PROT_FEE).unwrap_or(0);
        let total_fee = amount_in * pool.fee_tier / PRECISION;
        let prot_fee_amt = total_fee * prot_fee_bps / PRECISION;
        let lp_fee_amt = total_fee - prot_fee_amt;
        let amount_in_net = amount_in - total_fee;

        // AMM: amount_out = reserve_out * amount_in_net / (reserve_in + amount_in_net)
        let amount_out: i128;
        let fee_token: Address;
        if a_to_b {
            amount_out = pool.reserve_b * amount_in_net / (pool.reserve_a + amount_in_net);
            // Price impact check
            let impact = amount_out * PRECISION / pool.reserve_b;
            assert!(impact <= MAX_PRICE_IMPACT, "price impact too high");

            pool.reserve_a += amount_in;
            pool.reserve_b -= amount_out;
            pool.cumulative_vol_a += amount_in;

            // Accrue LP fees per share
            if pool.total_shares > 0 {
                pool.fee_a_accum += lp_fee_amt * 1_000_000_000_000 / pool.total_shares;
            }

            let tok_a = token::Client::new(&env, &pool.token_a);
            let tok_b = token::Client::new(&env, &pool.token_b);
            tok_a.transfer(&trader, &env.current_contract_address(), &amount_in);
            tok_b.transfer(&env.current_contract_address(), &trader, &amount_out);
            fee_token = pool.token_a.clone();
        } else {
            amount_out = pool.reserve_a * amount_in_net / (pool.reserve_b + amount_in_net);
            let impact = amount_out * PRECISION / pool.reserve_a;
            assert!(impact <= MAX_PRICE_IMPACT, "price impact too high");

            pool.reserve_b += amount_in;
            pool.reserve_a -= amount_out;
            pool.cumulative_vol_b += amount_in;

            if pool.total_shares > 0 {
                pool.fee_b_accum += lp_fee_amt * 1_000_000_000_000 / pool.total_shares;
            }

            let tok_a = token::Client::new(&env, &pool.token_a);
            let tok_b = token::Client::new(&env, &pool.token_b);
            tok_b.transfer(&trader, &env.current_contract_address(), &amount_in);
            tok_a.transfer(&env.current_contract_address(), &trader, &amount_out);
            fee_token = pool.token_b.clone();
        }

        // Route protocol fee through EarningsDistributor (partner_id=0 → full protocol)
        if prot_fee_amt > 0 {
            let earnings: Address = env.storage().instance().get(&EARNINGS).unwrap();
            EarningsClient::new(&env, &earnings).receive_revenue(
                &env.current_contract_address(),
                &fee_token,
                &prot_fee_amt,
                &Symbol::new(&env, "swapfee"),
                &partner_id,
            );
        }

        assert!(amount_out >= min_out, "slippage: output below minimum");
        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool_id.clone()), &pool);

        SwapEvent {
            trader,
            pool_id,
            amount_in,
            amount_out,
            a_to_b,
            partner_id,
        }
        .publish(&env);
        amount_out
    }

    // ── Claim accumulated LP fees ─────────────────────────────────
    pub fn claim_fees(env: Env, provider: Address, pool_id: Symbol) -> (i128, i128) {
        provider.require_auth();
        Self::_require_kyc(&env, &provider);
        let pool = Self::_get_pool(&env, &pool_id);
        Self::_settle_fees(&env, &pool_id, &provider, &pool);
        Self::_do_claim(&env, &pool_id, &provider, &pool)
    }

    // ── Views ─────────────────────────────────────────────────────
    pub fn get_pool(env: Env, pool_id: Symbol) -> Pool {
        Self::_get_pool(&env, &pool_id)
    }

    pub fn get_lp_position(env: Env, pool_id: Symbol, provider: Address) -> Option<LpPosition> {
        env.storage()
            .persistent()
            .get(&DataKey::LpPos(pool_id, provider))
    }

    pub fn get_price(env: Env, pool_id: Symbol, a_to_b: bool) -> i128 {
        let pool = Self::_get_pool(&env, &pool_id);
        if a_to_b {
            pool.reserve_b * WAD / pool.reserve_a
        } else {
            pool.reserve_a * WAD / pool.reserve_b
        }
    }

    pub fn quote(env: Env, pool_id: Symbol, amount_in: i128, a_to_b: bool) -> i128 {
        let pool = Self::_get_pool(&env, &pool_id);
        let fee = amount_in * pool.fee_tier / PRECISION;
        let net = amount_in - fee;
        if a_to_b {
            pool.reserve_b * net / (pool.reserve_a + net)
        } else {
            pool.reserve_a * net / (pool.reserve_b + net)
        }
    }

    pub fn claimable_fees(env: Env, pool_id: Symbol, provider: Address) -> (i128, i128) {
        let pool = Self::_get_pool(&env, &pool_id);
        let pos: LpPosition = match env
            .storage()
            .persistent()
            .get(&DataKey::LpPos(pool_id.clone(), provider))
        {
            Some(p) => p,
            None => return (0, 0),
        };
        let a =
            pos.pending_a + (pool.fee_a_accum - pos.fee_a_debt) * pos.shares / 1_000_000_000_000;
        let b =
            pos.pending_b + (pool.fee_b_accum - pos.fee_b_debt) * pos.shares / 1_000_000_000_000;
        (a, b)
    }

    pub fn set_paused(env: Env, admin: Address, pool_id: Symbol, paused: bool) {
        Self::_require_admin(&env, &admin);
        let mut pool = Self::_get_pool(&env, &pool_id);
        pool.paused = paused;
        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool_id), &pool);
    }

    pub fn set_protocol_fee(env: Env, dao: Address, fee_bps: i128) {
        Self::_require_dao(&env, &dao);
        assert!(fee_bps <= 2_500, "protocol fee cannot exceed 25%");
        env.storage().instance().set(&PROT_FEE, &fee_bps);
    }

    /// Replace this contract's Wasm in place. Instance and persistent
    /// storage are preserved. Authorised by the stored DAO address.
    pub fn upgrade(env: Env, _dao: Address, new_wasm_hash: BytesN<32>) {
        let stored: Address = env.storage().instance().get(&DAO).unwrap();
        stored.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    // ── Internals ─────────────────────────────────────────────────
    fn _settle_fees(env: &Env, pool_id: &Symbol, provider: &Address, pool: &Pool) {
        let mut pos: LpPosition = match env
            .storage()
            .persistent()
            .get(&DataKey::LpPos(pool_id.clone(), provider.clone()))
        {
            Some(p) => p,
            None => return,
        };
        if pos.shares == 0 {
            return;
        }
        pos.pending_a += (pool.fee_a_accum - pos.fee_a_debt) * pos.shares / 1_000_000_000_000;
        pos.pending_b += (pool.fee_b_accum - pos.fee_b_debt) * pos.shares / 1_000_000_000_000;
        pos.fee_a_debt = pool.fee_a_accum;
        pos.fee_b_debt = pool.fee_b_accum;
        env.storage()
            .persistent()
            .set(&DataKey::LpPos(pool_id.clone(), provider.clone()), &pos);
    }

    fn _do_claim(env: &Env, pool_id: &Symbol, provider: &Address, pool: &Pool) -> (i128, i128) {
        let mut pos: LpPosition = match env
            .storage()
            .persistent()
            .get(&DataKey::LpPos(pool_id.clone(), provider.clone()))
        {
            Some(p) => p,
            None => return (0, 0),
        };
        let claim_a = pos.pending_a;
        let claim_b = pos.pending_b;
        if claim_a == 0 && claim_b == 0 {
            return (0, 0);
        }

        pos.pending_a = 0;
        pos.pending_b = 0;
        env.storage()
            .persistent()
            .set(&DataKey::LpPos(pool_id.clone(), provider.clone()), &pos);

        if claim_a > 0 {
            let tok = token::Client::new(env, &pool.token_a);
            tok.transfer(&env.current_contract_address(), provider, &claim_a);
        }
        if claim_b > 0 {
            let tok = token::Client::new(env, &pool.token_b);
            tok.transfer(&env.current_contract_address(), provider, &claim_b);
        }

        ClaimFeesEvent {
            provider: provider.clone(),
            pool_id: pool_id.clone(),
            claim_a,
            claim_b,
        }
        .publish(&env);
        (claim_a, claim_b)
    }

    fn _sqrt(x: i128) -> i128 {
        if x == 0 {
            return 0;
        }
        let mut z = x;
        let mut y = (x / 2) + 1;
        while y < z {
            z = y;
            y = (x / y + y) / 2;
        }
        z
    }

    fn _get_pool(env: &Env, pool_id: &Symbol) -> Pool {
        env.storage()
            .persistent()
            .get(&DataKey::Pool(pool_id.clone()))
            .expect("pool not found")
    }

    /// No address may add/remove liquidity, swap, or claim fees without a
    /// live KYC attestation in an unrestricted jurisdiction.
    fn _require_kyc(env: &Env, user: &Address) {
        let comp_addr: Address = env.storage().instance().get(&COMPLIANCE).unwrap();
        ComplianceClient::new(env, &comp_addr)
            .assert_user_compliant(&env.current_contract_address(), user);
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
