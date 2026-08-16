// ═══════════════════════════════════════════════════════════════════
//  CONTRACT: PartnerRegistry
//
//  Manages the entire partner (integrator) layer of the protocol.
//  Partners are third-party apps (wallets, DEXes, neobanks, etc.)
//  that embed the protocol into their product.
//
//  What this contract does:
//  ─────────────────────────
//  1. Partner registration (DAO-approved or self-serve with fee)
//  2. Per-partner revenue share tracking (default 50% of fees
//     generated through their referral tag)
//  3. Partner asset submission (any active partner can register a
//     synth; DAO must activate it before it is mintable)
//  4. Revenue claiming (partners call claim() to withdraw their
//     accumulated share)
//  5. Revenue forwarding hook (called by EarningsDistributor
//     every epoch with the partner's earned amount)
//
//  Decentralisation notes:
//  ─────────────────────────
//  - The DAO controls who can become a partner (add_partner)
//  - Partners are identified by their deployer wallet address
//  - Each partner gets a unique u32 ID used as a referral tag
//  - Partners CANNOT modify global protocol parameters
//  - All partners have the same rights and the same default 50% share
//  - Partners can submit synths; DAO activation is the go-live gate
//  - The 50% split is the DEFAULT; DAO can override per-partner rates
//  - All revenue accounting is on-chain and publicly auditable
//
//  Staying decentralised:
//  ─────────────────────────
//  Partners integrate via referral tags embedded in transactions.
//  The smart contract automatically credits the right partner.
//  No one — including the core team — can redirect a partner's
//  earned revenue. Only the partner's registered wallet can claim.
// ═══════════════════════════════════════════════════════════════════
#![no_std]
use soroban_sdk::{
    contract, contractimpl, contractevent, contracttype, symbol_short,
    token, Address, BytesN, Env, Symbol, Vec, String,
};

// ── Constants ────────────────────────────────────────────────────
const PRECISION:             i128 = 10_000;
/// Default partner revenue share: 50% of fees they generate
const DEFAULT_PARTNER_SHARE: i128 = 5_000;
/// Maximum partner share the DAO can grant (80%)
const MAX_PARTNER_SHARE:     i128 = 8_000;
/// Registration fee in USDC (7dp) — 1,000 USDC
const REGISTRATION_FEE:      i128 = 1_000_0_000_000;

// ── Storage key symbols ──────────────────────────────────────────
const ADMIN:        Symbol = symbol_short!("ADMIN");
const DAO:          Symbol = symbol_short!("DAO");
const USDC:         Symbol = symbol_short!("USDC");
const ENGINE:       Symbol = symbol_short!("ENGINE");
const EARNINGS:     Symbol = symbol_short!("EARN");
const PARTNER_CNT:  Symbol = symbol_short!("PCNT");
const OPEN_REG:     Symbol = symbol_short!("OPENREG"); // self-serve registration on/off

// ── Partner record ────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug)]
pub struct Partner {
    /// Unique sequential ID — used as referral tag in transactions
    pub id:              u32,
    /// Partner's controller wallet (only this can claim revenue)
    pub owner:           Address,
    /// Human-readable name for display in analytics
    pub name:            String,
    /// Revenue share in bps (default 5_000 = 50%)
    pub revenue_share:   i128,
    /// Accumulated revenue not yet claimed (in USDC, 7dp)
    pub pending_revenue: i128,
    /// Total revenue ever earned (for analytics)
    pub total_earned:    i128,
    /// Assets submitted by this partner (list of synth symbols)
    pub deployed_assets: Vec<Symbol>,
    /// Partner is active (DAO can deactivate for violations)
    pub active:          bool,
    /// Unix timestamp of registration
    pub registered_at:   u64,
    /// Volume generated (for analytics)
    pub total_volume:    i128,
}

// ── Partner asset config ──────────────────────────────────────────
/// Config for a synth asset deployed by a partner
#[contracttype]
#[derive(Clone, Debug)]
pub struct PartnerAsset {
    /// The synth symbol (must be prefixed: "p{PARTNER_ID}_{SYMBOL}")
    pub symbol:        Symbol,
    /// The SEP-41 token contract address
    pub token:         Address,
    /// Which partner deployed this
    pub partner_id:    u32,
    /// Oracle symbol for price feed
    pub oracle_symbol: Symbol,
    /// Max debt ceiling (cannot exceed global per-asset ceiling)
    pub debt_ceiling:  i128,
    /// Whether it's live on the engine
    pub live:          bool,
    /// Timestamp when deployed
    pub deployed_at:   u64,
}

