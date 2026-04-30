pub mod engine;
pub mod report;

pub use engine::{BacktestConfig, BacktestEngine, BacktestResult, TradeRecord, TradeSide};
pub use report::print_backtest_report;
