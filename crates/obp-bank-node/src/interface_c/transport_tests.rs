//! Integration test of the Interface C *transport* — the `lapin` consumer
//! ([`super::consumer`]) against a real RabbitMQ broker.
//!
//! The router logic is unit-tested in [`super::router`]; what only a real
//! broker can prove is the AMQP wiring itself: queue declaration, dispatch by
//! the `MessageId` property, the reply envelope published to `replyTo` with
//! the `correlationId` carried over, acking, and that one bad message doesn't
//! stall the stream. This test publishes all four known message types plus a
//! malformed body and an unknown MessageId, then asserts every reply and the
//! evidence-store side effects.
//!
//! Ignored by default because it needs a broker:
//!
//! ```bash
//! docker run -d --name rabbitmq -p 5672:5672 rabbitmq:3-management
//! cargo test -p obp-bank-node -- --ignored interface_c_transport
//! ```
//!
//! Override the broker with `OBP_BN_TEST_AMQP_URI` (default
//! `amqp://guest:guest@localhost:5672/%2f`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use lapin::options::{
    BasicConsumeOptions, BasicPublishOptions, QueueDeclareOptions, QueueDeleteOptions,
};
use lapin::types::FieldTable;
use lapin::{BasicProperties, Connection, ConnectionProperties};

use super::consumer::{self, ConsumerConfig};
use super::types::{error_code, message_id};
use super::Router;
use crate::cbs::CbsClient;
use crate::evidence::EvidenceStore;
use obp_blockchain::PromiseRecord;

const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

fn amqp_uri() -> String {
    std::env::var("OBP_BN_TEST_AMQP_URI")
        .unwrap_or_else(|_| "amqp://guest:guest@localhost:5672/%2f".to_string())
}

