use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::{self, Display, Formatter},
    net::SocketAddr,
};

use prettytable::{cell, row, Row, Table};
use time::format_description;
use x509_parser::time::ASN1Time;

use crate::{
    proto::{
        command::{
            filtered_metrics, protobuf_endpoint, request::RequestType,
            response_content::ContentType, AggregatedMetrics, AvailableMetrics, CertificateAndKey,
            CertificateSummary, CertificatesWithFingerprints, ClusterMetrics, FilteredMetrics,
            HttpEndpoint, ListOfCertificatesByAddress, ListedFrontends, ListenersList,
            ProtobufEndpoint, QueryCertificatesFilters, RequestCounts, Response, ResponseContent,
            ResponseStatus, RunState, SocketAddress, TlsVersion, WorkerInfos, WorkerMetrics,
            WorkerResponses,
        },
        DisplayError,
    },
    AsString,
};

impl Display for CertificateAndKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let versions = self.versions.iter().fold(String::new(), |acc, tls_v| {
            acc + " "
                + match TlsVersion::try_from(*tls_v) {
                    Ok(v) => v.as_str_name(),
                    Err(_) => "",
                }
        });
        write!(
            f,
            "\tcertificate: {}\n\tcertificate_chain: {:?}\n\tkey: {}\n\tTLS versions: {}\n\tnames: {:?}",
            self.certificate, self.certificate_chain, self.key, versions,
            concatenate_vector(&self.names)
        )
    }
}

impl Display for CertificateSummary {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}:\t{}", self.fingerprint, self.domain)
    }
}

impl Display for QueryCertificatesFilters {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(d) = self.domain.clone() {
            write!(f, "domain:{}", d)
        } else if let Some(fp) = self.fingerprint.clone() {
            write!(f, "domain:{}", fp)
        } else {
            write!(f, "all certificates")
        }
    }
}

pub fn concatenate_vector(vec: &[String]) -> String {
    vec.join(", ")
}

pub fn format_request_type(request_type: &RequestType) -> &str {
    match request_type {
        RequestType::SaveState(_) => "SaveState",
        RequestType::LoadState(_) => "LoadState",
        RequestType::CountRequests(_) => "CountRequests",
        RequestType::ListWorkers(_) => "ListWorkers",
        RequestType::ListFrontends(_) => "ListFrontends",
        RequestType::ListListeners(_) => "ListListeners",
        RequestType::LaunchWorker(_) => "LaunchWorker",
        RequestType::UpgradeMain(_) => "UpgradeMain",
        RequestType::UpgradeWorker(_) => "UpgradeWorker",
        RequestType::SubscribeEvents(_) => "SubscribeEvents",
        RequestType::ReloadConfiguration(_) => "ReloadConfiguration",
        RequestType::Status(_) => "Status",
        RequestType::AddCluster(_) => "AddCluster",
        RequestType::RemoveCluster(_) => "RemoveCluster",
        RequestType::AddHttpFrontend(_) => "AddHttpFrontend",
        RequestType::RemoveHttpFrontend(_) => "RemoveHttpFrontend",
        RequestType::AddHttpsFrontend(_) => "AddHttpsFrontend",
        RequestType::RemoveHttpsFrontend(_) => "RemoveHttpsFrontend",
        RequestType::AddCertificate(_) => "AddCertificate",
        RequestType::ReplaceCertificate(_) => "ReplaceCertificate",
        RequestType::RemoveCertificate(_) => "RemoveCertificate",
        RequestType::AddTcpFrontend(_) => "AddTcpFrontend",
        RequestType::RemoveTcpFrontend(_) => "RemoveTcpFrontend",
        RequestType::AddBackend(_) => "AddBackend",
        RequestType::RemoveBackend(_) => "RemoveBackend",
        RequestType::AddHttpListener(_) => "AddHttpListener",
        RequestType::AddHttpsListener(_) => "AddHttpsListener",
        RequestType::AddTcpListener(_) => "AddTcpListener",
        RequestType::RemoveListener(_) => "RemoveListener",
        RequestType::ActivateListener(_) => "ActivateListener",
        RequestType::DeactivateListener(_) => "DeactivateListener",
        RequestType::QueryClusterById(_) => "QueryClusterById",
        RequestType::QueryClustersByDomain(_) => "QueryClustersByDomain",
        RequestType::QueryClustersHashes(_) => "QueryClustersHashes",
        RequestType::QueryMetrics(_) => "QueryMetrics",
        RequestType::SoftStop(_) => "SoftStop",
        RequestType::HardStop(_) => "HardStop",
        RequestType::ConfigureMetrics(_) => "ConfigureMetrics",
        RequestType::Logging(_) => "Logging",
        RequestType::ReturnListenSockets(_) => "ReturnListenSockets",
        RequestType::QueryCertificatesFromTheState(_) => "QueryCertificatesFromTheState",
        RequestType::QueryCertificatesFromWorkers(_) => "QueryCertificatesFromWorkers",
    }
}

