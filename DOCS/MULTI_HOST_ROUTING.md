# Paying across OBP hosts — design sketch

Status: a design for discussion, revised on 2026-11-23 (onward routes, and Bank C's acceptance on the chain as the proof that makes leg 1 final). Section 4 is built: a Bank Node can be on several OBP hosts (the `hosts:` config, a client, consumer and watchers per OBP host, rows tagged with their OBP host, and the `/obp-bank-node/v5.1.0/hosts/HOST/banks/BANK_ID/...` URLs). A first version of forwarding is built too, without the conditional promise and with the sending bank naming the hub: see section 9. Onward routes (section 5), routing by OBP-API (section 6) and the conditional promise (section 8) are not built yet. It extends the "Multi-host posture" section of `ARCHITECTURE.md`, which commits the identifiers and records that make this possible but deliberately builds no federation machinery.

The reasoning behind the design, in plain terms, is on the Bank Node UI's `/rationale` page.

## 1. The problem

Several organisations may each run their own OBP host: one OBP-API plus the Bank Nodes of its member banks. Each operator builds its own relationships with banks in its own region, and no operator can, or needs to, sign up the other's banks. A bank on OBP host A should still be able to pay a customer of a bank on OBP host B.

This design does that without linking the OBP hosts to each other. There is no shared directory, no trust between OBP hosts and no API between them. The link is a bank that is a member of both.

## 2. Terms

- **OBP host**: one OBP-API and the Bank Nodes of its member banks. A complete, self-contained deployment, named by the host of its OBP-API.
- **Network**: the OBP hosts that can reach each other through hubs. An OBP host is not a network (`ARCHITECTURE.md`).
- **Member**: a bank listed in an OBP host's getBanks. Its operator has onboarded and checked it under that host's rules.
- **Hub**: a member of two or more OBP hosts that carries payments between them. It has a corridor on each OBP host and takes the credit and FX risk between the two legs of a payment.
- **Onward route**: a hub's statement, on its own bank record, that a given bank on another OBP host can be reached through it.
- **Beneficiary bank**: the bank of the customer being paid. In ISO 20022 terms, the creditor agent.
- **Next bank**: the bank a leg is paid to. For a payment through a hub, the hub. In ISO 20022 terms, the intermediary agent.
- **Leg**: one promise on one OBP host. A payment through a hub has two legs.

## 3. Principles

1. **One hop.** A payment crosses at most one hub, so each payment has two legs that two sets of rules cover. The conditional promise (section 8) composes, so longer routes could be added later with the same mechanism.
2. **getBanks lists members only.** A bank on another OBP host is never listed as a bank. It appears only inside a hub's onward routes. Everything that relies on getBanks meaning "listed on this OBP" keeps working.
3. **The hub declares its reach, and it expires.** A hub's own node confirms its onward routes from time to time. A route that is not confirmed again expires, so a hub that goes quiet drops out by itself. Nobody else adds or removes routes.
4. **Maximum visibility.** Onward routes are as visible as the bank itself, so regulators, sending banks and other hubs can see who reaches whom. Only bank-to-bank reach is shown, never customers or volumes.
5. **The payment names the real beneficiary.** `to` names the beneficiary bank. OBP-API picks the hub, within the sending bank's policy. The sending bank never writes the route into the payment.
6. **The payment path is deterministic.** Routing and conditional promises follow fixed rules and are fully audited. Judgement, whether by people or by agents, sits around the payment path: setting a hub's policy, watching scores, tuning limits. It never sits inside it.
7. **Operators stay in charge.** Each OBP host's operator decides which members may act as hubs, how long routes stay valid, and within what limits. Within those limits the network regulates itself.

## 4. One Bank Node, several OBP hosts

A hub runs one Bank Node connected to several OBP hosts, rather than one node per OBP host passing messages between them. Inside one process, handing a payment from one leg to the next is a database insert, using the same pattern the Interface C router already uses for return promises. There is also one place that decides when the first leg becomes final.

What becomes per OBP host:

- the OBP-API connection (Interface B);
- the RabbitMQ consumer (Interface C);
- the routing-scheme cache;
- the bank's `bank_id` and corridor accounts. `bank_id` is local to each OBP host, so a hub may have a different one on each. Its BIC is the same everywhere.

What stays shared:

- core banking delivery;
- the evidence store;
- the outbox, with each row tagged with its OBP host;
- the wallet and FX.

Configuration: the node lists the OBP hosts it is on, under `hosts:`. A bank on one OBP host has a list of one. Each OBP host is identified by the host of its `obp_api.base_url` (with the port, when the URL names one), so it needs no separate name.

```yaml
hosts:
  - obp_api: { base_url: "https://a.example", token_url: "...", client_id_env: "...", client_secret_env: "..." }
    rabbitmq: { host: "...", virtual_host: "/bank.hub.a" }
    bank: { bank_id: "hub.a", corridor_accounts: { USD: "settlement-usd" } }
  - obp_api: { base_url: "https://b.example", token_url: "...", client_id_env: "...", client_secret_env: "..." }
    rabbitmq: { host: "...", virtual_host: "/bank.hub.b" }
    bank: { bank_id: "hub.b", corridor_accounts: { USD: "settlement-usd", JMD: "settlement-jmd" } }
```

The node's REST API names the OBP host and the node's own bank on it, as OBP-API names a bank: `/obp-bank-node/v5.1.0/hosts/HOST/banks/BANK_ID/...`, for example `/obp-bank-node/v5.1.0/hosts/b.example/banks/hub.b/transaction-requests`. Everything after `/hosts/HOST` reads as it would on that OBP-API. `GET /obp-bank-node/v5.1.0/hosts` lists the OBP hosts the node is on.

Each OBP host gets its own workers, so a stuck queue on one OBP host cannot stall payments on another. The OBP-API contract test has to cover each connection, because OBP hosts may run different OBP-API versions.

## 5. Onward routes

A hub's bank record carries `onward_routes`, next to `bank_routings`. `bank_routings` says how the bank is addressed; `onward_routes` says which banks can be reached through it. Each route looks like a small version of a bank, with its own list of `bank_routings`, because a bank on another OBP host can be addressed in more than one way too.

Get Banks (`GET /obp/v6.0.0/banks`) and Get Bank (`GET /obp/v6.0.0/banks/BANK_ID`) show it with the rest of the bank, and `GET /obp/v6.0.0/banks/BANK_ID/onward-routes` returns `{"onward_routes": [...]}` on its own. v5.1.0 is stable, so its JSON does not change. On OBP host A (`localhost:8080`), with Bank B as the hub to Bank C on OBP host B:

```json
{
  "bank_id": "rt.bank.b4",
  "bank_code": "rt.bank.b4",
  "full_name": "Bank B",
  "logo": "",
  "website": "",
  "bank_routings": [
    { "scheme": "OBP", "address": "rt.bank.b4" },
    { "scheme": "BIC", "address": "DEMBZZ22" }
  ],
  "attributes": [
    { "name": "OPEN_CORRIDOR_PARTICIPANT", "value": "true" }
  ],
  "onward_routes": [
    {
      "full_name": "Bank C",
      "bank_routings": [
        { "scheme": "BIC", "address": "DEMCZZ22" }
      ],
      "obp_host": "localhost:8081",
      "currencies": ["KES", "NGN", "USD", "EUR"],
      "last_confirmed": "2026-11-23T10:00:00Z",
      "valid_until": "2026-11-23T11:00:00Z"
    }
  ]
}
```

Every bank has the field. It is filled in only for banks that currently forward payments; for every other bank it is an empty list. On OBP host B, Bank B carries the reverse route, to Bank A on `localhost:8080`.

**Who writes them.** Only the hub's own node, through an endpoint guarded by its own role or Scope, and only for its own bank. The node builds the list from its other OBP hosts: the Open Corridor participants there (getBanks), and the currencies it has a corridor account for. The hub's policy decides which of them it lists. Neither the operator nor another bank writes a hub's routes.

**Validity.** The node only reports "confirmed now". OBP-API stamps `last_confirmed` and works out `valid_until` from a period the host sets, with a default (section 13). A hub cannot give itself a longer validity than the host allows. Expired routes drop out of every response, so readers never check validity themselves. A route is in effect a cache of the hub's answer to "can you reach this bank?". A hub that goes quiet stops confirming, and its routes expire: expiry turns silence into withdrawal.

**Why a field and not bank attributes.** Bank attributes are flat name/value strings, so a route would be JSON inside a string. OBP-API could not check it or search it, and the node's automatic updates would sit among the attributes the operator edits by hand.

**One bank per routing pair.** Within one OBP host, a scheme and address should identify one bank. Two hubs can both list the same BIC, and that gives the sending bank a choice. If their entries for it disagree, for example with different LEIs, the data contradicts itself, and OBP-API refuses to choose rather than guess, as it already does when a bank's BIC in its routing pair and its `swift_bic` do not match.

## 6. Choosing a hub

The sending bank's payment names the beneficiary bank honestly: `to` carries the beneficiary bank's own scheme and address, and the beneficiary's account. OBP-API then:

1. looks for a member with that pair in `bank_routings`. If there is one, it is a direct payment, as today;
2. otherwise looks for members with an unexpired onward route carrying that pair, for the payment's currency;
3. keeps only the hubs the sending bank's standing policy allows: hubs it will not use, and countries or OBP hosts it excludes, for example for sanctions;
4. ranks what is left by the hub's score (section 7), and sends leg 1 to the best one;
5. records the beneficiary bank and the next bank separately on the transaction request.

If no hub fits the policy, the payment is refused with a clear reason: no route, or no route the policy allows.

The bank sees the outcome and the route used, but is not asked to choose. This keeps re-routing automatic: a person or a core banking system in the loop for every choice would stop the network healing itself.

**Bank confirms mode.** A bank, or a whole OBP host, can switch automatic choice off. OBP-API then returns the hubs that fit the policy, and the sending bank confirms one before leg 1 is sent. Re-routing after a failure then also waits for the bank. This is for markets where a regulator wants the bank to make each routing choice itself (section 15). An OBP host can start in this mode and move to automatic choice later.

## 7. How the network heals itself

- **Routes expire.** A hub that leaves an OBP host, or whose node stops confirming, drops out within one validity period, without anyone noticing or deciding.
- **The payment is the final test.** A route can be stale within its validity. Leg 1 is conditional (section 8), so a hub that can no longer reach the beneficiary bank rejects it, and that costs delay, not money.
- **Failure re-routes.** If a hub refuses, lets leg 2 time out, or its node reports unhealthy, OBP-API cancels leg 1 and tries the next hub that fits the policy, within the payment's deadline. The sending side remembers a failed route for a short while, so it does not keep trying the same dead hub.
- **Hubs earn a score.** Each OBP host scores its hubs from what actually happens: the share of legs completed, the time to credit the beneficiary, and settling on time. The score feeds route choice, so an unreliable hub loses traffic without anyone deciding to remove it.
- **New hubs get a chance.** A hub with no history gets a small share of the traffic its routes fit, so it can earn a score. Without this, the first hub would keep all the traffic forever.

Operators and agents work around this loop, not inside it. They can see the routes, scores and failures, change which members may act as hubs, and change limits. An agent such as Senti or Opey could watch the scores and propose or make changes within limits the operator sets.

## 8. Two legs, completing together

A hub payment is two promises chained together, each made on its own OBP host under that host's rules. Each leg is an ordinary Open Corridor promise: a commitment on the ledger that the receiving bank can check, and a credit notification to the receiving bank's node. What is new is the link between them, and when each becomes final.

A payment from Bank A (OBP host A) to a customer of Bank C (OBP host B) through Bank B:

1. **Bank A sends.** `to` names Bank C and the customer's account. OBP-A chooses Bank B (section 6).
2. **Leg 1, conditional, on A.** Bank A promises Bank B. The promise carries the full instruction (the beneficiary bank, the account, the amount and the originator's details) and is marked conditional, with a deadline: it becomes final only if Bank C accepts leg 2 before then.
3. **Bank B decides.** Its node applies its own policy: sanctions screening of the beneficiary, its exposure to Bank A, its liquidity, and the FX rate if the currency changes. It can refuse here, and nothing has happened yet.
4. **Leg 2, on B.** Bank B's node inserts leg 2 into its OBP host B outbox: Bank B promises Bank C. Leg 2 names leg 1 (its OBP host, transaction_request_id and commitment), carries the same originator details, as the FATF Travel Rule requires, names Bank C's acceptance key, and has a shorter deadline than leg 1. Leg 2 itself is an ordinary, unconditional promise.
5. **Bank C accepts, on the chain.** Bank C credits its customer at promise time, as for a direct payment. Its node then posts an acceptance on Cardano: a transaction whose metadata references leg 2's commitment and is signed by the acceptance key leg 2 names. It carries only hashes, like the commitments: no customer, account or amount.
6. **Leg 1 becomes final.** Leg 1 is final if, and only if, Bank C's acceptance of leg 2 is on the chain, safely settled, before leg 1's deadline. OBP host A learns of it either by watching the chain itself, or from a node that points it to the transaction, which OBP-A then checks on the chain before acting. Bank B passes nothing back.
7. **Settlement.** A and B net and settle under OBP host A's rules; B and C under OBP host B's, possibly on a different rail. A and C never settle with each other. OBP-API holds leg 1 out of netting until it is final.

**Where the risk sits.** Bank C carries no risk from Bank A: it only deals with Bank B's promise. Bank A carries no risk from Bank C: leg 1 is not final until Bank C has accepted. Bank B carries the gap: once it sends leg 2, it owes Bank C whatever happens to leg 1. That is the hub's job, and what it is paid for.

**The acceptance is on the chain, and only there.** The two OBP hosts never talk to each other, but they read the same ledger, so Bank C can tell Bank A something without any link between the hosts and without relying on Bank B. Every party checks the same record and gets the same answer. Whether the acceptance came before the deadline is decided by the block it is in, not by anyone's logs. The acceptance is evidence, not a promise: Bank C takes on no obligation towards Bank A by posting it.

**Why Bank A can trust Bank C's signature.** Bank A has no relationship with Bank C and does not know its keys. Bank B vouches: leg 2's commitment names Bank C's acceptance key, and leg 1's condition is "an acceptance of leg 2, signed by the key leg 2 names". Bank B commits to the key before anything happens, and Bank A trusts Bank B under host A's rules, so Bank A never has to trust Bank C directly. Lightning and Interledger make the condition a hash whose secret the receiver reveals instead, but that needs the receiver to hand out the hash before the payment starts. Here Bank A has no contact with Bank C beforehand.

**Deadlines must be in order.** Leg 2's deadline falls before leg 1's, with room for the acceptance to be safely settled on the chain (a block comes about every 20 seconds, and a transaction is safely settled after several more, so a few minutes). Otherwise Bank C could accept leg 2 just after leg 1 lapsed, and Bank B would owe Bank C with no claim on Bank A. The Lightning Network's hash time-locked contracts and Interledger use the same ordering.

**The chain becomes a dependency for hub payments.** If Cardano is congested or down, acceptances are late and legs lapse. That costs delay, not money, and the deadlines need room for it. Each acceptance is one transaction fee, paid by Bank C; at volume it can post one transaction covering many acceptances (a Merkle root).

**What can go wrong:**

| What happens | Result |
|---|---|
| Bank B refuses leg 1 | Leg 1 is rejected. Nothing is final, and Bank A tells its customer, or OBP-A tries another hub. |
| Bank C refuses leg 2 | Bank B rejects leg 1 with Bank C's reason. Nothing is final on either OBP host. |
| Bank B goes quiet after receiving leg 1, before sending leg 2 | Leg 1 lapses at its deadline. Bank A loses only time. |
| Bank B goes quiet after sending leg 2 | Nothing changes: Bank C's acceptance on the chain makes leg 1 final without Bank B. |
| Bank C's acceptance is settled on the chain after leg 1's deadline (for example, the chain is congested) | Leg 1 lapses, and Bank B still owes Bank C. The ordered deadlines leave room for this, so it means Bank C accepted very late. |
| A later return from Bank C | It runs as two new legs the other way: C to B on OBP host B, then B to A on OBP host A. |

**What is new to build.** The conditional promise and its deadline; Bank C's acceptance on the chain, and watching for it; an outbox state for "promised, waiting for the next leg"; and OBP-API holding leg 1 out of netting until it is final. The commitment preimage must cover the condition and the deadline, and for leg 2 also leg 1's commitment and Bank C's acceptance key.

**For the agreements.** Each OBP host's rulebook needs a short section on conditional promises: what makes one final (an acceptance of the next leg on the chain, safely settled before the deadline), what counts as that acceptance (signed by the key the next leg names, referencing the next leg's commitment), and who carries what when a deadline passes. The hub's own agreements on each side cover its fees, FX and screening duties. Neither OBP host needs to know the other's rules.

## 9. What is built: forwarding without the conditional promise

This first version came before the design above, and will be replaced by it. The sending bank names the hub itself, because OBP-API does not route by onward routes yet.

1. **Leg 1.** P's node sends an ordinary promise to the hub H on OBP host A. `to` names H (for example by BIC), with the final beneficiary's account and name. An optional field, `final_bank_routing`, names the final bank on the other OBP host, for example `{"scheme": "BIC", "address": "DEMCZZ22"}`. The field is part of the instruction P's node hashes, so it reaches H in the promise preimage on the credit notification, and OBP-API needs no change. OBP-API ignores the extra field, and it does not check the beneficiary account (the beneficiary's bank does that).
2. **H forwards.** When H's node gets a verified credit notification whose instruction has `final_bank_routing`, it does not credit a customer. It looks the final bank up on its other OBP hosts (getBanks v6.0.0: an Open Corridor participant with that routing), and puts leg 2 into its outbox on that OBP host. Leg 2 is paid from H's corridor account for the currency there, to the final bank, with the same value, beneficiary, originator (the Travel Rule details) and charge policy. It names leg 1 in `previous_leg`: the OBP host, the transaction_request_id and the promise commitment. The dispatcher then makes leg 2 like any payment. A redelivered credit finds the leg 2 already made (one per leg 1, by idempotency key).
3. **Records.** H's evidence row for leg 1 has the CBS delivery status `FORWARDED`, and names leg 2 and its OBP host (`forwarded_as_transaction_request_id`, `forwarded_on_obp_host`). The Bank Node UI shows "carried on as … on …" on the credits page, and the Send form has an optional "final bank" field.
4. **No route.** If no other OBP host of H's has the final bank, or H has no corridor account there for the currency, leg 1 is returned to the originator, as for a credit the CBS refuses (status `NOT_FORWARDED`). If an OBP host cannot be asked (its OBP-API fails), the credit is refused as transient, and OBP-API delivers it again later.

