//! The `lapin` transport shell for Interface C.
//!
//! Connects to the bank's RabbitMQ vhost, consumes `obp_rpc_queue`, hands each
//! delivery to the [`Router`], and publishes the reply envelope to the AMQP
//! `replyTo` queue (correlated by `correlationId`). All message *logic* lives in
//! the router; this file is just connect → consume → reply → ack.
//!
//! Runs against a real broker only, so it has no unit tests — the router it
//! drives is exhaustively tested instead.

use std::sync::Arc;

use anyhow::Context;
use futures_util::StreamExt;
use lapin::options::{
    BasicAckOptions, BasicConsumeOptions, BasicPublishOptions, QueueDeclareOptions,
};
use lapin::types::FieldTable;
use lapin::{BasicProperties, Connection, ConnectionProperties};
use tracing::{error, info, warn};

use super::Router;

pub struct ConsumerConfig {
    /// Full AMQP URI including the per-bank vhost, e.g.
    /// `amqp://user:pass@host:5672/%2fbank.ke.01.kcs`.
    pub uri: String,
    pub queue: String,
    pub consumer_tag: String,
}

/// Connect and consume forever. Returns only on a fatal connection error or when
/// the broker closes the stream; the caller decides whether to restart.
pub async fn run(config: ConsumerConfig, router: Arc<Router>) -> anyhow::Result<()> {
    // Drive lapin on the existing tokio runtime rather than its default executor.
    let props = ConnectionProperties::default()
        .with_executor(tokio_executor_trait::Tokio::current())
        .with_reactor(tokio_reactor_trait::Tokio);

    let conn = Connection::connect(&config.uri, props)
        .await
        .with_context(|| format!("connecting to RabbitMQ ({})", redact(&config.uri)))?;
    let channel = conn
        .create_channel()
        .await
        .context("creating AMQP channel")?;

    channel
        .queue_declare(
            &config.queue,
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .with_context(|| format!("declaring queue {}", config.queue))?;

    let mut consumer = channel
        .basic_consume(
            &config.queue,
            &config.consumer_tag,
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .with_context(|| format!("consuming from {}", config.queue))?;

    info!(queue = %config.queue, "Interface C consumer started");

    while let Some(delivery) = consumer.next().await {
        let delivery = match delivery {
            Ok(d) => d,
            Err(e) => {
                error!(error = %e, "Interface C: delivery error");
                continue;
            }
        };

        let message_id = str_prop(delivery.properties.message_id());
        let correlation_id = str_prop(delivery.properties.correlation_id());
        let reply_to = delivery
            .properties
            .reply_to()
            .as_ref()
            .map(|s| s.as_str().to_string());

        let reply = router
            .handle(&message_id, &correlation_id, &delivery.data)
            .await;

        match &reply_to {
            Some(reply_to) => match serde_json::to_vec(&reply) {
                Ok(bytes) => {
                    let rprops = BasicProperties::default()
                        .with_correlation_id(correlation_id.clone().into());
                    if let Err(e) = channel
                        .basic_publish("", reply_to, BasicPublishOptions::default(), &bytes, rprops)
                        .await
                    {
                        error!(error = %e, %reply_to, "Interface C: failed to publish reply");
                    }
                }
                Err(e) => error!(error = %e, "Interface C: failed to serialize reply"),
            },
            None => warn!(%message_id, "Interface C: message had no replyTo; not replying"),
        }

        if let Err(e) = delivery.ack(BasicAckOptions::default()).await {
            error!(error = %e, "Interface C: failed to ack delivery");
        }
    }

    warn!("Interface C consumer stream ended");
    Ok(())
}

fn str_prop(prop: &Option<lapin::types::ShortString>) -> String {
    prop.as_ref()
        .map(|s| s.as_str().to_string())
        .unwrap_or_default()
}

/// Hide credentials in a logged AMQP URI: `amqp://user:pass@host` → `amqp://***@host`.
fn redact(uri: &str) -> String {
    match (uri.find("://"), uri.rfind('@')) {
        (Some(scheme_end), Some(at)) if at > scheme_end + 3 => {
            format!("{}://***@{}", &uri[..scheme_end], &uri[at + 1..])
        }
        _ => uri.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn redact_hides_credentials() {
        assert_eq!(
            redact("amqp://guest:secret@localhost:5672/%2fbank.x"),
            "amqp://***@localhost:5672/%2fbank.x"
        );
        // No credentials → unchanged.
        assert_eq!(
            redact("amqp://localhost:5672/%2f"),
            "amqp://localhost:5672/%2f"
        );
    }
}