pub fn print_json_response<T: ::serde::Serialize>(input: &T) -> Result<(), DisplayError> {
    let pretty_json = serde_json::to_string_pretty(&input).map_err(DisplayError::Json)?;
    println!("{pretty_json}");
    Ok(())
}

impl Response {
    pub fn display(&self, json: bool) -> Result<(), DisplayError> {
        match self.status() {
            ResponseStatus::Ok => {
                // avoid displaying anything else than JSON
                if !json {
                    println!("Success: {}", self.message)
                }
            }
            ResponseStatus::Failure => println!("Failure: {}", self.message),
            ResponseStatus::Processing => {
                return Err(DisplayError::WrongResponseType(
                    "ResponseStatus::Processing".to_string(),
                ))
            }
        }

        match &self.content {
            Some(content) => content.display(json),
            None => {
                if json {
                    println!("{{}}");
                } else {
                    println!("No content");
                }
                Ok(())
            }
        }
    }
}

impl ResponseContent {
    fn display(&self, json: bool) -> Result<(), DisplayError> {
        let content_type = match &self.content_type {
            Some(content_type) => content_type,
            None => return Ok(println!("No content")),
        };

        if json {
            return print_json_response(&content_type);
        }

        match content_type {
            ContentType::Workers(worker_infos) => print_status(worker_infos),
            ContentType::Metrics(aggr_metrics) => print_metrics(aggr_metrics),
            ContentType::FrontendList(frontends) => print_frontends(frontends),
            ContentType::ListenersList(listeners) => print_listeners(listeners),
            ContentType::WorkerMetrics(worker_metrics) => print_worker_metrics(worker_metrics),
            ContentType::AvailableMetrics(list) => print_available_metrics(list),
            ContentType::RequestCounts(request_counts) => print_request_counts(request_counts),
            ContentType::CertificatesWithFingerprints(certs) => {
                print_certificates_with_validity(certs)
            }
            ContentType::WorkerResponses(worker_responses) => {
                // exception when displaying clusters
                if worker_responses.contain_cluster_infos() {
                    print_cluster_infos(worker_responses)
                } else if worker_responses.contain_cluster_hashes() {
                    print_cluster_hashes(worker_responses)
                } else {
                    print_responses_by_worker(worker_responses, json)
                }
            }
            ContentType::Clusters(_) | ContentType::ClusterHashes(_) => Ok(()), // not displayed directly, see print_cluster_responses
            ContentType::CertificatesByAddress(certs) => print_certificates_by_address(certs),
            ContentType::Event(_event) => Ok(()), // not event displayed yet!
        }
    }
}

impl WorkerResponses {
    fn contain_cluster_infos(&self) -> bool {
        for (_worker_id, response) in self.map.iter() {
            if let Some(content_type) = &response.content_type {
                if matches!(content_type, ContentType::Clusters(_)) {
                    return true;
                }
            }
        }
        false
    }

