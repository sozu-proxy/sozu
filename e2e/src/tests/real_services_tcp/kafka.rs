//! Kafka over Sōzu TCP.
//!
//! The client speaks the Kafka wire protocol directly through `kafka-protocol`,
//! which only encodes and decodes messages: framing, correlation and error codes
//! are handled here. A single KRaft node is broker, controller and partition
//! leader, so every request goes to the one address Sōzu fronts and no metadata
//! or leader discovery is needed.

use std::{fmt, io, net::SocketAddr, time::Duration};

use kafka_protocol::{
    messages::{
        ApiKey, ApiVersionsRequest, ApiVersionsResponse, CreateTopicsRequest, CreateTopicsResponse,
        DeleteTopicsRequest, DeleteTopicsResponse, FetchRequest, FetchResponse, ProduceRequest,
        ProduceResponse, RequestHeader, ResponseHeader, TopicName,
        create_topics_request::CreatableTopic,
        fetch_request::{FetchPartition, FetchTopic},
        produce_request::{PartitionProduceData, TopicProduceData},
    },
    protocol::{Decodable, Encodable, HeaderVersion, StrBytes, encode_request_header_into_buffer},
    records::{
        Compression, Record, RecordBatchDecoder, RecordBatchEncoder, RecordEncodeOptions,
        TimestampType,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use super::fixture::{ContainerSpec, ServiceHarness, eventually};

const IMAGE: &str =
    "apache/kafka:4.1.2@sha256:5cc2a2fd93fa2687b44015eee04fb2c3edd9e526bd64bf8bec5ff1e268772e0e";

const CLIENT_ID: &str = "sozu-real-service-e2e";
const TIMESTAMP_MS: i64 = 1_700_000_000_000;

// Versions every Kafka 4.x broker accepts: Kafka 4.0 removed Produce < 3 and
// Fetch < 4, and Fetch 13+ addresses topics by id instead of by name.
const API_VERSIONS_VERSION: i16 = 3;
const CREATE_TOPICS_VERSION: i16 = 7;
const PRODUCE_VERSION: i16 = 9;
const FETCH_VERSION: i16 = 12;
const DELETE_TOPICS_VERSION: i16 = 4;

/// A failed exchange: the socket failed, a message did not encode or decode,
/// or the broker answered with a non-zero Kafka error code.
#[derive(Debug)]
enum KafkaError {
    Io(io::Error),
    Codec(String),
    Broker { api: &'static str, code: i16 },
}

impl fmt::Display for KafkaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "Kafka socket error: {error}"),
            Self::Codec(error) => write!(f, "Kafka codec error: {error}"),
            Self::Broker { api, code } => write!(f, "Kafka {api} returned error code {code}"),
        }
    }
}

impl From<io::Error> for KafkaError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn broker_error(api: &'static str, code: i16) -> Result<(), KafkaError> {
    if code == 0 {
        Ok(())
    } else {
        Err(KafkaError::Broker { api, code })
    }
}

struct Connection {
    stream: TcpStream,
    correlation_id: i32,
}

impl Connection {
    /// Connects and completes an ApiVersions exchange, so a connection only
    /// counts once the broker behind Sōzu answers Kafka requests.
    async fn open(address: SocketAddr) -> Result<Self, KafkaError> {
        let mut connection = Self {
            stream: TcpStream::connect(address).await?,
            correlation_id: 0,
        };
        let response: ApiVersionsResponse = connection
            .call(
                ApiKey::ApiVersions,
                API_VERSIONS_VERSION,
                &ApiVersionsRequest::default()
                    .with_client_software_name(StrBytes::from_static_str(CLIENT_ID))
                    .with_client_software_version(StrBytes::from_static_str("1")),
            )
            .await?;
        broker_error("ApiVersions", response.error_code)?;
        Ok(connection)
    }

