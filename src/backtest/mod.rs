pub mod engine;
pub mod report;

pub use engine::{
    default_benchmark, simulate, BacktestConfig, BacktestEngine, BacktestResult, ClosedTrade,
    ExecutionTiming, TradeRecord, TradeSide,
};
pub use report::print_backtest_report;
