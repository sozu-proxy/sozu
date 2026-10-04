//! MongoDB over Sōzu TCP.

use std::{net::SocketAddr, time::Duration};

use futures::TryStreamExt;
use mongodb::{Client, bson::doc};

use super::fixture::{ContainerSpec, ServiceHarness, eventually};

const IMAGE: &str =
    "mongo:8.0.32-noble@sha256:0393ab544cbbe92b2dd64719205ecb14a8b3824b17ea75051e2f22482c3e4e66";

async fn connect(address: SocketAddr) -> mongodb::error::Result<Client> {
    let client = Client::with_uri_str(format!(
        "mongodb://127.0.0.1:{}/?directConnection=true&serverSelectionTimeoutMS=2000&connectTimeoutMS=1000",
        address.port()
    ))
    .await?;
    client
        .database("admin")
        .run_command(doc! { "ping": 1 })
        .await?;
    Ok(client)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_trip_and_reconnect_via_sozu() {
    let mut harness = ServiceHarness::start(|_| {
        ContainerSpec::new("mongodb", IMAGE, 27017)
            .command(["mongod", "--bind_ip_all", "--quiet"])
            .health_command("mongosh --quiet --eval 'quit(db.runCommand({ ping: 1 }).ok ? 0 : 1)' mongodb://127.0.0.1:27017/admin")
            .startup_timeout(Duration::from_secs(120))
    });
    let address = harness.frontend();
    let database_name = format!("sozu_tcp_{}", address.port());
    let collection_name = "documents";

    let client = eventually(
        "MongoDB connection through Sōzu",
        Duration::from_secs(90),
        || connect(address),
    )
    .await;
    let collection = client
        .database(&database_name)
        .collection::<mongodb::bson::Document>(collection_name);
    let inserted = collection
        .insert_many([
            doc! { "_id": 1, "payload": "alpha", "bytes": mongodb::bson::Binary { subtype: mongodb::bson::spec::BinarySubtype::Generic, bytes: vec![0, 1] } },
            doc! { "_id": 2, "payload": "café", "bytes": mongodb::bson::Binary { subtype: mongodb::bson::spec::BinarySubtype::Generic, bytes: vec![0xfe, 0x7f] } },
            doc! { "_id": 3, "payload": "東京", "bytes": mongodb::bson::Binary { subtype: mongodb::bson::spec::BinarySubtype::Generic, bytes: vec![0xaa, 0x55] } },
        ])
        .await
        .expect("insert MongoDB documents");
    assert_eq!(inserted.inserted_ids.len(), 3);
    let updated = collection
        .update_one(doc! { "_id": 2 }, doc! { "$set": { "payload": "updated" } })
        .await
        .expect("update MongoDB document");
    assert_eq!(updated.matched_count, 1);
    assert_eq!(updated.modified_count, 1);
    drop(client);

    harness.deactivate_and_assert_no_bypass(address);
    harness.reactivate();

    let client = connect(address)
        .await
        .expect("reconnect MongoDB through Sōzu");
    let documents = client
        .database(&database_name)
        .collection::<mongodb::bson::Document>(collection_name)
        .find(doc! {})
        .sort(doc! { "_id": 1 })
        .await
        .expect("find MongoDB documents after reconnect")
        .try_collect::<Vec<_>>()
        .await
        .expect("collect MongoDB documents");
    assert_eq!(documents.len(), 3);
    assert_eq!(documents[0].get_i32("_id"), Ok(1));
    assert_eq!(documents[0].get_str("payload"), Ok("alpha"));
    assert_eq!(documents[0].get_binary_generic("bytes"), Ok(&vec![0, 1]));
    assert_eq!(documents[1].get_i32("_id"), Ok(2));
    assert_eq!(documents[1].get_str("payload"), Ok("updated"));
    assert_eq!(
        documents[1].get_binary_generic("bytes"),
        Ok(&vec![0xfe, 0x7f])
    );
    assert_eq!(documents[2].get_i32("_id"), Ok(3));
    assert_eq!(documents[2].get_str("payload"), Ok("東京"));
    assert_eq!(
        documents[2].get_binary_generic("bytes"),
        Ok(&vec![0xaa, 0x55])
    );
    client
        .database(&database_name)
        .drop()
        .await
        .expect("drop MongoDB database");
    drop(client);
    harness.finish();
}
