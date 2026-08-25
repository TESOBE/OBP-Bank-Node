# Joining Open Corridor — bank-facing copy

Text for a non-technical / decision-maker audience describing what it takes
for a bank to connect.

---

Any bank can join Open Corridor using its existing systems and a small tech
team. The connection point is the OBP Bank Node — an open-source gateway each
bank runs on its own infrastructure, under its own control.

- **Open code, open logs** — everything the node does can be inspected and
  audited.
- **Outbound only** — the node dials out to the Open Corridor platform;
  nothing from outside connects into the bank's network.
- **No core rebuild** — the node connects to the systems a bank already runs.
- **No resident engineers** — banks operate the node themselves.
- **Support when wanted** — integration help is available for banks that
  need it.

**What integration involves:** two touchpoints, both inside the bank's
network. The bank's systems call the node's API to send payments, and the
bank provides one interface the node calls to credit incoming payments into
its core system.
