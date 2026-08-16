// ═══════════════════════════════════════════════════════════════════
//  CONTRACT: EarningsDistributor (UPDATED — partner-aware)
//
//  Changes from v1:
//  ─────────────────
//  1. Every receive_revenue() call now accepts an optional partner_id
//     (encoded in the source symbol or as a separate field).
//  2. When a partner tag is present, the partner's share (default 50%)
//     is deducted FIRST from the total fee, then the remaining 50%
//     is split among protocol participants using the existing ratios.
//  3. The PartnerRegistry contract is called to credit the partner.
//  4. Base splits are re-scaled to apply to the REMAINING 50%:
//       Holders     35% of 50% = 17.5% of total fee
//       LPs         30% of 50% = 15.0%
//       StabPool    20% of 50% = 10.0%
//       Treasury    10% of 50% =  5.0%
//       Dev          5% of 50% =  2.5%
//  5. Untagged revenue (no partner) uses the original full splits.
//
//  Revenue flow with partner tag:
//  ───────────────────────────────
//  Total fee = 100
//  Partner share (50%) → credited to PartnerRegistry → partner claims
//  Protocol share (50%) → split as before:
//    35% → Stability Pool depositors (pro-rata by USDC deposit)
//    30% → LPs
//    20% → Stability pool (protocol-owned reserve top-up)
//    10% → Treasury
//     5% → Dev fund
//
//  NOTE: there is no GOV token in this protocol. The "35% holder"
//  bucket from earlier designs is now a Stability Pool *depositor*
//  bucket instead: each claimer's share is their `get_compounded_deposit()`
//  pro-rata against `get_total_deposits()`, both snapshotted at epoch
//  finalisation. This reuses the existing Stability Pool accounting
//  rather than requiring a separate staking/governance token.
// ═══════════════════════════════════════════════════════════════════
#![no_std]
use soroban_sdk::{
    contract, contractimpl, contractevent, contracttype, symbol_short,
    token, Address, BytesN, Env, Symbol, Vec,
};
use shared_interfaces::{PartnerRegistryClient, StabilityPoolClient};

// ── Splits (applied to the protocol's portion after partner cut) ──
const PRECISION:      i128 = 10_000;
const EPOCH_DURATION: u64  = 604_800; // 7 days

// These apply to (total_fee - partner_share):
const HOLDER_SHARE:   i128 = 3_500;   // 35%
const LP_SHARE:       i128 = 3_000;   // 30%
const STAB_SHARE:     i128 = 2_000;   // 20%
const TREASURY_SHARE: i128 = 1_000;   // 10%
const DEV_SHARE:      i128 =   500;   //  5%

// Default partner share when a partner tag is present:
const DEFAULT_PARTNER_SHARE: i128 = 5_000; // 50%

const ADMIN:        Symbol = symbol_short!("ADMIN");
const TREASURY:     Symbol = symbol_short!("TREAS");
const DEV_FUND:     Symbol = symbol_short!("DEV");
const STAB_POOL:    Symbol = symbol_short!("STABPOOL");
const DAO:          Symbol = symbol_short!("DAO");
const PARTNER_REG:  Symbol = symbol_short!("PREG");  // ← NEW
const CUR_EPOCH:    Symbol = symbol_short!("EPOCH");

// ── Types ─────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug)]
pub struct EpochData {
    pub epoch_id:             u64,
    pub start_time:           u64,
    pub end_time:             u64,
    pub total_revenue:        i128,
    pub partner_revenue:      i128,   // ← NEW: how much went to partners
    pub protocol_revenue:     i128,   // ← NEW: what remained for protocol
    /// Total Stability Pool USDC deposits at epoch-finalise time — the
    /// denominator used to pro-rate each claimer's "staker" bucket share.
    pub staker_snapshot:      i128,
    pub per_token_revenue:    i128,
    pub finalized:            bool,
}

