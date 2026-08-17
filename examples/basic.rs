//! Shows how a ladder resolves a request to a rung, with no network involved.
//!
//! Run with `cargo run --example basic`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use llm_ladder_router::{
    Config, CostBasis, CreditState, ModelPrices, Offer, PriceTable, Result, select,
};

fn main() -> Result<()> {
    let config = Config::parse(
        r#"
        [providers.surplus]
        kind = "surplus"
        base_url = "https://api.surplusintelligence.ai"
        api_key_env = "SURPLUS_API_KEY"

        [providers.openrouter]
        kind = "open_router"
        base_url = "https://openrouter.ai/api/v1"
        api_key_env = "OPENROUTER_API_KEY"

        [[ladders]]
        name = "reasoning"

          [[ladders.rungs]]
          provider = "surplus"
          model = "deepseek-v4-pro"
          max_cost_per_1m = 0.30

          [[ladders.rungs]]
          provider = "openrouter"
          model = "deepseek/deepseek-v4-flash"
          max_cost_per_1m = 0.30
        "#,
    )?;

    // Prices as observed on 2026-08-18: the cheapest Surplus seller for Pro was
    // above the $0.30 ceiling, while OpenRouter's cheapest flash endpoint fit.
    let mut prices = PriceTable::new();
    prices.insert(
        "surplus",
        "deepseek-v4-pro",
        ModelPrices::new(vec![seller("Z.ai", 0.63)]),
    );
    prices.insert(
        "openrouter",
        "deepseek/deepseek-v4-flash",
        ModelPrices::new(vec![seller("DeepInfra", 0.14)]),
    );

    let mut credits = CreditState::new();
    credits.set_balance("surplus", 74.67);
    credits.set_balance("openrouter", 12.00);

    let ladder = config.ladder("reasoning").expect("just defined");
    let selection = select(&config, ladder, &prices, &credits, &[]);

    for skipped in &selection.skipped {
        println!(
            "rung {} ({} {}) skipped: {}",
            skipped.rung, skipped.provider, skipped.model, skipped.reason
        );
    }

    let chosen = selection.chosen.expect("the last rung fits");
    println!(
        "rung {} chosen: {} {} at ${:.4}/Mtok under a ${:.2} ceiling",
        chosen.rung,
        chosen.provider,
        chosen.model,
        chosen.cheapest_per_1m.unwrap_or_default(),
        chosen.cap_per_1m.unwrap_or_default(),
    );

    // The engine is pure, so the same inputs always produce the same decision.
    assert_eq!(chosen.rung, 1);
    assert_eq!(chosen.admitted, vec!["DeepInfra"]);
    let _ = CostBasis::Completion;
    Ok(())
}

fn seller(name: &str, completion_per_1m: f64) -> Offer {
    Offer {
        provider: name.to_string(),
        tag: None,
        prompt_per_1m: completion_per_1m / 2.0,
        completion_per_1m,
        direct_completion_per_1m: Some(3.30),
        usable: true,
    }
}
