//! Budget-aware routing across LLM marketplace tiers.
//!
//! A *ladder* is an ordered list of rungs. Each rung names a marketplace, a
//! model, and the most it may pay per million tokens. The router walks the
//! ladder and dispatches to the first rung whose sellers fit under that
//! ceiling, falling through to the next rung when none do — so a request takes
//! the strongest model the budget allows rather than failing or overpaying.
//!
//! Ceilings come from configuration and combine: a provider-wide ceiling bounds
//! every rung that uses it, and a rung may tighten it further. The tighter of
//! the two applies.
//!
//! Two marketplaces are supported, and their differences are real rather than
//! cosmetic. `OpenRouter` publishes a per-sub-provider price list and enforces a
//! price ceiling directly. Surplus Intelligence publishes a full order book but
//! ignores its documented price-cap parameters, so a ceiling is restated as the
//! equivalent minimum discount, which it does enforce. See
//! `docs/specs/marketplace-apis.md` for the evidence behind both.
//!
//! ```
//! use llm_ladder_router::Config;
//!
//! let config = Config::parse(
//!     r#"
//!     [providers.openrouter]
//!     kind = "open_router"
//!     base_url = "https://openrouter.ai/api/v1"
//!     api_key_env = "OPENROUTER_API_KEY"
//!     max_cost_per_1m = 0.50
//!
//!     [[ladders]]
//!     name = "flash"
//!
//!       [[ladders.rungs]]
//!       provider = "openrouter"
//!       model = "deepseek/deepseek-v4-flash"
//!       max_cost_per_1m = 0.30
//!     "#,
//! )?;
//!
//! let ladder = config.ladder("flash").expect("the ladder was just defined");
//! // The rung's ceiling is tighter than the provider's, so it wins.
//! assert_eq!(config.cap_for(&ladder.rungs[0]), Some(0.30));
//! # Ok::<(), llm_ladder_router::Error>(())
//! ```

pub mod cli;
pub mod config;
pub mod cooldown;
pub mod credits;
pub mod error;
pub mod ladder;
pub mod pricing;
pub mod provider;
pub mod proxy;
pub mod session;

pub use config::{Config, CostBasis, Ladder, Provider, ProviderKind, Rung, Sessions};
pub use credits::CreditState;
pub use error::{Error, Result};
pub use ladder::{Chosen, Selection, SkipReason, Skipped, select, select_pinned};
pub use pricing::{ModelPrices, Offer, PriceTable};
pub use proxy::serve;
pub use session::{Pin, PinRejected, SessionPins};
