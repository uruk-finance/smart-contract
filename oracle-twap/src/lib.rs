// ═══════════════════════════════════════════════════════════════════
//  CONTRACT 2: Decentralized Oracle with TWAP
//
//  Architecture:
//  - N trusted data providers (voted in/out by DAO)
//  - Each provider pushes a signed price observation
//  - Outlier rejection: drop top/bottom 25% before median
//  - TWAP: time-weighted average over configurable window
//  - Staleness guard: price unusable if last update > MAX_AGE
//  - Circuit breaker: reject price moves > 20% per update
//  - Per-asset price feeds (XLM/USD, BTC/USD, ETH/USD …)
// ═══════════════════════════════════════════════════════════════════
#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, contractevent, symbol_short,
    Address, Env, Symbol, Vec,
};

// ── Constants ────────────────────────────────────────────────────
const MAX_AGE_SECS:       u64  = 300;    // 5 minutes
const TWAP_WINDOW_SECS:   u64  = 3_600;  // 1 hour default
const MAX_PROVIDERS:      u32  = 21;     // odd for clean median
const MIN_PROVIDERS:      u32  = 3;      // minimum to accept price
const CIRCUIT_BPS:        i128 = 2_000;  // 20% max move per update
const PRECISION:          i128 = 10_000;
const ADMIN:              Symbol = symbol_short!("ADMIN");
const DAO:                Symbol = symbol_short!("DAO");
const PROVIDER_COUNT:     Symbol = symbol_short!("PCNT");
const TWAP_WINDOW:        Symbol = symbol_short!("TWINDOW");

// ── Types ─────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug)]
pub struct PriceObservation {
    pub price:     i128,   // 7 decimal places (e.g. 2_000_000_0 = $200.00000)
    pub timestamp: u64,
    pub provider:  Address,
}

/// Cumulative sum for TWAP calculation
#[contracttype]
#[derive(Clone, Debug)]
pub struct TwapAccumulator {
    pub cumulative_price: i128,   // sum of (price × Δt)
    pub last_price:       i128,
    pub last_timestamp:   u64,
    pub window_start:     u64,
    pub window_cum_start: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct AssetFeed {
    pub latest_price:    i128,
    pub latest_ts:       u64,
    pub twap_price:      i128,   // last computed TWAP
    pub twap_ts:         u64,
    pub observation_count: u32,
}

#[contracttype]
pub enum DataKey {
    Provider(Address),              // is this address a provider?
    ProviderList(u32),              // index → provider address
    Observation(Symbol, u32),       // (asset, provider_idx) → latest obs
    Feed(Symbol),                   // per-asset aggregated state
    Accumulator(Symbol),            // per-asset TWAP accumulator
    PendingObservations(Symbol),    // Vec<PriceObservation> before aggregation
}


// ─────────────────────────────────────────────────────────────────
//  Events (using #[contractevent] macro)
// ─────────────────────────────────────────────────────────────────
#[contractevent(topics = ["provider"])]
pub struct ProviderEvent {
    #[topic]
    pub action: Symbol,
    pub provider: Address,
}

#[contractevent(topics = ["price"])]
pub struct PriceEvent {
    #[topic]
    pub asset: Symbol,
    pub provider: Address,
    pub price: i128,
}

#[contractevent(topics = ["price"])]
pub struct PriceAggEvent {
    #[topic]
    pub asset: Symbol,
    pub median_price: i128,
}

#[contractevent(topics = ["twap"])]
pub struct TwapEvent {
    #[topic]
    pub asset: Symbol,
    pub twap_price: i128,
}

#[contract]
pub struct OracleTwap;

#[contractimpl]
impl OracleTwap {
    // ── Init ──────────────────────────────────────────────────────
    pub fn initialize(env: Env, admin: Address, dao: Address, twap_window: u64) {
        admin.require_auth();
        assert!(!env.storage().instance().has(&ADMIN), "already initialised");
        env.storage().instance().set(&ADMIN, &admin);
        env.storage().instance().set(&DAO, &dao);
        env.storage().instance().set(&PROVIDER_COUNT, &0_u32);
        env.storage().instance().set(
            &TWAP_WINDOW,
            &if twap_window == 0 { TWAP_WINDOW_SECS } else { twap_window },
        );
    }

