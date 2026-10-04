//! Kafka over Sōzu TCP.

use std::{collections::BTreeMap, net::SocketAddr, time::Duration};

use rskafka::{
    chrono::{TimeZone, Utc},
    client::{
        Client, ClientBuilder,
        partition::{Compression, UnknownTopicHandling},
    },
    record::Record,
};

use super::fixture::{ContainerSpec, ServiceHarness, eventually};

const IMAGE: &str =
    "apache/kafka:4.1.2@sha256:5cc2a2fd93fa2687b44015eee04fb2c3edd9e526bd64bf8bec5ff1e268772e0e";

async fn connect(address: SocketAddr) -> rskafka::client::error::Result<Client> {
    ClientBuilder::new(vec![address.to_string()])
        .client_id("sozu-real-service-e2e")
        .build()
        .await
}

fn record(key: &[u8], value: &[u8], id: &str) -> Record {
    Record {
        key: Some(key.to_vec()),
        value: Some(value.to_vec()),
        headers: BTreeMap::from([("sozu-id".to_owned(), id.as_bytes().to_vec())]),
        timestamp: Utc
            .timestamp_millis_opt(1_700_000_000_000)
            .single()
            .expect("fixed Kafka test timestamp must be valid"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_trip_and_reconnect_via_sozu() {
    let mut harness = ServiceHarness::start(|frontend| {
        let broker_port = frontend.port().to_string();
        ContainerSpec::new("kafka", IMAGE, frontend.port())
            .environment("KAFKA_NODE_ID", "1")
            .environment("KAFKA_PROCESS_ROLES", "broker,controller")
            .environment(
                "KAFKA_LISTENERS",
                format!("PLAINTEXT://0.0.0.0:{broker_port},CONTROLLER://0.0.0.0:9093"),
            )
            .environment(
                "KAFKA_ADVERTISED_LISTENERS",
                format!("PLAINTEXT://127.0.0.1:{broker_port}"),
            )
            .environment(
                "KAFKA_LISTENER_SECURITY_PROTOCOL_MAP",
                "PLAINTEXT:PLAINTEXT,CONTROLLER:PLAINTEXT",
            )
            .environment("KAFKA_CONTROLLER_LISTENER_NAMES", "CONTROLLER")
            .environment("KAFKA_CONTROLLER_QUORUM_VOTERS", "1@127.0.0.1:9093")
            .environment("KAFKA_OFFSETS_TOPIC_REPLICATION_FACTOR", "1")
            .environment("KAFKA_TRANSACTION_STATE_LOG_REPLICATION_FACTOR", "1")
            .environment("KAFKA_TRANSACTION_STATE_LOG_MIN_ISR", "1")
            .environment("KAFKA_AUTO_CREATE_TOPICS_ENABLE", "false")
            .health_command(format!(
                "/opt/kafka/bin/kafka-broker-api-versions.sh --bootstrap-server 127.0.0.1:{broker_port} >/dev/null"
            ))
            .startup_timeout(Duration::from_secs(120))
    });
    let address = harness.frontend();
    let topic = format!("sozu_tcp_{}", address.port());

    let client = eventually(
        "Kafka connection through Sōzu",
        Duration::from_secs(90),
        || connect(address),
    )
    .await;
    client
        .controller_client()
        .expect("create Kafka controller client")
        .create_topic(&topic, 1, 1, 30_000)
        .await
        .expect("create Kafka topic");
    let partition = client
        .partition_client(&topic, 0, UnknownTopicHandling::Retry)
        .await
        .expect("open Kafka partition producer");
    let records = vec![
        record(b"id-1", b"alpha", "id-1"),
        record(b"id-2", "café".as_bytes(), "id-2"),
        record(b"id-3", &[0, 0xff, 0x7f], "id-3"),
    ];
    let offsets = partition
        .produce(records, Compression::NoCompression)
        .await
        .expect("publish Kafka records");
    assert_eq!(offsets.len(), 3, "Kafka must acknowledge every record");
    assert_eq!(offsets, vec![0, 1, 2]);
    drop(partition);
    drop(client);

    harness.deactivate_and_assert_no_bypass(address);
    harness.reactivate();

    let client = connect(address)
        .await
        .expect("reconnect Kafka through Sōzu");
    let partition = client
        .partition_client(&topic, 0, UnknownTopicHandling::Retry)
        .await
        .expect("open Kafka partition consumer after reconnect");
    let (received, high_watermark) = partition
        .fetch_records(0, 1..1_000_000, 5_000)
        .await
        .expect("consume Kafka records after reconnect");
    assert_eq!(high_watermark, 3);
    assert_eq!(received.len(), 3);
    let actual = received
        .into_iter()
        .map(|entry| {
            (
                entry.offset,
                entry.record.key,
                entry.record.value,
                entry.record.headers.get("sozu-id").cloned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            (
                0,
                Some(b"id-1".to_vec()),
                Some(b"alpha".to_vec()),
                Some(b"id-1".to_vec())
            ),
            (
                1,
                Some(b"id-2".to_vec()),
                Some("café".as_bytes().to_vec()),
                Some(b"id-2".to_vec())
            ),
            (
                2,
                Some(b"id-3".to_vec()),
                Some(vec![0, 0xff, 0x7f]),
                Some(b"id-3".to_vec())
            ),
        ]
    );
    client
        .controller_client()
        .expect("recreate Kafka controller client")
        .delete_topic(&topic, 30_000)
        .await
        .expect("delete Kafka topic");
    drop(partition);
    drop(client);
    harness.finish();
}
