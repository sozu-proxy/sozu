//! MySQL over Sōzu TCP.

use std::{net::SocketAddr, time::Duration};

use mysql_async::{Conn, Opts, OptsBuilder, TxOpts, params, prelude::Queryable};

use super::fixture::{ContainerSpec, ServiceHarness, eventually};

const IMAGE: &str =
    "mysql:8.4.11@sha256:6ea90827b1100f8f2ae306a539f86d2c264a26ed435a2a9f75551dd5c3aeb242";

async fn connect(address: SocketAddr) -> mysql_async::Result<Conn> {
    let opts = Opts::from(
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(address.port())
            .user(Some("sozu"))
            .pass(Some("sozu"))
            .db_name(Some("sozu")),
    );
    Conn::new(opts).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_trip_and_reconnect_via_sozu() {
    let mut harness = ServiceHarness::start(|_| {
        ContainerSpec::new("mysql", IMAGE, 3306)
            .environment("MYSQL_DATABASE", "sozu")
            .environment("MYSQL_USER", "sozu")
            .environment("MYSQL_PASSWORD", "sozu")
            .environment("MYSQL_ROOT_PASSWORD", "sozu-root")
            .health_command("mysqladmin ping --host 127.0.0.1 --user sozu --password=sozu --silent")
            .startup_timeout(Duration::from_secs(120))
    });
    let address = harness.frontend();
    let table = format!("sozu_tcp_{}", address.port());

    let mut connection = eventually(
        "MySQL connection through Sōzu",
        Duration::from_secs(90),
        || connect(address),
    )
    .await;
    connection
        .query_drop(format!(
            "CREATE TABLE {table} (id INTEGER PRIMARY KEY, payload VARCHAR(64) NOT NULL, bytes VARBINARY(16) NOT NULL)"
        ))
        .await
        .expect("create MySQL table");
    let mut transaction = connection
        .start_transaction(TxOpts::default())
        .await
        .expect("start MySQL transaction");
    for (id, payload, bytes) in [
        (1_u32, "alpha", vec![0_u8, 1]),
        (2_u32, "café", vec![0xfe, 0x7f]),
        (3_u32, "東京", vec![0xaa, 0x55]),
    ] {
        transaction
            .exec_drop(
                format!("INSERT INTO {table} (id, payload, bytes) VALUES (:id, :payload, :bytes)"),
                params! { "id" => id, "payload" => payload, "bytes" => bytes },
            )
            .await
            .expect("insert MySQL row");
    }
    transaction
        .exec_drop(
            format!("UPDATE {table} SET payload = :payload WHERE id = :id"),
            params! { "id" => 2, "payload" => "updated" },
        )
        .await
        .expect("update MySQL row");
    transaction
        .commit()
        .await
        .expect("commit MySQL transaction");
    let mut rollback = connection
        .start_transaction(TxOpts::default())
        .await
        .expect("start MySQL rollback transaction");
    rollback
        .exec_drop(
            format!("UPDATE {table} SET payload = :payload WHERE id = :id"),
            params! { "id" => 3, "payload" => "must-not-commit" },
        )
        .await
        .expect("stage MySQL rollback update");
    rollback
        .rollback()
        .await
        .expect("rollback MySQL transaction");
    connection
        .disconnect()
        .await
        .expect("disconnect MySQL writer");

    harness.deactivate_and_assert_no_bypass(address);
    harness.reactivate();

    let mut connection = connect(address)
        .await
        .expect("reconnect MySQL through Sōzu");
    let rows: Vec<(u32, String, Vec<u8>)> = connection
        .query(format!(
            "SELECT id, payload, bytes FROM {table} ORDER BY id"
        ))
        .await
        .expect("read MySQL rows after reconnect");
    assert_eq!(
        rows,
        vec![
            (1, "alpha".to_owned(), vec![0, 1]),
            (2, "updated".to_owned(), vec![0xfe, 0x7f]),
            (3, "東京".to_owned(), vec![0xaa, 0x55]),
        ]
    );
    connection
        .query_drop(format!("DROP TABLE {table}"))
        .await
        .expect("drop MySQL table");
    connection
        .disconnect()
        .await
        .expect("disconnect MySQL reader");
    harness.finish();
}