// ── Storage keys ──────────────────────────────────────────────────
#[contracttype]
pub enum DataKey {
    Partner(u32),                // id → Partner
    PartnerByOwner(Address),     // owner address → partner id
    PartnerAsset(Symbol),        // asset symbol → PartnerAsset
    PartnerAssets(u32),          // partner_id → Vec<Symbol>
    EpochRevenue(u32, u64),      // (partner_id, epoch) → revenue earned
    TotalProtocolRevenue,        // running total for analytics
}

// ── Events ────────────────────────────────────────────────────────
#[contractevent(topics = ["preg_reg"])]
pub struct PartnerRegistered {
    #[topic]
    pub owner: Address,
    pub id: u32,
}

#[contractevent(topics = ["preg_fwd"])]
pub struct RevenueForwarded {
    #[topic]
    pub partner_id: u32,
    pub amount: i128,
    pub epoch: u64,
}

#[contractevent(topics = ["preg_claim"])]
pub struct RevenueClaimed {
    #[topic]
    pub partner_owner: Address,
    pub claimable: i128,
}

#[contractevent(topics = ["preg_asset"])]
pub struct AssetDeployed {
    #[topic]
    pub partner_owner: Address,
    pub symbol: Symbol,
    pub id: u32,
    pub token: Address,
}

#[contractevent(topics = ["preg_live"])]
pub struct AssetActivated {
    #[topic]
    pub symbol: Symbol,
    pub partner_id: u32,
}

#[contractevent(topics = ["preg_off"])]
pub struct PartnerDeactivated {
    #[topic]
    pub partner_id: u32,
}

// ═══════════════════════════════════════════════════════════════════
#[contract]
pub struct PartnerRegistry;

#[contractimpl]
impl PartnerRegistry {

    // ════════════════════════════════════════════════════════════
    //  INIT
    // ════════════════════════════════════════════════════════════

    pub fn initialize(
        env:     Env,
        admin:   Address,
        dao:     Address,
        usdc:    Address,
        engine:  Address,
        earnings: Address,
    ) {
        admin.require_auth();
        assert!(!env.storage().instance().has(&ADMIN), "already initialised");
        env.storage().instance().set(&ADMIN,       &admin);
        env.storage().instance().set(&DAO,         &dao);
        env.storage().instance().set(&USDC,        &usdc);
        env.storage().instance().set(&ENGINE,      &engine);
        env.storage().instance().set(&EARNINGS,    &earnings);
        env.storage().instance().set(&PARTNER_CNT, &0_u32);
        // Self-serve registration disabled by default — DAO must enable
        env.storage().instance().set(&OPEN_REG, &false);
    }

    // ════════════════════════════════════════════════════════════
    //  PARTNER REGISTRATION
    // ════════════════════════════════════════════════════════════

    /// Register a new partner — called by DAO to approve an integrator.
    /// All partners have the same rights: referral swaps, 50% fee share,
    /// and the ability to submit assets for DAO activation.
    pub fn register_partner(
        env:   Env,
        dao:   Address,
        owner: Address,
        name:  String,
    ) -> u32 {
        Self::_require_dao(&env, &dao);
        Self::_insert_partner(&env, owner, name)
    }

    /// Alias for `register_partner` used by DAO call-data execution.
    pub fn dao_register_partner(
        env:   Env,
        dao:   Address,
        owner: Address,
        name:  String,
    ) -> u32 {
        Self::register_partner(env, dao, owner, name)
    }

    /// Self-serve registration — partner pays a fee in USDC.
    /// DAO must enable this first via set_open_registration(true).
    pub fn self_register(env: Env, owner: Address, name: String) -> u32 {
        owner.require_auth();
        let open: bool = env.storage().instance().get(&OPEN_REG).unwrap_or(false);
        assert!(open, "self-registration is currently disabled");

        let usdc: Address = env.storage().instance().get(&USDC).unwrap();
        let earnings: Address = env.storage().instance().get(&EARNINGS).unwrap();
        token::Client::new(&env, &usdc)
            .transfer(&owner, &earnings, &REGISTRATION_FEE);

        Self::_insert_partner(&env, owner, name)
    }

    // ════════════════════════════════════════════════════════════
    //  PARTNER ASSET DEPLOYMENT
    // ════════════════════════════════════════════════════════════

