#![cfg(test)]

use super::*;
use soroban_sdk::{testutils::Address as _, vec, Address, Env, Symbol};

fn setup() -> (Env, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let dao = Address::generate(&env);
    let provider = Address::generate(&env);
    let id = env.register(OracleTwap, ());
    let client = OracleTwapClient::new(&env, &id);
    client.initialize(&admin, &dao, &3600);
    client.add_provider(&dao, &provider);
    client.set_min_providers(&dao, &1);
    (env, id, provider)
}

#[test]
fn submit_prices_writes_all_feeds() {
    let (env, id, provider) = setup();
    let client = OracleTwapClient::new(&env, &id);
    let assets = vec![
        &env,
        Symbol::new(&env, "AAPL"),
        Symbol::new(&env, "TSLA"),
        Symbol::new(&env, "BTCUSD"),
    ];
    let prices = vec![&env, 120_0000000_i128, 200_0000000_i128, 63_000_0000000_i128];
    client.submit_prices(&provider, &assets, &prices);

    assert_eq!(client.get_price(&Symbol::new(&env, "AAPL")), 120_0000000);
    assert_eq!(client.get_price(&Symbol::new(&env, "TSLA")), 200_0000000);
    assert_eq!(client.get_price(&Symbol::new(&env, "BTCUSD")), 63_000_0000000);
}

#[test]
fn submit_price_still_works() {
    let (env, id, provider) = setup();
    let client = OracleTwapClient::new(&env, &id);
    client.submit_price(&provider, &Symbol::new(&env, "AAPL"), &150_0000000);
    assert_eq!(client.get_price(&Symbol::new(&env, "AAPL")), 150_0000000);
}

#[test]
#[should_panic(expected = "assets/prices length mismatch")]
fn submit_prices_rejects_length_mismatch() {
    let (env, id, provider) = setup();
    let client = OracleTwapClient::new(&env, &id);
    let assets = vec![&env, Symbol::new(&env, "AAPL")];
    let prices = vec![&env, 120_0000000_i128, 200_0000000_i128];
    client.submit_prices(&provider, &assets, &prices);
}
