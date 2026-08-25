# Settlement System — bank-facing copy

Text for the "Settlement System" deck slide.

---

## Settlement

Open Corridor handles messaging, promises, and netting between banks. Moving
the settled value itself needs a rail — a medium of exchange.

Settlement rails across Africa are evolving fast: PAPSS, and local stablecoin
initiatives such as cNGN for the Naira. Open Corridor therefore takes no bet
on a winner: **the network is settlement-agnostic** — the rail is a
per-corridor choice, not a design commitment.

**Phase 1 — pilot.** Settle over Cardano (ADA), the chain already used for
the promise layer. Exposure to the asset is minimal by construction: only the
netted difference between two banks moves, the exchange rate is locked at
settlement time, and the asset is held only for a short time.

**Phase 2 — bank rails, made efficient.** In parallel, integrate the banks'
existing settlement rails. Existing correspondent arrangements then carry
one netted settlement instead of every payment — fewer fees, one FX
conversion at a locked rate, and customer speed no longer depends on SWIFT:
customers are credited in seconds at promise time, while the rail moves only
the occasional net.

**Phase 3 — stablecoins.** First **USDCx** — the Cardano-native stablecoin
backed 1:1 by USDC via Circle's xReserve, live on mainnet since February
2026 — as a per-corridor settlement choice alongside ADA, for banks whose
treasury policy calls for a fiat-denominated asset over the settlement
window. Then, as African stablecoins mature, adopt them per corridor — cNGN
for a Nigeria corridor, for example — and where a corridor needs one, issue
a corridor-specific stablecoin.