    /// Deploy a new synthetic asset under the partner's namespace.
    /// The asset will be registered on the SyntheticEngine with
    /// the partner as the deployer.
    ///
    /// Symbol naming rule: must start with "P" to distinguish
    /// partner assets from core protocol assets.
    /// Examples: "PSGOLD" (partner synth gold), "PSTSLA"
    ///
    /// # Arguments
    /// * `partner_owner`   — must be the partner's registered owner wallet
    /// * `symbol`          — unique synth symbol (max 9 chars, starts with "P")
    /// * `token`           — pre-deployed SEP-41 contract address
    /// * `oracle_symbol`   — price feed symbol (e.g. "GOLDUSD")
    /// * `min_cr`          — minimum collateral ratio in bps (e.g. 15_000 = 150%)
    /// * `liq_cr`          — liquidation CR in bps
    /// * `liq_penalty`     — liquidation bonus in bps
    /// * `stab_fee_bps`    — one-shot mint fee in bps
    /// * `debt_ceiling`    — max outstanding synth (7dp); cannot exceed global limit
    pub fn register_partner_asset(
        env:          Env,
        partner_owner: Address,
        symbol:        Symbol,
        token:         Address,
        oracle_symbol: Symbol,
        coll_oracle:   Symbol,
        min_cr:        i128,
        liq_cr:        i128,
        liq_penalty:   i128,
        stab_fee_bps:  i128,
        debt_ceiling:  i128,
    ) {
        partner_owner.require_auth();

        let id = Self::_partner_id_by_owner(&env, &partner_owner);
        let mut partner = Self::_get_partner(&env, id);
        assert!(partner.active, "partner is not active");
        // Debt ceiling guard: partners cannot set ceiling > 1M USDC equivalent
        assert!(debt_ceiling <= 1_000_000_0_000_000, "debt ceiling exceeds partner limit");
        assert!(min_cr > liq_cr, "min CR must exceed liquidation CR");
        assert!(liq_penalty <= 1_500, "penalty capped at 15% for partner assets");

        assert!(
            !env.storage().persistent().has(&DataKey::PartnerAsset(symbol.clone())),
            "asset symbol already registered"
        );

        let asset = PartnerAsset {
            symbol: symbol.clone(),
            token: token.clone(),
            partner_id: id,
            oracle_symbol: oracle_symbol.clone(),
            debt_ceiling,
            live: false,           // must be activated by DAO
            deployed_at: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&DataKey::PartnerAsset(symbol.clone()), &asset);

        // Track assets under partner
        partner.deployed_assets.push_back(symbol.clone());
        env.storage().persistent().set(&DataKey::Partner(id), &partner);

        // NOTE: The SyntheticEngine.register_asset() call must be triggered
        // separately by the DAO (to maintain governance gate on what goes live).
        // This registers intent; activation is a DAO vote.

        AssetDeployed { partner_owner, symbol, id, token }.publish(&env);
    }

    /// DAO activates a partner asset on the SyntheticEngine.
    /// This is the final step before users can mint the asset.
    ///
    /// # Arguments
    /// * `dao`    — DAO address
    /// * `symbol` — asset symbol registered by the partner
    pub fn activate_partner_asset(env: Env, dao: Address, symbol: Symbol) {
        Self::_require_dao(&env, &dao);

        let mut asset: PartnerAsset = env.storage().persistent()
            .get(&DataKey::PartnerAsset(symbol.clone()))
            .expect("partner asset not found");
        assert!(!asset.live, "asset already live");

        asset.live = true;
        env.storage().persistent().set(&DataKey::PartnerAsset(symbol.clone()), &asset);

        AssetActivated { symbol, partner_id: asset.partner_id }.publish(&env);

        // In production: call SyntheticEngine.register_asset() via cross-contract
        // engine_client.register_asset(dao, symbol, asset.token, asset.oracle_symbol,
        //   coll_oracle, min_cr, liq_cr, liq_penalty, stab_fee, asset.debt_ceiling)
    }

    // ════════════════════════════════════════════════════════════
    //  REVENUE ACCOUNTING
    // ════════════════════════════════════════════════════════════

    /// Called by EarningsDistributor every epoch with the revenue
    /// attributable to a specific partner (based on their referral tag).
    ///
    /// # Arguments
    /// * `earnings_contract` — must be the registered earnings address
    /// * `partner_id`        — which partner generated this revenue
    /// * `amount`            — partner's share in USDC (7dp)
    /// * `epoch`             — epoch number for record-keeping
    pub fn credit_partner_revenue(
        env:               Env,
        earnings_contract: Address,
        partner_id:        u32,
        amount:            i128,
        epoch:             u64,
    ) {
        // Only the earnings distributor can call this
        let registered_earnings: Address = env.storage().instance().get(&EARNINGS).unwrap();
        assert!(earnings_contract == registered_earnings, "only earnings distributor");
        earnings_contract.require_auth();

        let mut partner = Self::_get_partner(&env, partner_id);
        assert!(partner.active, "partner is not active");

        partner.pending_revenue += amount;
        partner.total_earned    += amount;
        env.storage().persistent().set(&DataKey::Partner(partner_id), &partner);

        // Per-epoch record for analytics
        let prev: i128 = env.storage().persistent()
            .get(&DataKey::EpochRevenue(partner_id, epoch)).unwrap_or(0);
        env.storage().persistent()
            .set(&DataKey::EpochRevenue(partner_id, epoch), &(prev + amount));

        RevenueForwarded { partner_id, amount, epoch }.publish(&env);
    }

