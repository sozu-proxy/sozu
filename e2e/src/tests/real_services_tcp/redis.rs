//! Redis over Sōzu TCP.

use std::{net::SocketAddr, time::Duration};

use redis::{AsyncCommands, aio::MultiplexedConnection};

use super::fixture::{ContainerSpec, ServiceHarness, eventually};

pub(super) const IMAGE: &str =
    "redis:8.2.10-alpine@sha256:b51665e66f00759be7c3152ad5ac3c66fb2f619c13ef62dea7cc1f9914524635";

async fn connect(address: SocketAddr) -> redis::RedisResult<MultiplexedConnection> {
    redis::Client::open(format!("redis://127.0.0.1:{}/", address.port()))?
        .get_multiplexed_async_connection()
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_trip_and_reconnect_via_sozu() {
    let mut harness = ServiceHarness::start(|_| {
        ContainerSpec::new("redis", IMAGE, 6379)
            .command(["redis-server", "--save", "", "--appendonly", "no"])
            .health_command("redis-cli ping | grep -q PONG")
    });
    let address = harness.frontend();
    let keys = [
        format!("sozu:tcp:{}:1", address.port()),
        format!("sozu:tcp:{}:2", address.port()),
        format!("sozu:tcp:{}:3", address.port()),
    ];

    let mut connection = eventually(
        "Redis connection through Sōzu",
        Duration::from_secs(30),
        || connect(address),
    )
    .await;
    let mut writes = redis::pipe();
    writes
        .atomic()
        .set(&keys[0], b"alpha".as_slice())
        .ignore()
        .set(&keys[1], "café".as_bytes())
        .ignore()
        .set(&keys[2], [0_u8, 0xff, 0x7f].as_slice())
        .ignore();
    let _: () = writes
        .query_async(&mut connection)
        .await
        .expect("write Redis pipeline");
    drop(connection);

    harness.deactivate_and_assert_no_bypass(address);
    harness.reactivate();

    let mut connection = connect(address)
        .await
        .expect("reconnect Redis through Sōzu");
    let values: Vec<Option<Vec<u8>>> = redis::cmd("MGET")
        .arg(&keys)
        .query_async(&mut connection)
        .await
        .expect("MGET Redis values");
    assert_eq!(
        values,
        vec![
            Some(b"alpha".to_vec()),
            Some("café".as_bytes().to_vec()),
            Some(vec![0, 0xff, 0x7f]),
        ]
    );
    let deleted: u64 = connection.del(&keys).await.expect("DEL Redis values");
    assert_eq!(deleted, 3);
    let absent: Vec<Option<Vec<u8>>> = redis::cmd("MGET")
        .arg(&keys)
        .query_async(&mut connection)
        .await
        .expect("MGET deleted Redis values");
    assert_eq!(absent, vec![None, None, None]);
    drop(connection);
    harness.finish();
}