    fn contain_cluster_hashes(&self) -> bool {
        for (_worker_id, response) in self.map.iter() {
            if let Some(content_type) = &response.content_type {
                if matches!(content_type, ContentType::ClusterHashes(_)) {
                    return true;
                }
            }
        }
        false
    }
}

pub fn print_status(worker_infos: &WorkerInfos) -> Result<(), DisplayError> {
    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
    table.add_row(row!["worker id", "pid", "run state"]);

    let mut sorted_infos = worker_infos.vec.clone();
    sorted_infos.sort_by_key(|worker| worker.id);

    for worker_info in &sorted_infos {
        let row = row!(
            worker_info.id,
            worker_info.pid,
            RunState::try_from(worker_info.run_state)
                .map_err(DisplayError::DecodeError)?
                .as_str_name()
        );
        table.add_row(row);
    }

    table.printstd();
    Ok(())
}

pub fn print_metrics(aggregated_metrics: &AggregatedMetrics) -> Result<(), DisplayError> {
    // main process metrics
    println!("\nMAIN PROCESS\n============");
    print_proxy_metrics(&aggregated_metrics.main);

    // workers
    for (worker_id, worker_metrics) in aggregated_metrics.workers.iter() {
        println!("\nWorker {worker_id}\n=========");
        print_worker_metrics(worker_metrics)?;
    }
    Ok(())
}

fn print_proxy_metrics(proxy_metrics: &BTreeMap<String, FilteredMetrics>) {
    let filtered = filter_metrics(proxy_metrics);
    print_gauges_and_counts(&filtered);
    print_percentiles(&filtered);
}

fn print_worker_metrics(worker_metrics: &WorkerMetrics) -> Result<(), DisplayError> {
    print_proxy_metrics(&worker_metrics.proxy);
    print_cluster_metrics(&worker_metrics.clusters);

    Ok(())
}

fn print_cluster_metrics(cluster_metrics: &BTreeMap<String, ClusterMetrics>) {
    for (cluster_id, cluster_metrics_data) in cluster_metrics.iter() {
        println!("\nCluster {cluster_id}\n--------");

        let filtered = filter_metrics(&cluster_metrics_data.cluster);
        print_gauges_and_counts(&filtered);
        print_percentiles(&filtered);

        for backend_metrics in cluster_metrics_data.backends.iter() {
            println!("\n{cluster_id}/{}\n--------", backend_metrics.backend_id);
            let filtered = filter_metrics(&backend_metrics.metrics);
            print_gauges_and_counts(&filtered);
            print_percentiles(&filtered);
        }
    }
}

fn filter_metrics(
    metrics: &BTreeMap<String, FilteredMetrics>,
) -> BTreeMap<String, FilteredMetrics> {
    let mut filtered_metrics = BTreeMap::new();

    for (metric_key, filtered_value) in metrics.iter() {
        filtered_metrics.insert(
            metric_key.replace('\t', ".").to_string(),
            filtered_value.clone(),
        );
    }
    filtered_metrics
}

fn print_gauges_and_counts(filtered_metrics: &BTreeMap<String, FilteredMetrics>) {
    let mut titles: Vec<String> = filtered_metrics
        .iter()
        .filter_map(|(title, filtered_data)| match filtered_data.inner {
            Some(filtered_metrics::Inner::Count(_)) | Some(filtered_metrics::Inner::Gauge(_)) => {
                Some(title.to_owned())
            }
            _ => None,
        })
        .collect();

    // sort the titles so they always appear in the same order
    titles.sort();

    if titles.is_empty() {
        return;
    }

    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);

    table.set_titles(Row::new(vec![cell!(""), cell!("gauge"), cell!("count")]));

    for title in titles {
        let mut row = vec![cell!(title)];
        match filtered_metrics.get(&title) {
            Some(filtered_metrics) => match filtered_metrics.inner {
                Some(filtered_metrics::Inner::Count(c)) => {
                    row.push(cell!(""));
                    row.push(cell!(c))
                }
                Some(filtered_metrics::Inner::Gauge(c)) => {
                    row.push(cell!(c));
                    row.push(cell!(""))
                }
                _ => {}
            },
            _ => row.push(cell!("")),
        }
        table.add_row(Row::new(row));
    }

    table.printstd();
}