    // ── Provider management (DAO-governed) ───────────────────────
    pub fn add_provider(env: Env, dao: Address, provider: Address) {
        Self::_require_dao(&env, &dao);
        let count: u32 = env.storage().instance().get(&PROVIDER_COUNT).unwrap_or(0);
        assert!(count < MAX_PROVIDERS, "max providers reached");
        assert!(
            !env.storage().persistent().has(&DataKey::Provider(provider.clone())),
            "already a provider"
        );
        env.storage().persistent().set(&DataKey::Provider(provider.clone()), &true);
        env.storage().persistent().set(&DataKey::ProviderList(count), &provider);
        env.storage().instance().set(&PROVIDER_COUNT, &(count + 1));
        ProviderEvent {
            action: symbol_short!("add"),
            provider,
        }.publish(&env);
    }

    pub fn remove_provider(env: Env, dao: Address, provider: Address) {
        Self::_require_dao(&env, &dao);
        assert!(
            env.storage().persistent().has(&DataKey::Provider(provider.clone())),
            "not a provider"
        );
        env.storage().persistent().remove(&DataKey::Provider(provider.clone()));
        ProviderEvent {
            action: symbol_short!("rem"),
            provider,
        }.publish(&env);
    }

    // ── Submit price observation ──────────────────────────────────
    pub fn submit_price(env: Env, provider: Address, asset: Symbol, price: i128) {
        provider.require_auth();
        assert!(
            env.storage().persistent().has(&DataKey::Provider(provider.clone())),
            "not an authorised provider"
        );
        assert!(price > 0, "price must be positive");

        let now = env.ledger().timestamp();

        // Circuit breaker: reject > 20% move from last accepted price
        let feed_opt: Option<AssetFeed> = env.storage().persistent()
            .get(&DataKey::Feed(asset.clone()));
        if let Some(ref feed) = feed_opt {
            if feed.latest_price > 0 {
                let delta = (price - feed.latest_price).abs();
                let max_delta = feed.latest_price * CIRCUIT_BPS / PRECISION;
                assert!(delta <= max_delta, "price move exceeds circuit breaker");
            }
        }

        // Store observation
        let obs = PriceObservation { price, timestamp: now, provider: provider.clone() };
        // Append to pending observations for this asset
        let mut pending: Vec<PriceObservation> = env.storage().persistent()
            .get(&DataKey::PendingObservations(asset.clone()))
            .unwrap_or(Vec::new(&env));
        pending.push_back(obs);
        env.storage().persistent().set(&DataKey::PendingObservations(asset.clone()), &pending);

        PriceEvent {
            asset: asset.clone(),
            provider: provider.clone(),
            price,
        }.publish(&env);

        // Auto-aggregate when enough observations collected
        let count: u32 = env.storage().instance().get(&PROVIDER_COUNT).unwrap_or(0);
        if pending.len() >= count.min(MAX_PROVIDERS) / 3 + 1 {
            Self::_aggregate(&env, asset);
        }
    }

    // ── Force aggregation (anyone can call) ───────────────────────
    pub fn aggregate(env: Env, asset: Symbol) {
        Self::_aggregate(&env, asset);
    }

    // ── Compute / refresh TWAP ────────────────────────────────────
    pub fn update_twap(env: Env, asset: Symbol) -> i128 {
        let feed: AssetFeed = env.storage().persistent()
            .get(&DataKey::Feed(asset.clone()))
            .expect("no price feed for asset");

        let now = env.ledger().timestamp();
        let window: u64 = env.storage().instance().get(&TWAP_WINDOW).unwrap();

        let mut acc: TwapAccumulator = env.storage().persistent()
            .get(&DataKey::Accumulator(asset.clone()))
            .unwrap_or(TwapAccumulator {
                cumulative_price: 0,
                last_price:       feed.latest_price,
                last_timestamp:   now,
                window_start:     now,
                window_cum_start: 0,
            });

        let elapsed = now.saturating_sub(acc.last_timestamp);
        if elapsed > 0 {
            acc.cumulative_price += acc.last_price * elapsed as i128;
        }
        acc.last_price     = feed.latest_price;
        acc.last_timestamp = now;

        // TWAP = (cumulative_now - cumulative_window_start) / window
        let window_elapsed = now.saturating_sub(acc.window_start);
        let twap = if window_elapsed >= window {
            let cum_delta = acc.cumulative_price - acc.window_cum_start;
            let twap_val  = cum_delta / window as i128;
            // Roll the window
            acc.window_start     = now;
            acc.window_cum_start = acc.cumulative_price;
            twap_val
        } else if window_elapsed > 0 {
            (acc.cumulative_price - acc.window_cum_start) / window_elapsed as i128
        } else {
            feed.latest_price
        };

        env.storage().persistent().set(&DataKey::Accumulator(asset.clone()), &acc);

        let mut new_feed = feed;
        new_feed.twap_price = twap;
        new_feed.twap_ts    = now;
        env.storage().persistent().set(&DataKey::Feed(asset.clone()), &new_feed);

        TwapEvent {
            asset,
            twap_price: twap,
        }.publish(&env);
        twap
    }

