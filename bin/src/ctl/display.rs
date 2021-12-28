use anyhow::{self, Context};
use prettytable::{Row, Table};
use sozu_command_lib::{
    command::{CommandResponseData, ListedFrontends},
    proxy::{FilteredData, QueryAnswer, QueryAnswerMetrics, Route},
};

use std::collections::{BTreeMap, HashMap, HashSet};

pub fn print_frontend_list(frontends: ListedFrontends) {
    trace!(" We received this frontends to display {:#?}", frontends);
    // HTTP frontends
    if !frontends.http_frontends.is_empty() {
        let mut table = Table::new();
        table.add_row(row!["HTTP frontends"]);
        table.add_row(row![
            "route", "address", "hostname", "path", "method", "position"
        ]);
        for http_frontend in frontends.http_frontends.iter() {
            table.add_row(row!(
                http_frontend.route,
                http_frontend.address.to_string(),
                http_frontend.hostname.to_string(),
                format!("{:?}", http_frontend.path),
                format!("{:?}", http_frontend.method),
                format!("{:?}", http_frontend.position),
            ));
        }
        table.printstd();
    }

    // HTTPS frontends
    if !frontends.https_frontends.is_empty() {
        let mut table = Table::new();
        table.add_row(row!["HTTPS frontends"]);
        table.add_row(row![
            "route", "address", "hostname", "path", "method", "position"
        ]);
        for https_frontend in frontends.https_frontends.iter() {
            table.add_row(row!(
                https_frontend.route,
                https_frontend.address.to_string(),
                https_frontend.hostname.to_string(),
                format!("{:?}", https_frontend.path),
                format!("{:?}", https_frontend.method),
                format!("{:?}", https_frontend.position),
            ));
        }
        table.printstd();
    }

    // TCP frontends
    if !frontends.tcp_frontends.is_empty() {
        let mut table = Table::new();
        table.add_row(row!["TCP frontends"]);
        table.add_row(row!["Cluster ID", "address"]);
        for tcp_frontend in frontends.tcp_frontends.iter() {
            table.add_row(row!(tcp_frontend.cluster_id, tcp_frontend.address,));
        }
        table.printstd();
    }
}

pub fn print_query_answers(
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

    print_metrics("Result", &answers);
    Ok(())
}

