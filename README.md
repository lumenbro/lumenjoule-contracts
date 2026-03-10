# LumenJoule Contracts

**LumenJoule** is an energy-denominated AI compute credit on [Stellar](https://stellar.org). 1 LumenJoule = 1,000 Joules of estimated H100 GPU inference energy, priced via a Vast.ai oracle.

LumenJoule is a **classic Stellar asset** (`LumenJoule` credit_alphanum12) wrapped by a **Stellar Asset Contract** (SAC), with a unified **RebalancerAdmin** contract that serves as SAC admin, oracle price feed, and Soroswap V2 pool stabilizer.

## Architecture

```
Oracle Service → RebalancerAdmin (SAC admin + oracle + stabilizer)
                      ↕                    ↕
              Soroswap V2 Pair      LumenJoule SAC ← Classic Asset
```

**RebalancerAdmin** is the only custom contract. The SAC handles all SEP-41 token operations natively (transfer, balance, burn, metadata, trustlines). RebalancerAdmin handles:

- **Oracle price feed** — circuit breaker (20% max swing), bounds check, nonce replay protection
- **Mint/burn supply management** — via SAC admin wrapper (OZ `SACAdminWrapper` trait)
- **Pool stabilization** — mint-and-sell when overpriced, buyback-and-burn when underpriced
- **Soroswap V2 integration** — constant-product AMM with ceiling-div 0.3% fee

## Contracts

| Contract | Description |
|----------|-------------|
| `rebalancer-admin` | SAC admin wrapper + oracle + pool stabilizer |

## Build

Requires [Stellar CLI](https://developers.stellar.org/docs/tools/developer-tools/cli/stellar-cli) v23.4.1 and Rust 1.92.0:

```bash
stellar contract build --package rebalancer-admin --optimize --out-dir out
```

## Verification

Tagged releases trigger CI that:
1. Builds optimized WASM with `--meta source_repo` and `--meta home_domain`
2. Computes SHA256 hash
3. Creates GitHub Release with WASM artifact
4. Submits hash to [StellarExpert](https://stellar.expert) for verified badge
5. Attests build provenance via GitHub Actions

The **StellarExpert Verify** workflow uses the official [`stellar-expert/soroban-build-workflow`](https://github.com/stellar-expert/soroban-build-workflow) reusable workflow for verified badge eligibility.

## Dependencies

- `soroban-sdk = "23.4.0"`
- `stellar-tokens = "0.6.0"` (OpenZeppelin Stellar Contracts)
- `stellar-access = "0.6.0"`
- `stellar-macros = "0.6.0"`
- `stellar-contract-utils = "0.6.0"`

## Testnet Addresses

| Component | Address |
|-----------|---------|
| LumenJoule SAC | `CCFVNEQCBMTCJKI24G543SPMQWUZPARRSMBXCYIYI25XBUQSCSRNVJNY` |
| RebalancerAdmin | `CBDSVAOTLA2AG44LFS4EAXUOKVFOT6SUEEE7JJFV4FK7ZHQ25MO74UHC` |
| Soroswap V2 Pool | `CDK4WFGLIE6UD6YK2BQLTWZZ2IJKEIWTOGBTVYXCPG772NXZOUVD7V22` |

## Related

- [joule-contracts](https://github.com/lumenbro/joule-contracts) — Legacy JOULE token + rebalancer (pure Soroban, SushiSwap V3)
- [x402 Protocol](https://x402.org) — Agentic commerce protocol

## License

MIT
