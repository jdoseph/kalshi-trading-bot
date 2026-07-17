//! Automated strategies built on the execution engine. Each strategy is a thin
//! decision loop over `market_data` + `orders` + `risk`; the trading-safety
//! gates apply uniformly through `OrderPlacer`.

pub mod crypto_convergence;
pub mod resolution_sniper;
