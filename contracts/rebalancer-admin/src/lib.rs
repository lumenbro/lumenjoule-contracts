#![no_std]

mod errors;
mod oracle;
mod pool;

use errors::RebalancerAdminError;
use oracle::PriceData;
use pool::{get_pool_reserves, pool_swap};

use soroban_sdk::{
    contract, contractimpl, contracttype, token::TokenClient, Address, BytesN, Env, Symbol,
};
use stellar_tokens::fungible::sac_admin_wrapper::{self, SACAdminWrapper};

// ─── Storage Keys ────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    // Pool / DEX
    Pool,
    QuoteToken,
    LjouleIsToken0,
    // Auth
    Oracle,
    Owner,
    // Oracle price feed
    PriceData,
    QuotePrice,
    PriceFloor,
    PriceCeiling,
    MintCap,
    // Rebalance params
    UpperBps,
    LowerBps,
    MaxMint,
    MaxQuoteSpend,
    MaxStaleLedgers,
    CooldownLedgers,
    LastRebalanceLedger,
    MinReserve,
    // Supply tracking
    TotalMinted,
    TotalBurned,
    // Guard
    Initialized,
}

// ─── Defaults ────────────────────────────────────────────────────

const DEFAULT_MAX_STALE_LEDGERS: u32 = 1000; // ~83 min at 5s/ledger
const DEFAULT_COOLDOWN_LEDGERS: u32 = 12; // ~1 min
const DEFAULT_MIN_RESERVE: i128 = 10_000_000; // 1 token (7 decimals)

// TTL constants
const TTL_THRESHOLD: u32 = 17_280; // ~1 day
const TTL_EXTEND_TO: u32 = 518_400; // ~30 days