How it differs from the design:

- **`to` names the hub, and the sending bank chooses the route.** In the design, `to` names the beneficiary bank and OBP-API chooses (section 6). `final_bank_routing` goes when that is built.
- **No conditional promise.** Leg 1 is accepted as soon as H receives it, before leg 2 is accepted, so Bank A carries H's risk. If C refuses leg 2, C returns it to H, and H's CBS is credited; the return does not travel on back to P.
- **No hub fees or currency conversion.** Leg 2 has the same value and currency as leg 1.
- **No hub policy.** H carries any payment that names a final bank it can reach. Limits per sending bank, per currency and per day are not applied.

In the local demo, Bank B (rt.bank.b4) is on both OBP hosts and is the hub: A pays B directly on OBP host A, and pays C on OBP host B through B.

## 10. Visibility

- getBanks still lists only members. No OBP host lists another's members as banks.
- A hub's onward routes are visible to anyone who can see the bank. Regulators can see which routes exist. Sending banks can screen a route before paying: which hub, and which OBP host and jurisdiction a payment would pass through. Hubs that reach the same bank compete in the open.
- Only bank-to-bank reach is shown, and bank names and BICs are public anyway. No customers, amounts or volumes are.
- Each hub decides which of the banks it can reach it lists. Leaving one out is its business.

