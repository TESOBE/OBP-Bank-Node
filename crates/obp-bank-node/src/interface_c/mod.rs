//! Interface C — inbound message channel from OBP-API over RabbitMQ (AMQP).
//!
//! OBP-API is the initiator here: it publishes `obp_credit_notification`,
//! `obp_settlement_instruction`, `obp_netting_snapshot`, and `obp_status_update`
//! to the bank's `obp_rpc_queue` (on the bank's own vhost), and the Bank Node
//! consumes them, acts, and replies with an OBP inbound-envelope on `replyTo`.
//!
//! - [`types`] — the envelope + message bodies (and the salt-carrying fields on
//!   the credit notification).
//! - [`router`] — transport-free dispatch + handlers (unit-testable).
//! - [`consumer`] — the `lapin` shell that connects, consumes, and replies.

pub mod consumer;
pub mod router;
pub mod types;

pub use router::Router;
