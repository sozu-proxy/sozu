//! Pulsar over Sōzu TCP with the Magnetar client.

use std::{collections::BTreeSet, net::SocketAddr, time::Duration};

use magnetar::{
    OutgoingMessage, PulsarClient, ack_with_interceptors, proto::pb, receive_with_interceptors,
    send_with_interceptors,
};

use super::fixture::{ContainerSpec, ServiceHarness, eventually};

const IMAGE: &str = "apachepulsar/pulsar:4.2.4@sha256:cd5d4a64a32c0770d5d2dbb526169a70081605bbec4b59b73471b68d0a451fb4";

async fn connect(address: SocketAddr) -> Result<PulsarClient, magnetar::PulsarError> {
    PulsarClient::builder()
        .service_url(format!("pulsar://127.0.0.1:{}", address.port()))
        .operation_timeout(Duration::from_secs(10))
        .build()
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_trip_and_reconnect_via_sozu() {
    let mut harness = ServiceHarness::start(|frontend| {
        let broker_port = frontend.port().to_string();
        ContainerSpec::new("pulsar", IMAGE, frontend.port())
            .environment("PULSAR_PREFIX_advertisedAddress", "127.0.0.1")
            .environment("PULSAR_PREFIX_brokerServicePort", &broker_port)
            .environment("PULSAR_PREFIX_webServicePort", "8080")
            .environment(
                "PULSAR_MEM",
                "-Xms512m -Xmx512m -XX:MaxDirectMemorySize=512m",
            )
            .command([
                "bash",
                "-c",
                "bin/apply-config-from-env.py conf/standalone.conf && exec bin/pulsar standalone",
            ])
            .health_command(concat!(
                "test \"$(curl -fsS --max-time 2 ",
                "http://127.0.0.1:8080/admin/v2/brokers/health)\" = ok",
                " && curl -fsS --max-time 2 ",
                "http://127.0.0.1:8080/admin/v2/namespaces/public/default >/dev/null",
            ))
            .startup_timeout(Duration::from_secs(180))
    });
    let address = harness.frontend();
    let topic = format!("persistent://public/default/sozu-tcp-{}", address.port());
    let subscription = format!("sozu-tcp-{}", address.port());

    let client = eventually(
        "Pulsar connection through Sōzu",
        Duration::from_secs(120),
        || connect(address),
    )
    .await;
    let producer = client
        .producer(&topic)
        .name(format!("sozu-producer-{}", address.port()))
        .create()
        .await
        .expect("create Magnetar Pulsar producer");
    let mut published_ids = BTreeSet::new();
    for (key, payload) in [
        ("id-1", b"alpha".as_slice()),
        ("id-2", "café".as_bytes()),
        ("id-3", &[0, 0xff, 0x7f]),
    ] {
        let message_id = send_with_interceptors(
            &producer,
            OutgoingMessage::with_payload(payload)
                .key(key)
                .property("sozu-id", key),
            &[],
        )
        .await
        .expect("publish Pulsar message through Sōzu");
        assert!(
            published_ids.insert(message_id),
            "Pulsar returned a duplicate broker message ID"
        );
    }
    assert_eq!(
        published_ids.len(),
        3,
        "Pulsar must acknowledge every publication"
    );
    producer
        .close()
        .await
        .expect("close Magnetar Pulsar producer");
    client.close().await;

    harness.deactivate_and_assert_no_bypass(address);
    harness.reactivate();

    let client = connect(address)
        .await
        .expect("reconnect Pulsar through Sōzu");
    let consumer = client
        .consumer(&topic)
        .name(format!("sozu-consumer-{}", address.port()))
        .subscription(&subscription)
        .subscription_type(pb::command_subscribe::SubType::Exclusive)
        .initial_position(pb::command_subscribe::InitialPosition::Earliest)
        .subscribe()
        .await
        .expect("create fresh Magnetar Pulsar consumer");
    let expected = [
        ("id-1", b"alpha".as_slice()),
        ("id-2", "café".as_bytes()),
        ("id-3", &[0, 0xff, 0x7f]),
    ];
    for (expected_key, expected_payload) in expected {
        let message = tokio::time::timeout(
            Duration::from_secs(10),
            receive_with_interceptors(&consumer, &[]),
        )
        .await
        .expect("Pulsar message receive timed out")
        .expect("receive Pulsar message");
        assert_eq!(message.key(), Some(expected_key));
        assert_eq!(message.property("sozu-id"), Some(expected_key));
        assert_eq!(message.payload.as_ref(), expected_payload);
        ack_with_interceptors(&consumer, message.id, &[])
            .await
            .expect("ack Pulsar message");
    }
    consumer
        .close()
        .await
        .expect("close Magnetar Pulsar consumer");
    client.close().await;
    harness.finish();
}
