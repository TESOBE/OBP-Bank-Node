# How the on-chain hashes would be used by lawyers for Bank B

How Bank B builds a legal case against Bank A when Bank A fails to settle for
payments Bank B made on the strength of Bank A's promises — and how the on-chain
commitment combines with the off-chain platform data to make that case.

This is a design/evidence note, not a legal opinion. It records the intended
evidentiary model so the Interface C and provisioning work are built to support
it. See also `LEDGER_DESIGN.md` (the netting ledger) and `CERT_TODO.md` /
`PROVISIONING_API.md` (key ↔ identity registration).

## The scenario

In an Open Corridor cycle, Bank A's customers instruct payments to beneficiaries
banked at Bank B. Each instruction is a **Promise** from Bank A to settle. Bank B
credits the beneficiaries immediately, in reliance on those promises. At
settlement, the promises are netted and Bank A owes Bank B a net amount **X** —
which Bank A does not pay. Bank B sues for X.

## The two data layers

Neither layer alone is sufficient. The case is built by **joining** them.

- **On-chain (Cardano).** Per promise, a transaction **signed by Bank A's wallet
  key**, whose metadata holds `{schema, commitment, ts}`, where
  `commitment = SHA-256(salt ‖ canonical_instruction)`. The `canonical_instruction`
  (built by the Bank Node dispatcher) binds:
  - `transaction_request_id`
  - `originating_bank_id` (Bank A) and `originating_account_id` (A's settlement account)
  - the full A1.1 `instruction`: amount, currency, beneficiary routing
    (bank + account), originator (the upstream payer's name/address/account), description

  Only the commitment hash reaches the chain — **never cleartext amounts or PII**
  (see the hash-only privacy decision). The salt is held off-chain.

- **Off-chain (OBP-API platform DB / REST).** The cleartext of all of the above,
  **plus the salt**, plus the double-entry ledger (`PROMISED → NETTED → SETTLED`),
  plus the netting snapshot computing the net position.

## What the hash does and does not prove

A SHA-256 commitment, signed by Bank A's key and timestamped in a block, proves:

- **Integrity** — change any term (e.g. one digit of the amount) and the hash no
  longer matches; Bank A cannot argue the terms were different.
- **Non-repudiation / authorship** — the transaction is signed by Bank A's key,
  so Bank A cannot deny it issued the commitment. *(This rests entirely on the
  key being bound to Bank A's legal identity — see Gap 1.)*
- **Existence in time** — the block timestamps the commitment, defeating any
  claim that it was fabricated after the fact.

It does **not** prove: that Bank B actually paid the beneficiary (that is Bank B's
own records — see the proof matrix), that the instruction was validly authorised
inside Bank A, or the legal characterisation that a Promise is a binding debt
(that is the framework agreement). The chain is a **notarisation / non-repudiation
layer** — necessary, not sufficient.

## Commit–reveal: verifying a single promise

1. Bank B (or its lawyer) obtains the cleartext instruction **and the salt**.
2. They recompute `SHA-256(salt ‖ canonical_instruction)` → `H`.
3. They locate the Cardano transaction containing `H`, at block time `T`, signed
   by Bank A's registered key.
4. Conclusion: *Bank A, at time T, cryptographically attested to an instruction
   with exactly these terms.*

The verification is **reproducible by anyone** — "run this and see," not "trust
our database." This is the property that converts a he-said/she-said over mutable
records into an objective, independently checkable fact.

## Proof matrix — what proves each element of the claim

| Legal element | Artifact | Where it lives |
|---|---|---|
| A Promise is a **binding settlement obligation** | Scheme / membership framework agreement | Contract (off-system) |
| Bank A **made these specific promises** | Cardano txs signed by A's key, committing to `Hᵢ` at `Tᵢ` | Public chain |
| The promises had **these exact terms** | Cleartext instruction + salt → recompute `Hᵢ` | OBP-API DB (+ salt) |
| Bank A's **key = Bank A** | Registration / onboarding records binding cert/wallet to A | OBP-API / CA (`CERT_TODO.md`) |
| Bank B **performed** (paid the beneficiaries) | Bank B's CBS / payment records | **Bank B's own systems** |
| The **net owed** = X | Netting snapshot (Record 2 + OBP ledger) | OBP-API (+ chain anchor) |
| Bank A **did not settle** | Absence of Record 5 / settlement tx; ledger stuck at `NETTED` | Chain absence + OBP ledger |

The claim is for the **net** of a cycle. Netting already credits Bank A for any
promises running the other way, so the figure is clean: "after netting both
directions, Bank A owes Bank B X, unpaid."

## How the combined data answers Bank A's likely defenses

| Bank A's defense | Answered by |
|---|---|
| "We never promised." | Bank A's own signature on the chain transaction. |
| "The terms were different." | The reveal — any altered term breaks the hash match. |
| "You backdated / fabricated it." | The block timestamp; the commitment provably existed by time T. |
| "The netting was wrong." | The snapshot draws on **both** banks' anchored promises; it is recomputable. |
| "Bank B never actually paid the beneficiary." | **The chain does not answer this** — proved only by Bank B's own CBS records (see Gap 3). |
| "Our key was compromised / stolen." | Addressed contractually + technically — see Gap 1. |

The chain's role is to make Bank A's denials **cryptographically untenable**, not
to win the contract case by itself.

## Load-bearing gaps in the current build

The architecture supports this evidentiary theory, but two pieces are **legally
load-bearing and not yet implemented**. A competent defendant's lawyer would
drive straight through them.

### Gap 1 — Key ↔ legal identity (NOT built)

The entire non-repudiation argument rests on "this Cardano wallet key is Bank A's."
That binding is **not intrinsic to the chain** — it needs the registration / CA
records (`CERT_TODO.md` / `PROVISIONING_API.md`, not yet built). Until then,
"prove this key is ours" is open, and "our key was compromised" is a live defense.

Mitigations: HSM-held signing keys; a signed onboarding agreement registering the
key/cert to Bank A; and scheme rules that **contractually allocate key-security
risk to the key holder** (signatures from your registered key bind you).

### Gap 2 — Salt custody (NOT built)

Today the salt lives only in **Bank A's** outbox (the `commitment_salt` column).
If the defendant holds the only copy of the salt, Bank B cannot open the
commitment without Bank A's cooperation or a disclosure order — which guts the
scheme in exactly the adversarial situation it exists for.

Fix: the salt must reach Bank B with the credit notification (**Interface C**)
**and/or** be escrowed at the neutral OBP-API platform. This is a correctness
requirement, not a nicety. (Tracked against the Promise commitment design;
the salt is currently generated and stored Bank-A-side only.)

### Gap 3 — Proof of Bank B's performance (out of system)

"Did Bank B really pay the beneficiary?" is the one element the anchor does not
cover. It is proved by Bank B's own CBS / payment records today. Optional
hardening: **bilateral attestation** — Bank B writes its *own* signed commitment
("I credited the beneficiary; hash of proof = …", the Record-4/ack counterpart),
so both the obligation and the performance are independently anchored.

## What would make the case close to airtight

- **A framework agreement that deems the platform + on-chain records conclusive
  evidence** absent manifest error — the highest-leverage move, removing most
  authenticity / admissibility fights before they start.
- **OBP-API (TESOBE) as the neutral third party** holding cleartext + salt, so
  Bank B's evidence comes from an independent operator, not from the defendant.
- **Bilateral attestation** (Gap 3) to close the performance gap.
- **HSM + registered keys** (Gap 1) so authorship is not seriously contestable.

## Bottom line

A lawyer for Bank B wins this not by asserting "the blockchain says so," but by
filing an **evidence bundle**:

> framework agreement + Bank A's signed on-chain commitments + the platform's
> cleartext-and-salt reveal + Bank B's payment records + the netting snapshot

where the chain's job is to make Bank A's denials cryptographically untenable.
Closing **Gap 1 (key registration)** and **Gap 2 (salt availability to Bank B)**
is what moves this from "compelling" to "watertight."