#[contracttype]
pub enum DataKey {
    Epoch(u64),
    EpochRevenue(u64, Address),      // (epoch, token) → amount
    EpochTokenList(u64),
    EpochPartnerRevenue(u64, u32),   // (epoch, partner_id) → amount ← NEW
    Claimed(Address, u64),
    LpContract(u32),
    LpCount,
}

// ── Events ────────────────────────────────────────────────────────
#[contractevent]
pub struct RevenueReceived {
    #[topic]
    pub sender: Address,
    #[topic]
    pub source: Symbol,
    pub token: Address,
    pub amount: i128,
    pub epoch: u64,
}

#[contractevent]
pub struct EpochClosed {
    pub epoch: u64,
    pub total_revenue: i128,
}

#[contractevent]
pub struct EpochOpened {
    pub epoch: u64,
}

#[contractevent]
pub struct RevenueClaimed {
    #[topic]
    pub claimer: Address,
    pub total: i128,
}

#[contractevent]
pub struct PartnerRevenueForwarded {
    #[topic]
    pub partner_id: u32,
    #[topic]
    pub epoch: u64,
    pub partner_cut: i128,
    pub source: Symbol,
}

#[contract]
pub struct EarningsDistributor;

#[contractimpl]
impl EarningsDistributor {

    // ── Init ──────────────────────────────────────────────────────
    pub fn initialize(
        env:          Env,
        admin:        Address,
        treasury:     Address,
        dev_fund:     Address,
        stab_pool:    Address,
        dao:          Address,
        partner_reg:  Address,   // ← NEW: PartnerRegistry address
    ) {
        admin.require_auth();
        assert!(!env.storage().instance().has(&ADMIN), "already initialised");
        env.storage().instance().set(&ADMIN,       &admin);
        env.storage().instance().set(&TREASURY,    &treasury);
        env.storage().instance().set(&DEV_FUND,    &dev_fund);
        env.storage().instance().set(&STAB_POOL,   &stab_pool);
        env.storage().instance().set(&DAO,         &dao);
        env.storage().instance().set(&PARTNER_REG, &partner_reg);
        env.storage().instance().set(&DataKey::LpCount, &0_u32);

        let now = env.ledger().timestamp();
        env.storage().instance().set(&CUR_EPOCH, &0_u64);
        env.storage().persistent().set(&DataKey::Epoch(0), &EpochData {
            epoch_id: 0, start_time: now, end_time: now + EPOCH_DURATION,
            total_revenue: 0, partner_revenue: 0, protocol_revenue: 0,
            staker_snapshot: 0, per_token_revenue: 0, finalized: false,
        });
    }

    // ── Register LP contract ──────────────────────────────────────
    pub fn register_lp(env: Env, admin: Address, lp: Address) {
        Self::_require_admin(&env, &admin);
        let count: u32 = env.storage().instance().get(&DataKey::LpCount).unwrap_or(0);
        env.storage().persistent().set(&DataKey::LpContract(count), &lp);
        env.storage().instance().set(&DataKey::LpCount, &(count + 1));
    }

    // ════════════════════════════════════════════════════════════
    //  CORE: RECEIVE REVENUE (UPDATED)
    // ════════════════════════════════════════════════════════════

