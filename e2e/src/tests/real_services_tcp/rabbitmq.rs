//! RabbitMQ over Sōzu TCP.

use std::{net::SocketAddr, time::Duration};

use lapin::{
    BasicProperties, Channel, Connection, ConnectionProperties,
    options::{
        BasicAckOptions, BasicGetOptions, BasicPublishOptions, ConfirmSelectOptions,
        QueueDeclareOptions, QueueDeleteOptions,
    },
    types::FieldTable,
};

use super::fixture::{ContainerSpec, ServiceHarness, eventually};

const IMAGE: &str =
    "rabbitmq:4.2.9-alpine@sha256:d5d8797191db5828a2a2a3dabf295d2b558ae81e215e6c6cba26d567ae8fb236";

async fn connect(address: SocketAddr) -> lapin::Result<Connection> {
    Connection::connect(
        &format!("amqp://sozu:sozu@127.0.0.1:{}/%2f", address.port()),
        ConnectionProperties::default(),
    )
    .await
}

async fn open_channel(connection: &Connection) -> lapin::Result<Channel> {
    connection.create_channel().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_trip_and_reconnect_via_sozu() {
    let mut harness = ServiceHarness::start(|_| {
        ContainerSpec::new("rabbitmq", IMAGE, 5672)
            .environment("RABBITMQ_DEFAULT_USER", "sozu")
            .environment("RABBITMQ_DEFAULT_PASS", "sozu")
            .health_command("su-exec rabbitmq rabbitmq-diagnostics -q ping")
            .startup_timeout(Duration::from_secs(120))
    });
    let address = harness.frontend();
    let queue = format!("sozu.tcp.{}", address.port());

    let connection = eventually(
        "RabbitMQ connection through Sōzu",
        Duration::from_secs(90),
        || connect(address),
    )
    .await;
    let channel = open_channel(&connection)
        .await
        .expect("create RabbitMQ publisher channel");
    channel
        .queue_declare(
            queue.clone().into(),
            QueueDeclareOptions::default(),
            FieldTable::default(),
        )
        .await
        .expect("declare RabbitMQ queue");
    channel
        .confirm_select(ConfirmSelectOptions::default())
        .await
        .expect("enable RabbitMQ publisher confirms");
    for payload in [
        b"id-1:alpha".as_slice(),
        "id-2:café".as_bytes(),
        &[0, 0xff, 0x7f],
    ] {
        let confirmation = channel
            .basic_publish(
                "".into(),
                queue.clone().into(),
                BasicPublishOptions {
                    mandatory: true,
                    ..BasicPublishOptions::default()
                },
                payload,
                BasicProperties::default(),
            )
            .await
            .expect("publish RabbitMQ message")
            .await
            .expect("receive RabbitMQ publisher confirm");
        assert!(confirmation.is_ack(), "RabbitMQ must ack every publication");
        assert!(
            confirmation.take_message().is_none(),
            "RabbitMQ unexpectedly returned a publication as unroutable"
        );
    }
    connection
        .close(200, "writer complete".into())
        .await
        .expect("close RabbitMQ publisher connection");

    harness.deactivate_and_assert_no_bypass(address);
    harness.reactivate();

    let connection = connect(address)
        .await
        .expect("reconnect RabbitMQ through Sōzu");
    let channel = open_channel(&connection)
        .await
        .expect("create RabbitMQ consumer channel");
    let expected = [
        b"id-1:alpha".as_slice(),
        "id-2:café".as_bytes(),
        &[0, 0xff, 0x7f],
    ];
    for (index, expected_payload) in expected.into_iter().enumerate() {
        let message = channel
            .basic_get(queue.clone().into(), BasicGetOptions::default())
            .await
            .expect("consume RabbitMQ message")
            .expect("RabbitMQ queue ended before all messages were consumed");
        assert_eq!(message.delivery.data, expected_payload);
        assert_eq!(usize::try_from(message.message_count).unwrap(), 2 - index);
        assert!(
            message
                .delivery
                .acker
                .ack(BasicAckOptions::default())
                .await
                .expect("ack RabbitMQ message"),
            "RabbitMQ acknowledgement handle was already consumed"
        );
    }
    assert!(
        channel
            .basic_get(queue.clone().into(), BasicGetOptions::default())
            .await
            .expect("check RabbitMQ queue exhaustion")
            .is_none(),
        "RabbitMQ queue contained unexpected extra messages"
    );
    channel
        .queue_delete(queue.into(), QueueDeleteOptions::default())
        .await
        .expect("delete RabbitMQ queue");
    connection
        .close(200, "reader complete".into())
        .await
        .expect("close RabbitMQ consumer connection");
    harness.finish();
}
