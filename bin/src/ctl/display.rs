use std::{
    collections::{BTreeMap, HashMap, HashSet},
    process::exit,
};

use anyhow::{self, Context};
use prettytable::{Row, Table};

use sozu_command_lib::{
    command::{
        AggregatedMetrics, AvailableMetrics, ListedFrontends, ListenersList, ResponseContent,
        WorkerInfo,
    },
    worker::{ClusterMetrics, FilteredMetrics, Route, WorkerCertificates, WorkerMetrics},
};

pub fn print_listeners(listeners_list: ListenersList) {
    println!("\nHTTP LISTENERS\n================");

    for (_, (http_listener, activated)) in listeners_list.http_listeners.iter() {
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
        table.add_row(row!["activated", activated]);
        table.printstd();
    }

    println!("\nHTTPS LISTENERS\n================");

    for (_, (https_listener, activated)) in listeners_list.https_listeners.iter() {
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
        table.add_row(row!["activated", activated]);
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
        for (_, (tcp_listener, activated)) in listeners_list.tcp_listeners.iter() {
            table.add_row(row![
                format!("{:?}", tcp_listener.address),
                format!("{:?}", tcp_listener.public_address),
                tcp_listener.expect_proxy,
                tcp_listener.front_timeout,
                tcp_listener.back_timeout,
                tcp_listener.connect_timeout,
                activated,
            ]);
        }
        table.printstd();
    }
}