    async fn call<Req, Resp>(
        &mut self,
        api_key: ApiKey,
        version: i16,
        request: &Req,
    ) -> Result<Resp, KafkaError>
    where
        Req: Encodable + HeaderVersion,
        Resp: Decodable + HeaderVersion,
    {
        self.correlation_id += 1;
        let header = RequestHeader::default()
            .with_request_api_key(api_key as i16)
            .with_request_api_version(version)
            .with_correlation_id(self.correlation_id)
            .with_client_id(Some(StrBytes::from_static_str(CLIENT_ID)));

        let mut payload = Vec::new();
        encode_request_header_into_buffer(&mut payload, &header)
            .map_err(|error| KafkaError::Codec(error.to_string()))?;
        request
            .encode(&mut payload, version)
            .map_err(|error| KafkaError::Codec(error.to_string()))?;
        let size = i32::try_from(payload.len())
            .map_err(|_| KafkaError::Codec("request larger than i32::MAX".to_owned()))?;
        self.stream.write_all(&size.to_be_bytes()).await?;
        self.stream.write_all(&payload).await?;

        let mut size = [0; 4];
        self.stream.read_exact(&mut size).await?;
        let size = usize::try_from(i32::from_be_bytes(size))
            .map_err(|_| KafkaError::Codec("negative response size".to_owned()))?;
        let mut response = vec![0; size];
        self.stream.read_exact(&mut response).await?;

        let mut response = response.as_slice();
        let header = ResponseHeader::decode(&mut response, Resp::header_version(version))
            .map_err(|error| KafkaError::Codec(error.to_string()))?;
        if header.correlation_id != self.correlation_id {
            return Err(KafkaError::Codec(format!(
                "correlation id {} answered request {}",
                header.correlation_id, self.correlation_id
            )));
        }
        Resp::decode(&mut response, version).map_err(|error| KafkaError::Codec(error.to_string()))
    }

    async fn create_topic(&mut self, topic: &TopicName) -> Result<(), KafkaError> {
        let response: CreateTopicsResponse = self
            .call(
                ApiKey::CreateTopics,
                CREATE_TOPICS_VERSION,
                &CreateTopicsRequest::default()
                    .with_timeout_ms(30_000)
                    .with_topics(vec![
                        CreatableTopic::default()
                            .with_name(topic.to_owned())
                            .with_num_partitions(1)
                            .with_replication_factor(1),
                    ]),
            )
            .await?;
        let [created] = response.topics.as_slice() else {
            return Err(KafkaError::Codec(
                "CreateTopics must answer one topic".to_owned(),
            ));
        };
        broker_error("CreateTopics", created.error_code)
    }

    /// Appends `records` to partition 0 in one batch and returns the offsets
    /// the broker assigned, derived from the batch's base offset.
    async fn produce(
        &mut self,
        topic: &TopicName,
        records: &[Record],
    ) -> Result<Vec<i64>, KafkaError> {
        let mut batch = Vec::new();
        RecordBatchEncoder::encode(
            &mut batch,
            records,
            &RecordEncodeOptions {
                version: 2,
                compression: Compression::None,
            },
        )
        .map_err(|error| KafkaError::Codec(error.to_string()))?;
        let response: ProduceResponse = self
            .call(
                ApiKey::Produce,
                PRODUCE_VERSION,
                &ProduceRequest::default()
                    .with_acks(-1)
                    .with_timeout_ms(30_000)
                    .with_topic_data(vec![
                        TopicProduceData::default()
                            .with_name(topic.to_owned())
                            .with_partition_data(vec![
                                PartitionProduceData::default()
                                    .with_index(0)
                                    .with_records(Some(batch.into())),
                            ]),
                    ]),
            )
            .await?;
        let [topic_response] = response.responses.as_slice() else {
            return Err(KafkaError::Codec(
                "Produce must answer one topic".to_owned(),
            ));
        };
        let [partition] = topic_response.partition_responses.as_slice() else {
            return Err(KafkaError::Codec(
                "Produce must answer one partition".to_owned(),
            ));
        };
        broker_error("Produce", partition.error_code)?;
        let count = i64::try_from(records.len()).expect("record count fits i64");
        Ok((partition.base_offset..partition.base_offset + count).collect())
    }

    /// Fetches partition 0 from offset 0 and returns its records and high watermark.
    async fn fetch(&mut self, topic: &TopicName) -> Result<(Vec<Record>, i64), KafkaError> {
        let response: FetchResponse = self
            .call(
                ApiKey::Fetch,
                FETCH_VERSION,
                &FetchRequest::default()
                    .with_max_wait_ms(5_000)
                    .with_min_bytes(1)
                    .with_max_bytes(1_000_000)
                    .with_topics(vec![
                        FetchTopic::default()
                            .with_topic(topic.to_owned())
                            .with_partitions(vec![
                                FetchPartition::default()
                                    .with_partition(0)
                                    .with_fetch_offset(0)
                                    .with_partition_max_bytes(1_000_000),
                            ]),
                    ]),
            )
            .await?;
        broker_error("Fetch", response.error_code)?;
        let [topic_response] = response.responses.as_slice() else {
            return Err(KafkaError::Codec("Fetch must answer one topic".to_owned()));
        };
        let [partition] = topic_response.partitions.as_slice() else {
            return Err(KafkaError::Codec(
                "Fetch must answer one partition".to_owned(),
            ));
        };
        broker_error("Fetch", partition.error_code)?;
        let mut batches = partition.records.to_owned().unwrap_or_default();
        let records = RecordBatchDecoder::decode_all(&mut batches)
            .map_err(|error| KafkaError::Codec(error.to_string()))?
            .into_iter()
            .flat_map(|set| set.records)
            .collect();
        Ok((records, partition.high_watermark))
    }