/// Stub CBS accepting every credit with a fixed reference.
async fn spawn_stub_cbs() -> String {
    use axum::{routing::post, Json};
    let app = axum::Router::new().route(
        "/credit",
        post(|| async {
            Json(serde_json::json!({ "status": "ACCEPTED", "cbs_reference": "CBS-ITEST-1" }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/credit")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a local RabbitMQ on 5672 (or OBP_BN_TEST_AMQP_URI); run with --ignored"]
async fn interface_c_transport_round_trips_all_message_types() {
    let uri = amqp_uri();
    let unique = format!(
        "{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_micros()
    );
    let rpc_queue = format!("obp_rpc_queue_itest_{unique}");
    let reply_queue = format!("obp_reply_itest_{unique}");

    // ---- assemble the node side: evidence store, stub CBS, router, consumer.
    let evidence = EvidenceStore::connect_in_memory()
        .await
        .expect("evidence store");
    let cbs = CbsClient::new(spawn_stub_cbs().await, None, 5).expect("cbs client");
    // No settlement rail on this router: the settlement_instruction case below
    // asserts the NOT_CONFIGURED reply. The idempotency/finality path has its
    // own unit coverage in router.rs / settlement_store.rs / finality.rs.
    let router = Arc::new(Router::new("test-bank", evidence.clone(), cbs, None));

    let consumer_task = tokio::spawn(consumer::run(
        ConsumerConfig {
            uri: uri.clone(),
            queue: rpc_queue.clone(),
            consumer_tag: format!("itest-{unique}"),
        },
        router,
    ));

    // ---- test-side publisher + reply consumer.
    let props = ConnectionProperties::default()
        .with_executor(tokio_executor_trait::Tokio::current())
        .with_reactor(tokio_reactor_trait::Tokio);
    let conn = Connection::connect(&uri, props)
        .await
        .expect("connecting to RabbitMQ — is the broker up? (see test docs)");
    let channel = conn.create_channel().await.expect("channel");

    // Declare the rpc queue with the consumer's options (durable) so messages
    // published before the consumer attaches are parked, not dropped; the
    // consumer's own declare is then an idempotent no-op.
    channel
        .queue_declare(
            &rpc_queue,
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .expect("declare rpc queue");
    channel
        .queue_declare(
            &reply_queue,
            QueueDeclareOptions {
                exclusive: true,
                auto_delete: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .expect("declare reply queue");

    // ---- the message set: every known MessageId + malformed + unknown.
    let preimage = r#"{"instruction":"pay tr-itest-ok"}"#;
    let salt = "itest-salt-1";
    let commitment = PromiseRecord::compute_commitment(preimage.as_bytes(), salt.as_bytes());

    let credit_ok = serde_json::json!({
        "transaction_request_id": "tr-itest-ok",
        "value": { "currency": "KES", "amount": "100.00" },
        "originator": { "name": "Ayo Ndlela" },
        "promise_commitment": commitment,
        "promise_salt": salt,
        "promise_preimage": preimage,
        "promise_id": "cardano-tx-itest",
        "promise_blockchain": "cardano",
    })
    .to_string();
    let credit_tampered = serde_json::json!({
        "transaction_request_id": "tr-itest-tampered",
        "value": { "currency": "KES", "amount": "100.00" },
        "promise_commitment": commitment,
        "promise_salt": "wrong-salt",
        "promise_preimage": preimage,
    })
    .to_string();

    let messages: Vec<(&str, &str, String)> = vec![
        ("c1", message_id::CREDIT_NOTIFICATION, credit_ok),
        ("c2", message_id::CREDIT_NOTIFICATION, credit_tampered),
        (
            "c3",
            message_id::CREDIT_NOTIFICATION,
            "this is not json".into(),
        ),
        ("c4", message_id::SETTLEMENT_INSTRUCTION, "{}".into()),
        (
            "c5",
            message_id::NETTING_SNAPSHOT,
            r#"{"snapshot_id":"snap-itest"}"#.into(),
        ),
        (
            "c6",
            message_id::STATUS_UPDATE,
            r#"{"transaction_request_id":"tr-itest-ok","status":"COMPLETED"}"#.into(),
        ),
        ("c7", "obp_totally_unknown", "{}".into()),
    ];

    for (correlation_id, msg_id, body) in &messages {
        let props = BasicProperties::default()
            .with_message_id((*msg_id).into())
            .with_correlation_id((*correlation_id).into())
            .with_reply_to(reply_queue.as_str().into());
        channel
            .basic_publish(
                "",
                &rpc_queue,
                BasicPublishOptions::default(),
                body.as_bytes(),
                props,
            )
            .await
            .expect("publish")
            .await
            .expect("publish confirm");
    }

    // ---- collect one reply per message, keyed by the correlationId the
    // consumer must carry over into the envelope.
    let mut reply_consumer = channel
        .basic_consume(
            &reply_queue,
            "itest-replies",
            BasicConsumeOptions {
                no_ack: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .expect("consume replies");

    let mut replies: HashMap<String, serde_json::Value> = HashMap::new();
    while replies.len() < messages.len() {
        let delivery = tokio::time::timeout(REPLY_TIMEOUT, reply_consumer.next())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out waiting for replies; got {} of {}: {:?}",
                    replies.len(),
                    messages.len(),
                    replies.keys().collect::<Vec<_>>()
                )
            })
            .expect("reply stream ended")
            .expect("reply delivery");
        let envelope: serde_json::Value =
            serde_json::from_slice(&delivery.data).expect("reply envelope is JSON");
        let correlation_id = envelope["inboundAdapterCallContext"]["correlationId"]
            .as_str()
            .expect("reply carries correlationId")
            .to_string();
        // The AMQP property must match the envelope's own correlation id.
        assert_eq!(
            delivery
                .properties
                .correlation_id()
                .as_ref()
                .map(|s| s.as_str()),
            Some(correlation_id.as_str()),
            "AMQP correlationId property must match the envelope"
        );
        replies.insert(correlation_id, envelope);
    }

    let code = |c: &str| {
        replies[c]["status"]["errorCode"]
            .as_str()
            .unwrap_or("?")
            .to_string()
    };

    // Valid credit: ok reply carrying the CBS reference and verified=true.
    assert_eq!(
        code("c1"),
        "",
        "valid credit must succeed: {:?}",
        replies["c1"]
    );
    assert_eq!(replies["c1"]["data"]["cbs_reference"], "CBS-ITEST-1");
    assert_eq!(replies["c1"]["data"]["verified"], true);
    // Tampered evidence: refused, customer not credited.
    assert_eq!(code("c2"), error_code::COMMITMENT_MISMATCH);
    // Malformed body: rejected but the stream keeps flowing (c4..c7 arrived).
    assert_eq!(code("c3"), error_code::BAD_MESSAGE);
    // No settlement rail configured on this router.
    assert_eq!(code("c4"), error_code::SETTLEMENT_NOT_CONFIGURED);
    assert_eq!(code("c5"), "", "netting snapshot is acknowledged");
    assert_eq!(code("c6"), "", "status update is acknowledged");
    assert_eq!(code("c7"), error_code::NOT_IMPLEMENTED);

    // ---- evidence-store side effects.
    let ok = evidence
        .get("tr-itest-ok")
        .await
        .expect("query")
        .expect("evidence stored");
    assert!(ok.verified, "valid triplet must be stored verified");
    assert_eq!(ok.promise_salt, salt);
    assert_eq!(ok.promise_id.as_deref(), Some("cardano-tx-itest"));
    let tampered = evidence
        .get("tr-itest-tampered")
        .await
        .expect("query")
        .expect("tampered evidence must still be stored (it documents the tampering)");
    assert!(!tampered.verified);

    // ---- the queue must be fully drained (everything acked, nothing requeued).
    let redeclared = channel
        .queue_declare(
            &rpc_queue,
            QueueDeclareOptions {
                durable: true,
                passive: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .expect("passive redeclare");
    assert_eq!(
        redeclared.message_count(),
        0,
        "all messages must be consumed and acked"
    );

    // ---- cleanup: stop the consumer, remove the durable test queue.
    consumer_task.abort();
    channel
        .queue_delete(&rpc_queue, QueueDeleteOptions::default())
        .await
        .expect("delete rpc queue");
}