pub fn print_status(worker_info_vec: Vec<WorkerInfo>) {
    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
    table.add_row(row!["worker id", "pid", "run state"]);

    for worker_info in worker_info_vec {
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
            "route", "address", "hostname", "path", "method", "position", "tags"
        ]);
        for http_frontend in frontends.http_frontends.iter() {
            table.add_row(row!(
                http_frontend.route,
                http_frontend.address.to_string(),
                http_frontend.hostname.to_string(),
                format!("{:?}", http_frontend.path),
                format!("{:?}", http_frontend.method),
                format!("{:?}", http_frontend.position),
                format_tags_to_string(http_frontend.tags.as_ref())
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
            "route", "address", "hostname", "path", "method", "position", "tags"
        ]);
        for https_frontend in frontends.https_frontends.iter() {
            table.add_row(row!(
                https_frontend.route,
                https_frontend.address.to_string(),
                https_frontend.hostname.to_string(),
                format!("{:?}", https_frontend.path),
                format!("{:?}", https_frontend.method),
                format!("{:?}", https_frontend.position),
                format_tags_to_string(https_frontend.tags.as_ref())
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
                format_tags_to_string(tcp_frontend.tags.as_ref())
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

    print_proxy_metrics(&Some(aggregated_metrics.main));

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

fn print_proxy_metrics(proxy_metrics: &Option<BTreeMap<String, FilteredMetrics>>) {
    if let Some(metrics) = proxy_metrics {
        let filtered = filter_metrics(metrics);
        print_gauges_and_counts(&filtered);
        print_percentiles(&filtered);
    }
}

fn print_cluster_metrics(cluster_metrics: &Option<BTreeMap<String, ClusterMetrics>>) {
    if let Some(cluster_metrics) = cluster_metrics {
        for (cluster_id, cluster_metrics_data) in cluster_metrics.iter() {
            println!("\nCluster {cluster_id}\n--------");

            if let Some(cluster) = &cluster_metrics_data.cluster {
                let filtered = filter_metrics(cluster);
                print_gauges_and_counts(&filtered);
                print_percentiles(&filtered);
            }

            if let Some(backends) = &cluster_metrics_data.backends {
                for (backend_id, backend_metrics) in backends.iter() {
                    println!("\n{cluster_id}/{backend_id}\n--------");
                    let filtered = filter_metrics(backend_metrics);
                    print_gauges_and_counts(&filtered);
                    print_percentiles(&filtered);
                }
            }
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
        .filter_map(|(title, filtered_data)| match filtered_data {
            FilteredMetrics::Count(_) | FilteredMetrics::Gauge(_) => Some(title.to_owned()),
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
            Some(FilteredMetrics::Count(c)) => {
                row.push(cell!(""));
                row.push(cell!(c))
            }
            Some(FilteredMetrics::Gauge(c)) => {
                row.push(cell!(c));
                row.push(cell!(""))
            }
            _ => row.push(cell!("")),
        }
        table.add_row(Row::new(row));
    }

    table.printstd();
}

fn print_percentiles(filtered_metrics: &BTreeMap<String, FilteredMetrics>) {
    let mut percentile_titles: Vec<String> = filtered_metrics
        .iter()
        .filter_map(|(title, filtered_data)| match filtered_data {
            FilteredMetrics::Percentiles(_) => Some(title.to_owned()),
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
        match filtered_metrics.get(&title) {
            Some(FilteredMetrics::Percentiles(p)) => {
                percentile_table.add_row(Row::new(vec![
                    cell!(title),
                    cell!(p.samples),
                    cell!(p.p_50),
                    cell!(p.p_90),
                    cell!(p.p_99),
                    cell!(p.p_99_9),
                    cell!(p.p_99_99),
                    cell!(p.p_99_999),
                    cell!(p.p_100),
                ]));
            }
            _ => println!("Something went VERY wrong here"),
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

pub fn create_queried_cluster_table(
    headers: Vec<&str>,
    response_content: &BTreeMap<String, ResponseContent>,
) -> Table {
    let mut table = Table::new();
    table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
    let mut row_header: Vec<_> = headers.iter().map(|h| cell!(h)).collect();
    for ref key in response_content.keys() {
        row_header.push(cell!(&key));
    }
    table.add_row(Row::new(row_header));
    table
}

pub fn print_query_response_data(
    cluster_id: Option<String>,
    domain: Option<String>,
    data: Option<ResponseContent>,
    json: bool,
) -> anyhow::Result<()> {
    if let Some(needle) = cluster_id.or(domain) {
        if let Some(ResponseContent::Query(data)) = &data {
            if json {
                return print_json_response(data);
            }

            let cluster_headers = vec!["id", "sticky_session", "https_redirect"];
            let mut cluster_table = create_queried_cluster_table(cluster_headers, data);

            let http_headers = vec!["id", "hostname", "path"];
            let mut frontend_table = create_queried_cluster_table(http_headers, data);

            let https_headers = vec!["id", "hostname", "path"];
            let mut https_frontend_table = create_queried_cluster_table(https_headers, data);

            let tcp_headers = vec!["id", "address"];
            let mut tcp_frontend_table = create_queried_cluster_table(tcp_headers, data);

            let backend_headers = vec!["backend id", "IP address", "Backup"];
            let mut backend_table = create_queried_cluster_table(backend_headers, data);

            let keys: HashSet<&String> = data.keys().collect();

            let mut cluster_data = HashMap::new();
            let mut frontend_data = HashMap::new();
            let mut https_frontend_data = HashMap::new();
            let mut tcp_frontend_data = HashMap::new();
            let mut backend_data = HashMap::new();

            for (key, metrics) in data.iter() {
                //let m: u8 = metrics;
                if let ResponseContent::WorkerClusters(clusters) = metrics {
                    for cluster in clusters.iter() {
                        let entry = cluster_data.entry(cluster).or_insert(Vec::new());
                        entry.push(key.to_owned());

                        for frontend in cluster.http_frontends.iter() {
                            let entry = frontend_data.entry(frontend).or_insert(Vec::new());
                            entry.push(key.to_owned());
                        }

                        for frontend in cluster.https_frontends.iter() {
                            let entry = https_frontend_data.entry(frontend).or_insert(Vec::new());
                            entry.push(key.to_owned());
                        }

                        for frontend in cluster.tcp_frontends.iter() {
                            let entry = tcp_frontend_data.entry(frontend).or_insert(Vec::new());
                            entry.push(key.to_owned());
                        }

                        for backend in cluster.backends.iter() {
                            let entry = backend_data.entry(backend).or_insert(Vec::new());
                            entry.push(key.to_owned());
                        }
                    }
                }
            }

            println!("Cluster level configuration for {needle}:\n");

            for (key, values) in cluster_data.iter() {
                let mut row = Vec::new();
                row.push(cell!(key
                    .configuration
                    .as_ref()
                    .map(|conf| conf.cluster_id.to_owned())
                    .unwrap_or_else(String::new)));
                row.push(cell!(key
                    .configuration
                    .as_ref()
                    .map(|conf| conf.sticky_session)
                    .unwrap_or_else(|| false)));
                row.push(cell!(key
                    .configuration
                    .as_ref()
                    .map(|conf| conf.https_redirect)
                    .unwrap_or_else(|| false)));

                for val in values {
                    if keys.contains(val) {
                        row.push(cell!("X"));
                    } else {
                        row.push(cell!(""));
                    }
                }

                cluster_table.add_row(Row::new(row));
            }

            cluster_table.printstd();

            println!("\nHTTP frontends configuration for {needle}:\n");

            for (key, values) in frontend_data.iter() {
                let mut row = Vec::new();
                match &key.route {
                    Route::ClusterId(cluster_id) => row.push(cell!(cluster_id)),
                    Route::Deny => row.push(cell!("-")),
                }
                row.push(cell!(key.hostname));
                row.push(cell!(key.path));

                for val in values.iter() {
                    if keys.contains(val) {
                        row.push(cell!("X"));
                    } else {
                        row.push(cell!(""));
                    }
                }

                frontend_table.add_row(Row::new(row));
            }

            frontend_table.printstd();

            println!("\nHTTPS frontends configuration for {needle}:\n");

            for (key, values) in https_frontend_data.iter() {
                let mut row = Vec::new();
                match &key.route {
                    Route::ClusterId(cluster_id) => row.push(cell!(cluster_id)),
                    Route::Deny => row.push(cell!("-")),
                }
                row.push(cell!(key.hostname));
                row.push(cell!(key.path));

                for val in values.iter() {
                    if keys.contains(val) {
                        row.push(cell!("X"));
                    } else {
                        row.push(cell!(""));
                    }
                }

                https_frontend_table.add_row(Row::new(row));
            }

            https_frontend_table.printstd();

            println!("\nTCP frontends configuration for {needle}:\n");

            for (key, values) in tcp_frontend_data.iter() {
                let mut row = vec![cell!(key.cluster_id), cell!(format!("{}", key.address))];

                for val in values.iter() {
                    if keys.contains(val) {
                        row.push(cell!(String::from("X")));
                    } else {
                        row.push(cell!(String::from("")));
                    }
                }

                tcp_frontend_table.add_row(Row::new(row));
            }

            tcp_frontend_table.printstd();

            println!("\nbackends configuration for {needle}:\n");

            for (key, values) in backend_data.iter() {
                let mut row = vec![
                    cell!(key.backend_id),
                    cell!(format!("{}", key.address)),
                    cell!(key
                        .backup
                        .map(|b| if b { "X" } else { "" })
                        .unwrap_or_else(|| "")),
                ];

                for val in values {
                    if keys.contains(&val) {
                        row.push(cell!("X"));
                    } else {
                        row.push(cell!(""));
                    }
                }

                backend_table.add_row(Row::new(row));
            }

            backend_table.printstd();
        }
    } else if let Some(ResponseContent::Query(data)) = &data {
        let mut table = Table::new();
        table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
        let mut header = vec![cell!("key")];
        for key in data.keys() {
            header.push(cell!(&key));
        }
        header.push(cell!("desynchronized"));
        table.add_row(Row::new(header));

        let mut query_data = HashMap::new();

        for metrics in data.values() {
            //let m: u8 = metrics;
            if let ResponseContent::WorkerClustersHashes(clusters) = metrics {
                for (key, value) in clusters.iter() {
                    query_data.entry(key).or_insert(Vec::new()).push(value);
                }
            }
        }

        for (key, values) in query_data.iter() {
            let mut row = vec![cell!(key)];
            for val in values.iter() {
                row.push(cell!(format!("{val}")));
            }

            let hs: HashSet<&u64> = values.iter().cloned().collect();
            if hs.len() > 1 {
                row.push(cell!("X"));
            } else {
                row.push(cell!(""));
            }

            table.add_row(Row::new(row));
        }

        table.printstd();
    }
    Ok(())
}

pub fn print_certificates(
    response_content: BTreeMap<String, ResponseContent>,
    json: bool,
) -> anyhow::Result<()> {
    if json {
        print_json_response(&response_content)?;
        return Ok(());
    }

    //println!("received: {:?}", data);
    let it = response_content.iter().map(|(k, v)| match v {
        ResponseContent::WorkerCertificates(c) => (k, c),
        v => {
            eprintln!("unexpected certificates query answer: {v:?}");
            exit(1);
        }
    });

    for (k, value) in it {
        println!("process '{k}':");

        match value {
            WorkerCertificates::All(h) => {
                for (addr, h2) in h.iter() {
                    println!("\t{addr}:");

                    for (domain, fingerprint) in h2.iter() {
                        println!("\t\t{}:\t{}", domain, hex::encode(fingerprint.0.clone()));
                    }

                    println!();
                }
            }
            WorkerCertificates::Domain(h) => {
                for (addr, opt) in h.iter() {
                    println!("\t{addr}:");
                    if let Some((key, fingerprint)) = opt {
                        println!("\t\t{}:\t{}", key, hex::encode(fingerprint.0.clone()));
                    } else {
                        println!("\t\tnot found");
                    }

                    println!();
                }
            }
            WorkerCertificates::Fingerprint(opt) => {
                if let Some((s, v)) = opt {
                    println!("\tfrontends: {v:?}\ncertificate:\n{s}");
                } else {
                    println!("\tnot found");
                }
            }
        }
        println!();
    }
    Ok(())
}

fn format_tags_to_string(tags: Option<&BTreeMap<String, String>>) -> String {
    tags.map(|tags| {
        tags.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    })
    .unwrap_or_default()
}

pub fn print_available_metrics(available_metrics: &AvailableMetrics) -> anyhow::Result<()> {
    println!("Available metrics on the main process:");
    for metric in available_metrics.main.iter() {
        println!("\t{metric}");
    }
    for worker in available_metrics.workers.iter() {
        println!("Available metrics on worker {}:", worker.0);
        println!("\tProxy metrics: {:?}", worker.1.proxy_metrics);
        println!("\tCluster metrics: {:?}", worker.1.cluster_metrics);
    }
    Ok(())
}

fn list_string_vec(vec: &Vec<String>) -> String {
    let mut output = String::new();
    for item in vec.iter() {
        output.push_str(item);
        output.push('\n');
    }
    output
}