// ─── Return types ────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug)]
pub struct PoolStatus {
    pub reserve_quote: i128,
    pub reserve_ljoule: i128,
    pub pool_ljoule_usd_x7: i128,
    pub oracle_ljoule_usd_x7: i128,
    pub quote_usd_x7: i128,
    pub deviation_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Config {
    pub sac: Address,
    pub pool: Address,
    pub quote_token: Address,
    pub oracle: Address,
    pub owner: Address,
    pub quote_price: i128,
    pub upper_bps: u32,
    pub lower_bps: u32,
    pub max_mint: i128,
    pub max_quote_spend: i128,
    pub ljoule_is_token0: bool,
    pub max_stale_ledgers: u32,
    pub cooldown_ledgers: u32,
    pub min_reserve: i128,
}

// ─── Helpers ─────────────────────────────────────────────────────

/// Integer square root via Newton's method.
fn isqrt(n: i128) -> i128 {
    if n <= 0 {
        return 0;
    }
    if n == 1 {
        return 1;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

fn require_initialized(env: &Env) {
    let init: bool = env
        .storage()
        .instance()
        .get(&DataKey::Initialized)
        .unwrap_or(false);
    assert!(init, "Contract not initialized");
}

fn require_oracle(env: &Env) {
    let oracle: Address = env
        .storage()
        .instance()
        .get(&DataKey::Oracle)
        .expect("Oracle not set");
    oracle.require_auth();
}

fn require_owner(env: &Env) {
    let owner: Address = env
        .storage()
        .instance()
        .get(&DataKey::Owner)
        .expect("Owner not set");
    owner.require_auth();
}

fn require_oracle_or_owner(env: &Env, operator: &Address) {
    let oracle: Address = env
        .storage()
        .instance()
        .get(&DataKey::Oracle)
        .expect("Oracle not set");
    let owner: Address = env
        .storage()
        .instance()
        .get(&DataKey::Owner)
        .expect("Owner not set");
    assert!(
        operator == &oracle || operator == &owner,
        "Unauthorized: must be oracle or owner"
    );
}

// ─── Contract ────────────────────────────────────────────────────

#[contract]
pub struct RebalancerAdmin;

// ─── SACAdminWrapper trait impl ──────────────────────────────────

#[contractimpl]
impl SACAdminWrapper for RebalancerAdmin {
    fn set_admin(e: Env, new_admin: Address, operator: Address) {
        require_initialized(&e);
        require_owner(&e);
        operator.require_auth();
        sac_admin_wrapper::set_admin(&e, &new_admin);
        e.events()
            .publish((Symbol::new(&e, "sac_admin_changed"),), new_admin);
    }

    fn set_authorized(e: Env, id: Address, authorize: bool, operator: Address) {
        require_initialized(&e);
        require_owner(&e);
        operator.require_auth();
        sac_admin_wrapper::set_authorized(&e, &id, authorize);
        e.events()
            .publish((Symbol::new(&e, "authorization_changed"),), (id, authorize));
    }

    fn mint(e: Env, to: Address, amount: i128, operator: Address) {
        require_initialized(&e);
        require_oracle_or_owner(&e, &operator);
        operator.require_auth();

        // Enforce mint cap
        let mint_cap = oracle::get_mint_cap(&e);
        if amount > mint_cap {
            panic!("Mint cap exceeded");
        }

        sac_admin_wrapper::mint(&e, &to, amount);

        // Track total minted
        let total: i128 = e
            .storage()
            .instance()
            .get(&DataKey::TotalMinted)
            .unwrap_or(0);
        e.storage()
            .instance()
            .set(&DataKey::TotalMinted, &(total + amount));

        e.events()
            .publish((Symbol::new(&e, "minted"),), (to, amount));
    }

    fn clawback(e: Env, from: Address, amount: i128, operator: Address) {
        require_initialized(&e);
        require_oracle_or_owner(&e, &operator);
        operator.require_auth();

        sac_admin_wrapper::clawback(&e, &from, amount);

        // Track total burned (clawback destroys tokens)
        let total: i128 = e
            .storage()
            .instance()
            .get(&DataKey::TotalBurned)
            .unwrap_or(0);
        e.storage()
            .instance()
            .set(&DataKey::TotalBurned, &(total + amount));

        e.events()
            .publish((Symbol::new(&e, "clawback"),), (from, amount));
    }
}

// ─── Main implementation ─────────────────────────────────────────

#[contractimpl]
impl RebalancerAdmin {
    // ─── Initialization ──────────────────────────────────────────

    /// Initialize with SAC address, pool, quote token, oracle, and owner.
    pub fn initialize(
        env: Env,
        sac: Address,
        pool: Address,
        quote_token: Address,
        oracle: Address,
        owner: Address,
        ljoule_is_token0: bool,
    ) {
        let already: bool = env
            .storage()
            .instance()
            .get(&DataKey::Initialized)
            .unwrap_or(false);
        assert!(!already, "Already initialized");

        // Store SAC address via OZ helper
        sac_admin_wrapper::set_sac_address(&env, &sac);

        env.storage().instance().set(&DataKey::Pool, &pool);
        env.storage()
            .instance()
            .set(&DataKey::QuoteToken, &quote_token);
        env.storage().instance().set(&DataKey::Oracle, &oracle);
        env.storage().instance().set(&DataKey::Owner, &owner);
        env.storage()
            .instance()
            .set(&DataKey::LjouleIsToken0, &ljoule_is_token0);

        // Defaults
        env.storage()
            .instance()
            .set(&DataKey::UpperBps, &500u32);
        env.storage()
            .instance()
            .set(&DataKey::LowerBps, &500u32);
        env.storage()
            .instance()
            .set(&DataKey::MaxMint, &100_000_000_000i128);
        env.storage()
            .instance()
            .set(&DataKey::MaxQuoteSpend, &50_000_000_000i128);
        env.storage()
            .instance()
            .set(&DataKey::MaxStaleLedgers, &DEFAULT_MAX_STALE_LEDGERS);
        env.storage()
            .instance()
            .set(&DataKey::CooldownLedgers, &DEFAULT_COOLDOWN_LEDGERS);
        env.storage()
            .instance()
            .set(&DataKey::MinReserve, &DEFAULT_MIN_RESERVE);
        env.storage()
            .instance()
            .set(&DataKey::Initialized, &true);

        env.events()
            .publish((Symbol::new(&env, "initialized"),), (sac, pool));
    }

    // ─── Oracle price feed ───────────────────────────────────────

    /// Oracle sets quote token USD price (7-decimal fixed-point).
    pub fn set_quote_price(env: Env, price: i128) -> Result<(), RebalancerAdminError> {
        require_initialized(&env);
        require_oracle(&env);
        assert!(price > 0, "Price must be positive");
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);

        env.storage()
            .instance()
            .set(&DataKey::QuotePrice, &price);

        env.events()
            .publish((Symbol::new(&env, "quote_price_set"),), price);

        Ok(())
    }

    /// Oracle updates LJOULE/USD price. Stores directly (no forwarding needed).
    /// Validates: nonce > previous, bounds check, circuit breaker (20% max swing).
    pub fn update_price(
        env: Env,
        price_scaled: i128,
        nonce: u64,
    ) -> Result<(), RebalancerAdminError> {
        require_initialized(&env);
        require_oracle(&env);
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);

        // Nonce must be strictly increasing
        let old_nonce = oracle::get_nonce(&env);
        if nonce <= old_nonce {
            return Err(RebalancerAdminError::NonceTooLow);
        }

        // Bounds check
        oracle::check_bounds(&env, price_scaled)?;

        // Circuit breaker (skip on first price set)
        if let Some(old_data) = oracle::get_price_data(&env) {
            oracle::check_circuit_breaker(old_data.price, price_scaled)?;
        }

        let data = PriceData {
            price: price_scaled,
            nonce,
            ledger: env.ledger().sequence(),
        };
        oracle::set_price_data(&env, &data);

        env.events()
            .publish((Symbol::new(&env, "price_updated"),), (price_scaled, nonce));

        Ok(())
    }

    /// Returns (price_x7, ledger_when_set).
    pub fn get_price(env: Env) -> (i128, u32) {
        require_initialized(&env);
        match oracle::get_price_data(&env) {
            Some(data) => (data.price, data.ledger),
            None => (0, 0),
        }
    }

    /// Owner emergency price override (skips circuit breaker).
    pub fn owner_set_price(
        env: Env,
        price_scaled: i128,
        nonce: u64,
    ) -> Result<(), RebalancerAdminError> {
        require_initialized(&env);
        require_owner(&env);
        oracle::check_bounds(&env, price_scaled)?;

        let data = PriceData {
            price: price_scaled,
            nonce,
            ledger: env.ledger().sequence(),
        };
        oracle::set_price_data(&env, &data);

        env.events()
            .publish((Symbol::new(&env, "price_override"),), (price_scaled, nonce));

        Ok(())
    }

    /// Owner sets price bounds (floor, ceiling).
    pub fn set_price_bounds(env: Env, floor: i128, ceiling: i128) {
        require_initialized(&env);
        require_owner(&env);
        assert!(floor > 0 && ceiling > floor, "Invalid bounds");
        env.storage().instance().set(&DataKey::PriceFloor, &floor);
        env.storage()
            .instance()
            .set(&DataKey::PriceCeiling, &ceiling);
        env.events()
            .publish((Symbol::new(&env, "price_bounds_set"),), (floor, ceiling));
    }

    /// Owner sets mint cap per oracle_mint call.
    pub fn set_mint_cap(env: Env, cap: i128) {
        require_initialized(&env);
        require_owner(&env);
        assert!(cap > 0, "Cap must be positive");
        env.storage().instance().set(&DataKey::MintCap, &cap);
    }

    /// Returns current mint cap.
    pub fn mint_cap(env: Env) -> i128 {
        oracle::get_mint_cap(&env)
    }

    /// Returns current price bounds (floor, ceiling).
    pub fn price_bounds(env: Env) -> (i128, i128) {
        (
            oracle::get_price_floor(&env),
            oracle::get_price_ceiling(&env),
        )
    }

    // ─── Supply tracking ─────────────────────────────────────────

    pub fn total_minted(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalMinted)
            .unwrap_or(0)
    }

