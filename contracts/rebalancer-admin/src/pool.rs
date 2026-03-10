use soroban_sdk::{token::TokenClient, Address, Env, IntoVal, Symbol, Val, Vec};

use crate::DataKey;
use stellar_tokens::fungible::sac_admin_wrapper;

/// Get reserves from Soroswap V2 pair.
/// Returns (reserve_ljoule, reserve_quote).
pub fn get_pool_reserves(env: &Env) -> (i128, i128) {
    let pool: Address = env
        .storage()
        .instance()
        .get(&DataKey::Pool)
        .expect("Pool not set");
    let sac_address = sac_admin_wrapper::get_sac_address(env);
    let quote_token: Address = env
        .storage()
        .instance()
        .get(&DataKey::QuoteToken)
        .expect("Quote not set");

    let ljoule_client = TokenClient::new(env, &sac_address);
    let quote_client = TokenClient::new(env, &quote_token);

    let reserve_ljoule = ljoule_client.balance(&pool);
    let reserve_quote = quote_client.balance(&pool);

    (reserve_ljoule, reserve_quote)
}

/// Calculate output amount matching Soroswap V2's exact K-constant check.
/// Soroswap uses ceiling division for the 0.3% fee: fee = ceil(amount_in * 3 / 1000).
/// This differs from standard UniV2 which uses balance * 1000 - amount_in * 3.
fn calculate_output(reserve_in: i128, reserve_out: i128, amount_in: i128) -> i128 {
    // Match Soroswap's checked_ceiling_div: fee = ceil(amount_in * 3 / 1000)
    let fee = (amount_in * 3 + 999) / 1000;
    let amount_in_net = amount_in - fee;
    (reserve_out * amount_in_net) / (reserve_in + amount_in_net)
}

/// Swap tokens through Soroswap V2 pair.
/// Pattern: transfer input tokens to pair, then call pair.swap(amount0_out, amount1_out, to).
/// Returns the amount of output tokens received.
pub fn pool_swap(env: &Env, token_in: &Address, amount_in: i128) -> i128 {
    let pool: Address = env
        .storage()
        .instance()
        .get(&DataKey::Pool)
        .expect("Pool not set");
    let sac_address = sac_admin_wrapper::get_sac_address(env);
    let ljoule_is_token0: bool = env
        .storage()
        .instance()
        .get(&DataKey::LjouleIsToken0)
        .unwrap_or(true);
    let self_addr = env.current_contract_address();

    // Get reserves and determine direction
    let (reserve_ljoule, reserve_quote) = get_pool_reserves(env);

    let selling_ljoule = token_in == &sac_address;
    let (reserve_in, reserve_out) = if selling_ljoule {
        (reserve_ljoule, reserve_quote)
    } else {
        (reserve_quote, reserve_ljoule)
    };

    // Calculate expected output with 0.3% fee
    let amount_out = calculate_output(reserve_in, reserve_out, amount_in);

    // Step 1: Transfer input tokens to pair
    let token_client = TokenClient::new(env, token_in);
    token_client.transfer(&self_addr, &pool, &amount_in);

    // Step 2: Call pair.swap(amount0_out, amount1_out, to)
    // Soroswap V2: token0 gets amount0_out, token1 gets amount1_out
    let (amount0_out, amount1_out) = if selling_ljoule {
        // Selling LJOULE (already transferred), want quote out
        if ljoule_is_token0 {
            (0i128, amount_out) // LJOULE is token0, want token1 out
        } else {
            (amount_out, 0i128) // LJOULE is token1, want token0 out
        }
    } else {
        // Selling quote (already transferred), want LJOULE out
        if ljoule_is_token0 {
            (amount_out, 0i128) // Want token0 (LJOULE) out
        } else {
            (0i128, amount_out) // Want token1 (LJOULE) out
        }
    };

    let mut swap_args: Vec<Val> = Vec::new(env);
    swap_args.push_back(amount0_out.into_val(env));
    swap_args.push_back(amount1_out.into_val(env));
    swap_args.push_back(self_addr.into_val(env)); // to

    env.invoke_contract::<Val>(&pool, &Symbol::new(env, "swap"), swap_args);

    amount_out
}
