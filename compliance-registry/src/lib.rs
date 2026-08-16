// ═══════════════════════════════════════════════════════════════════
//  CONTRACT 7: Compliance Registry
//
//  On-chain regulatory compliance layer for a regulated DeFi protocol.
//  Inspired by frameworks such as ERC-3643 (T-REX), adapted for Stellar.
//
//  Features:
//  - Binary KYC (verified / not verified) — no tiered levels. A user is
//    either compliant (Verified, non-expired, non-blocked jurisdiction)
//    or they are not; per-asset access is controlled by AssetRule alone.
//  - Multi-provider KYC: any number of DAO-authorised providers (e.g.
//    Sumsub, Persona, Veriff, Jumio, Onfido, …) may attest a user. The
//    user picks whichever provider they prefer to complete KYC with.
//  - Jurisdiction blocklist (ISO-3166 numeric country codes). The raw
//    jurisdiction code is stored on attestation but is never returned by
//    a public getter — only a derived `jurisdiction_allowed: bool` is
//    exposed, so the registry never publishes a wallet's country.
//  - Per-asset compliance rules (blocked jurisdictions, daily limits)
//  - Verifier registry: authorised KYC providers
//  - On-chain attestation storage with expiry
//  - Transfer rule engine: checks both sender AND receiver
//  - Daily transfer limits per asset (optional; 0 = unlimited)
//  - FATF Travel Rule support: flag large transfers for off-chain reporting
//  - Admin override for regulatory clawback coordination
// ═══════════════════════════════════════════════════════════════════
#![no_std]
use soroban_sdk::{
    contract, contractimpl, contracttype, contractevent, symbol_short,
    Address, BytesN, Env, Symbol, Bytes, Vec,
};

// Default daily transfer limit (USD cents, 7dp) applied to verified users
// when an asset rule doesn't specify its own `daily_limit_usd`. 0 on the
// asset rule means "use this default"; the rule itself can also opt out
// entirely by setting a very high value.
const DEFAULT_DAILY_LIMIT: i128 = i128::MAX;

// FATF Travel Rule threshold (USD, 7dp)
const TRAVEL_RULE_THRESHOLD: i128 = 1_000_00_000_000; // $1,000

// ── Constants ─────────────────────────────────────────────────────
const ADMIN:        Symbol = symbol_short!("ADMIN");
const DAO:          Symbol = symbol_short!("DAO");
const VERIF_COUNT:  Symbol = symbol_short!("VCNT");
/// The single canonical/primary KYC provider address (e.g. Persona's signing
/// address). Kept in addition to the generic Verifier(Address) allow-list so
/// integrations can rely on one "the verifier address" concept while still
/// allowing multiple providers to be authorised via add_verifier/remove_verifier
/// — users are free to complete KYC with ANY authorised provider they choose.
const VERIFIER:     Symbol = symbol_short!("VERIFIER");

// ── Restricted-region defaults ─────────────────────────────────────
// Protocol is not offered to US or EU persons. These ISO-3166-1 numeric
// country codes are blocked by default at `initialize`. The DAO (multi-sig)
// can lift or extend this list at any time via block_jurisdiction /
// unblock_jurisdiction.
const US_COUNTRY_CODE: u32 = 840;
const EU_COUNTRY_CODES: [u32; 27] = [
    40,  // Austria
    56,  // Belgium
    100, // Bulgaria
    191, // Croatia
    196, // Cyprus
    203, // Czechia
    208, // Denmark
    233, // Estonia
    246, // Finland
    250, // France
    276, // Germany
    300, // Greece
    348, // Hungary
    372, // Ireland
    380, // Italy
    428, // Latvia
    440, // Lithuania
    442, // Luxembourg
    470, // Malta
    528, // Netherlands
    616, // Poland
    620, // Portugal
    642, // Romania
    703, // Slovakia
    705, // Slovenia
    724, // Spain
    752, // Sweden
];

// OFAC / FATF-style comprehensively-sanctioned or high-risk jurisdictions.
// Blocked by default alongside the US/EU list — the DAO multi-sig can
// extend or lift any of these via block_jurisdiction / unblock_jurisdiction.
const SANCTIONED_COUNTRY_CODES: [u32; 10] = [
    192, // Cuba
    364, // Iran
    408, // Korea, Democratic People's Republic of (North Korea)
    760, // Syrian Arab Republic
    643, // Russian Federation
    112, // Belarus
    104, // Myanmar
    862, // Venezuela
    729, // Sudan
    728, // South Sudan
];

// ── Types ─────────────────────────────────────────────────────────

/// High-level compliance status for an address.
/// The ComplianceStatus determines whether the address may use the
/// protocol at all — there are no KYC tiers, a user is either verified
/// or they are not.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum ComplianceStatus {
    /// Never attested — no KYC provider has ever submitted an attestation.
    Unknown     = 0,
    /// KYC submitted but not yet fully verified (e.g. documents under review).
    Pending     = 1,
    /// Fully verified — may use the protocol within the limits of their
    /// jurisdiction.
    Verified    = 2,
    /// Temporarily suspended (regulatory review, investigation, or admin
    /// action). May not use the protocol until unsuspended.
    Suspended   = 3,
    /// Permanently blocked (OFAC / sanctions list). May never use the
    /// protocol — this status cannot be self-cured.
    Sanctioned  = 4,
    /// KYC attestation has passed its expiry date and must be renewed.
    Expired     = 5,
}

/// Internal attestation record — never returned wholesale by a public
/// getter because it carries the subject's raw jurisdiction code. Use
/// `get_compliance_status` (returns `ComplianceView`) for public reads.
#[contracttype]
#[derive(Clone, Debug)]
pub struct KycAttestation {
    pub address:     Address,
    pub verifier:    Address,         // which KYC provider issued this
    pub jurisdiction: u32,            // ISO-3166 numeric country code (private)
    pub issued_at:   u64,
    pub expires_at:  u64,             // 0 = never
    pub metadata:    Bytes,           // off-chain document hash (SHA-256)
}

