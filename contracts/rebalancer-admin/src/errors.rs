use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RebalancerAdminError {
    Unauthorized = 1,
    NoRebalanceNeeded = 2,
    InsufficientQuote = 3,
    PoolEmpty = 4,
    QuotePriceNotSet = 5,
    AlreadyInitialized = 6,
    NotInitialized = 7,
    OracleStale = 8,
    CooldownActive = 9,
    SwapSlippage = 10,
    PriceOutOfBounds = 11,
    CircuitBreakerTripped = 12,
    NonceTooLow = 13,
    MintCapExceeded = 14,
}
