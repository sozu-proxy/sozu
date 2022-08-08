use std::{
    collections::{BTreeMap, HashMap, HashSet},
    process::exit,
};

use anyhow::{self, Context};
use prettytable::{Row, Table};

use sozu_command_lib::{
    command::{CommandResponseData, ListedFrontends},
    proxy::{FilteredData, QueryAnswer, QueryAnswerCertificate, QueryAnswerMetrics, Route},
};

pub fn print_frontend_list(frontends: ListedFrontends) {
    trace!(" We received this frontends to display {:#?}", frontends);
    // HTTP frontends
    if !frontends.http_frontends.is_empty() {
        let mut table = Table::new();
        table.add_row(row!["HTTP frontends"]);
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
        table.add_row(row!["TCP frontends"]);
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
    answers: BTreeMap<String, QueryAnswer>,
    json: bool,
    list: bool,
) -> anyhow::Result<()> {
    if json {
        return print_json_response(&answers);
    }

    //println!("got answers: {:#?}", answers);
    if list {
        let metrics: HashSet<_> = answers
            .values()
            .filter_map(|value| match value {
                QueryAnswer::Metrics(QueryAnswerMetrics::List(v)) => Some(v.iter()),
                _ => None,
            })
            .flatten()
            .map(|s| s.replace("\t", "."))
            .collect();
        let mut metrics: Vec<_> = metrics.iter().collect();
        metrics.sort();
        println!("available metrics: {:?}", metrics);
        return Ok(());
    }

    let answers = answers
        .iter()
        .filter_map(|(key, value)| match value {
            QueryAnswer::Metrics(QueryAnswerMetrics::Cluster(d)) => {
                let mut metrics = BTreeMap::new();
                for (cluster_id, cluster_metrics) in d.iter() {
                    for (metric_key, value) in cluster_metrics.iter() {
                        metrics.insert(
                            format!("{} {}", cluster_id, metric_key.replace("\t", ".")),
                            value.clone(),
                        );
                    }
                }
                Some((key.clone(), metrics))
            }
            QueryAnswer::Metrics(QueryAnswerMetrics::Backend(d)) => {
                let mut metrics = BTreeMap::new();
                for (cluster_id, cluster_metrics) in d.iter() {
                    for (backend_id, backend_metrics) in cluster_metrics.iter() {
                        for (metric_key, value) in backend_metrics.iter() {
                            metrics.insert(
                                format!(
                                    "{}/{} {}",
                                    cluster_id,
                                    backend_id,
                                    metric_key.replace("\t", ".")
                                ),
                                value.clone(),
                            );
                        }
                    }
                }
                Some((key.clone(), metrics))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();

    let mut metrics = answers
        .values()
        .flat_map(|map| {
            map.iter().filter_map(|(k, v)| match v {
                FilteredData::Count(_) | FilteredData::Gauge(_) => Some(k),
                _ => None,
            })
        })
        .collect::<HashSet<_>>();

    // sort the metrics so they always appear in the same order
    let mut metrics: Vec<_> = metrics.drain().collect();
    metrics.sort();
    //println!("metrics list: {:?}", metrics);

    if !metrics.is_empty() {
        let mut table = Table::new();
        table.set_format(*prettytable::format::consts::FORMAT_NO_BORDER_LINE_SEPARATOR);

        let mut row = vec![cell!("Result")];
        for key in answers.keys() {
            row.push(cell!(key));
        }
        table.set_titles(Row::new(row));

        for metric in metrics {
            let mut row = vec![cell!(metric)];
            for worker_data in answers.values() {
                match worker_data.get(metric) {
                    Some(FilteredData::Count(c)) => row.push(cell!(c)),
                    Some(FilteredData::Gauge(c)) => row.push(cell!(c)),
                    _ => row.push(cell!("")),
                }
            }
            table.add_row(Row::new(row));
        }

        table.printstd();
    }

    let mut time_metrics = answers
        .values()
        .flat_map(|map| {
            map.iter().filter_map(|(k, v)| match v {
                FilteredData::Percentiles(_) => Some(k),
                _ => None,
            })
        })
        .collect::<HashSet<_>>();

    // sort the metrics so they always appear in the same order
    let mut time_metrics: Vec<_> = time_metrics.drain().collect();
    time_metrics.sort();
    //println!("time metrics list: {:?}", time_metrics);

    if !time_metrics.is_empty() {
        let mut timing_table = Table::new();
        timing_table.set_format(*prettytable::format::consts::FORMAT_NO_BORDER_LINE_SEPARATOR);

        let mut row = vec![cell!("Results")];
        for key in answers.keys() {
            row.push(cell!(key));
        }
        timing_table.set_titles(Row::new(row));

        for metric in time_metrics {
            let mut row_samples = vec![cell!(format!("{}.samples", metric))];
            let mut row_p50 = vec![cell!(format!("{}.p50", metric))];
            let mut row_p90 = vec![cell!(format!("{}.p90", metric))];
            let mut row_p99 = vec![cell!(format!("{}.p99", metric))];
            let mut row_p99_9 = vec![cell!(format!("{}.p99.9", metric))];
            let mut row_p99_99 = vec![cell!(format!("{}.p99.99", metric))];
            let mut row_p99_999 = vec![cell!(format!("{}.p99.999", metric))];
            let mut row_p100 = vec![cell!(format!("{}.p100", metric))];

            for worker_data in answers.values() {
                match worker_data.get(metric) {
                    Some(FilteredData::Percentiles(p)) => {
                        row_samples.push(cell!(p.samples));
                        row_p50.push(cell!(p.p_50));
                        row_p90.push(cell!(p.p_90));
                        row_p99.push(cell!(p.p_99));
                        row_p99_9.push(cell!(p.p_99_9));
                        row_p99_99.push(cell!(p.p_99_99));
                        row_p99_999.push(cell!(p.p_99_999));
                        row_p100.push(cell!(p.p_100));
                    }
                    _ => {
                        row_samples.push(cell!(""));
                        row_p50.push(cell!(""));
                        row_p90.push(cell!(""));
                        row_p99.push(cell!(""));
                        row_p99_9.push(cell!(""));
                        row_p99_99.push(cell!(""));
                        row_p99_999.push(cell!(""));
                        row_p100.push(cell!(""));
                    }
                }
            }
            timing_table.add_row(Row::new(row_samples));
            timing_table.add_row(Row::new(row_p50));
            timing_table.add_row(Row::new(row_p90));
            timing_table.add_row(Row::new(row_p99));
            timing_table.add_row(Row::new(row_p99_9));
            timing_table.add_row(Row::new(row_p99_99));
            timing_table.add_row(Row::new(row_p99_999));
            timing_table.add_row(Row::new(row_p100));
        }

        timing_table.printstd();
    }
    Ok(())
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
    data: &BTreeMap<String, QueryAnswer>,
) -> Table {
    let mut table = Table::new();
    let mut row_header: Vec<_> = headers.iter().map(|h| cell!(h)).collect();
    for ref key in data.keys() {
        row_header.push(cell!(&key));
    }
    table.add_row(Row::new(row_header));
    table
}

pub fn print_query_response_data(
    cluster_id: Option<String>,
    domain: Option<String>,
    data: Option<CommandResponseData>,
    json: bool,
) -> anyhow::Result<()> {
    if let Some(needle) = cluster_id.or_else(|| domain) {
        if let Some(CommandResponseData::Query(data)) = &data {
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
                if let QueryAnswer::Clusters(clusters) = metrics {
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

            println!("Cluster level configuration for {}:\n", needle);

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

            println!("\nHTTP frontends configuration for {}:\n", needle);

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

            println!("\nHTTPS frontends configuration for {}:\n", needle);

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

            println!("\nTCP frontends configuration for {}:\n", needle);

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

            println!("\nbackends configuration for {}:\n", needle);

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
    } else {
        if let Some(CommandResponseData::Query(data)) = &data {
            let mut table = Table::new();
            let mut header = vec![cell!("key")];
            for key in data.keys() {
                header.push(cell!(&key));
            }
            header.push(cell!("desynchronized"));
            table.add_row(Row::new(header));

            let mut query_data = HashMap::new();

            for metrics in data.values() {
                //let m: u8 = metrics;
                if let QueryAnswer::ClustersHashes(clusters) = metrics {
                    for (key, value) in clusters.iter() {
                        query_data.entry(key).or_insert(Vec::new()).push(value);
                    }
                }
            }

            for (key, values) in query_data.iter() {
                let mut row = vec![cell!(key)];
                for val in values.iter() {
                    row.push(cell!(format!("{}", val)));
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
    }
    Ok(())
}

pub fn print_certificates(data: BTreeMap<String, QueryAnswer>, json: bool) -> anyhow::Result<()> {
    if json {
        print_json_response(&data)?;
        return Ok(());
    }

    //println!("received: {:?}", data);
    let it = data.iter().map(|(k, v)| match v {
        QueryAnswer::Certificates(c) => (k, c),
        v => {
            eprintln!("unexpected certificates query answer: {:?}", v);
            exit(1);
        }
    });

    for (k, v) in it {
        println!("process '{}':", k);

        match v {
            QueryAnswerCertificate::All(h) => {
                for (addr, h2) in h.iter() {
                    println!("\t{}:", addr);

                    for (domain, fingerprint) in h2.iter() {
                        println!("\t\t{}:\t{}", domain, hex::encode(fingerprint));
                    }

                    println!("");
                }
            }
            QueryAnswerCertificate::Domain(h) => {
                for (addr, opt) in h.iter() {
                    println!("\t{}:", addr);
                    if let Some((key, fingerprint)) = opt {
                        println!("\t\t{}:\t{}", key, hex::encode(fingerprint));
                    } else {
                        println!("\t\tnot found");
                    }

                    println!("");
                }
            }
            QueryAnswerCertificate::Fingerprint(opt) => {
                if let Some((s, v)) = opt {
                    println!("\tfrontends: {:?}\ncertificate:\n{}", v, s);
                } else {
                    println!("\tnot found");
                }
            }
        }
        println!("");
    }
    Ok(())
}

fn format_tags_to_string(tags: Option<&BTreeMap<String, String>>) -> String {
    tags.map(|tags| {
        tags.iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join(", ")
    })
    .unwrap_or_default()
}
