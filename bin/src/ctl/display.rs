use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{self, Context};
use prettytable::{Row, Table};

use sozu_command_lib::proto::{
    command::{
        filtered_metrics, response_content::ContentType, AggregatedMetrics, AvailableMetrics,
        CertificateAndKey, CertificatesWithFingerprints, ClusterMetrics, FilteredMetrics,
        ListedFrontends, ListenersList, ResponseContent, WorkerInfos, WorkerMetrics,
        WorkerResponses,
    },
    display::concatenate_vector,
};
use time::format_description;
use x509_parser::time::ASN1Time;

pub fn print_listeners(listeners_list: ListenersList) {
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
}

pub fn print_status(worker_infos: WorkerInfos) {
    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
    table.add_row(row!["worker id", "pid", "run state"]);

    for worker_info in worker_infos.vec {
        let row = row!(worker_info.id, worker_info.pid, worker_info.run_state);
        table.add_row(row);
    }

    table.printstd();
}

pub fn print_frontend_list(frontends: ListedFrontends) {
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
}

pub fn print_metrics(
    // main & worker metrics
    aggregated_metrics: AggregatedMetrics,
    json: bool,
) -> anyhow::Result<()> {
    if json {
        println!("Here are the metrics, per worker");
        return print_json_response(&aggregated_metrics);
    }

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

fn print_worker_metrics(worker_metrics: &WorkerMetrics) -> anyhow::Result<()> {
    print_proxy_metrics(&worker_metrics.proxy);
    print_cluster_metrics(&worker_metrics.clusters);

    Ok(())
}

fn print_proxy_metrics(proxy_metrics: &BTreeMap<String, FilteredMetrics>) {
    let filtered = filter_metrics(proxy_metrics);
    print_gauges_and_counts(&filtered);
    print_percentiles(&filtered);
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

pub fn print_json_response<T: ::serde::Serialize>(input: &T) -> Result<(), anyhow::Error> {
    println!(
        "{}",
        serde_json::to_string_pretty(&input).context("Error while parsing response to JSON")?
    );
    Ok(())
}

/// Creates an empty table of the form
/// ```
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
pub fn create_cluster_table(headers: Vec<&str>, data: &BTreeMap<String, ResponseContent>) -> Table {
    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
    let mut row_header: Vec<_> = headers.iter().map(|h| cell!(h)).collect();
    for ref key in data.keys() {
        row_header.push(cell!(&key));
    }
    table.add_row(Row::new(row_header));
    table
}

pub fn print_cluster_responses(
    cluster_id: Option<String>,
    domain: Option<String>,
    worker_responses: WorkerResponses,
    json: bool,
) -> anyhow::Result<()> {
    if let Some(needle) = cluster_id.or(domain) {
        if json {
            return print_json_response(&worker_responses);
        }

        let mut cluster_table = create_cluster_table(
            vec!["id", "sticky_session", "https_redirect"],
            &worker_responses.map,
        );

        let mut frontend_table =
            create_cluster_table(vec!["id", "hostname", "path"], &worker_responses.map);

        let mut https_frontend_table =
            create_cluster_table(vec!["id", "hostname", "path"], &worker_responses.map);

        let mut tcp_frontend_table =
            create_cluster_table(vec!["id", "address"], &worker_responses.map);

        let mut backend_table = create_cluster_table(
            vec!["backend id", "IP address", "Backup"],
            &worker_responses.map,
        );

        let worker_ids: HashSet<&String> = worker_responses.map.keys().collect();

        let mut cluster_infos = HashMap::new();
        let mut http_frontends = HashMap::new();
        let mut https_frontends = HashMap::new();
        let mut tcp_frontends = HashMap::new();
        let mut backends = HashMap::new();

        for (worker_id, response_content) in worker_responses.map.iter() {
            if let Some(ContentType::Clusters(clusters)) = &response_content.content_type {
                for cluster in clusters.vec.iter() {
                    let entry = cluster_infos.entry(cluster).or_insert(Vec::new());
                    entry.push(worker_id.to_owned());

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

        println!("Cluster level configuration for {needle}:\n");

        for (cluster_info, workers_the_cluster_is_present_on) in cluster_infos.iter() {
            let mut row = Vec::new();
            row.push(cell!(cluster_info
                .configuration
                .as_ref()
                .map(|conf| conf.cluster_id.to_owned())
                .unwrap_or_else(String::new)));
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

        println!("\nHTTP frontends configuration for {needle}:\n");

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

        println!("\nHTTPS frontends configuration for {needle}:\n");

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

        println!("\nTCP frontends configuration for {needle}:\n");

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

        println!("\nbackends configuration for {needle}:\n");

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

        return Ok(());
    }

    // display all clusters in a simplified table showing their hashes
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

pub fn print_certificates_by_worker(
    response_contents: BTreeMap<String, ResponseContent>,
    json: bool,
) -> anyhow::Result<()> {
    if json {
        print_json_response(&response_contents)?;
        return Ok(());
    }

    for (worker_id, response_content) in response_contents.iter() {
        println!("Worker {}", worker_id);
        match &response_content.content_type {
            Some(ContentType::CertificatesByAddress(list)) => {
                for certs in list.certificates.iter() {
                    println!("\t{}:", certs.address);

                    for summary in certs.certificate_summaries.iter() {
                        println!("\t\t{}", summary);
                    }

                    println!();
                }
            }
            Some(ContentType::CertificatesWithFingerprints(CertificatesWithFingerprints {
                certs,
            })) => print_certificates_with_validity(certs.clone())?,

            _ => {}
        }
        println!();
    }
    Ok(())
}

fn format_tags_to_string(tags: &BTreeMap<String, String>) -> String {
    tags.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn print_available_metrics(available_metrics: &AvailableMetrics) -> anyhow::Result<()> {
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

fn list_string_vec(vec: &[String]) -> String {
    let mut output = String::new();
    for item in vec.iter() {
        output.push_str(item);
        output.push('\n');
    }
    output
}

pub fn print_certificates_with_validity(
    certs: BTreeMap<String, CertificateAndKey>,
) -> anyhow::Result<()> {
    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_CLEAN);
    table.add_row(row![
        "fingeprint",
        "valid not before",
        "valide not after",
        "domain names",
    ]);

    for (fingerprint, cert) in certs {
        let (_unparsed, pem_certificate) =
            x509_parser::pem::parse_x509_pem(cert.certificate.as_bytes())
                .with_context(|| "Could not parse pem certificate")?;

        let x509_certificate = pem_certificate
            .parse_x509()
            .with_context(|| "Could not parse x509 certificate")?;

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

// ISO 8601
fn format_datetime(asn1_time: ASN1Time) -> anyhow::Result<String> {
    let datetime = asn1_time.to_datetime();

    let formatted = datetime
        .format(&format_description::well_known::Iso8601::DEFAULT)
        .with_context(|| "Could not format the datetime to ISO 8601")?;
    Ok(formatted)
}