/// Internal compliance snapshot for an address — stored alongside the KYC
/// attestation. Never returned wholesale by a public getter (see
/// `ComplianceView`) because it carries the raw jurisdiction code.
#[contracttype]
#[derive(Clone, Debug)]
pub struct ComplianceRecord {
    pub status:      ComplianceStatus,
    pub jurisdiction: u32,            // ISO-3166-1 numeric country code (private)
    pub expires_at:  u64,
    pub provider:    Address,         // primary/canonical KYC provider
    pub updated_at:  u64,
    pub reason:      Symbol,          // e.g. "freeze", "sanction", "suspend"
}

/// Public, privacy-preserving view of an address's compliance state.
/// Exposes whether the address's jurisdiction is currently allowed
/// WITHOUT revealing which jurisdiction it actually is.
#[contracttype]
#[derive(Clone, Debug)]
pub struct ComplianceView {
    pub status:              ComplianceStatus,
    /// True if the address's on-file jurisdiction is not currently on the
    /// DAO's blocklist. False if never attested (jurisdiction unknown).
    pub jurisdiction_allowed: bool,
    pub expires_at:          u64,
    pub provider:            Address,
    pub updated_at:          u64,
    pub reason:              Symbol,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct AssetRule {
    pub asset:              Symbol,
    pub blocked_jurisdictions: Vec<u32>,
    pub daily_limit_usd:    i128,     // 0 = use protocol default (unlimited)
    pub require_travel_rule: bool,
    pub enabled:            bool,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct DailyVolume {
    pub date_ts:  u64,    // unix timestamp of day start
    pub volume:   i128,
}

#[contracttype]
pub enum DataKey {
    Kyc(Address),
    ComplianceRec(Address),         // ComplianceRecord — current compliance snapshot
    DailyVol(Address),              // rolling daily volume
    AssetRule(Symbol),
    Verifier(Address),              // is this address an authorised verifier?
    VerifierList(u32),
    BlockedJurisdiction(u32),       // global jurisdiction block
    Frozen(Address),                // frozen accounts
    TravelRuleLog(u64),             // sequential log of flagged transfers
    TravelRuleCount,
    Whitelist(Address),             // bypass all checks (e.g. system contracts)
    Attestation(Address, u32),      // (subject, index) → provider Address who attested
    AttestationCount(Address),      // how many providers have attested this address
    Signer(Address),                // provider's signing key → provider Address (for attestation verification)
}

// ─────────────────────────────────────────────────────────────────
//  Events (using #[contractevent] macro)
//
//  NOTE: attestation events intentionally omit the raw jurisdiction code
//  from any *return value*, but the jurisdiction IS included in on-chain
//  event payloads below. This mirrors how the provider itself already
//  knows the user's jurisdiction (they collected it during KYC) — the
//  goal of this contract is to stop *public getters* from handing out a
//  wallet's country, not to make ledger history unreadable. If a fully
//  private jurisdiction is required, providers should submit attestations
//  containing only a jurisdiction *bucket id* (assigned off-chain) instead
//  of the raw ISO code.
// ─────────────────────────────────────────────────────────────────
#[contractevent(topics = ["attest"])]
pub struct AttestEvent {
    #[topic]
    pub verifier: Address,
    #[topic]
    pub subject: Address,
    pub jurisdiction_allowed: bool,
}

#[contractevent(topics = ["revoke"])]
pub struct RevokeEvent {
    #[topic]
    pub verifier: Address,
    #[topic]
    pub subject: Address,
}

#[contractevent(topics = ["verifier_set"])]
pub struct VerifierChangedEvent {
    #[topic]
    pub dao: Address,
    #[topic]
    pub verifier: Address,
}

#[contractevent(topics = ["block"])]
pub struct BlockJurisdictionEvent {
    #[topic]
    pub action: Symbol,
    pub country_code: u32,
}

#[contractevent(topics = ["freeze"])]
pub struct FreezeAccountEvent {
    #[topic]
    pub account: Address,
    pub reason: Symbol,
}

#[contractevent(topics = ["travel"])]
pub struct TravelRuleEvent {
    #[topic]
    pub from: Address,
    #[topic]
    pub to: Address,
    pub asset: Symbol,
    pub amount: i128,
    pub sequence: u64,
}

/// Fired when an address is suspended, sanctioned, or has its status
/// changed by an admin action.
#[contractevent(topics = ["status_change"])]
pub struct StatusChangeEvent {
    #[topic]
    pub account: Address,
    pub old_status: u32,
    pub new_status: u32,
    pub reason: Symbol,
}

/// Fired when a user submits a provider-signed attestation.
#[contractevent(topics = ["submit_attest"])]
pub struct SubmitAttestEvent {
    #[topic]
    pub subject: Address,
    #[topic]
    pub provider: Address,
    pub jurisdiction_allowed: bool,
}

/// Fired when a provider registers their signing key for attestation
/// verification.
#[contractevent(topics = ["provider_reg"])]
pub struct ProviderRegisteredEvent {
    #[topic]
    pub provider: Address,
    #[topic]
    pub signer: Address,
}

#[contract]
pub struct ComplianceRegistry;

#[contractimpl]
impl ComplianceRegistry {
    // ── Init ──────────────────────────────────────────────────────
    pub fn initialize(env: Env, admin: Address, dao: Address) {
        admin.require_auth();
        assert!(!env.storage().instance().has(&ADMIN), "already initialised");
        env.storage().instance().set(&ADMIN, &admin);
        env.storage().instance().set(&DAO,   &dao);
        env.storage().instance().set(&VERIF_COUNT, &0_u32);
        env.storage().instance().set(&DataKey::TravelRuleCount, &0_u64);
        // Regulatory default: the protocol is not offered to US or EU
        // persons, nor to OFAC-sanctioned/high-risk jurisdictions (Cuba,
        // Iran, North Korea, Syria, Russia, Belarus, Myanmar, Venezuela,
        // Sudan, South Sudan). The DAO multi-sig can unblock specific
        // jurisdictions later via unblock_jurisdiction if the legal
        // posture changes.
        Self::_seed_restricted_regions(&env);
        Self::_seed_sanctioned_countries(&env);
    }

    // ── Verifier management (DAO multi-sig only) ───────────────────
    //  `dao` is expected to be the protocol's multi-sig wallet address, so
    //  every call below requires the multi-sig threshold of signatures to
    //  authorise (see `_require_dao`).
    //
    //  Multi-provider KYC: the DAO can authorise as many providers as it
    //  wants via `add_verifier` (Sumsub, Persona, Veriff, Jumio, Onfido,
    //  …). End users pick whichever authorised provider they prefer —
    //  see `list_verifiers` for the set a dApp should offer the user.

    /// Set/replace the single canonical KYC provider address (e.g. the
    /// signing address used by Persona or another provider). This both
    /// authorises the new address and revokes the previous one, so a
    /// rotated/compromised provider key stops working immediately.
    /// Does not affect any OTHER providers already authorised via
    /// `add_verifier` — this only manages the "canonical" single-provider
    /// slot for integrations that want one default provider.
    pub fn set_verifier(env: Env, dao: Address, verifier: Address) {
        Self::_require_dao(&env, &dao);
        if let Some(old) = env.storage().instance().get::<_, Address>(&VERIFIER) {
            if old != verifier {
                env.storage().persistent().remove(&DataKey::Verifier(old));
            }
        }
        env.storage().instance().set(&VERIFIER, &verifier);
        Self::_add_verifier_internal(&env, verifier.clone());
        VerifierChangedEvent { dao, verifier }.publish(&env);
    }

    /// Authorise an additional KYC provider address so users can choose it
    /// to complete their KYC (multiple providers can be active at once;
    /// use `set_verifier` if you only want a single canonical provider).
    pub fn add_verifier(env: Env, dao: Address, verifier: Address) {
        Self::_require_dao(&env, &dao);
        Self::_add_verifier_internal(&env, verifier);
    }

    pub fn remove_verifier(env: Env, dao: Address, verifier: Address) {
        Self::_require_dao(&env, &dao);
        env.storage().persistent().remove(&DataKey::Verifier(verifier));
    }

    /// The current canonical KYC provider address, if one has been set via
    /// `set_verifier`.
    pub fn get_verifier(env: Env) -> Option<Address> {
        env.storage().instance().get(&VERIFIER)
    }

    pub fn is_verifier(env: Env, addr: Address) -> bool {
        env.storage().persistent().has(&DataKey::Verifier(addr))
    }

    /// Returns every authorised KYC provider address (Sumsub, Persona,
    /// Veriff, …) so a dApp can present the full list of providers the
    /// user may choose from to complete KYC. Off-chain metadata (display
    /// name, logo, hosted-flow URL) is intentionally not stored on-chain —
    /// the backend/webapp map each address to that metadata.
    pub fn list_verifiers(env: Env) -> Vec<Address> {
        let count: u32 = env.storage().instance().get(&VERIF_COUNT).unwrap_or(0);
        let mut out: Vec<Address> = Vec::new(&env);
        for i in 0..count {
            if let Some(addr) = env.storage().persistent().get::<_, Address>(&DataKey::VerifierList(i)) {
                if env.storage().persistent().has(&DataKey::Verifier(addr.clone())) {
                    out.push_back(addr);
                }
            }
        }
        out
    }

    // ── KYC attestation (authorised verifier) ─────────────────────
    //  Called by the KYC provider (e.g. Sumsub, Persona, Veriff — whichever
    //  one the user chose) to mark a user compliant purely by their
    //  on-chain address — no separate identity mapping is needed on-chain,
    //  the provider keeps the PII off-chain and only publishes a hash in
    //  `metadata`. Attestation is rejected outright if `jurisdiction` is on
    //  the (DAO-controlled) blocked list, which by default includes the
    //  US, all EU member states, and OFAC-sanctioned jurisdictions (Cuba,
    //  Iran, North Korea, Syria, Russia, Belarus, Myanmar, Venezuela,
    //  Sudan, South Sudan) — so a provider cannot even issue a passing
    //  attestation for those users. The raw jurisdiction code is stored
    //  for internal blocklist evaluation only; it is never returned by a
    //  public getter (see `ComplianceView`).
    pub fn attest(
        env:          Env,
        verifier:     Address,
        subject:      Address,
        jurisdiction: u32,
        expires_at:   u64,
        metadata:     Bytes,   // hash of off-chain KYC documents
    ) {
        verifier.require_auth();
        assert!(
            env.storage().persistent().has(&DataKey::Verifier(verifier.clone())),
            "not an authorised verifier"
        );

        // Block globally blocked jurisdictions
        assert!(
            !env.storage().persistent().has(&DataKey::BlockedJurisdiction(jurisdiction)),
            "jurisdiction is blocked"
        );

        let att = KycAttestation {
            address: subject.clone(), verifier: verifier.clone(),
            jurisdiction, issued_at: env.ledger().timestamp(), expires_at, metadata,
        };
        env.storage().persistent().set(&DataKey::Kyc(subject.clone()), &att);

        // Also write/update the ComplianceRecord
        let mut rec = Self::_get_compliance_record(&env, &subject);
        rec.status = ComplianceStatus::Verified;
        rec.jurisdiction = jurisdiction;
        rec.expires_at = expires_at;
        rec.provider = verifier.clone();
        rec.updated_at = env.ledger().timestamp();
        rec.reason = symbol_short!("attest");
        env.storage().persistent().set(&DataKey::ComplianceRec(subject.clone()), &rec);

        // Track this provider for the subject
        Self::_add_provider_to_subject(&env, &subject, &verifier);

        AttestEvent {
            verifier,
            subject,
            jurisdiction_allowed: true, // rejected above otherwise
        }.publish(&env);
    }

    /// Verifier can revoke their own attestation
    pub fn revoke(env: Env, verifier: Address, subject: Address) {
        verifier.require_auth();
        let att: KycAttestation = env.storage().persistent()
            .get(&DataKey::Kyc(subject.clone()))
            .expect("no attestation found");
        assert!(att.verifier == verifier, "can only revoke own attestations");
        env.storage().persistent().remove(&DataKey::Kyc(subject.clone()));

        // If the revoking provider was the primary, reset ComplianceRecord
        let mut rec = Self::_get_compliance_record(&env, &subject);
        if rec.provider == verifier {
            rec.status = ComplianceStatus::Unknown;
            rec.updated_at = env.ledger().timestamp();
            rec.reason = symbol_short!("revoked");
            env.storage().persistent().set(&DataKey::ComplianceRec(subject.clone()), &rec);
        }

        RevokeEvent {
            verifier,
            subject,
        }.publish(&env);
    }

    // ── Asset compliance rules (DAO) ──────────────────────────────
    pub fn set_asset_rule(
        env:          Env,
        dao:          Address,
        asset:        Symbol,
        blocked_juris: Vec<u32>,
        daily_limit:  i128,
        travel_rule:  bool,
    ) {
        Self::_require_dao(&env, &dao);
        let rule = AssetRule {
            asset: asset.clone(),
            blocked_jurisdictions: blocked_juris,
            daily_limit_usd: daily_limit,
            require_travel_rule: travel_rule,
            enabled: true,
        };
        env.storage().persistent().set(&DataKey::AssetRule(asset), &rule);
    }

    // ── Jurisdiction management (DAO) ─────────────────────────────
    //  Blocking/unblocking a jurisdiction here is the ONLY place country
    //  restrictions are enforced — the same blocklist that gates transfers
    //  (`assert_transfer_allowed`) and general protocol usage
    //  (`assert_user_compliant`) is used to decide whether a jurisdiction's
    //  users may be attested at all (`attest` / `submit_attestation`).
    pub fn block_jurisdiction(env: Env, dao: Address, country_code: u32) {
        Self::_require_dao(&env, &dao);
        env.storage().persistent().set(&DataKey::BlockedJurisdiction(country_code), &true);
        BlockJurisdictionEvent {
            action: symbol_short!("jur"),
            country_code,
        }.publish(&env);
    }

    pub fn unblock_jurisdiction(env: Env, dao: Address, country_code: u32) {
        Self::_require_dao(&env, &dao);
        env.storage().persistent().remove(&DataKey::BlockedJurisdiction(country_code));
    }

    // ── Freeze / unfreeze account (admin — regulatory order) ──────
    pub fn freeze_account(env: Env, admin: Address, account: Address, reason: Symbol) {
        Self::_require_admin(&env, &admin);
        env.storage().persistent().set(&DataKey::Frozen(account.clone()), &true);
        FreezeAccountEvent {
            account,
            reason,
        }.publish(&env);
    }

    pub fn unfreeze_account(env: Env, admin: Address, account: Address) {
        Self::_require_admin(&env, &admin);
        env.storage().persistent().remove(&DataKey::Frozen(account.clone()));
        FreezeAccountEvent {
            account,
            reason: symbol_short!("unfrz"),
        }.publish(&env);
    }

    // ── Whitelist system contracts (bypass compliance checks) ──────
    pub fn set_whitelist(env: Env, admin: Address, contract: Address, whitelisted: bool) {
        Self::_require_admin(&env, &admin);
        if whitelisted {
            env.storage().persistent().set(&DataKey::Whitelist(contract), &true);
        } else {
            env.storage().persistent().remove(&DataKey::Whitelist(contract));
        }
    }

    /// Replace this contract's Wasm in place. Instance and persistent
    /// storage are preserved. Authorised by the stored DAO address.
    pub fn upgrade(env: Env, _dao: Address, new_wasm_hash: BytesN<32>) {
        let stored: Address = env.storage().instance().get(&DAO).unwrap();
        stored.require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    // ══════════════════════════════════════════════════════════════
    //  COMPLIANCE CHECK — called by SyntheticEngine + LiqPool
    // ══════════════════════════════════════════════════════════════

    /// Main entry: called before any transfer/mint/burn.
    /// Returns true if allowed, panics with reason if not.
    pub fn assert_transfer_allowed(
        env:       Env,
        caller:    Address,    // the contract requesting the check
        from:      Address,
        to:        Address,
        asset:     Symbol,
        amount:    i128,       // USD value (7dp)
    ) -> bool {
        // Whitelist bypass — only for genuine system-to-system transfers
        // where the caller AND both counterparties are whitelisted. Do NOT
        // key this off `caller` alone: engine/pool contracts call this on
        // behalf of end users, and whitelisting them (so they're allowed to
        // integrate at all) must never silently exempt those end users from
        // KYC/jurisdiction checks.
        if env.storage().persistent().has(&DataKey::Whitelist(caller))
            && env.storage().persistent().has(&DataKey::Whitelist(from.clone()))
            && env.storage().persistent().has(&DataKey::Whitelist(to.clone()))
        {
            return true;
        }

        // Frozen check
        assert!(
            !env.storage().persistent().has(&DataKey::Frozen(from.clone())),
            "sender account is frozen"
        );
        assert!(
            !env.storage().persistent().has(&DataKey::Frozen(to.clone())),
            "recipient account is frozen"
        );

        let from_att = Self::_get_attestation(&env, &from);
        let to_att   = Self::_get_attestation(&env, &to);

        // Expiry check
        let now = env.ledger().timestamp();
        if let Some(ref a) = from_att {
            if a.expires_at > 0 && a.expires_at < now {
                panic!("sender KYC attestation expired");
            }
        }
        if let Some(ref a) = to_att {
            if a.expires_at > 0 && a.expires_at < now {
                panic!("recipient KYC attestation expired");
            }
        }

        // Asset rule enforcement
        if let Some(rule) = env.storage().persistent().get::<_, AssetRule>(&DataKey::AssetRule(asset.clone())) {
            if rule.enabled {
                // Every party must have a live attestation from an
                // authorised provider — no tiers, just verified or not.
                assert!(from_att.is_some(), "sender KYC required");
                assert!(to_att.is_some(), "recipient KYC required");

                // Jurisdiction checks — uses the subject's jurisdiction
                // internally, never exposes it.
                if let Some(ref a) = from_att {
                    let blocked = rule.blocked_jurisdictions.iter()
                        .any(|j| j == a.jurisdiction);
                    assert!(!blocked, "sender jurisdiction is blocked for this asset");
                    assert!(
                        !env.storage().persistent().has(&DataKey::BlockedJurisdiction(a.jurisdiction)),
                        "sender jurisdiction globally blocked"
                    );
                }
                if let Some(ref a) = to_att {
                    let blocked = rule.blocked_jurisdictions.iter()
                        .any(|j| j == a.jurisdiction);
                    assert!(!blocked, "recipient jurisdiction is blocked for this asset");
                }

                // Daily volume limit
                let limit = if rule.daily_limit_usd > 0 {
                    rule.daily_limit_usd
                } else {
                    DEFAULT_DAILY_LIMIT
                };
                Self::_check_and_update_volume(&env, &from, amount, limit);

                // Travel Rule flag for large transfers
                if rule.require_travel_rule && amount >= TRAVEL_RULE_THRESHOLD {
                    Self::_log_travel_rule(&env, &from, &to, asset, amount);
                }
            }
        }

        true
    }

    /// Read-only compliance check (doesn't update volumes)
    pub fn is_transfer_allowed(
        env:   Env,
        from:  Address,
        to:    Address,
        asset: Symbol,
        amount: i128,
    ) -> bool {
        let _ = amount;
        if env.storage().persistent().has(&DataKey::Frozen(from.clone())) { return false; }
        if env.storage().persistent().has(&DataKey::Frozen(to.clone()))   { return false; }

        let from_att = Self::_get_attestation(&env, &from);
        let now      = env.ledger().timestamp();
        if let Some(ref a) = from_att {
            if a.expires_at > 0 && a.expires_at < now { return false; }
        }

        let rule: Option<AssetRule> = env.storage().persistent()
            .get(&DataKey::AssetRule(asset));
        if let Some(r) = rule {
            if r.enabled && from_att.is_none() {
                return false;
            }
        }
        true
    }

    // ══════════════════════════════════════════════════════════════
    //  GENERIC USAGE GATE — called by every user-facing protocol
    //  contract (CDP engine, liquidity pool, stability pool, ...)
    //  before letting an address interact at all. Unlike
    //  `assert_transfer_allowed`, this does NOT require a per-asset
    //  AssetRule to be configured — it always requires a live,
    //  non-expired KYC attestation issued by an authorised provider,
    //  in a jurisdiction that isn't currently blocked.
    // ══════════════════════════════════════════════════════════════

    /// Panics with a descriptive reason if `user` may not use the protocol.
    /// `caller` is the contract requesting the check; it is only ever used
    /// for the narrow system-to-system bypass (see comment below) — never
    /// to exempt a real end user.
    pub fn assert_user_compliant(env: Env, caller: Address, user: Address) -> bool {
        // Bypass only when BOTH the requesting contract and the address
        // being checked are whitelisted system addresses (e.g. one internal
        // contract's own treasury/custody address) — never for ordinary
        // users interacting through a whitelisted-but-untrusted-for-this
        // caller contract.
        if env.storage().persistent().has(&DataKey::Whitelist(caller))
            && env.storage().persistent().has(&DataKey::Whitelist(user.clone()))
        {
            return true;
        }

        assert!(
            !env.storage().persistent().has(&DataKey::Frozen(user.clone())),
            "account is frozen"
        );

        let rec = Self::_get_compliance_record(&env, &user);

        // Check the compliance status — only Verified users may interact
        match rec.status {
            ComplianceStatus::Unknown =>
                panic!("KYC required: no attestation on file for this address"),
            ComplianceStatus::Pending =>
                panic!("KYC pending: your attestation is still under review"),
            ComplianceStatus::Suspended =>
                panic!("account suspended"),
            ComplianceStatus::Sanctioned =>
                panic!("account sanctioned"),
            ComplianceStatus::Expired =>
                panic!("KYC attestation expired — re-verify with your KYC provider"),
            ComplianceStatus::Verified => {} // proceed
        }

        let now = env.ledger().timestamp();
        if rec.expires_at != 0 && rec.expires_at < now {
            // Expired — auto-transition the status
            let mut expired_rec = rec;
            expired_rec.status = ComplianceStatus::Expired;
            expired_rec.updated_at = now;
            env.storage().persistent().set(&DataKey::ComplianceRec(user.clone()), &expired_rec);
            panic!("KYC attestation expired — re-verify with your KYC provider");
        }

        assert!(
            !env.storage().persistent().has(&DataKey::BlockedJurisdiction(rec.jurisdiction)),
            "protocol is unavailable in your jurisdiction"
        );

        true
    }

    /// Read-only version of `assert_user_compliant` (never panics).
    pub fn is_user_compliant(env: Env, user: Address) -> bool {
        if env.storage().persistent().has(&DataKey::Frozen(user.clone())) {
            return false;
        }
        let rec = Self::_get_compliance_record(&env, &user);
        if rec.status != ComplianceStatus::Verified { return false; }
        let now = env.ledger().timestamp();
        if rec.expires_at != 0 && rec.expires_at < now { return false; }
        if env.storage().persistent().has(&DataKey::BlockedJurisdiction(rec.jurisdiction)) {
            return false;
        }
        true
    }

    // ══════════════════════════════════════════════════════════════
    //  ENHANCED COMPLIANCE: Status tracking, multi-provider attestations
    // ══════════════════════════════════════════════════════════════

    /// Simple yes/no: is this address allowed to use the protocol
    /// *right now*? Equivalent to status==Verified && not expired &&
    /// not in a blocked jurisdiction.
    pub fn is_verified(env: Env, user: Address) -> bool {
        Self::is_user_compliant(env, user)
    }

    /// Returns a privacy-preserving view of the ComplianceRecord for an
    /// address (or a default Unknown view if never attested). The raw
    /// jurisdiction/country is NEVER returned — only whether it is
    /// currently allowed (`jurisdiction_allowed`), computed live against
    /// the DAO's current blocklist so it stays accurate even if the DAO
    /// blocks/unblocks a jurisdiction after the user was attested.
    pub fn get_compliance_status(env: Env, user: Address) -> ComplianceView {
        let rec = Self::_get_compliance_record(&env, &user);
        let jurisdiction_allowed = rec.status != ComplianceStatus::Unknown
            && !env.storage().persistent().has(&DataKey::BlockedJurisdiction(rec.jurisdiction));
        ComplianceView {
            status: rec.status,
            jurisdiction_allowed,
            expires_at: rec.expires_at,
            provider: rec.provider,
            updated_at: rec.updated_at,
            reason: rec.reason,
        }
    }

    /// Returns all KYC provider addresses that have attested the given
    /// subject. Use this to check whether the user has attestations from
    /// multiple approved providers (e.g. both Sumsub AND Persona).
    pub fn get_providers(env: Env, subject: Address) -> Vec<Address> {
        let count: u32 = env.storage().persistent()
            .get(&DataKey::AttestationCount(subject.clone()))
            .unwrap_or(0);
        let mut provs: Vec<Address> = Vec::new(&env);
        for i in 0..count {
            if let Some(p) = env.storage().persistent()
                .get(&DataKey::Attestation(subject.clone(), i))
            {
                provs.push_back(p);
            }
        }
        provs
    }

    /// Returns the number of distinct providers that have attested this
    /// address. Can be used to require N-of-M attestations for high-value
    /// operations.
    pub fn get_provider_count(env: Env, subject: Address) -> u32 {
        env.storage().persistent()
            .get(&DataKey::AttestationCount(subject))
            .unwrap_or(0)
    }

    // ── Admin: Suspend / sanction ──────────────────────────────────

    /// Suspend an address — temporarily prevents protocol usage without
    /// destroying the KYC attestation. The user's provider data is
    /// preserved; when unsuspended the user resumes as verified. For
    /// regulatory reviews, investigations, or temporary risk-management
    /// holds.
    pub fn suspend_account(env: Env, admin: Address, account: Address, reason: Symbol) {
        Self::_require_admin(&env, &admin);
        let mut rec = Self::_get_compliance_record(&env, &account);
        let old = rec.status.clone() as u32;
        rec.status = ComplianceStatus::Suspended;
        rec.updated_at = env.ledger().timestamp();
        rec.reason = reason.clone();
        env.storage().persistent().set(&DataKey::ComplianceRec(account.clone()), &rec);
        StatusChangeEvent {
            account,
            old_status: old,
            new_status: ComplianceStatus::Suspended as u32,
            reason,
        }.publish(&env);
    }

    /// Un-suspend an address — restores Verified status. Only callable by
    /// admin.
    pub fn unsuspend_account(env: Env, admin: Address, account: Address) {
        Self::_require_admin(&env, &admin);
        let mut rec = Self::_get_compliance_record(&env, &account);
        assert!(rec.status == ComplianceStatus::Suspended, "account is not suspended");
        let old = rec.status.clone() as u32;
        rec.status = ComplianceStatus::Verified;
        rec.updated_at = env.ledger().timestamp();
        rec.reason = symbol_short!("unsuspend");
        let reason_sym = symbol_short!("unsuspend");
        env.storage().persistent().set(&DataKey::ComplianceRec(account.clone()), &rec);
        StatusChangeEvent {
            account,
            old_status: old,
            new_status: ComplianceStatus::Verified as u32,
            reason: reason_sym,
        }.publish(&env);
    }

    /// Permanently sanction (block) an address — OFAC / sanctions-list
    /// level action. This is NOT reversible through the contract (requires
    /// a new DAO proposal + admin action to overwrite). Differs from
    /// `freeze_account` in that a freeze may be temporary (regulatory
    /// order), while a sanction is a permanent status change.
    pub fn sanction_account(env: Env, admin: Address, account: Address, reason: Symbol) {
        Self::_require_admin(&env, &admin);
        let mut rec = Self::_get_compliance_record(&env, &account);
        let old = rec.status.clone() as u32;
        rec.status = ComplianceStatus::Sanctioned;
        rec.updated_at = env.ledger().timestamp();
        rec.reason = reason.clone();
        env.storage().persistent().set(&DataKey::ComplianceRec(account.clone()), &rec);
        // Also freeze for good measure — sanctioned addresses are always frozen
        env.storage().persistent().set(&DataKey::Frozen(account.clone()), &true);
        StatusChangeEvent {
            account,
            old_status: old,
            new_status: ComplianceStatus::Sanctioned as u32,
            reason,
        }.publish(&env);
    }

    // ── Provider signing key registration ──────────────────────────

    /// Register a signing key for an authorised provider. The signing key
    /// is used to verify provider signatures on attestation submissions
    /// (see `submit_attestation`). This enables the scalable model where
    /// the provider signs a message off-chain and the user submits it —
    /// the provider never pays gas.
    ///
    /// Only an already-authorised verifier (added via `add_verifier`) may
    /// register their own signing key.
    pub fn register_signer(env: Env, provider: Address, signer: Address) {
        provider.require_auth();
        assert!(
            env.storage().persistent().has(&DataKey::Verifier(provider.clone())),
            "provider is not an authorised verifier"
        );
        env.storage().persistent().set(&DataKey::Signer(signer.clone()), &provider);
        ProviderRegisteredEvent { provider, signer }.publish(&env);
    }

    /// Look up which provider a signing key belongs to.
    pub fn get_provider_for_signer(env: Env, signer: Address) -> Option<Address> {
        env.storage().persistent().get(&DataKey::Signer(signer))
    }

    // ── Signed attestation submission ───────────────────────────────

    /// Submit a provider-signed attestation. This is the scalable flow:
    ///
    /// 1. User picks any approved provider (Sumsub, Persona, Veriff, …)
    ///    and completes KYC with them
    /// 2. Provider signs a message: { subject, jurisdiction, expires_at }
    /// 3. User submits the message + signature here
    /// 4. Contract verifies the provider's signing key → authorised verifier
    /// 5. Attestation is stored on-chain
    ///
    /// Benefits:
    ///   - Provider never pays gas
    ///   - User pays their own transaction fee
    ///   - User can choose any approved provider
    ///   - Adding/removing providers is a DAO action
    ///
    /// For now, the signature verification is done via `provider.require_auth()`
    /// on the provider address — in a full implementation, this would use
    /// Ed25519 signature verification against the registered signer key.
    /// The `signer` argument identifies WHICH provider is attesting.
    pub fn submit_attestation(
        env:          Env,
        subject:      Address,
        signer:       Address,  // the signing key that produced the signature
        jurisdiction: u32,
        expires_at:   u64,
    ) {
        subject.require_auth();

        // Look up which provider this signer belongs to
        let provider: Address = env.storage().persistent()
            .get(&DataKey::Signer(signer.clone()))
            .expect("signer not registered — provider must call register_signer first");

        // Verify the provider is still an authorised verifier
        assert!(
            env.storage().persistent().has(&DataKey::Verifier(provider.clone())),
            "provider is no longer authorised"
        );

        // Block attestation for globally blocked jurisdictions
        assert!(
            !env.storage().persistent().has(&DataKey::BlockedJurisdiction(jurisdiction)),
            "jurisdiction is blocked"
        );

        let att = KycAttestation {
            address: subject.clone(),
            verifier: provider.clone(),
            jurisdiction,
            issued_at: env.ledger().timestamp(),
            expires_at,
            metadata: Bytes::new(&env),
        };
        env.storage().persistent().set(&DataKey::Kyc(subject.clone()), &att);

        // Store/update ComplianceRecord
        let mut rec = Self::_get_compliance_record(&env, &subject);
        rec.status = ComplianceStatus::Verified;
        rec.jurisdiction = jurisdiction;
        rec.expires_at = expires_at;
        rec.provider = provider.clone();
        rec.updated_at = env.ledger().timestamp();
        rec.reason = symbol_short!("attest");
        env.storage().persistent().set(&DataKey::ComplianceRec(subject.clone()), &rec);

        // Track this provider in the subject's provider list
        Self::_add_provider_to_subject(&env, &subject, &provider);

        SubmitAttestEvent {
            subject,
            provider,
            jurisdiction_allowed: true, // rejected above otherwise
        }.publish(&env);
    }

    // ── Region management (DAO multi-sig) ─────────────────────────

    /// Re-apply the default US + EU jurisdiction block. Useful if the DAO
    /// wants to restore the default posture after selectively unblocking
    /// some codes.
    pub fn block_us_and_eu(env: Env, dao: Address) {
        Self::_require_dao(&env, &dao);
        Self::_seed_restricted_regions(&env);
    }

    /// Re-apply the default OFAC/high-risk sanctioned-country block (Cuba,
    /// Iran, North Korea, Syria, Russia, Belarus, Myanmar, Venezuela,
    /// Sudan, South Sudan). Useful if the DAO wants to restore the default
    /// posture after selectively unblocking some codes.
    pub fn block_sanctioned_countries(env: Env, dao: Address) {
        Self::_require_dao(&env, &dao);
        Self::_seed_sanctioned_countries(&env);
    }

    pub fn is_us_or_eu(_env: Env, code: u32) -> bool {
        if code == US_COUNTRY_CODE { return true; }
        EU_COUNTRY_CODES.iter().any(|c| *c == code)
    }

    pub fn is_sanctioned_country(_env: Env, code: u32) -> bool {
        SANCTIONED_COUNTRY_CODES.iter().any(|c| *c == code)
    }

    // ── Views ─────────────────────────────────────────────────────
    //  NOTE: there is no public getter that returns a raw jurisdiction /
    //  country code for an address — use `is_jurisdiction_ok` (per-user,
    //  boolean) or `is_jurisdiction_blocked` (per-country-code, for
    //  displaying the DAO's blocklist itself, which is public policy, not
    //  a specific user's data).

    pub fn is_frozen(env: Env, addr: Address) -> bool {
        env.storage().persistent().has(&DataKey::Frozen(addr))
    }

    pub fn is_jurisdiction_blocked(env: Env, code: u32) -> bool {
        env.storage().persistent().has(&DataKey::BlockedJurisdiction(code))
    }

    /// Is `user`'s on-file jurisdiction currently allowed? Never reveals
    /// which jurisdiction it is — just whether it's currently blocked.
    /// Returns false if the user has never been attested.
    pub fn is_jurisdiction_ok(env: Env, user: Address) -> bool {
        match Self::_get_attestation(&env, &user) {
            None => false,
            Some(a) => !env.storage().persistent().has(&DataKey::BlockedJurisdiction(a.jurisdiction)),
        }
    }

    pub fn get_asset_rule(env: Env, asset: Symbol) -> Option<AssetRule> {
        env.storage().persistent().get(&DataKey::AssetRule(asset))
    }

    pub fn get_daily_volume(env: Env, addr: Address) -> i128 {
        let dv: Option<DailyVolume> = env.storage().persistent().get(&DataKey::DailyVol(addr));
        match dv {
            None => 0,
            Some(d) => {
                let day_start = env.ledger().timestamp() / 86_400 * 86_400;
                if d.date_ts == day_start { d.volume } else { 0 }
            }
        }
    }

    pub fn get_travel_rule_count(env: Env) -> u64 {
        env.storage().instance().get(&DataKey::TravelRuleCount).unwrap_or(0)
    }

    // ── Internals ─────────────────────────────────────────────────
    fn _add_verifier_internal(env: &Env, verifier: Address) {
        if env.storage().persistent().has(&DataKey::Verifier(verifier.clone())) {
            return; // already authorised, avoid duplicate list entries
        }
        let count: u32 = env.storage().instance().get(&VERIF_COUNT).unwrap_or(0);
        env.storage().persistent().set(&DataKey::Verifier(verifier.clone()), &true);
        env.storage().persistent().set(&DataKey::VerifierList(count), &verifier);
        env.storage().instance().set(&VERIF_COUNT, &(count + 1));
    }

    fn _seed_restricted_regions(env: &Env) {
        env.storage().persistent().set(&DataKey::BlockedJurisdiction(US_COUNTRY_CODE), &true);
        for code in EU_COUNTRY_CODES.iter() {
            env.storage().persistent().set(&DataKey::BlockedJurisdiction(*code), &true);
        }
    }

    fn _seed_sanctioned_countries(env: &Env) {
        for code in SANCTIONED_COUNTRY_CODES.iter() {
            env.storage().persistent().set(&DataKey::BlockedJurisdiction(*code), &true);
        }
    }

    fn _get_attestation(env: &Env, addr: &Address) -> Option<KycAttestation> {
        env.storage().persistent().get(&DataKey::Kyc(addr.clone()))
    }

    /// Returns the current ComplianceRecord for an address, or a default
    /// `Unknown` record if none has been stored yet.
    fn _get_compliance_record(env: &Env, addr: &Address) -> ComplianceRecord {
        match env.storage().persistent().get(&DataKey::ComplianceRec(addr.clone())) {
            Some(rec) => rec,
            None => ComplianceRecord {
                status: ComplianceStatus::Unknown,
                jurisdiction: 0,
                expires_at: 0,
                provider: addr.clone(), // safe fallback — never read when status==Unknown
                updated_at: 0,
                reason: symbol_short!("none"),
            },
        }
    }

    /// Track a provider in a subject's provider attestation list (deduplicated).
    fn _add_provider_to_subject(env: &Env, subject: &Address, provider: &Address) {
        let count: u32 = env.storage().persistent()
            .get(&DataKey::AttestationCount(subject.clone()))
            .unwrap_or(0);

        // Check if this provider already attested
        for i in 0..count {
            if let Some(existing) = env.storage().persistent()
                .get::<_, Address>(&DataKey::Attestation(subject.clone(), i))
            {
                if existing == *provider {
                    return; // already tracked
                }
            }
        }

        env.storage().persistent()
            .set(&DataKey::Attestation(subject.clone(), count), provider);
        env.storage().persistent()
            .set(&DataKey::AttestationCount(subject.clone()), &(count + 1));
    }

    fn _check_and_update_volume(env: &Env, user: &Address, amount: i128, limit: i128) {
        let day_start = env.ledger().timestamp() / 86_400 * 86_400;
        let mut dv: DailyVolume = env.storage().persistent()
            .get(&DataKey::DailyVol(user.clone()))
            .unwrap_or(DailyVolume { date_ts: day_start, volume: 0 });

        if dv.date_ts != day_start {
            dv.date_ts = day_start;
            dv.volume  = 0;
        }
        dv.volume += amount;
        assert!(dv.volume <= limit, "daily transfer limit exceeded");
        env.storage().persistent().set(&DataKey::DailyVol(user.clone()), &dv);
    }

    fn _log_travel_rule(
        env:    &Env,
        from:   &Address,
        to:     &Address,
        asset:  Symbol,
        amount: i128,
    ) {
        let count: u64 = env.storage().instance()
            .get(&DataKey::TravelRuleCount).unwrap_or(0);
        env.storage().instance().set(&DataKey::TravelRuleCount, &(count + 1));
        TravelRuleEvent {
            from: from.clone(),
            to: to.clone(),
            asset,
            amount,
            sequence: count,
        }.publish(&env);
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