fn print_percentiles(filtered_metrics: &BTreeMap<String, FilteredMetrics>) {
    let mut percentile_titles: Vec<String> = filtered_metrics
        .iter()
        .filter_map(|(title, filtered_data)| match filtered_data.inner.clone() {
            Some(filtered_metrics::Inner::Percentiles(_)) => Some(title.to_owned()),
            _ => None,
        })
        .collect();

    // sort the metrics so they always appear in the same order
    percentile_titles.sort();

    if percentile_titles.is_empty() {
        return;
    }

    let mut percentile_table = Table::new();
    percentile_table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);

    percentile_table.set_titles(Row::new(vec![
        cell!("Percentiles"),
        cell!("samples"),
        cell!("p50"),
        cell!("p90"),
        cell!("p99"),
        cell!("p99.9"),
        cell!("p99.99"),
        cell!("p99.999"),
        cell!("p100"),
    ]));

    for title in percentile_titles {
        if let Some(FilteredMetrics {
            inner: Some(filtered_metrics::Inner::Percentiles(percentiles)),
        }) = filtered_metrics.get(&title)
        {
            percentile_table.add_row(Row::new(vec![
                cell!(title),
                cell!(percentiles.samples),
                cell!(percentiles.p_50),
                cell!(percentiles.p_90),
                cell!(percentiles.p_99),
                cell!(percentiles.p_99_9),
                cell!(percentiles.p_99_99),
                cell!(percentiles.p_99_999),
                cell!(percentiles.p_100),
            ]));
        } else {
            println!("Something went VERY wrong here");
        }
    }

    percentile_table.printstd();
}

fn print_available_metrics(available_metrics: &AvailableMetrics) -> Result<(), DisplayError> {
    println!("Available metrics on the proxy level:");
    for metric_name in &available_metrics.proxy_metrics {
        println!("\t{metric_name}");
    }
    println!("Available metrics on the cluster level:");
    for metric_name in &available_metrics.cluster_metrics {
        println!("\t{metric_name}");
    }
    Ok(())
}

fn print_frontends(frontends: &ListedFrontends) -> Result<(), DisplayError> {
    trace!(" We received this frontends to display {:#?}", frontends);
    // HTTP frontends
    if !frontends.http_frontends.is_empty() {
        let mut table = Table::new();
        table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
        table.add_row(row!["HTTP frontends "]);
        table.add_row(row![
            "cluster_id",
            "address",
            "hostname",
            "path",
            "method",
            "position",
            "tags"
        ]);
        for http_frontend in frontends.http_frontends.iter() {
            table.add_row(row!(
                http_frontend
                    .cluster_id
                    .clone()
                    .unwrap_or("Deny".to_owned()),
                http_frontend.address.to_string(),
                http_frontend.hostname.to_string(),
                format!("{:?}", http_frontend.path),
                format!("{:?}", http_frontend.method),
                format!("{:?}", http_frontend.position),
                format_tags_to_string(&http_frontend.tags)
            ));
        }
        table.printstd();
    }

    // HTTPS frontends
    if !frontends.https_frontends.is_empty() {
        let mut table = Table::new();
        table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
        table.add_row(row!["HTTPS frontends"]);
        table.add_row(row![
            "cluster_id",
            "address",
            "hostname",
            "path",
            "method",
            "position",
            "tags"
        ]);
        for https_frontend in frontends.https_frontends.iter() {
            table.add_row(row!(
                https_frontend
                    .cluster_id
                    .clone()
                    .unwrap_or("Deny".to_owned()),
                https_frontend.address.to_string(),
                https_frontend.hostname.to_string(),
                format!("{:?}", https_frontend.path),
                format!("{:?}", https_frontend.method),
                format!("{:?}", https_frontend.position),
                format_tags_to_string(&https_frontend.tags)
            ));
        }
        table.printstd();
    }

    // TCP frontends
    if !frontends.tcp_frontends.is_empty() {
        let mut table = Table::new();
        table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
        table.add_row(row!["TCP frontends  "]);
        table.add_row(row!["Cluster ID", "address", "tags"]);
        for tcp_frontend in frontends.tcp_frontends.iter() {
            table.add_row(row!(
                tcp_frontend.cluster_id,
                tcp_frontend.address,
                format_tags_to_string(&tcp_frontend.tags)
            ));
        }
        table.printstd();
    }
    Ok(())
}

