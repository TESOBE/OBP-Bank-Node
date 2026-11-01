# OBP-Bank-Node as a Platform App — parked (2026-11-01)

Decision for now: **the nodes keep DirectLogin** as their service users (`rt.node.a`, `rt.node.b`, tokens
from `dev/setup_obp.sh` in `dev/env.sh`). Nothing in the node's auth or OBP-API's authModes changes.
This note records why, so we can pick it up again.

## What the node calls on OBP-API

| Call | Where | Authorised by |
|---|---|---|
| `POST /obp/v7.0.0/banks/B/accounts/A/owner/transaction-request-types/OPEN_CORRIDOR_PROMISE/transaction-requests` | `dispatcher.rs` step 1, per payment | Account access: the `owner` view of the corridor account. No Role, default authMode (UserOnly) |
| `POST …/transaction-requests/TR_ID/open-corridor/promise` (report-back) | `dispatcher.rs` step 3 | `CanAttachOpenCorridorPromise` at the bank, UserXORApplication |
| `POST` / `GET /obp/v7.0.0/banks/B/open-corridor/settlements[/ID]` | `rest/handlers.rs` (settle request) | `CanSettleOpenCorridor` at the bank, UserXORApplication |
| `GET /obp/v7.0.0/routing-schemes` | `routing.rs` refresher | None |

The node already supports OAuth2 client credentials (`obp_api.token_url`, `client_id`, `client_secret` in
its config, `ObpAuth::ClientCredentials` in `obp_client.rs`). The Scope-based calls would work as a
Platform App today, with one Consumer per node so that node A can't use Scopes at bank B. The blocker is
the first call: creating the Transaction Request is a **payment initiation** on the bank's corridor
account, on behalf of the customer whose payment the node relays (the A1.1 `originator`).

## Options considered and set aside

- **Give the Consumer's own user account access.** With OBP-OIDC client credentials, OBP-API has a user
  for the app (provider `obp-oidc#client`, `idGivenByProvider` = the consumer key). Making that user the
  owner of `settlement-a` would work without an OBP-API change, but the same principal would then be the
  *application* in role checks (`isApplicationActingAsItself`) and a *user* in account access. Its
  identity also follows the consumer key: rotate the key and the account access is gone. Not comfortable
  relying on it, and not clearly better than DirectLogin.
- **A new Role + Scope for creating OPEN_CORRIDOR_PROMISE** (UserXORApplication). Any Consumer granted the
  Scope could then initiate payments from a bank's corridor account without account access. Too broad
  for payment initiation.
- **Consent.** Rejected: Consents are given by people, but here a Consumer is acting, not a person on
  whose behalf it acts.

## The idea to come back to

**Account access granted to a consumer_id**, alongside account access granted to a user. Then "who may
initiate payments from this account" stays on the account (as it does for users today), and the actor is
the Consumer itself, without a stand-in user. That fits attribution following authorization: action →
account access → the Consumer it was granted to (and who granted it).

Questions for when we do:

- How OBP-API would model it: a view grant to a Consumer, next to the existing view grants to users.
- Who may grant it (the account holder, a bank role), and how it shows in the account's access list.
- How the view check reads when the caller is an application acting as itself, under UserXORApplication.
- Then the node becomes a Platform App: client credentials, declared Scopes for the report-back and
  settlement calls, and account access for its own Consumer on its corridor accounts.

User-initiated actions such as netting stay with people calling OBP-API directly (or through another app
that uses OAuth), since a Platform App can't start an OAuth/OIDC login.