    /// Receive protocol revenue, optionally tagged with a partner ID.
    ///
    /// # Arguments
    /// * `sender`     — the contract sending fees (engine, LP, etc.)
    /// * `token`      — the fee token address (usually USDC)
    /// * `amount`     — total fee amount in 7dp units
    /// * `source`     — fee type: "stabfee" | "swapfee" | "liqfee"
    /// * `partner_id` — optional partner referral tag; pass 0 for none.
    ///                  When > 0, the partner's registered share is
    ///                  deducted before the protocol split.
    pub fn receive_revenue(
        env:        Env,
        sender:     Address,
        token:      Address,
        amount:     i128,
        source:     Symbol,
        partner_id: u32,     // ← NEW: 0 = no partner, >0 = tagged
    ) {
        sender.require_auth();
        assert!(amount > 0, "amount must be positive");

        // Pull tokens from sender
        token::Client::new(&env, &token)
            .transfer(&sender, &env.current_contract_address(), &amount);

        let epoch: u64 = env.storage().instance().get(&CUR_EPOCH).unwrap_or(0);
        let mut ep: EpochData = env.storage().persistent()
            .get(&DataKey::Epoch(epoch)).unwrap();

        let protocol_amount: i128;

        // ── Partner revenue routing ───────────────────────────────
        if partner_id > 0 {
            // Fetch partner's configured revenue share from PartnerRegistry
            let partner_share = Self::_get_partner_share(&env, partner_id);
            let partner_cut   = amount * partner_share / PRECISION;
            protocol_amount   = amount - partner_cut;

            // Escrow tokens at PartnerRegistry + credit pending claim balance
            if partner_cut > 0 {
                let preg_addr: Address = env.storage().instance().get(&PARTNER_REG).unwrap();
                token::Client::new(&env, &token)
                    .transfer(&env.current_contract_address(), &preg_addr, &partner_cut);
                PartnerRegistryClient::new(&env, &preg_addr).credit_partner_revenue(
                    &env.current_contract_address(),
                    &partner_id,
                    &partner_cut,
                    &epoch,
                );

                // Track per-epoch partner revenue
                let prev: i128 = env.storage().persistent()
                    .get(&DataKey::EpochPartnerRevenue(epoch, partner_id)).unwrap_or(0);
                env.storage().persistent()
                    .set(&DataKey::EpochPartnerRevenue(epoch, partner_id), &(prev + partner_cut));

                ep.partner_revenue += partner_cut;
                PartnerRevenueForwarded { 
                    partner_id, 
                    epoch, 
                    partner_cut, 
                    source: source.clone() 
                }.publish(&env);
            }
        } else {
            // No partner tag — full amount goes to protocol
            protocol_amount = amount;
        }

        // ── Accumulate protocol portion for epoch finalisation ────
        ep.total_revenue    += amount;
        ep.protocol_revenue += protocol_amount;

        // Track per-token amounts for the distribution step
        let prev: i128 = env.storage().persistent()
            .get(&DataKey::EpochRevenue(epoch, token.clone())).unwrap_or(0);
        // Store only the protocol portion (partner's cut already sent)
        env.storage().persistent()
            .set(&DataKey::EpochRevenue(epoch, token.clone()), &(prev + protocol_amount));

        // De-duplicate token list for epoch
        let mut tok_list: Vec<Address> = env.storage().persistent()
            .get(&DataKey::EpochTokenList(epoch))
            .unwrap_or(Vec::new(&env));
        let mut exists = false;
        for i in 0..tok_list.len() {
            if tok_list.get(i).unwrap() == token { exists = true; break; }
        }
        if !exists { tok_list.push_back(token.clone()); }
        env.storage().persistent().set(&DataKey::EpochTokenList(epoch), &tok_list);
        env.storage().persistent().set(&DataKey::Epoch(epoch), &ep);

        RevenueReceived { sender, source, token, amount, epoch }.publish(&env);

        // Auto-close epoch if window has passed
        if env.ledger().timestamp() >= ep.end_time {
            Self::_finalize_epoch(&env, epoch);
        }
    }

    // ── Force epoch close (permissionless after deadline) ─────────
    pub fn finalize_epoch(env: Env) -> u64 {
        let epoch: u64 = env.storage().instance().get(&CUR_EPOCH).unwrap_or(0);
        let ep: EpochData = env.storage().persistent().get(&DataKey::Epoch(epoch)).unwrap();
        assert!(env.ledger().timestamp() >= ep.end_time, "epoch not yet complete");
        assert!(!ep.finalized, "epoch already finalized");
        Self::_finalize_epoch(&env, epoch);
        epoch
    }