pub fn print_listeners(listeners_list: &ListenersList) -> Result<(), DisplayError> {
    println!("\nHTTP LISTENERS\n================");

    for (_, http_listener) in listeners_list.http_listeners.iter() {
        let mut table = Table::new();
        table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
        table.add_row(row![
            "socket address",
            format!("{:?}", http_listener.address)
        ]);
        table.add_row(row![
            "public address",
            format!("{:?}", http_listener.public_address),
        ]);
        table.add_row(row!["404", http_listener.answer_404]);
        table.add_row(row!["503", http_listener.answer_503]);
        table.add_row(row!["expect proxy", http_listener.expect_proxy]);
        table.add_row(row!["sticky name", http_listener.sticky_name]);
        table.add_row(row!["front timeout", http_listener.front_timeout]);
        table.add_row(row!["back timeout", http_listener.back_timeout]);
        table.add_row(row!["connect timeout", http_listener.connect_timeout]);
        table.add_row(row!["request timeout", http_listener.request_timeout]);
        table.add_row(row!["activated", http_listener.active]);
        table.printstd();
    }

    println!("\nHTTPS LISTENERS\n================");

    for (_, https_listener) in listeners_list.https_listeners.iter() {
        let mut table = Table::new();
        table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
        let mut tls_versions = String::new();
        for tls_version in https_listener.versions.iter() {
            tls_versions.push_str(&format!("{tls_version:?}\n"));
        }

        table.add_row(row![
            "socket address",
            format!("{:?}", https_listener.address)
        ]);
        table.add_row(row![
            "public address",
            format!("{:?}", https_listener.public_address)
        ]);
        table.add_row(row!["404", https_listener.answer_404,]);
        table.add_row(row!["503", https_listener.answer_503,]);
        table.add_row(row!["versions", tls_versions]);
        table.add_row(row![
            "cipher list",
            list_string_vec(&https_listener.cipher_list),
        ]);
        table.add_row(row![
            "cipher suites",
            list_string_vec(&https_listener.cipher_suites),
        ]);
        table.add_row(row![
            "signature algorithms",
            list_string_vec(&https_listener.signature_algorithms),
        ]);
        table.add_row(row![
            "groups list",
            list_string_vec(&https_listener.groups_list),
        ]);
        table.add_row(row!["key", format!("{:?}", https_listener.key),]);
        table.add_row(row!["expect proxy", https_listener.expect_proxy,]);
        table.add_row(row!["sticky name", https_listener.sticky_name,]);
        table.add_row(row!["front timeout", https_listener.front_timeout,]);
        table.add_row(row!["back timeout", https_listener.back_timeout,]);
        table.add_row(row!["connect timeout", https_listener.connect_timeout,]);
        table.add_row(row!["request timeout", https_listener.request_timeout,]);
        table.add_row(row!["activated", https_listener.active]);
        table.printstd();
    }

    println!("\nTCP LISTENERS\n================");

    if !listeners_list.tcp_listeners.is_empty() {
        let mut table = Table::new();
        table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
        table.add_row(row!["TCP frontends"]);
        table.add_row(row![
            "socket address",
            "public address",
            "expect proxy",
            "front timeout",
            "back timeout",
            "connect timeout",
            "activated"
        ]);
        for (_, tcp_listener) in listeners_list.tcp_listeners.iter() {
            table.add_row(row![
                format!("{:?}", tcp_listener.address),
                format!("{:?}", tcp_listener.public_address),
                tcp_listener.expect_proxy,
                tcp_listener.front_timeout,
                tcp_listener.back_timeout,
                tcp_listener.connect_timeout,
                tcp_listener.active,
            ]);
        }
        table.printstd();
    }
    Ok(())
}

