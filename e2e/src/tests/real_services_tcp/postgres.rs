//! PostgreSQL over Sōzu TCP.

use std::{net::SocketAddr, time::Duration};

use tokio::task::JoinHandle;
use tokio_postgres::{Client, Error, NoTls};

use super::fixture::{ContainerSpec, ServiceHarness, eventually};

const IMAGE: &str =
    "postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873";

async fn connect(address: SocketAddr) -> Result<(Client, JoinHandle<Result<(), Error>>), Error> {
    let config = format!(
        "host=127.0.0.1 port={} user=sozu password=sozu dbname=sozu connect_timeout=2",
        address.port()
    );
    let (client, connection) = tokio_postgres::connect(&config, NoTls).await?;
    let task = tokio::spawn(connection);
    Ok((client, task))
}

async fn close(client: Client, task: JoinHandle<Result<(), Error>>) {
    drop(client);
    let result = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("PostgreSQL connection task did not finish")
        .expect("PostgreSQL connection task panicked");
    result.expect("PostgreSQL connection ended with an error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_trip_and_reconnect_via_sozu() {
    let mut harness = ServiceHarness::start(|_| {
        ContainerSpec::new("postgres", IMAGE, 5432)
            .environment("POSTGRES_USER", "sozu")
            .environment("POSTGRES_PASSWORD", "sozu")
            .environment("POSTGRES_DB", "sozu")
            .health_command("pg_isready -U sozu -d sozu")
    });
    let address = harness.frontend();
    let table = format!("sozu_tcp_{}", address.port());

    let (mut client, task) = eventually(
        "PostgreSQL connection through Sōzu",
        Duration::from_secs(60),
        || connect(address),
    )
    .await;
    client
        .batch_execute(&format!(
            "CREATE TABLE {table} (id INTEGER PRIMARY KEY, payload TEXT NOT NULL, bytes BYTEA NOT NULL)"
        ))
        .await
        .expect("create PostgreSQL table");
    let transaction = client
        .transaction()
        .await
        .expect("start PostgreSQL transaction");
    for (id, payload, bytes) in [
        (1_i32, "alpha", b"\0\x01".as_slice()),
        (2_i32, "café", b"\xfe\x7f".as_slice()),
        (3_i32, "東京", b"\xaa\x55".as_slice()),
    ] {
        assert_eq!(
            transaction
                .execute(
                    &format!("INSERT INTO {table} (id, payload, bytes) VALUES ($1, $2, $3)"),
                    &[&id, &payload, &bytes],
                )
                .await
                .expect("insert PostgreSQL row"),
            1
        );
    }
    assert_eq!(
        transaction
            .execute(
                &format!("UPDATE {table} SET payload = $1 WHERE id = $2"),
                &[&"updated", &2_i32],
            )
            .await
            .expect("update PostgreSQL row"),
        1
    );
    transaction
        .batch_execute("SAVEPOINT rollback_probe")
        .await
        .expect("create PostgreSQL savepoint");
    transaction
        .execute(
            &format!("UPDATE {table} SET payload = $1 WHERE id = $2"),
            &[&"must-not-commit", &3_i32],
        )
        .await
        .expect("stage PostgreSQL rollback update");
    transaction
        .batch_execute("ROLLBACK TO SAVEPOINT rollback_probe")
        .await
        .expect("rollback PostgreSQL savepoint");
    transaction
        .commit()
        .await
        .expect("commit PostgreSQL transaction");
    close(client, task).await;

    harness.deactivate_and_assert_no_bypass(address);
    harness.reactivate();

    let (client, task) = connect(address)
        .await
        .expect("reconnect PostgreSQL through Sōzu");
    let rows = client
        .query(
            &format!("SELECT id, payload, bytes FROM {table} ORDER BY id"),
            &[],
        )
        .await
        .expect("read PostgreSQL rows after reconnect");
    let actual = rows
        .iter()
        .map(|row| {
            (
                row.get::<_, i32>(0),
                row.get::<_, String>(1),
                row.get::<_, Vec<u8>>(2),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            (1, "alpha".to_owned(), vec![0, 1]),
            (2, "updated".to_owned(), vec![0xfe, 0x7f]),
            (3, "東京".to_owned(), vec![0xaa, 0x55]),
        ]
    );
    client
        .batch_execute(&format!("DROP TABLE {table}"))
        .await
        .expect("drop PostgreSQL table");
    close(client, task).await;
    harness.finish();
}