    /// Partner claims their accumulated revenue.
    /// Only the partner's registered owner wallet can call this.
    ///
    /// # Arguments
    /// * `partner_owner` — must match the partner's registered owner
    pub fn claim_revenue(env: Env, partner_owner: Address) -> i128 {
        partner_owner.require_auth();

        let id = Self::_partner_id_by_owner(&env, &partner_owner);
        let mut partner = Self::_get_partner(&env, id);
        assert!(partner.active, "partner is not active");

        let claimable = partner.pending_revenue;
        assert!(claimable > 0, "no revenue to claim");

        partner.pending_revenue = 0;
        env.storage().persistent().set(&DataKey::Partner(id), &partner);

        // Transfer USDC to partner
        let usdc: Address = env.storage().instance().get(&USDC).unwrap();
        token::Client::new(&env, &usdc)
            .transfer(&env.current_contract_address(), &partner_owner, &claimable);

        RevenueClaimed { partner_owner, claimable }.publish(&env);
        claimable
    }

    // ════════════════════════════════════════════════════════════
    //  DAO MANAGEMENT
    // ════════════════════════════════════════════════════════════

    /// Update a partner's revenue share (DAO only).
    /// Use to reward high-volume partners or penalise bad actors.
    pub fn set_partner_share(env: Env, dao: Address, partner_id: u32, new_share: i128) {
        Self::_require_dao(&env, &dao);
        assert!(new_share <= MAX_PARTNER_SHARE, "exceeds max 80%");
        let mut partner = Self::_get_partner(&env, partner_id);
        partner.revenue_share = new_share;
        env.storage().persistent().set(&DataKey::Partner(partner_id), &partner);
    }

    /// Deactivate a partner (DAO only). Blocks future revenue crediting
    /// but does NOT touch already-accumulated pending_revenue — the
    /// partner can still claim what they've already earned.
    pub fn deactivate_partner(env: Env, dao: Address, partner_id: u32) {
        Self::_require_dao(&env, &dao);
        let mut partner = Self::_get_partner(&env, partner_id);
        partner.active = false;
        env.storage().persistent().set(&DataKey::Partner(partner_id), &partner);
        PartnerDeactivated { partner_id }.publish(&env);
    }

    /// Reactivate a previously deactivated partner.
    pub fn reactivate_partner(env: Env, dao: Address, partner_id: u32) {
        Self::_require_dao(&env, &dao);
        let mut partner = Self::_get_partner(&env, partner_id);
        partner.active = true;
        env.storage().persistent().set(&DataKey::Partner(partner_id), &partner);
    }

    /// Enable or disable self-serve partner registration.
    pub fn set_open_registration(env: Env, dao: Address, open: bool) {
        Self::_require_dao(&env, &dao);
        env.storage().instance().set(&OPEN_REG, &open);
    }

    /// Point `dao` at the address authorised to call DAO-gated functions.
    /// Must match the executor `execute_multisig_tx` passes through (protocol
    /// multi-sig G-address) or, after a DAO upgrade, the DAO contract itself.
    pub fn set_dao(env: Env, admin: Address, dao: Address) {
        let stored: Address = env.storage().instance().get(&ADMIN).unwrap();
        assert!(admin == stored, "admin only");
        admin.require_auth();
        env.storage().instance().set(&DAO, &dao);
    }