    // ── Views ─────────────────────────────────────────────────────
    pub fn get_price(env: Env, asset: Symbol) -> i128 {
        let feed: AssetFeed = env.storage().persistent()
            .get(&DataKey::Feed(asset)).expect("no feed");
        let age = env.ledger().timestamp().saturating_sub(feed.latest_ts);
        assert!(age <= MAX_AGE_SECS, "price is stale");
        feed.latest_price
    }

    pub fn get_twap(env: Env, asset: Symbol) -> i128 {
        let feed: AssetFeed = env.storage().persistent()
            .get(&DataKey::Feed(asset)).expect("no feed");
        let age = env.ledger().timestamp().saturating_sub(feed.twap_ts);
        assert!(age <= MAX_AGE_SECS * 2, "TWAP is stale");
        feed.twap_price
    }

    pub fn get_feed(env: Env, asset: Symbol) -> AssetFeed {
        env.storage().persistent()
            .get(&DataKey::Feed(asset)).expect("no feed")
    }

    pub fn is_stale(env: Env, asset: Symbol) -> bool {
        let feed: Option<AssetFeed> = env.storage().persistent()
            .get(&DataKey::Feed(asset));
        match feed {
            None => true,
            Some(f) => env.ledger().timestamp().saturating_sub(f.latest_ts) > MAX_AGE_SECS,
        }
    }

    pub fn is_provider(env: Env, addr: Address) -> bool {
        env.storage().persistent().has(&DataKey::Provider(addr))
    }

    pub fn provider_count(env: Env) -> u32 {
        env.storage().instance().get(&PROVIDER_COUNT).unwrap_or(0)
    }

    // ── Internal: aggregate observations → single price ──────────
    fn _aggregate(env: &Env, asset: Symbol) {
        let pending: Vec<PriceObservation> = env.storage().persistent()
            .get(&DataKey::PendingObservations(asset.clone()))
            .unwrap_or(Vec::new(env));

        let n = pending.len() as usize;
        assert!(n >= MIN_PROVIDERS as usize, "insufficient observations");

        // Collect prices into a fixed-size array and sort (insertion sort, no_std)
        let mut prices: Vec<i128> = Vec::new(env);
        for i in 0..n {
            prices.push_back(pending.get(i as u32).unwrap().price);
        }
        // Sort prices ascending (insertion sort)
        let mut sorted: Vec<i128> = prices.clone();
        for i in 1..sorted.len() {
            let mut j = i;
            while j > 0 {
                let a = sorted.get(j - 1).unwrap();
                let b = sorted.get(j).unwrap();
                if a > b {
                    sorted.set(j - 1, b);
                    sorted.set(j, a);
                    j -= 1;
                } else { break; }
            }
        }

        // Drop top/bottom 25% (outlier rejection)
        let trim = n / 4;
        let start = trim as u32;
        let end   = (n - trim) as u32;
        let mut sum = 0_i128;
        let mut cnt = 0_i128;
        for i in start..end {
            sum += sorted.get(i).unwrap();
            cnt += 1;
        }
        let median_price = if cnt > 0 { sum / cnt } else { sorted.get(n as u32 / 2).unwrap() };

        // Update feed
        let now  = env.ledger().timestamp();
        let feed = AssetFeed {
            latest_price:      median_price,
            latest_ts:         now,
            twap_price:        median_price, // will be updated by update_twap
            twap_ts:           now,
            observation_count: n as u32,
        };
        env.storage().persistent().set(&DataKey::Feed(asset.clone()), &feed);

        // Clear pending observations
        let empty: Vec<PriceObservation> = Vec::new(env);
        env.storage().persistent().set(&DataKey::PendingObservations(asset.clone()), &empty);

        PriceAggEvent {
            asset,
            median_price,
        }.publish(&env);
    }

    fn _require_dao(env: &Env, caller: &Address) {
        let dao: Address = env.storage().instance().get(&DAO).unwrap();
        assert!(*caller == dao, "DAO only");
        caller.require_auth();
    }
}