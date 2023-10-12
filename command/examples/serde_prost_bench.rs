use prost::Message;
use serde::{de::DeserializeOwned, ser::Serialize};
use serde_json;
use sozu_command_lib::{
    buffer::growable::Buffer,
    proto::command::{PathRule, RequestHttpFrontend, RulePosition},
};
use std::{collections::BTreeMap, time::Instant};
use std::{io::Write, str::from_utf8};

fn main() {
    let mut tags = BTreeMap::new();
    tags.insert("owner".to_string(), "Clever Cloud".to_string());
    tags.insert(
        "app_id".to_string(),
        "a782f2e4-ee42-4384-9757-2b13d4198105".to_string(),
    );
    let http_front = RequestHttpFrontend {
        cluster_id: Some(String::from("cluster_1")),
        address: "127.0.0.1:8080".to_string(),
        hostname: String::from("lolcatho.st"),
        path: PathRule::prefix(String::from("/")),
        method: Some("GET".to_string()),
        position: RulePosition::Tree.into(),
        tags,
    };

    let repetitions = 1_000_000;

    println!(
        "We will serialize, write in a buffer, read from the buffer and deserialize, a {} times, this object:\n{:?}",
        repetitions,
        http_front,
    );

    let serde_start = Instant::now();

    let mut buffer = Buffer::with_capacity(20000);

    for i in 0..repetitions {
        let json_front = serde_json::to_string(&http_front).unwrap();
        let bytes = json_front.into_bytes();
        let message_len = bytes.len();
        if i == 1 {
            println!("serde_json serialized it in {} bytes", message_len);
        }
        buffer.write(&bytes).unwrap();

        let data = &buffer.data()[..message_len];
        let utf8_str = from_utf8(data).unwrap();
        let deserialized: RequestHttpFrontend = serde_json::from_str(utf8_str).unwrap();
        buffer.consume(message_len);
        // println!("{i}: {deserialized}");
    }

    let serde_time = serde_start.elapsed();

    println!("Serde JSON needed {:?}", serde_time);

    let prost_start = Instant::now();

    for i in 0..repetitions {
        let binary_front = http_front.encode_to_vec();
        let message_len = binary_front.len();
        if i == 1 {
            println!("prost serialized it in {} bytes", message_len);
        }
        buffer
            .write(&binary_front)
            .expect("could not write binary-serialized front on buffer");

        let data = &buffer.data()[..message_len];
        let deserialized = RequestHttpFrontend::decode(data).expect("could not decode buffer data");
        // println!("{i}: {deserialized}");

        buffer.consume(message_len);
    }

    let serde_time = prost_start.elapsed();

    println!("Prost needed {:?}", serde_time);
}
