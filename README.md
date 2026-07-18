# OBP-Bank-Node

The OBP Bank Node connects a bank's Core Banking System to the Open
Corridor interbank payment network (an OBP API instance hosted by TESOBE). See
[`DOCS/OBP-Bank-Node-Spec.md`](DOCS/OBP-Bank-Node-Spec.md) for the full
specification.

## TL;DR

Every step of a cross-border payment leaves a tamper-proof, independently
checkable record — written to a public blockchain (Cardano) and signed by the
bank. The records are cryptographic fingerprints, so the public ledger never
exposes who paid whom or how much. Anyone entitled to it — a regulator, or the
bank on the other end of the corridor — can verify the whole trail without
trusting any single bank's private database.

Each stage of a payment produces its own record:

| Record | Think of it as… | What it proves |
|---|---|---|
| **Promise** | An IOU receipt | One bank has committed, at a specific moment, to pay a specific amount across the corridor. Signed and time-stamped, so it can't be backdated or denied. |
| **Netting Snapshot** | A statement / tally | Rather than moving money for every single payment, a batch of IOUs is tallied into one net figure. The snapshot records which IOUs went in and what the net came to. |
| **Settlement** | A "paid" receipt | The net amount was actually moved and the books cleared — a reference record that it happened, plus, on the Cardano rail, the value transfer itself. |
| **Exception** | A flag on the record | A payment that couldn't complete (timeout, dispute) is recorded as such — failures are visible and accountable, not quietly dropped. |
| **Reversal** | A cancellation receipt | If something has to be unwound, that is recorded too, so the history stays honest end to end. |

What this gives you:

- Every stage from "I owe you" to "paid" has its own record, and even failures
  and reversals are on the record — there are no off-book steps.
- The public proof is a fingerprint, not a name or an amount, so privacy is
  preserved.
- A regulator or the receiving bank can confirm each record independently,
  rather than relying on one institution's word.
- Netting means banks settle the net difference rather than thousands of
  individual transfers.
- If two parties disagree, the Promise and its off-ledger details can be
  revealed to prove exactly what was committed and when.

Current state: the **Promise**, **Settlement**, and **Exception** records are
built; the **Netting Snapshot** and **Reversal** records are designed but not
yet implemented in the node, so the complete chain of records is the target
rather than the shipped state.

## Licence

Copyright (C) 2026 TESOBE GmbH.

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero General Public License v3.0 (AGPLv3) as published
by the Free Software Foundation. See [LICENSE](LICENSE) for the full text.
