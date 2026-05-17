# OBP Bank Node — Interfaces

Five interfaces in total. **A1** and **A2** are the bank's surface area — what
the bank's CBS touches. **B**, **C**, and **D** are how the Bank Node talks to
the rest of the Open Corridor network on the bank's behalf.

The table takes the Bank Node's point of view: each row reads as
"the Bank Node [Action] in order to [Purpose]."

| Interface | Purpose | Action (Bank Node POV) | Protocol | Trigger | What flows |
|---|---|---|---|---|---|
| **A1** | Hand off outbound payment instructions from the bank to Open Corridor | **Receives** payment initiation and status queries from the bank's CBS | REST / JSON, localhost | Bank's customer initiates an outbound payment, or the bank queries an existing one | OBP SIMPLE Transaction Request payload (POST), or `transaction_request_id` lookup (GET) |
| **A2** | Notify the bank that an inbound payment must be posted to a customer | **Delivers** credit notification to the bank's CBS | webhook / Postgres write / file drop (bank picks one at config time) | A credit notification arrived via Interface C | Credit instruction with value, beneficiary, blockchain refs |
| **B** | Get the outbound payment onto the Open Corridor network | **Submits** Transaction Request to the OBP API (TESOBE-hosted); **fetches** counterparty state | HTTPS + OAuth2 | A payment from A1 has been persisted to the outbox | OBP SIMPLE Transaction Request payload, status queries |
| **C** | Receive instructions back from the network: credits to deliver, snapshots to record, settlements to post | **Receives** credit notifications, **netting snapshots**, **settlement instructions**, status updates from the OBP API via RabbitMQ (TESOBE-hosted) | AMQP over TLS, per-bank vhost | Long-running — connected at all times | OBP inbound-envelope messages dispatched by AMQP `MessageId` |
| **D** | Anchor the payment lifecycle in an immutable, independently-verifiable audit trail | **Writes** Promise / Settlement Reference / Exception records to Cardano via the bank's local `cardano-node` + Ogmios; **reads** chain state | JSON-RPC over WebSocket | Promise: after B submission. Settlement Reference: after C settlement instruction. Exception: after A2 retry exhaustion. | Metadata-only Cardano transactions |
