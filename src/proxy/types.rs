//! Shared proxy state and the headers that explain a routing decision.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::config::Config;
use crate::credits::CreditState;
use crate::pricing::PriceTable;
use crate::provider::Client;
use crate::session::SessionPins;

/// Response header naming the ladder a request used.
pub const HEADER_LADDER: &str = "x-ladder-name";
/// Response header naming the zero-based rung that served.
pub const HEADER_RUNG: &str = "x-ladder-rung";
/// Response header naming the marketplace that served.
pub const HEADER_PROVIDER: &str = "x-ladder-provider";
/// Response header naming the model that served.
pub const HEADER_MODEL: &str = "x-ladder-model";
/// Response header naming the sub-provider the marketplace routed to.
pub const HEADER_SUB_PROVIDER: &str = "x-ladder-sub-provider";
/// Response header carrying the ceiling that applied, in USD per Mtok.
pub const HEADER_CAP: &str = "x-ladder-cap-per-1m";
/// Response header naming the reasoning depth the router asked for, when it
/// asked for one. Absent means the request was relayed with whatever depth the
/// caller sent, which is not the same as the model having thought shallowly.
pub const HEADER_EFFORT: &str = "x-ladder-reasoning-effort";
/// Response header carrying the rung's score — its cheapest admitted seller
/// divided by its quality multiplier, in USD per million baseline-equivalent
/// tokens. This is the number the rung actually won on.
pub const HEADER_SCORE: &str = "x-ladder-score";
/// Response header counting the rungs passed over before this one.
pub const HEADER_SKIPPED: &str = "x-ladder-skipped";
/// Response header naming the session this request was attributed to.
pub const HEADER_SESSION: &str = "x-ladder-session";
/// Response header saying whether the session's pin decided the rung.
pub const HEADER_PINNED: &str = "x-ladder-pinned";

/// Everything a request handler needs, shared across connections.
#[derive(Debug, Clone)]
pub struct State {
    /// The validated configuration.
    pub config: Arc<Config>,
    /// One client per configured provider.
    pub clients: Arc<BTreeMap<String, Client>>,
    /// The latest price snapshot, replaced wholesale by the refresher.
    pub prices: Arc<RwLock<PriceTable>>,
    /// The latest balances, replaced wholesale by the poller.
    pub credits: Arc<RwLock<CreditState>>,
    /// Which rung each live conversation is pinned to.
    pub sessions: Arc<RwLock<SessionPins>>,
}