    async fn delete_topic(&mut self, topic: &TopicName) -> Result<(), KafkaError> {
        let response: DeleteTopicsResponse = self
            .call(
                ApiKey::DeleteTopics,
                DELETE_TOPICS_VERSION,
                &DeleteTopicsRequest::default()
                    .with_timeout_ms(30_000)
                    .with_topic_names(vec![topic.to_owned()]),
            )
            .await?;
        let [deleted] = response.responses.as_slice() else {
            return Err(KafkaError::Codec(
                "DeleteTopics must answer one topic".to_owned(),
            ));
        };
        broker_error("DeleteTopics", deleted.error_code)
    }
}

fn record(offset: i64, key: &[u8], value: &[u8], id: &str) -> Record {
    let mut headers = kafka_protocol::indexmap::IndexMap::new();
    headers.insert(
        StrBytes::from_static_str("sozu-id"),
        Some(id.as_bytes().to_owned().into()),
    );
    Record {
        transactional: false,
        control: false,
        delete_horizon: false,
        partition_leader_epoch: -1,
        producer_id: -1,
        producer_epoch: -1,
        timestamp_type: TimestampType::Creation,
        offset,
        // `RecordBatchEncoder` starts a new batch whenever `offset - sequence`
        // changes, and Produce v3+ accepts exactly one batch per partition.
        // Without a producer id the broker ignores the sequence.
        sequence: i32::try_from(offset).expect("test offsets fit i32"),
        timestamp: TIMESTAMP_MS,
        key: Some(key.to_owned().into()),
        value: Some(value.to_owned().into()),
        headers,
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
    let topic = TopicName(StrBytes::from_string(format!(
        "sozu_tcp_{}",
        address.port()
    )));

    let mut connection = eventually(
        "Kafka connection through Sōzu",
        Duration::from_secs(90),
        || Connection::open(address),
    )
    .await;
    connection
        .create_topic(&topic)
        .await
        .expect("create Kafka topic");
    let records = vec![
        record(0, b"id-1", b"alpha", "id-1"),
        record(1, b"id-2", "café".as_bytes(), "id-2"),
        record(2, b"id-3", &[0, 0xff, 0x7f], "id-3"),
    ];
    // A topic just created can briefly answer NOT_LEADER_OR_FOLLOWER or
    // UNKNOWN_TOPIC_OR_PARTITION until its partition has a leader; neither
    // appends, so retrying the batch cannot duplicate it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let offsets = loop {
        match connection.produce(&topic, &records).await {
            Ok(offsets) => break offsets,
            Err(KafkaError::Broker { code, .. })
                if matches!(code, 3 | 6) && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => panic!("publish Kafka records: {error}"),
        }
    };
    assert_eq!(offsets.len(), 3, "Kafka must acknowledge every record");
    assert_eq!(offsets, vec![0, 1, 2]);
    drop(connection);

    harness.deactivate_and_assert_no_bypass(address);
    harness.reactivate();

    let mut connection = Connection::open(address)
        .await
        .expect("reconnect Kafka through Sōzu");
    let (received, high_watermark) = connection
        .fetch(&topic)
        .await
        .expect("consume Kafka records after reconnect");
    assert_eq!(high_watermark, 3);
    assert_eq!(received.len(), 3);
    let sozu_id = StrBytes::from_static_str("sozu-id");
    let actual = received
        .into_iter()
        .map(|entry| {
            (
                entry.offset,
                entry.key.map(|key| key.to_vec()),
                entry.value.map(|value| value.to_vec()),
                entry
                    .headers
                    .get(&sozu_id)
                    .cloned()
                    .flatten()
                    .map(|id| id.to_vec()),
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
    connection
        .delete_topic(&topic)
        .await
        .expect("delete Kafka topic");
    drop(connection);
    harness.finish();
}
