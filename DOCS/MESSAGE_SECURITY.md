# Message Security — bank-facing copy

Text for a non-technical / decision-maker audience, to accompany the Open
Bank Project message-security diagram (JWS, QSEAL, QWAC, …).

---

## Message security

Open Corridor is built on the Open Bank Project stack, which follows the same
security standards European regulators already require of open banking:

- **Encrypted connections** — everything travels over HTTPS;
  system-to-system links use mutual TLS, so both ends prove their identity
  before a byte of data moves.
- **Verified participants** — banks register using qualified digital
  certificates, the same technology behind the EU's eIDAS scheme
  (QWAC / QSEAL), issued under the corridor's own trust framework; the
  register of participating banks is operated with the regulator.
- **Signed messages** — payloads carry a digital signature (JWS), so any
  tampering is detectable and every message is attributable to its sender.
  Signature enforcement is an OBP-API configuration setting, applied per
  deployment.
- **User logins** — OAuth 2.0 / OpenID Connect with PKCE; FAPI, the
  strictest OAuth security profile for financial services, is supported.

## Trust framework

Banks are admitted against the regulator's published list of licensed
institutions — no new approval process. For the pilot the platform operates
the corridor register and issues certificates; both roles can pass to the
regulator or a national CA later, with no change to your systems.

## One-line version

Follows open banking security standards: HTTPS + mutual TLS · eIDAS-style
qualified certificates (QWAC/QSEAL) · signed messages (JWS) ·
machine-to-machine OAuth 2.0 · FAPI supported.