## 11. Operator controls

- Which members may act as hubs, and so may write onward routes: a Scope, or a bank attribute set by the operator.
- How long a confirmed route stays valid: a host setting, with a default (section 13).
- Limits on hub traffic per hub, per currency and per day.
- Visibility of routes, scores and failures.

## 12. What changes where

Bank Node:

- configuration as a list of OBP hosts, with per-OBP host workers (built);
- confirming its onward routes on each OBP host it is on, from its other OBP hosts and its hub policy;
- on a conditional leg 1: screening, then leg 2 with a shorter deadline, a reference to leg 1 and Bank C's acceptance key, or rejecting leg 1 with the reason;
- on the beneficiary side: posting the acceptance of a leg on the chain, signed by its acceptance key;
- on the sending side: treating leg 1 as pending until it is final, and letting it lapse at its deadline;
- removing `final_bank_routing` and the forwarding of section 9, with no compatibility kept.

OBP-API:

- onward routes: storage keyed by bank_id, the endpoint the hub's node writes through, `onward_routes` in v6.0.0 Get Banks and Get Bank, `GET /banks/BANK_ID/onward-routes`, the validity setting, and filtering out expired routes;
- routing an Open Corridor payment whose beneficiary bank is not a member: the onward-route lookup, the sending bank's policy, the conflict check, and recording the beneficiary bank and the next bank separately;
- the conditional promise: the condition and deadline on leg 1, checking for leg 2's acceptance on the chain, holding leg 1 out of netting until final, and cancelling it on refusal or at its deadline;
- hub scores and the operator controls above;
- nothing for telling local corridor banks apart: that is already done. Each Open Corridor bank carries the bank attribute `OPEN_CORRIDOR_PARTICIPANT` = `true`, which v6.0.0 getBanks returns (v5.1.0's does not), and the setup page sets it.

## 13. Assumptions and open questions

Working assumptions, to be revisited once hubs carry real traffic:

1. **Route validity.** The host sets the period, with a default of one hour. A hub's node confirms well within it.
2. **Choosing between hubs.** OBP-API ranks hubs by their recent failure rate. A hub with no history is treated as average, and gets up to 5% of the traffic its routes fit until it has 50 completed legs. The operator can change both numbers.
3. **Who carries a hub's failure.** Once a hub has promised leg 2, it owes the beneficiary bank, as with any promise. If the hub then fails to settle, that is the hub's default towards the beneficiary bank, handled under OBP host B's rules. The sending bank's exposure is to the hub, not to the beneficiary bank. The rules of each OBP host must say this.
4. **Hubs netting between themselves.** Not in the first version. Each leg nets and settles within its own pair on its own OBP host.

Not decided yet:

1. When the sending bank's customer sees the payment as complete: when leg 1 becomes final, or earlier, when the hub accepts leg 1.
2. When a hub that converts currency fixes the rate: when it accepts leg 1, so the sending bank knows the exact amount that will arrive before anything is final.
3. Where the sending bank's standing policy (hubs it will not use, excluded countries and OBP hosts) is stored.
4. How hub fees are shown and charged: on the route, per payment, or by the charge policy.
5. Whether OBP-API watches the chain for acceptances itself, or a node points it to the transaction and OBP-API checks it.
6. How many blocks count as safely settled, and so how far apart the two deadlines must be.

Settled: the acceptance of leg 2 is posted on the chain by Bank C, and that is the only proof that makes leg 1 final (section 8). A late acceptance does not count: the block it is in decides.

## 14. The Bank Node UI

The Bank Node UI already shows the OBP host each node is on: each node lists its OBP hosts on `/obp-bank-node/v5.1.0/status` (a list of one today), the UI has a tab per OBP host, and the header groups the nodes by OBP host. A hub node appears under each of its OBP hosts.

The Network page (`/network`) draws the Open Corridor banks the UI's nodes can see, one box per OBP host, with the payments between them. A node sees every Open Corridor bank on the OBP hosts it is on, so a hub's UI shows both OBP hosts, with the hub's two banks (the same BIC) joined. Once onward routes are built, the page can also show, from Get Banks alone, the banks on other OBP hosts that each hub reaches.

## 15. Regulatory notes

These notes are a general view for design purposes, not legal advice. Each operator needs advice from local counsel in each jurisdiction it serves before enabling routing through hubs.

**What changes.** Choosing which intermediary carries a payment is what a payment switch does. Each OBP host already routes interbank requests to the receiving bank it is given, and its operator may already need a payment system or switching licence for that, with or without hubs. Hub selection adds one thing: the OBP host now chooses an intermediary, not just delivers to a named recipient.

**What regulators usually look for once a system makes routing choices:**

- **Fair access and published rules.** Hubs are chosen by published, non-discriminatory rules, with no hidden preference, for example because the operator earns more from one hub than another.
- **Explainability.** Every choice can be reconstructed afterwards: the routes available, the sender's policy, the scores, and why the chosen hub won.
- **Transparency.** Onward routes are visible (section 10), so a regulator can see who reaches whom without asking.
- **Resilience.** A routing system that many banks depend on is more likely to come under oversight. A systemically important one may be expected to follow the CPMI-IOSCO Principles for Financial Market Infrastructures.
- **Financial crime.** Sanctions and AML checks stay with the banks. Because routing crosses borders, a sending bank must be able to exclude hubs and jurisdictions in its policy, and the Travel Rule details must travel on both legs (section 8).
- **Pricing.** The hub's fee and FX rate are interbank prices. Keeping them separate from what banks charge their customers keeps the operator out of customer pricing.

**How the design keeps the operator's role narrow:**

1. **The bank decides.** OBP-API applies the sending bank's own standing policy (section 6), so the route is chosen by the bank's instruction, carried out by fixed rules. The operator makes no discretionary routing decision of its own.
2. **Principals stay principals.** The sending bank is the principal on leg 1 and the hub on leg 2, as in correspondent banking. No funds pass through the operator or its platform.
3. **The hub vouches for its own reach.** A route is the hub's statement on its own record. The operator vouches for its members, not for banks on other OBP hosts.
4. **Everything is recorded.** The routes available, the policy applied and the choice made are kept in the audit trail, so every route can be explained.
5. **Bank confirms mode.** Where a regulator wants the bank to make each choice, automatic choice is switched off (section 6).
6. **Software supplier and operator are separate.** The company that runs an OBP host is its operator. TESOBE supplies the software and is not the operator of any OBP host.