fn print_cluster_infos(worker_responses: &WorkerResponses) -> Result<(), DisplayError> {
    let mut cluster_table = create_cluster_table(
        vec!["id", "sticky_session", "https_redirect"],
        &worker_responses.map,
    );

    let mut frontend_table =
        create_cluster_table(vec!["id", "hostname", "path"], &worker_responses.map);

    let mut https_frontend_table =
        create_cluster_table(vec!["id", "hostname", "path"], &worker_responses.map);

    let mut tcp_frontend_table = create_cluster_table(vec!["id", "address"], &worker_responses.map);

    let mut backend_table = create_cluster_table(
        vec!["backend id", "IP address", "Backup"],
        &worker_responses.map,
    );

    let worker_ids: HashSet<&String> = worker_responses.map.keys().collect();

    let mut cluster_infos = BTreeMap::new();
    let mut http_frontends = BTreeMap::new();
    let mut https_frontends = BTreeMap::new();
    let mut tcp_frontends = BTreeMap::new();
    let mut backends = BTreeMap::new();

    for (worker_id, response_content) in worker_responses.map.iter() {
        if let Some(ContentType::Clusters(clusters)) = &response_content.content_type {
            for cluster in clusters.vec.iter() {
                if cluster.configuration.is_some() {
                    let entry = cluster_infos.entry(cluster).or_insert(Vec::new());
                    entry.push(worker_id.to_owned());
                }

                for frontend in cluster.http_frontends.iter() {
                    let entry = http_frontends.entry(frontend).or_insert(Vec::new());
                    entry.push(worker_id.to_owned());
                }

                for frontend in cluster.https_frontends.iter() {
                    let entry = https_frontends.entry(frontend).or_insert(Vec::new());
                    entry.push(worker_id.to_owned());
                }

                for frontend in cluster.tcp_frontends.iter() {
                    let entry = tcp_frontends.entry(frontend).or_insert(Vec::new());
                    entry.push(worker_id.to_owned());
                }

                for backend in cluster.backends.iter() {
                    let entry = backends.entry(backend).or_insert(Vec::new());
                    entry.push(worker_id.to_owned());
                }
            }
        }
    }

    if cluster_infos.is_empty() {
        println!("no cluster found");
        return Ok(());
    }

    println!("Cluster level configuration:\n");

    for (cluster_info, workers_the_cluster_is_present_on) in cluster_infos.iter() {
        let mut row = Vec::new();
        row.push(cell!(cluster_info
            .configuration
            .as_ref()
            .map(|conf| conf.cluster_id.to_owned())
            .unwrap_or_else(|| String::from("None"))));
        row.push(cell!(cluster_info
            .configuration
            .as_ref()
            .map(|conf| conf.sticky_session)
            .unwrap_or_else(|| false)));
        row.push(cell!(cluster_info
            .configuration
            .as_ref()
            .map(|conf| conf.https_redirect)
            .unwrap_or_else(|| false)));

        for worker in workers_the_cluster_is_present_on {
            if worker_ids.contains(worker) {
                row.push(cell!("X"));
            } else {
                row.push(cell!(""));
            }
        }

        cluster_table.add_row(Row::new(row));
    }

    cluster_table.printstd();

    println!("\nHTTP frontends configuration for:\n");

    for (key, values) in http_frontends.iter() {
        let mut row = Vec::new();
        match &key.cluster_id {
            Some(cluster_id) => row.push(cell!(cluster_id)),
            None => row.push(cell!("-")),
        }
        row.push(cell!(key.hostname));
        row.push(cell!(key.path));

        for val in values.iter() {
            if worker_ids.contains(val) {
                row.push(cell!("X"));
            } else {
                row.push(cell!(""));
            }
        }

        frontend_table.add_row(Row::new(row));
    }

    frontend_table.printstd();

    println!("\nHTTPS frontends configuration for:\n");

    for (key, values) in https_frontends.iter() {
        let mut row = Vec::new();
        match &key.cluster_id {
            Some(cluster_id) => row.push(cell!(cluster_id)),
            None => row.push(cell!("-")),
        }
        row.push(cell!(key.hostname));
        row.push(cell!(key.path));

        for val in values.iter() {
            if worker_ids.contains(val) {
                row.push(cell!("X"));
            } else {
                row.push(cell!(""));
            }
        }

        https_frontend_table.add_row(Row::new(row));
    }

    https_frontend_table.printstd();

    println!("\nTCP frontends configuration:\n");

    for (key, values) in tcp_frontends.iter() {
        let mut row = vec![cell!(key.cluster_id), cell!(format!("{}", key.address))];

        for val in values.iter() {
            if worker_ids.contains(val) {
                row.push(cell!(String::from("X")));
            } else {
                row.push(cell!(String::from("")));
            }
        }

        tcp_frontend_table.add_row(Row::new(row));
    }

    tcp_frontend_table.printstd();

    println!("\nbackends configuration:\n");

    for (key, values) in backends.iter() {
        let mut row = vec![
            cell!(key.backend_id),
            cell!(format!("{}", key.address)),
            cell!(key
                .backup
                .map(|b| if b { "X" } else { "" })
                .unwrap_or_else(|| "")),
        ];

        for val in values {
            if worker_ids.contains(&val) {
                row.push(cell!("X"));
            } else {
                row.push(cell!(""));
            }
        }

        backend_table.add_row(Row::new(row));
    }

    backend_table.printstd();

    Ok(())
}