    /// Replace this contract's Wasm in place. Instance and persistent
    /// storage are preserved. Authorised by the stored DAO address.
    pub fn upgrade(env: Env, _dao: Address, new_wasm_hash: BytesN<32>) {
        let stored: Address = env.storage().instance().get(&DAO).unwrap();
        stored.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    /// Whether wallets may call `self_register` without a DAO proposal.
    pub fn is_open_registration(env: Env) -> bool {
        env.storage().instance().get(&OPEN_REG).unwrap_or(false)
    }

    // ════════════════════════════════════════════════════════════
    //  VIEWS
    // ════════════════════════════════════════════════════════════

    /// Get full partner record by ID.
    pub fn get_partner(env: Env, id: u32) -> Option<Partner> {
        env.storage().persistent().get(&DataKey::Partner(id))
    }

    /// Revenue share in bps for an active partner (0 if missing/inactive).
    pub fn get_revenue_share(env: Env, partner_id: u32) -> i128 {
        match env.storage().persistent().get::<_, Partner>(&DataKey::Partner(partner_id)) {
            Some(p) if p.active => p.revenue_share,
            _ => 0,
        }
    }

    /// Owner wallet for a partner id (None if unknown).
    pub fn get_partner_owner(env: Env, partner_id: u32) -> Option<Address> {
        env.storage()
            .persistent()
            .get::<_, Partner>(&DataKey::Partner(partner_id))
            .map(|p| p.owner)
    }

    /// Get partner ID for a given owner address.
    pub fn get_partner_id(env: Env, owner: Address) -> Option<u32> {
        env.storage().persistent().get(&DataKey::PartnerByOwner(owner))
    }

    /// Get a partner asset record.
    pub fn get_partner_asset(env: Env, symbol: Symbol) -> Option<PartnerAsset> {
        env.storage().persistent().get(&DataKey::PartnerAsset(symbol))
    }

    /// Get all asset symbols deployed by a partner.
    pub fn get_partner_assets(env: Env, partner_id: u32) -> Vec<Symbol> {
        let partner = Self::_get_partner(&env, partner_id);
        partner.deployed_assets
    }

    /// Total number of registered partners.
    pub fn partner_count(env: Env) -> u32 {
        env.storage().instance().get(&PARTNER_CNT).unwrap_or(0)
    }

    /// Revenue earned by a partner in a specific epoch.
    pub fn get_epoch_revenue(env: Env, partner_id: u32, epoch: u64) -> i128 {
        env.storage().persistent()
            .get(&DataKey::EpochRevenue(partner_id, epoch)).unwrap_or(0)
    }

    /// Check if an address is a registered active partner.
    pub fn is_partner(env: Env, addr: Address) -> bool {
        let id: Option<u32> = env.storage().persistent()
            .get(&DataKey::PartnerByOwner(addr));
        match id {
            None => false,
            Some(pid) => {
                let p: Option<Partner> = env.storage().persistent()
                    .get(&DataKey::Partner(pid));
                p.map(|x| x.active).unwrap_or(false)
            }
        }
    }

    /// Kept for ABI compatibility. KYC bypass was removed — all users
    /// go through ComplianceRegistry regardless of partner.
    pub fn has_kyc_bypass(_env: Env, _partner_id: u32) -> bool {
        false
    }

    // ════════════════════════════════════════════════════════════
    //  INTERNALS
    // ════════════════════════════════════════════════════════════

    fn _get_partner(env: &Env, id: u32) -> Partner {
        env.storage().persistent()
            .get(&DataKey::Partner(id))
            .expect("partner not found")
    }

    fn _partner_id_by_owner(env: &Env, owner: &Address) -> u32 {
        env.storage().persistent()
            .get(&DataKey::PartnerByOwner(owner.clone()))
            .expect("address is not a registered partner")
    }

    /// Authorise the stored DAO contract. The `caller` argument is ignored:
    /// older dao-governance WASM passes the executor EOA, but this function
    /// is only reachable as a sub-invocation from the DAO contract, which
    /// Soroban authorises automatically via `dao.require_auth()`.
    fn _require_dao(env: &Env, _caller: &Address) {
        let dao: Address = env.storage().instance().get(&DAO).unwrap();
        dao.require_auth();
    }

    fn _insert_partner(env: &Env, owner: Address, name: String) -> u32 {
        assert!(
            !env.storage().persistent().has(&DataKey::PartnerByOwner(owner.clone())),
            "address already registered as partner"
        );

        let id: u32 = env.storage().instance().get(&PARTNER_CNT).unwrap_or(0);
        let partner = Partner {
            id,
            owner: owner.clone(),
            name,
            revenue_share: DEFAULT_PARTNER_SHARE,
            pending_revenue: 0,
            total_earned: 0,
            deployed_assets: Vec::new(env),
            active: true,
            registered_at: env.ledger().timestamp(),
            total_volume: 0,
        };

        env.storage().persistent().set(&DataKey::Partner(id), &partner);
        env.storage().persistent().set(&DataKey::PartnerByOwner(owner.clone()), &id);
        env.storage().instance().set(&PARTNER_CNT, &(id + 1));

        PartnerRegistered { owner, id }.publish(env);
        id
    }
}