// input: map worker_id -> (map key -> value)
pub fn print_metrics(
    table_name: &str,
    b_tree_map: &BTreeMap<String, BTreeMap<String, FilteredData>>,
) {
    let mut metrics = b_tree_map
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

        let mut row = vec![cell!(table_name)];
        for key in b_tree_map.keys() {
            row.push(cell!(key));
        }
        table.set_titles(Row::new(row));

        for metric in metrics {
            let mut row = vec![cell!(metric)];
            for worker_data in b_tree_map.values() {
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

    let mut time_metrics = b_tree_map
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

        let mut row = vec![cell!(table_name)];
        for key in b_tree_map.keys() {
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

            for worker_data in b_tree_map.values() {
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
}

pub fn print_json_response<T: ::serde::Serialize>(input: &T) -> Result<(), anyhow::Error> {
    println!(
        "{}",
        serde_json::to_string_pretty(&input).context("Error while parsing response to JSON")?
    );
    Ok(())
}

pub fn create_queried_application_table(
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
    application_id: Option<String>,
    domain: Option<String>,
    data: Option<CommandResponseData>,
    json: bool,
) -> anyhow::Result<()> {
    if let Some(needle) = application_id.or(domain) {
        if let Some(CommandResponseData::Query(data)) = data {
            if json {
                return print_json_response(&data);
            }

            let application_headers = vec!["id", "sticky_session", "https_redirect"];
            let mut application_table =
                create_queried_application_table(application_headers, &data);

            let http_headers = vec!["id", "hostname", "path"];
            let mut frontend_table = create_queried_application_table(http_headers, &data);

            let https_headers = vec!["id", "hostname", "path"];
            let mut https_frontend_table = create_queried_application_table(https_headers, &data);

            let tcp_headers = vec!["id", "address"];
            let mut tcp_frontend_table = create_queried_application_table(tcp_headers, &data);

            let backend_headers = vec!["backend id", "IP address", "Backup"];
            let mut backend_table = create_queried_application_table(backend_headers, &data);

            let keys: HashSet<&String> = data.keys().collect();

            let mut application_data = HashMap::new();
            let mut frontend_data = HashMap::new();
            let mut https_frontend_data = HashMap::new();
            let mut tcp_frontend_data = HashMap::new();
            let mut backend_data = HashMap::new();

            for (ref key, ref metrics) in data.iter() {
                //let m: u8 = metrics;
                if let &QueryAnswer::Applications(ref apps) = *metrics {
                    for app in apps.iter() {
                        let entry = application_data.entry(app).or_insert(Vec::new());
                        entry.push((*key).clone());

                        for frontend in app.http_frontends.iter() {
                            let entry = frontend_data.entry(frontend).or_insert(Vec::new());
                            entry.push((*key).clone());
                        }

                        for frontend in app.https_frontends.iter() {
                            let entry = https_frontend_data.entry(frontend).or_insert(Vec::new());
                            entry.push((*key).clone());
                        }

                        for frontend in app.tcp_frontends.iter() {
                            let entry = tcp_frontend_data.entry(frontend).or_insert(Vec::new());
                            entry.push((*key).clone());
                        }

                        for backend in app.backends.iter() {
                            let entry = backend_data.entry(backend).or_insert(Vec::new());
                            entry.push((*key).clone());
                        }
                    }
                }
            }

            println!("Cluster level configuration for {}:\n", needle);

            for (ref key, ref values) in application_data.iter() {
                let mut row = Vec::new();
                row.push(cell!(key
                    .configuration
                    .clone()
                    .map(|conf| conf.cluster_id)
                    .unwrap_or(String::from(""))));
                row.push(cell!(key
                    .configuration
                    .clone()
                    .map(|conf| conf.sticky_session)
                    .unwrap_or(false)));
                row.push(cell!(key
                    .configuration
                    .clone()
                    .map(|conf| conf.https_redirect)
                    .unwrap_or(false)));

                for val in values.iter() {
                    if keys.contains(val) {
                        row.push(cell!(String::from("X")));
                    } else {
                        row.push(cell!(String::from("")));
                    }
                }

                application_table.add_row(Row::new(row));
            }

            application_table.printstd();

            println!("\nHTTP frontends configuration for {}:\n", needle);

            for (ref key, ref values) in frontend_data.iter() {
                let mut row = Vec::new();
                match &key.route {
                    Route::ClusterId(cluster_id) => row.push(cell!(cluster_id)),
                    Route::Deny => row.push(cell!("-")),
                }
                row.push(cell!(key.hostname));
                row.push(cell!(key.path));

                for val in values.iter() {
                    if keys.contains(val) {
                        row.push(cell!(String::from("X")));
                    } else {
                        row.push(cell!(String::from("")));
                    }
                }

                frontend_table.add_row(Row::new(row));
            }

            frontend_table.printstd();

            println!("\nHTTPS frontends configuration for {}:\n", needle);

            for (ref key, ref values) in https_frontend_data.iter() {
                let mut row = Vec::new();
                match &key.route {
                    Route::ClusterId(cluster_id) => row.push(cell!(cluster_id)),
                    Route::Deny => row.push(cell!("-")),
                }
                row.push(cell!(key.hostname));
                row.push(cell!(key.path));

                for val in values.iter() {
                    if keys.contains(val) {
                        row.push(cell!(String::from("X")));
                    } else {
                        row.push(cell!(String::from("")));
                    }
                }

                https_frontend_table.add_row(Row::new(row));
            }

            https_frontend_table.printstd();

            println!("\nTCP frontends configuration for {}:\n", needle);

            for (ref key, ref values) in tcp_frontend_data.iter() {
                let mut row = Vec::new();
                row.push(cell!(key.cluster_id));
                row.push(cell!(format!("{}", key.address)));

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

            for (ref key, ref values) in backend_data.iter() {
                let mut row = Vec::new();
                let backend_backup = key.backup.map(|b| if b { "X" } else { "" }).unwrap_or("");
                row.push(cell!(key.backend_id));
                row.push(cell!(format!("{}", key.address)));
                row.push(cell!(backend_backup));

                for val in values.iter() {
                    if keys.contains(val) {
                        row.push(cell!(String::from("X")));
                    } else {
                        row.push(cell!(String::from("")));
                    }
                }

                backend_table.add_row(Row::new(row));
            }

            backend_table.printstd();
        }
    } else {
        if let Some(CommandResponseData::Query(data)) = data {
            let mut table = Table::new();
            let mut header = Vec::new();
            header.push(cell!("key"));
            for ref key in data.keys() {
                header.push(cell!(&key));
            }
            header.push(cell!("desynchronized"));
            table.add_row(Row::new(header));

            let mut query_data = HashMap::new();

            for ref metrics in data.values() {
                //let m: u8 = metrics;
                if let &QueryAnswer::ApplicationsHashes(ref apps) = *metrics {
                    for (ref key, ref value) in apps.iter() {
                        (*(query_data.entry((*key).clone()).or_insert(Vec::new()))).push(*value);
                    }
                }
            }

            for (ref key, ref values) in query_data.iter() {
                let mut row = Vec::new();
                row.push(cell!(key));

                for val in values.iter() {
                    row.push(cell!(format!("{}", val)));
                }

                let hs: HashSet<&u64> = values.iter().cloned().collect();

                let diff = hs.len() > 1;

                if diff {
                    row.push(cell!(String::from("X")));
                } else {
                    row.push(cell!(String::from("")));
                }

                table.add_row(Row::new(row));
            }

            table.printstd();
        }
    }
    Ok(())
}