/// display all clusters in a simplified table showing their hashes
fn print_cluster_hashes(worker_responses: &WorkerResponses) -> Result<(), DisplayError> {
    let mut clusters_table = Table::new();
    clusters_table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
    let mut header = vec![cell!("cluster id")];
    for worker_id in worker_responses.map.keys() {
        header.push(cell!(format!("worker {}", worker_id)));
    }
    header.push(cell!("desynchronized"));
    clusters_table.add_row(Row::new(header));

    let mut cluster_hashes = HashMap::new();

    for response_content in worker_responses.map.values() {
        if let Some(ContentType::ClusterHashes(hashes)) = &response_content.content_type {
            for (cluster_id, hash) in hashes.map.iter() {
                cluster_hashes
                    .entry(cluster_id)
                    .or_insert(Vec::new())
                    .push(hash);
            }
        }
    }

    for (cluster_id, hashes) in cluster_hashes.iter() {
        let mut row = vec![cell!(cluster_id)];
        for val in hashes.iter() {
            row.push(cell!(format!("{val}")));
        }

        let hs: HashSet<&u64> = hashes.iter().cloned().collect();
        if hs.len() > 1 {
            row.push(cell!("X"));
        } else {
            row.push(cell!(""));
        }

        clusters_table.add_row(Row::new(row));
    }

    clusters_table.printstd();
    Ok(())
}