    pub fn total_burned(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalBurned)
            .unwrap_or(0)
    }

    pub fn circulating_supply(env: Env) -> i128 {
        let minted: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalMinted)
            .unwrap_or(0);
        let burned: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalBurned)
            .unwrap_or(0);
        minted - burned
    }

    // ─── Rebalance ───────────────────────────────────────────────

    /// Main rebalance logic. Compares pool price vs oracle, mints or buys+burns.
    pub fn rebalance(env: Env) -> Result<(), RebalancerAdminError> {
        require_initialized(&env);
        require_oracle(&env);
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);

        // Cooldown check
        let cooldown_ledgers: u32 = env
            .storage()
            .instance()
            .get(&DataKey::CooldownLedgers)
            .unwrap_or(DEFAULT_COOLDOWN_LEDGERS);
        let last_rebalance: u32 = env
            .storage()
            .instance()
            .get(&DataKey::LastRebalanceLedger)
            .unwrap_or(0);
        let current_ledger = env.ledger().sequence();
        if last_rebalance > 0 && current_ledger - last_rebalance < cooldown_ledgers {
            return Err(RebalancerAdminError::CooldownActive);
        }

        let quote_usd: i128 = env
            .storage()
            .instance()
            .get(&DataKey::QuotePrice)
            .ok_or(RebalancerAdminError::QuotePriceNotSet)?;

        // Oracle staleness check — price stored locally
        let price_data = oracle::get_price_data(&env)
            .ok_or(RebalancerAdminError::QuotePriceNotSet)?;
        let ljoule_usd = price_data.price;
        let max_stale: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MaxStaleLedgers)
            .unwrap_or(DEFAULT_MAX_STALE_LEDGERS);
        if current_ledger - price_data.ledger > max_stale {
            return Err(RebalancerAdminError::OracleStale);
        }

        let (reserve_ljoule, reserve_quote) = get_pool_reserves(&env);

        // Minimum reserve threshold
        let min_reserve: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MinReserve)
            .unwrap_or(DEFAULT_MIN_RESERVE);
        if reserve_quote < min_reserve || reserve_ljoule < min_reserve {
            return Err(RebalancerAdminError::PoolEmpty);
        }

        let upper_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::UpperBps)
            .unwrap_or(500);
        let lower_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::LowerBps)
            .unwrap_or(500);

        // Cross-multiply to avoid division truncation
        let lhs = reserve_quote * quote_usd * 10_000;
        let rhs_upper = ljoule_usd * reserve_ljoule * (10_000 + upper_bps as i128);
        let rhs_lower = ljoule_usd * reserve_ljoule * (10_000 - lower_bps as i128);

        if lhs > rhs_upper {
            Self::do_mint_rebalance(
                &env,
                reserve_quote,
                reserve_ljoule,
                quote_usd,
                ljoule_usd,
                upper_bps,
            )?;
        } else if lhs < rhs_lower {
            Self::do_buyback_rebalance(
                &env,
                reserve_quote,
                reserve_ljoule,
                quote_usd,
                ljoule_usd,
            )?;
        } else {
            return Err(RebalancerAdminError::NoRebalanceNeeded);
        }

        env.storage()
            .instance()
            .set(&DataKey::LastRebalanceLedger, &current_ledger);

        Ok(())
    }

    // ─── Funding & withdrawal ────────────────────────────────────

    /// Fund the contract with quote token (USDC) for buyback operations.
    pub fn fund_quote(env: Env, from: Address, amount: i128) {
        require_initialized(&env);
        from.require_auth();
        assert!(amount > 0, "Amount must be positive");

        let quote_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::QuoteToken)
            .expect("Quote token not set");
        let quote = TokenClient::new(&env, &quote_addr);
        quote.transfer(&from, &env.current_contract_address(), &amount);

        env.events()
            .publish((Symbol::new(&env, "funded"),), (from, amount));
    }

    /// Owner withdraws any token from the contract.
    pub fn withdraw(env: Env, token: Address, to: Address, amount: i128) {
        require_initialized(&env);
        require_owner(&env);
        assert!(amount > 0, "Amount must be positive");

        let client = TokenClient::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);

        env.events()
            .publish((Symbol::new(&env, "withdraw"),), (token, to, amount));
    }

    // ─── Admin config ────────────────────────────────────────────

    /// Owner changes the oracle address.
    pub fn set_oracle(env: Env, oracle: Address) {
        require_initialized(&env);
        require_owner(&env);
        env.storage().instance().set(&DataKey::Oracle, &oracle);
        env.events()
            .publish((Symbol::new(&env, "oracle_changed"),), oracle);
    }

    /// Owner updates the Soroswap V2 pool.
    pub fn set_pool(env: Env, pool: Address, ljoule_is_token0: bool) {
        require_initialized(&env);
        require_owner(&env);
        env.storage().instance().set(&DataKey::Pool, &pool);
        env.storage()
            .instance()
            .set(&DataKey::LjouleIsToken0, &ljoule_is_token0);
        env.events()
            .publish((Symbol::new(&env, "pool_changed"),), pool);
    }

    /// Owner updates rebalancing parameters.
    pub fn set_params(
        env: Env,
        upper_bps: u32,
        lower_bps: u32,
        max_mint: i128,
        max_quote_spend: i128,
        cooldown_ledgers: u32,
        min_reserve: i128,
    ) {
        require_initialized(&env);
        require_owner(&env);
        assert!(upper_bps > 0 && upper_bps < 10_000, "Invalid upper_bps");
        assert!(lower_bps > 0 && lower_bps < 10_000, "Invalid lower_bps");
        assert!(max_mint > 0, "max_mint must be positive");
        assert!(max_quote_spend > 0, "max_quote_spend must be positive");
        assert!(min_reserve > 0, "min_reserve must be positive");

        env.storage()
            .instance()
            .set(&DataKey::UpperBps, &upper_bps);
        env.storage()
            .instance()
            .set(&DataKey::LowerBps, &lower_bps);
        env.storage()
            .instance()
            .set(&DataKey::MaxMint, &max_mint);
        env.storage()
            .instance()
            .set(&DataKey::MaxQuoteSpend, &max_quote_spend);
        env.storage()
            .instance()
            .set(&DataKey::CooldownLedgers, &cooldown_ledgers);
        env.storage()
            .instance()
            .set(&DataKey::MinReserve, &min_reserve);

        env.events().publish(
            (Symbol::new(&env, "params_updated"),),
            (upper_bps, lower_bps, max_mint, max_quote_spend),
        );
    }

    /// Owner updates max stale ledgers for oracle freshness.
    pub fn set_max_stale(env: Env, max_stale_ledgers: u32) {
        require_initialized(&env);
        require_owner(&env);
        assert!(max_stale_ledgers > 0, "Must be positive");
        env.storage()
            .instance()
            .set(&DataKey::MaxStaleLedgers, &max_stale_ledgers);
        env.events()
            .publish((Symbol::new(&env, "max_stale_changed"),), max_stale_ledgers);
    }

    /// Owner upgrades the contract WASM.
    pub fn upgrade(env: Env, wasm_hash: BytesN<32>) {
        require_initialized(&env);
        require_owner(&env);
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
        env.deployer().update_current_contract_wasm(wasm_hash);
    }

    // ─── Read-only queries ───────────────────────────────────────

    /// Returns pool price vs oracle price status.
    pub fn get_status(env: Env) -> Result<PoolStatus, RebalancerAdminError> {
        require_initialized(&env);

        let quote_usd: i128 = env
            .storage()
            .instance()
            .get(&DataKey::QuotePrice)
            .ok_or(RebalancerAdminError::QuotePriceNotSet)?;

        let price_data = oracle::get_price_data(&env)
            .ok_or(RebalancerAdminError::QuotePriceNotSet)?;
        let ljoule_usd = price_data.price;

        let (reserve_ljoule, reserve_quote) = get_pool_reserves(&env);

        if reserve_quote <= 0 || reserve_ljoule <= 0 {
            return Err(RebalancerAdminError::PoolEmpty);
        }

        let pool_ljoule_usd = reserve_quote * quote_usd / reserve_ljoule;
        let deviation_bps = (pool_ljoule_usd - ljoule_usd) * 10_000 / ljoule_usd;

        Ok(PoolStatus {
            reserve_quote,
            reserve_ljoule,
            pool_ljoule_usd_x7: pool_ljoule_usd,
            oracle_ljoule_usd_x7: ljoule_usd,
            quote_usd_x7: quote_usd,
            deviation_bps,
        })
    }

    /// Returns all configuration values.
    pub fn get_config(env: Env) -> Config {
        require_initialized(&env);
        Config {
            sac: sac_admin_wrapper::get_sac_address(&env),
            pool: env
                .storage()
                .instance()
                .get(&DataKey::Pool)
                .expect("not set"),
            quote_token: env
                .storage()
                .instance()
                .get(&DataKey::QuoteToken)
                .expect("not set"),
            oracle: env
                .storage()
                .instance()
                .get(&DataKey::Oracle)
                .expect("not set"),
            owner: env
                .storage()
                .instance()
                .get(&DataKey::Owner)
                .expect("not set"),
            quote_price: env
                .storage()
                .instance()
                .get(&DataKey::QuotePrice)
                .unwrap_or(0),
            upper_bps: env
                .storage()
                .instance()
                .get(&DataKey::UpperBps)
                .unwrap_or(500),
            lower_bps: env
                .storage()
                .instance()
                .get(&DataKey::LowerBps)
                .unwrap_or(500),
            max_mint: env
                .storage()
                .instance()
                .get(&DataKey::MaxMint)
                .unwrap_or(100_000_000_000),
            max_quote_spend: env
                .storage()
                .instance()
                .get(&DataKey::MaxQuoteSpend)
                .unwrap_or(50_000_000_000),
            ljoule_is_token0: env
                .storage()
                .instance()
                .get(&DataKey::LjouleIsToken0)
                .unwrap_or(true),
            max_stale_ledgers: env
                .storage()
                .instance()
                .get(&DataKey::MaxStaleLedgers)
                .unwrap_or(DEFAULT_MAX_STALE_LEDGERS),
            cooldown_ledgers: env
                .storage()
                .instance()
                .get(&DataKey::CooldownLedgers)
                .unwrap_or(DEFAULT_COOLDOWN_LEDGERS),
            min_reserve: env
                .storage()
                .instance()
                .get(&DataKey::MinReserve)
                .unwrap_or(DEFAULT_MIN_RESERVE),
        }
    }

    // ─── Internal rebalance methods ──────────────────────────────

    /// Mint LJOULE and sell through V2 pool to push price down (pool overpriced).
    /// Targets band midpoint. USDC received stays as buyback reserves.
    fn do_mint_rebalance(
        env: &Env,
        reserve_quote: i128,
        reserve_ljoule: i128,
        quote_usd: i128,
        ljoule_usd: i128,
        upper_bps: u32,
    ) -> Result<(), RebalancerAdminError> {
        let max_mint: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxMint)
            .unwrap_or(100_000_000_000);

        // Target band midpoint: ljoule_usd * (1 + upper_bps/2/10000)
        let target_ljoule_price = ljoule_usd * (10_000 + upper_bps as i128 / 2);
        let target_reserve_ljoule =
            reserve_quote * quote_usd * 10_000 / target_ljoule_price;
        let mut mint_amount = target_reserve_ljoule - reserve_ljoule;

        if mint_amount <= 0 {
            return Err(RebalancerAdminError::NoRebalanceNeeded);
        }

        if mint_amount > max_mint {
            mint_amount = max_mint;
        }

        // Mint LJOULE to self via SAC
        let self_addr = env.current_contract_address();
        sac_admin_wrapper::mint(env, &self_addr, mint_amount);

        // Track total minted
        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalMinted)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalMinted, &(total + mint_amount));

        // Swap LJOULE → USDC through V2 pair
        let sac_address = sac_admin_wrapper::get_sac_address(env);
        let usdc_received = pool_swap(env, &sac_address, mint_amount);

        // Slippage protection: verify USDC received >= 80% of oracle-implied value
        let expected_usdc = mint_amount * ljoule_usd / quote_usd;
        let min_usdc = expected_usdc * 80 / 100;
        if usdc_received < min_usdc {
            env.events().publish(
                (Symbol::new(env, "slippage_warning"),),
                (usdc_received, expected_usdc, min_usdc),
            );
        }

        env.events().publish(
            (Symbol::new(env, "rebalance_mint"),),
            (mint_amount, usdc_received, reserve_quote, reserve_ljoule),
        );

        Ok(())
    }

    /// Buy LJOULE from V2 pool with quote token and burn it (pool underpriced).
    fn do_buyback_rebalance(
        env: &Env,
        reserve_quote: i128,
        reserve_ljoule: i128,
        quote_usd: i128,
        ljoule_usd: i128,
    ) -> Result<(), RebalancerAdminError> {
        let max_quote_spend: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxQuoteSpend)
            .unwrap_or(50_000_000_000);

        // Calculate USDC to spend to restore peg (exact for V2 constant-product)
        let k = reserve_quote * reserve_ljoule;
        let target_reserve_quote = isqrt(k * ljoule_usd / quote_usd);
        let mut quote_to_spend = target_reserve_quote - reserve_quote;

        if quote_to_spend <= 0 {
            return Err(RebalancerAdminError::NoRebalanceNeeded);
        }

        if quote_to_spend > max_quote_spend {
            quote_to_spend = max_quote_spend;
        }

        let quote_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::QuoteToken)
            .expect("Quote token not set");
        let quote_client = TokenClient::new(env, &quote_addr);
        let quote_balance = quote_client.balance(&env.current_contract_address());

        if quote_balance < quote_to_spend {
            return Err(RebalancerAdminError::InsufficientQuote);
        }

        // Swap USDC → LJOULE through V2 pair
        let ljoule_received = pool_swap(env, &quote_addr, quote_to_spend);

        // Slippage protection
        let expected_ljoule = quote_to_spend * quote_usd / ljoule_usd;
        let min_ljoule = expected_ljoule * 80 / 100;
        if ljoule_received < min_ljoule {
            env.events().publish(
                (Symbol::new(env, "slippage_warning"),),
                (ljoule_received, expected_ljoule, min_ljoule),
            );
        }

        // Burn all LJOULE held by this contract via clawback
        let sac_address = sac_admin_wrapper::get_sac_address(env);
        let ljoule_client = TokenClient::new(env, &sac_address);
        let ljoule_balance = ljoule_client.balance(&env.current_contract_address());

        if ljoule_balance > 0 {
            sac_admin_wrapper::clawback(env, &env.current_contract_address(), ljoule_balance);

            // Track total burned
            let total: i128 = env
                .storage()
                .instance()
                .get(&DataKey::TotalBurned)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::TotalBurned, &(total + ljoule_balance));
        }

        env.events().publish(
            (Symbol::new(env, "rebalance_buyback"),),
            (quote_to_spend, ljoule_received, reserve_quote, reserve_ljoule),
        );

        Ok(())
    }
}
