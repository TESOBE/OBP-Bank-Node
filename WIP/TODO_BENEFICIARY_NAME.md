# TODO: beneficiary name collision on counterparty creation

Found 2026-08-20 during round-trip testing. No changes made yet.

## Symptom

Paying a beneficiary whose **name** was already used on this sending account
but with a **different account routing address** never completes. The outbox
row sits at `INITIATED` and the dispatcher retries forever:

```
OBP-API returned 404 Not Found: OBP-30072: Could not create the Counterparty..
<- OBP-30014: Counterparty already exists. Please specify a different
COUNTERPARTY_NAME(Phil Duke) for the BANK_ID(rt.bank.a3) and
ACCOUNT_ID(settlement-a) and VIEW_ID(owner)
```

Same name + same account number is fine (counterparty reused). New name +
same account number is fine (routing lookup matches, old record reused).
Only reused-name + new-account-number fails.

## Root cause (two defects)

1. **OBP-API** — `OpenCorridorProcessor.create` resolves the beneficiary via
   `getOrCreateCounterparty` (`LocalMappedConnector.scala:1655`). Lookup is
   by routing; a new account routing address falls through to *create*, and
   counterparty names are unique per (bank_id, account_id, view_id), so an
   already-used name collides → OBP-30014, returned with HTTP **404**.
   Two people with the same name, or one person with two accounts, is
   legitimate — the collision is a modelling gap: the counterparty name
   doubles as a view-scoped unique label *and* as the Travel Rule
   beneficiary name, which is not unique in the world.
2. **Bank Node** — `classify_failure` (`obp_client.rs:527`) treats only
   400/422 with an `OBP-` code as a terminal business rejection; 404 is
   assumed to be a wrong/misconfigured endpoint and retried. A business
   rejection arriving with status 404 therefore loops forever instead of
   parking as EXCEPTION with a reason.

## Fix options

- **OBP-API (preferred):** on name collision in `getOrCreateCounterparty`,
  de-duplicate the internal label (e.g. suffix the account routing address:
  `Phil Duke (876283789)`). The legally meaningful beneficiary name is
  preserved verbatim in the TR body / originator payload regardless — the
  counterparty name is only the view-scoped address-book label. Also return
  400 (or 409), not 404, for this failure.
- **Bank Node:** treat a 404 whose body carries a real business code
  (anything other than `OBP-10404`, OBP's genuine no-such-URI code) as
  terminal → row becomes EXCEPTION with `exception_reason` visible in the
  UI instead of a silent retry loop. Worth doing even after the OBP-API
  fix, as defence against other mis-statused business rejections.
- **To evaluate (Simon's idea):** carry the beneficiary name in/alongside
  the account routing — e.g. include the name in the counterparty
  uniqueness key, or a secondary routing acting as a name qualifier — so
  the model distinguishes "same label" from "same destination" instead of
  the label collision existing at all.

## Workaround until fixed

Keep name ↔ account-number pairs stable. A new account routing address
needs a name not used before on that sending account. Never reuse a name
with a new account number.

## Cleanup

Three rows from the discovery are still retrying on node A
(`dev/data/node-a3/outbox.db`, status `INITIATED`, ids `4f216440-…`,
`92b0caa2-…`, `37c9e513-…`, KES 222.00 to Phil Duke/876283789). Clear with
an `UPDATE outbox SET status='EXCEPTION', exception_reason='…'` (stop node
A first), or leave them to fail properly once the node-side fix lands.