    // ── Stability Pool depositor revenue claim ─────────────────────
    /// Claims a claimer's pro-rata share of the "35% staker" bucket for
    /// each finalized epoch listed, based on how much USDC they had
    /// deposited in the Stability Pool at the time each epoch closed
    /// (approximated by their *current* compounded deposit — deposits are
    /// generally stable/long-lived, so this is an acceptable trade-off vs.
    /// storing a full historical snapshot per depositor per epoch).
    pub fn claim(env: Env, claimer: Address, epochs: Vec<u64>) -> i128 {
        claimer.require_auth();
        let stab_pool: Address = env.storage().instance().get(&STAB_POOL).unwrap();
        let deposit = StabilityPoolClient::new(&env, &stab_pool)
            .get_compounded_deposit(&claimer);
        assert!(deposit > 0, "must have a Stability Pool deposit to claim");

        let mut total = 0_i128;
        for i in 0..epochs.len() {
            let epoch_id = epochs.get(i).unwrap();
            let already: bool = env.storage().persistent()
                .get(&DataKey::Claimed(claimer.clone(), epoch_id)).unwrap_or(false);
            if already { continue; }

            let ep: EpochData = match env.storage().persistent().get(&DataKey::Epoch(epoch_id)) {
                Some(e) => e, None => continue,
            };
            if !ep.finalized || ep.staker_snapshot == 0 { continue; }

            // Staker share is computed on the PROTOCOL portion only
            let share = HOLDER_SHARE * ep.protocol_revenue * deposit
                / (ep.staker_snapshot * PRECISION);
            if share > 0 {
                total += share;
                env.storage().persistent()
                    .set(&DataKey::Claimed(claimer.clone(), epoch_id), &true);
            }
        }

        if total > 0 {
            RevenueClaimed { claimer, total }.publish(&env);
        }
        total
    }

    // ── Views ─────────────────────────────────────────────────────
    pub fn get_epoch(env: Env, epoch_id: u64) -> Option<EpochData> {
        env.storage().persistent().get(&DataKey::Epoch(epoch_id))
    }

    pub fn get_current_epoch(env: Env) -> u64 {
        env.storage().instance().get(&CUR_EPOCH).unwrap_or(0)
    }

    pub fn get_epoch_revenue(env: Env, epoch_id: u64, token: Address) -> i128 {
        env.storage().persistent()
            .get(&DataKey::EpochRevenue(epoch_id, token)).unwrap_or(0)
    }

    pub fn get_partner_epoch_revenue(env: Env, epoch_id: u64, partner_id: u32) -> i128 {
        env.storage().persistent()
            .get(&DataKey::EpochPartnerRevenue(epoch_id, partner_id)).unwrap_or(0)
    }

    pub fn has_claimed(env: Env, claimer: Address, epoch_id: u64) -> bool {
        env.storage().persistent()
            .get(&DataKey::Claimed(claimer, epoch_id)).unwrap_or(false)
    }

    pub fn get_epoch_tokens(env: Env, epoch_id: u64) -> Vec<Address> {
        env.storage().persistent()
            .get(&DataKey::EpochTokenList(epoch_id))
            .unwrap_or(Vec::new(&env))
    }