fn print_responses_by_worker(
    worker_responses: &WorkerResponses,
    json: bool,
) -> Result<(), DisplayError> {
    for (worker_id, content) in worker_responses.map.iter() {
        println!("Worker {}", worker_id);
        content.display(json)?;
    }

    Ok(())
}

pub fn print_certificates_with_validity(
    certs: &CertificatesWithFingerprints,
) -> Result<(), DisplayError> {
    if certs.certs.is_empty() {
        return Ok(println!("No certificates match your request."));
    }

    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_CLEAN);
    table.add_row(row![
        "fingeprint",
        "valid not before",
        "valide not after",
        "domain names",
    ]);

    for (fingerprint, cert) in &certs.certs {
        let (_unparsed, pem_certificate) =
            x509_parser::pem::parse_x509_pem(cert.certificate.as_bytes())
                .expect("Could not parse pem certificate");

        let x509_certificate = pem_certificate
            .parse_x509()
            .expect("Could not parse x509 certificate");

        let validity = x509_certificate.validity();

        table.add_row(row!(
            fingerprint,
            format_datetime(validity.not_before)?,
            format_datetime(validity.not_after)?,
            concatenate_vector(&cert.names),
        ));
    }
    table.printstd();

    Ok(())
}

fn print_certificates_by_address(list: &ListOfCertificatesByAddress) -> Result<(), DisplayError> {
    for certs in list.certificates.iter() {
        println!("\t{}:", certs.address);

        for summary in certs.certificate_summaries.iter() {
            println!("\t\t{}", summary);
        }
    }
    Ok(())
}

fn print_request_counts(request_counts: &RequestCounts) -> Result<(), DisplayError> {
    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
    table.add_row(row!["request type", "count"]);

    for (request_type, count) in &request_counts.map {
        table.add_row(row!(request_type, count));
    }
    table.printstd();
    Ok(())
}

fn format_tags_to_string(tags: &BTreeMap<String, String>) -> String {
    tags.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn list_string_vec(vec: &[String]) -> String {
    let mut output = String::new();
    for item in vec.iter() {
        output.push_str(item);
        output.push('\n');
    }
    output
}

// ISO 8601
fn format_datetime(asn1_time: ASN1Time) -> Result<String, DisplayError> {
    let datetime = asn1_time.to_datetime();

    let formatted = datetime
        .format(&format_description::well_known::Iso8601::DEFAULT)
        .map_err(|_| DisplayError::DateTime)?;
    Ok(formatted)
}

/// Creates an empty table of the form
/// ```text
/// ┌────────────┬─────────────┬───────────┬────────┐
/// │            │ header      │ header    │ header │
/// ├────────────┼─────────────┼───────────┼────────┤
/// │ cluster_id │             │           │        │
/// ├────────────┼─────────────┼───────────┼────────┤
/// │ cluster_id │             │           │        │
/// ├────────────┼─────────────┼───────────┼────────┤
/// │ cluster_id │             │           │        │
/// └────────────┴─────────────┴───────────┴────────┘
/// ```
fn create_cluster_table(headers: Vec<&str>, data: &BTreeMap<String, ResponseContent>) -> Table {
    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
    let mut row_header: Vec<_> = headers.iter().map(|h| cell!(h)).collect();
    for ref key in data.keys() {
        row_header.push(cell!(&key));
    }
    table.add_row(Row::new(row_header));
    table
}

impl Display for SocketAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", SocketAddr::from(self.clone()))
    }
}

impl Display for ProtobufEndpoint {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match &self.inner {
            Some(protobuf_endpoint::Inner::Http(HttpEndpoint {
                method,
                authority,
                path,
                status,
                ..
            })) => write!(
                f,
                "{} {} {} -> {}",
                authority.as_string_or("-"),
                method.as_string_or("-"),
                path.as_string_or("-"),
                status.as_string_or("-"),
            ),
            Some(protobuf_endpoint::Inner::Tcp(_)) => {
                write!(f, "-")
            }
            None => Ok(()),
        }
    }
}