    /// Replace this contract's Wasm in place. Instance and persistent
    /// storage are preserved. Authorised by the stored DAO address.
    pub fn upgrade(env: Env, _dao: Address, new_wasm_hash: BytesN<32>) {
        let stored: Address = env.storage().instance().get(&DAO).unwrap();
        stored.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    // ── Internal epoch finalisation ───────────────────────────────
    fn _finalize_epoch(env: &Env, epoch: u64) {
        let mut ep: EpochData = env.storage().persistent()
            .get(&DataKey::Epoch(epoch)).unwrap();
        let tokens: Vec<Address> = env.storage().persistent()
            .get(&DataKey::EpochTokenList(epoch))
            .unwrap_or(Vec::new(env));

        let stab: Address      = env.storage().instance().get(&STAB_POOL).unwrap();
        let staker_supply      = StabilityPoolClient::new(env, &stab).get_total_deposits();
        let treasury: Address  = env.storage().instance().get(&TREASURY).unwrap();
        let dev: Address       = env.storage().instance().get(&DEV_FUND).unwrap();

        // Distribute the PROTOCOL portion (already net of partner cuts)
        for i in 0..tokens.len() {
            let tok_addr = tokens.get(i).unwrap();
            let protocol_amt: i128 = env.storage().persistent()
                .get(&DataKey::EpochRevenue(epoch, tok_addr.clone())).unwrap_or(0);
            if protocol_amt == 0 { continue; }

            let treasury_amt = protocol_amt * TREASURY_SHARE / PRECISION;
            let dev_amt      = protocol_amt * DEV_SHARE      / PRECISION;
            let stab_amt     = protocol_amt * STAB_SHARE     / PRECISION;
            let lp_total     = protocol_amt * LP_SHARE       / PRECISION;
            // holder_amt stays in contract, claimable by Stability Pool depositors

            let tok = token::Client::new(env, &tok_addr);
            if treasury_amt > 0 { tok.transfer(&env.current_contract_address(), &treasury, &treasury_amt); }
            if dev_amt > 0      { tok.transfer(&env.current_contract_address(), &dev,      &dev_amt); }
            if stab_amt > 0     { tok.transfer(&env.current_contract_address(), &stab,     &stab_amt); }

            // Distribute LP share across registered LP contracts
            let lp_count: u32 = env.storage().instance().get(&DataKey::LpCount).unwrap_or(0);
            if lp_count > 0 {
                let per_lp = lp_total / lp_count as i128;
                for j in 0..lp_count {
                    let lp: Address = env.storage().persistent()
                        .get(&DataKey::LpContract(j)).unwrap();
                    if per_lp > 0 {
                        tok.transfer(&env.current_contract_address(), &lp, &per_lp);
                    }
                }
            }
        }

        ep.staker_snapshot = staker_supply;
        ep.finalized       = true;

        // Open next epoch
        let next_epoch = epoch + 1;
        let now        = env.ledger().timestamp();
        env.storage().persistent().set(&DataKey::Epoch(epoch), &ep);
        env.storage().persistent().set(&DataKey::Epoch(next_epoch), &EpochData {
            epoch_id: next_epoch, start_time: now, end_time: now + EPOCH_DURATION,
            total_revenue: 0, partner_revenue: 0, protocol_revenue: 0,
            staker_snapshot: 0, per_token_revenue: 0, finalized: false,
        });
        env.storage().instance().set(&CUR_EPOCH, &next_epoch);

        EpochClosed { epoch, total_revenue: ep.total_revenue }.publish(&env);
        EpochOpened { epoch: next_epoch }.publish(&env);
    }

    /// Fetch a partner's configured revenue share from PartnerRegistry.
    /// Falls back to DEFAULT_PARTNER_SHARE if unset / inactive / missing registry.
    fn _get_partner_share(env: &Env, partner_id: u32) -> i128 {
        let preg_addr: Option<Address> = env.storage().instance().get(&PARTNER_REG);
        match preg_addr {
            None => DEFAULT_PARTNER_SHARE,
            Some(addr) => {
                let share = PartnerRegistryClient::new(env, &addr).get_revenue_share(&partner_id);
                if share > 0 { share } else { DEFAULT_PARTNER_SHARE }
            }
        }
    }

    fn _require_admin(env: &Env, caller: &Address) {
        let admin: Address = env.storage().instance().get(&ADMIN).unwrap();
        assert!(*caller == admin, "admin only");
        caller.require_auth();
    }
}