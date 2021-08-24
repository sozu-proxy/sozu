use crate::cli::MetricsCmd;
use sozu_command::certificate::{calculate_fingerprint, split_certificate_chain};
use sozu_command::channel::Channel;
use sozu_command::command::{
    CommandRequest, CommandRequestData, CommandResponse, CommandResponseData, CommandStatus,
    RunState, WorkerInfo,
};
use sozu_command::config::{Config, FileListenerProtocolConfig, Listener, ProxyProtocolConfig};
use sozu_command::proxy::{
    ActivateListener, AddCertificate, Backend, CertificateAndKey, CertificateFingerprint, Cluster,
    DeactivateListener, FilteredData, HttpFrontend, ListenerType, LoadBalancingAlgorithms,
    LoadBalancingParams, MetricsConfiguration, PathRule, ProxyRequestData, Query, QueryAnswer,
    QueryAnswerCertificate, QueryAnswerMetrics, QueryApplicationDomain, QueryApplicationType,
    QueryCertificateType, QueryMetricsType, RemoveBackend, RemoveCertificate, RemoveListener,
    ReplaceCertificate, Route, RulePosition, TcpFrontend, TcpListener, TlsVersion,
};

use super::create_channel;
use anyhow::{self, bail, Context};
use prettytable::{Row, Table};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use serde_json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::process::exit;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// Used to display the JSON response of the status command
#[derive(Serialize, Debug)]
struct WorkerStatus<'a> {
    pub worker: &'a WorkerInfo,
    pub status: &'a String,
}

fn generate_id() -> String {
    let s: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(6)
        .map(|c| c as char)
        .collect();
    format!("ID-{}", s)
}

fn generate_tagged_id(tag: &str) -> String {
    let s: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(6)
        .map(|c| c as char)
        .collect();
    format!("{}-{}", tag, s)
}

// Run the code waiting for messages in a separate thread. Just before finishing the thread sends a message.
// The calling code waits for this message with a timeout.
// Note: This macro is used only for simple command which has any/simple computing
// to do with the message received.
macro_rules! command_timeout {
  ($duration: expr, $block: expr) => (
    if $duration == 0 {
      $block
    } else {
      let (send, recv) = mpsc::channel();

      thread::spawn(move || {
        $block
        serde::__private::Ok::<(), anyhow::Error>(send.send(())?)

      });

      if recv.recv_timeout(Duration::from_millis($duration)).is_err() {
        eprintln!("Command timeout. The proxy didn't send an answer");
      }
    }
  )
}

pub fn save_state(
    mut channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    path: String,
) -> Result<(), anyhow::Error> {
    let id = generate_id();
    channel.write_message(&CommandRequest::new(
        id.clone(),
        CommandRequestData::SaveState { path },
        None,
    ));

    command_timeout!(timeout, {
        match channel.read_message() {
            None => {
                bail!("the proxy didn't answer");
            }
            Some(message) => {
                if id != message.id {
                    bail!("received message with invalid id: {:?}", message);
                }
                match message.status {
                    CommandStatus::Processing => {
                        // do nothing here
                        // for other messages, we would loop over read_message
                        // until an error or ok message was sent
                    }
                    CommandStatus::Error => {
                        bail!("could not save proxy state: {}", message.message)
                    }
                    CommandStatus::Ok => {
                        println!("{}", message.message);
                    }
                }
            }
        }
    });
    Ok(())
}

pub fn load_state(
    mut channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    path: String,
) -> Result<(), anyhow::Error> {
    let id = generate_id();
    channel.write_message(&CommandRequest::new(
        id.clone(),
        CommandRequestData::LoadState { path: path.clone() },
        None,
    ));

    command_timeout!(timeout, {
        match channel.read_message() {
            None => {
                bail!("the proxy didn't answer")
            }
            Some(message) => {
                if id != message.id {
                    bail!("received message with invalid id: {:?}", message);
                }
                match message.status {
                    CommandStatus::Processing => {
                        // do nothing here
                        // for other messages, we would loop over read_message
                        // until an error or ok message was sent
                    }
                    CommandStatus::Error => {
                        bail!("could not load proxy state: {}", message.message)
                    }
                    CommandStatus::Ok => {
                        println!("Proxy state loaded successfully from {}", path);
                    }
                }
            }
        }
    });
    Ok(())
}

pub fn dump_state(
    mut channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    json: bool,
) -> Result<(), anyhow::Error> {
    let id = generate_id();
    channel.write_message(&CommandRequest::new(
        id.clone(),
        CommandRequestData::DumpState,
        None,
    ));

    command_timeout!(timeout, {
        match channel.read_message() {
            None => {
                bail!("the proxy didn't answer");
            }
            Some(message) => {
                if id != message.id {
                    bail!("received message with invalid id: {:?}", message);
                }
                match message.status {
                    CommandStatus::Processing => {
                        // do nothing here
                        // for other messages, we would loop over read_message
                        // until an error or ok message was sent
                    }
                    CommandStatus::Error => {
                        if json {
                            print_json_response(&message.message)?;
                        }
                        bail!("could not dump proxy state: {}", message.message);
                    }
                    CommandStatus::Ok => {
                        if let Some(CommandResponseData::State(state)) = message.data {
                            if json {
                                print_json_response(&state)?;
                            } else {
                                println!("{:#?}", state);
                            }
                            return Ok(());
                        }
                        bail!("state dump was empty");
                    }
                }
            }
        }
    });
    Ok(())
}

pub fn soft_stop(
    mut channel: Channel<CommandRequest, CommandResponse>,
    proxy_id: Option<u32>,
) -> Result<(), anyhow::Error> {
    println!("shutting down proxy");
    let id = generate_id();
    channel.write_message(&CommandRequest::new(
        id.clone(),
        CommandRequestData::Proxy(ProxyRequestData::SoftStop),
        proxy_id,
    ));

    loop {
        match channel.read_message() {
            None => {
                bail!("the proxy didn't answer");
            }
            Some(message) => {
                if &id != &message.id {
                    bail!("received message with invalid id: {:?}", message);
                }
                match message.status {
                    CommandStatus::Processing => {
                        println!("Proxy is processing: {}", message.message);
                    }
                    CommandStatus::Error => {
                        bail!("could not stop the proxy: {}", message.message);
                    }
                    CommandStatus::Ok => {
                        println!("Proxy shut down with message: \"{}\"", message.message);
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn hard_stop(
    mut channel: Channel<CommandRequest, CommandResponse>,
    proxy_id: Option<u32>,
    timeout: u64,
) -> Result<(), anyhow::Error> {
    println!("shutting down proxy");
    let id = generate_id();
    channel.write_message(&CommandRequest::new(
        id.clone(),
        CommandRequestData::Proxy(ProxyRequestData::HardStop),
        proxy_id,
    ));

    command_timeout!(
        timeout,
        loop {
            match channel.read_message() {
                None => {
                    bail!("the proxy didn't answer");
                }
                Some(message) => match message.status {
                    CommandStatus::Processing => {
                        println!("Proxy is processing: {}", message.message);
                    }
                    CommandStatus::Error => {
                        bail!("could not stop the proxy: {}", message.message);
                    }
                    CommandStatus::Ok => {
                        if &id == &message.id {
                            println!("Proxy shut down: {}", message.message);
                            break;
                        }
                    }
                },
            }
        }
    );
    Ok(())
}

pub fn upgrade_main(
    mut channel: Channel<CommandRequest, CommandResponse>,
    config: &Config,
) -> Result<(), anyhow::Error> {
    println!("Preparing to upgrade proxy...");

    let id = generate_tagged_id("LIST-WORKERS");
    channel.write_message(&CommandRequest::new(
        id.clone(),
        CommandRequestData::ListWorkers,
        None,
    ));

    loop {
        match channel.read_message() {
            None => {
                bail!("Error: the proxy didn't list workers");
            }
            Some(message) => {
                if id != message.id {
                    bail!("Error: received unexpected message: {:?}", message);
                }
                match message.status {
                    CommandStatus::Processing => {
                        error!("Error: the proxy didn't return list of workers immediately");
                    }
                    CommandStatus::Error => {
                        bail!(
                            "Error: failed to get the list of worker: {}",
                            message.message
                        );
                    }
                    CommandStatus::Ok => {
                        if let Some(CommandResponseData::Workers(ref workers)) = message.data {
                            let mut table = Table::new();
                            table.add_row(row!["Worker", "pid", "run state"]);
                            for ref worker in workers.iter() {
                                let run_state = format!("{:?}", worker.run_state);
                                table.add_row(row![worker.id, worker.pid, run_state]);
                            }
                            println!("");
                            table.printstd();
                            println!("");

                            let id = generate_tagged_id("UPGRADE-MAIN");
                            channel.write_message(&CommandRequest::new(
                                id.clone(),
                                CommandRequestData::UpgradeMain,
                                None,
                            ));
                            println!("Upgrading main process");

                            loop {
                                match channel.read_message() {
                                    None => {
                                        bail!("Error: the proxy didn't start main upgrade");
                                    }
                                    Some(message) => {
                                        if &id != &message.id {
                                            bail!(
                                                "Error: received unexpected message: {:?}",
                                                message
                                            );
                                        }
                                        match message.status {
                                            CommandStatus::Processing => {}
                                            CommandStatus::Error => {
                                                bail!(
                                                    "Error: failed to upgrade the main: {}",
                                                    message.message
                                                );
                                            }
                                            CommandStatus::Ok => {
                                                println!(
                                                    "Main process upgrade succeeded: {}",
                                                    message.message
                                                );
                                                break;
                                            }
                                        }
                                    }
                                }
                            }

                            // Reconnect to the new main
                            println!("Reconnecting to new main process...");
                            let mut channel = create_channel(&config).with_context(|| {
                                "could not reconnect to the command unix socket"
                            })?;

                            // Do a rolling restart of the workers
                            let running_workers = workers
                                .iter()
                                .filter(|worker| worker.run_state == RunState::Running)
                                .collect::<Vec<_>>();
                            let running_count = running_workers.len();
                            for (i, ref worker) in running_workers.iter().enumerate() {
                                println!("Upgrading worker {} (of {})", i + 1, running_count);

                                channel = upgrade_worker(channel, 0, worker.id)?;
                                //thread::sleep(Duration::from_millis(1000));
                            }

                            println!("Proxy successfully upgraded!");
                        }
                        break Ok(());
                    }
                }
            }
        }
    }
}

pub fn upgrade_worker(
    mut channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    worker_id: u32,
) -> Result<Channel<CommandRequest, CommandResponse>, anyhow::Error> {
    println!("upgrading worker {}", worker_id);
    let id = generate_id();
    channel.write_message(&CommandRequest::new(
        id.clone(),
        CommandRequestData::UpgradeWorker(worker_id),
        //FIXME: we should be able to soft stop one specific worker
        None,
    ));

    // We do our own timeout so we can return the Channel object from the thread
    // and avoid ownership issues
    let (send, recv) = mpsc::channel();

    let timeout_thread = thread::spawn(move || {
        loop {
            let message = channel.read_message();
            match message {
                None => bail!("the proxy didn't answer"),
                Some(message) => match message.status {
                    CommandStatus::Processing => {
                        info!("Worker {} is processing: {}", worker_id, message.message);
                    }
                    CommandStatus::Error => bail!(
                        "could not stop the worker {}: {}",
                        worker_id,
                        message.message
                    ),
                    CommandStatus::Ok => {
                        if &id == &message.id {
                            info!("Worker {} shut down: {}", worker_id, message.message);
                            break;
                        }
                    }
                },
            }
        }
        send.send(())?;
        Ok(channel)
    });

    if timeout > 0 && recv.recv_timeout(Duration::from_millis(timeout)).is_err() {
        bail!("Command timeout. The proxy didn't send answer");
    }

    timeout_thread.join().map_err(|error| {
        anyhow::Error::msg(format!(
            "upgrade worker: thread timeout exceeded on join, {:?}",
            error
        ))
    })?
}

pub fn status(
    mut channel: Channel<CommandRequest, CommandResponse>,
    json: bool,
) -> Result<(), anyhow::Error> {
    let id = generate_id();
    channel.write_message(&CommandRequest::new(
        id.clone(),
        CommandRequestData::ListWorkers,
        None,
    ));

    match channel.read_message() {
        None => {
            bail!("the proxy didn't answer");
        }
        Some(message) => {
            if id != message.id {
                bail!("received message with invalid id: {:?}", message);
            }
            match message.status {
                CommandStatus::Processing => {
                    bail!("should have obtained an answer immediately");
                }
                CommandStatus::Error => {
                    if json {
                        print_json_response(&message.message)?;
                    }
                    bail!("could not get the worker list: {}", message.message);
                }
                CommandStatus::Ok => {
                    //println!("Worker list:\n{:?}", message.data);
                    if let Some(CommandResponseData::Workers(ref workers)) = message.data {
                        let mut expecting: HashSet<String> = HashSet::new();

                        let mut h = HashMap::new();
                        for ref worker in workers
                            .iter()
                            .filter(|worker| worker.run_state == RunState::Running)
                        {
                            let id = generate_id();
                            let msg = CommandRequest::new(
                                id.clone(),
                                CommandRequestData::Proxy(ProxyRequestData::Status),
                                Some(worker.id),
                            );
                            //println!("sending message: {:?}", msg);
                            channel.write_message(&msg);
                            expecting.insert(id.clone());
                            h.insert(id, (worker.id, CommandStatus::Processing));
                        }

                        let state = Arc::new(Mutex::new(h));
                        let st = state.clone();
                        let (send, recv) = mpsc::channel();

                        thread::spawn(move || {
                            loop {
                                //println!("expecting: {:?}", expecting);
                                if expecting.is_empty() {
                                    break;
                                }
                                match channel.read_message() {
                                    None => {
                                        eprintln!("the proxy didn't answer");
                                        exit(1);
                                    }
                                    Some(message) => {
                                        //println!("received message: {:?}", message);
                                        match message.status {
                                            CommandStatus::Processing => {}
                                            CommandStatus::Error => {
                                                eprintln!(
                                                    "error for message[{}]: {}",
                                                    message.id, message.message
                                                );
                                                if expecting.contains(&message.id) {
                                                    expecting.remove(&message.id);
                                                    //println!("status message with ID {} done", message.id);
                                                    if let Ok(mut h) = state.try_lock() {
                                                        if let Some(data) = h.get_mut(&message.id) {
                                                            *data =
                                                                ((*data).0, CommandStatus::Error);
                                                        }
                                                    }
                                                }
                                                exit(1);
                                            }
                                            CommandStatus::Ok => {
                                                if expecting.contains(&message.id) {
                                                    expecting.remove(&message.id);
                                                    //println!("status message with ID {} done", message.id);
                                                    if let Ok(mut h) = state.try_lock() {
                                                        if let Some(data) = h.get_mut(&message.id) {
                                                            *data = ((*data).0, CommandStatus::Ok);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            send.send(()).unwrap();
                        });

                        let finished = recv.recv_timeout(Duration::from_millis(1000)).is_ok();
                        let placeholder = if finished {
                            String::from("")
                        } else {
                            String::from("timeout")
                        };

                        let h2: HashMap<u32, String> = if let Ok(state) = st.try_lock() {
                            state
                                .values()
                                .map(|&(ref id, ref status)| {
                                    (
                                        *id,
                                        String::from(match *status {
                                            CommandStatus::Processing => {
                                                if finished {
                                                    "processing"
                                                } else {
                                                    "timeout"
                                                }
                                            }
                                            CommandStatus::Error => "error",
                                            CommandStatus::Ok => "ok",
                                        }),
                                    )
                                })
                                .collect()
                        } else {
                            HashMap::new()
                        };

                        if json {
                            let workers_status: Vec<WorkerStatus> = workers
                                .iter()
                                .map(|ref worker| WorkerStatus {
                                    worker: worker,
                                    status: h2.get(&worker.id).unwrap_or(&placeholder),
                                })
                                .collect();
                            print_json_response(&workers_status)?;
                        } else {
                            let mut table = Table::new();

                            table.add_row(row!["Worker", "pid", "run state", "answer"]);
                            for ref worker in workers.iter() {
                                let run_state = format!("{:?}", worker.run_state);
                                table.add_row(row![
                                    worker.id,
                                    worker.pid,
                                    run_state,
                                    h2.get(&worker.id).unwrap_or(&placeholder)
                                ]);
                            }

                            table.printstd();
                        }
                    }
                    Ok(())
                }
            }
        }
    }
}

pub fn metrics(
    mut channel: Channel<CommandRequest, CommandResponse>,
    cmd: MetricsCmd,
) -> Result<(), anyhow::Error> {
    let id = generate_id();
    //println!("will send message for metrics with id {}", id);

    let configuration = match cmd {
        MetricsCmd::Enable { time } => {
            if time {
                MetricsConfiguration::EnabledTimeMetrics(true)
            } else {
                MetricsConfiguration::Enabled(true)
            }
        }
        MetricsCmd::Disable { time } => {
            if time {
                MetricsConfiguration::EnabledTimeMetrics(false)
            } else {
                MetricsConfiguration::Enabled(false)
            }
        }
        MetricsCmd::Clear => MetricsConfiguration::Clear,
    };

    channel.write_message(&CommandRequest::new(
        id.clone(),
        CommandRequestData::Proxy(ProxyRequestData::Metrics(configuration)),
        None,
    ));

    // we should add a timeout somehow, otherwise it hangs
    loop {
        match channel.read_message() {
            None => {
                bail!("the proxy didn't answer");
            }
            Some(message) => match message.status {
                CommandStatus::Processing => {
                    println!("Proxy is processing: {}", message.message);
                }
                CommandStatus::Error => {
                    bail!("could not stop the proxy: {}", message.message);
                }
                CommandStatus::Ok => {
                    if &id == &message.id {
                        break Ok(());
                    }
                }
            },
        }
    }
}

// input: map worker_id -> (map key -> value)
fn print_metrics(table_name: &str, data: &BTreeMap<String, BTreeMap<String, FilteredData>>) {
    let mut metrics = data
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
        for key in data.keys() {
            row.push(cell!(key));
        }
        table.set_titles(Row::new(row));

        for metric in metrics {
            let mut row = vec![cell!(metric)];
            for worker_data in data.values() {
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

    let mut time_metrics = data
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
        for key in data.keys() {
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

            for worker_data in data.values() {
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

pub fn reload_configuration(
    mut channel: Channel<CommandRequest, CommandResponse>,
    path: Option<String>,
    json: bool,
) -> Result<(), anyhow::Error> {
    let id = generate_id();
    channel.write_message(&CommandRequest::new(
        id.clone(),
        CommandRequestData::ReloadConfiguration { path },
        None,
    ));

    match channel.read_message() {
        None => {
            bail!("the proxy didn't answer");
        }
        Some(message) => {
            if id != message.id {
                bail!("received message with invalid id: {:?}", message);
            }
            match message.status {
                CommandStatus::Processing => {
                    bail!("should have obtained an answer immediately");
                }
                CommandStatus::Error => {
                    if json {
                        print_json_response(&message.message)?;
                    }
                    bail!("could not get the worker list: {}", message.message);
                }
                CommandStatus::Ok => {
                    if json {
                        print_json_response(&message.message)?;
                    } else {
                        println!("Reloaded configuration: {}", message.message);
                    }
                    Ok(())
                }
            }
        }
    }
}

pub fn add_application(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    cluster_id: &str,
    sticky_session: bool,
    https_redirect: bool,
    send_proxy: bool,
    expect_proxy: bool,
    load_balancing: LoadBalancingAlgorithms,
) -> Result<(), anyhow::Error> {
    let proxy_protocol = match (send_proxy, expect_proxy) {
        (true, true) => Some(ProxyProtocolConfig::RelayHeader),
        (true, false) => Some(ProxyProtocolConfig::SendHeader),
        (false, true) => Some(ProxyProtocolConfig::ExpectHeader),
        _ => None,
    };

    order_command(
        channel,
        timeout,
        ProxyRequestData::AddCluster(Cluster {
            cluster_id: String::from(cluster_id),
            sticky_session,
            https_redirect,
            proxy_protocol,
            load_balancing,
            load_metric: None,
            answer_503: None,
        }),
    )
}

pub fn remove_application(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    cluster_id: &str,
) -> Result<(), anyhow::Error> {
    order_command(
        channel,
        timeout,
        ProxyRequestData::RemoveCluster {
            cluster_id: String::from(cluster_id),
        },
    )
}

pub fn add_http_frontend(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    route: Route,
    address: SocketAddr,
    hostname: &str,
    path: &str,
    method: Option<&str>,
    https: bool,
) -> Result<(), anyhow::Error> {
    if https {
        order_command(
            channel,
            timeout,
            ProxyRequestData::AddHttpsFrontend(HttpFrontend {
                route,
                address,
                hostname: String::from(hostname),
                path: PathRule::Prefix(String::from(path)),
                method: method.map(String::from),
                position: RulePosition::Tree,
            }),
        )
    } else {
        order_command(
            channel,
            timeout,
            ProxyRequestData::AddHttpFrontend(HttpFrontend {
                route,
                address,
                hostname: String::from(hostname),
                path: PathRule::Prefix(String::from(path)),
                method: method.map(String::from),
                position: RulePosition::Tree,
            }),
        )
    }
}

pub fn remove_http_frontend(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    route: Route,
    address: SocketAddr,
    hostname: &str,
    path: &str,
    method: Option<&str>,
    https: bool,
) -> Result<(), anyhow::Error> {
    if https {
        order_command(
            channel,
            timeout,
            ProxyRequestData::RemoveHttpsFrontend(HttpFrontend {
                route,
                address,
                hostname: String::from(hostname),
                path: PathRule::Prefix(String::from(path)),
                method: method.map(String::from),
                position: RulePosition::Tree,
            }),
        )
    } else {
        order_command(
            channel,
            timeout,
            ProxyRequestData::RemoveHttpFrontend(HttpFrontend {
                route,
                address,
                hostname: String::from(hostname),
                path: PathRule::Prefix(String::from(path)),
                method: method.map(String::from),
                position: RulePosition::Tree,
            }),
        )
    }
}

pub fn add_backend(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    cluster_id: &str,
    backend_id: &str,
    address: SocketAddr,
    sticky_id: Option<String>,
    backup: Option<bool>,
) -> Result<(), anyhow::Error> {
    order_command(
        channel,
        timeout,
        ProxyRequestData::AddBackend(Backend {
            cluster_id: String::from(cluster_id),
            address: address,
            backend_id: String::from(backend_id),
            load_balancing_parameters: Some(LoadBalancingParams::default()),
            sticky_id: sticky_id,
            backup: backup,
        }),
    )
}

pub fn remove_backend(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    cluster_id: &str,
    backend_id: &str,
    address: SocketAddr,
) -> Result<(), anyhow::Error> {
    order_command(
        channel,
        timeout,
        ProxyRequestData::RemoveBackend(RemoveBackend {
            cluster_id: String::from(cluster_id),
            address: address,
            backend_id: String::from(backend_id),
        }),
    )
}

pub fn add_certificate(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    address: SocketAddr,
    certificate_path: &str,
    certificate_chain_path: &str,
    key_path: &str,
    versions: Vec<TlsVersion>,
) -> Result<(), anyhow::Error> {
    if let Some(new_certificate) =
        load_full_certificate(certificate_path, certificate_chain_path, key_path, versions)?
    {
        order_command(
            channel,
            timeout,
            ProxyRequestData::AddCertificate(AddCertificate {
                address,
                certificate: new_certificate,
                names: vec![],
                expired_at: None,
            }),
        )?;
    }
    Ok(())
}

pub fn remove_certificate(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    address: SocketAddr,
    certificate_path: Option<&str>,
    fingerprint: Option<&str>,
) -> Result<(), anyhow::Error> {
    if certificate_path.is_some() && fingerprint.is_some() {
        bail!("Error: Either provide the certificate's path or its fingerprint");
    }

    if certificate_path.is_none() && fingerprint.is_none() {
        bail!("Error: Either provide the certificate's path or its fingerprint");
    }

    if let Some(fingerprint) = fingerprint
        .and_then(|s| match hex::decode(s) {
            Ok(v) => Some(CertificateFingerprint(v)),
            Err(e) => {
                eprintln!(
                    "Error decoding the certificate fingerprint (expected hexadecimal data): {:?}",
                    e
                );
                None
            }
        })
        .or(certificate_path.and_then(get_certificate_fingerprint))
    {
        order_command(
            channel,
            timeout,
            ProxyRequestData::RemoveCertificate(RemoveCertificate {
                address,
                fingerprint,
            }),
        )?
    }
    Ok(())
}

pub fn replace_certificate(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    address: SocketAddr,
    new_certificate_path: &str,
    new_certificate_chain_path: &str,
    new_key_path: &str,
    old_certificate_path: Option<&str>,
    old_fingerprint: Option<&str>,
    versions: Vec<TlsVersion>,
) -> Result<(), anyhow::Error> {
    if old_certificate_path.is_some() && old_fingerprint.is_some() {
        bail!("Error: Either provide the old certificate's path or its fingerprint");
    }

    if old_certificate_path.is_none() && old_fingerprint.is_none() {
        bail!("Error: Either provide the old certificate's path or its fingerprint");
    }

    if let Some(new_certificate) = load_full_certificate(
        new_certificate_path,
        new_certificate_chain_path,
        new_key_path,
        versions,
    )? {
        if let Some(old_fingerprint) = old_fingerprint.and_then(|s| {
        match hex::decode(s) {
            Ok(v) => Some(CertificateFingerprint(v)),
            Err(e) => {
                eprintln!("Error decoding the certificate fingerprint (expected hexadecimal data): {:?}", e);
                None
            }
        }
    }).or(old_certificate_path.and_then(get_certificate_fingerprint)) {
      order_command(channel, timeout, ProxyRequestData::ReplaceCertificate(ReplaceCertificate {
        address,
        new_certificate,
        old_fingerprint,
        new_names: vec![],
        new_expired_at: None,
      }))?;
    }
    }
    Ok(())
}

pub fn add_tcp_frontend(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    cluster_id: &str,
    address: SocketAddr,
) -> Result<(), anyhow::Error> {
    order_command(
        channel,
        timeout,
        ProxyRequestData::AddTcpFrontend(TcpFrontend {
            cluster_id: String::from(cluster_id),
            address,
        }),
    )
}

pub fn remove_tcp_frontend(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    cluster_id: &str,
    address: SocketAddr,
) -> Result<(), anyhow::Error> {
    order_command(
        channel,
        timeout,
        ProxyRequestData::RemoveTcpFrontend(TcpFrontend {
            cluster_id: String::from(cluster_id),
            address,
        }),
    )
}

pub fn add_http_listener(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    address: SocketAddr,
    public_address: Option<SocketAddr>,
    answer_404: Option<String>,
    answer_503: Option<String>,
    expect_proxy: bool,
    sticky_name: Option<String>,
) -> Result<(), anyhow::Error> {
    let mut listener = Listener::new(address, FileListenerProtocolConfig::Http);
    listener.public_address = public_address;
    listener.answer_404 = answer_404;
    listener.answer_503 = answer_503;
    listener.expect_proxy = Some(expect_proxy);
    if let Some(sticky_name) = sticky_name {
        listener.sticky_name = sticky_name;
    }

    match listener.to_http(None, None, None) {
        Some(conf) => order_command(channel, timeout, ProxyRequestData::AddHttpListener(conf)),
        None => bail!("Error creating HTTP listener"),
    }
}

pub fn add_https_listener(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    address: SocketAddr,
    public_address: Option<SocketAddr>,
    answer_404: Option<String>,
    answer_503: Option<String>,
    tls_versions: Vec<TlsVersion>,
    cipher_list: Option<String>,
    rustls_cipher_list: Vec<String>,
    expect_proxy: bool,
    sticky_name: Option<String>,
) -> Result<(), anyhow::Error> {
    let mut listener = Listener::new(address, FileListenerProtocolConfig::Https);
    listener.public_address = public_address;
    listener.answer_404 = answer_404;
    listener.answer_503 = answer_503;
    listener.expect_proxy = Some(expect_proxy);
    if let Some(sticky_name) = sticky_name {
        listener.sticky_name = sticky_name;
    }
    listener.cipher_list = cipher_list;
    listener.tls_versions = if tls_versions.len() == 0 {
        None
    } else {
        Some(tls_versions)
    };
    listener.rustls_cipher_list = if rustls_cipher_list.len() == 0 {
        None
    } else {
        Some(rustls_cipher_list)
    };

    match listener.to_tls(None, None, None) {
        Some(conf) => order_command(channel, timeout, ProxyRequestData::AddHttpsListener(conf)),
        None => bail!("Error creating HTTPS listener"),
    }
}

pub fn add_tcp_listener(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    address: SocketAddr,
    public_address: Option<SocketAddr>,
    expect_proxy: bool,
) -> Result<(), anyhow::Error> {
    order_command(
        channel,
        timeout,
        ProxyRequestData::AddTcpListener(TcpListener {
            address,
            public_address,
            expect_proxy,
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
        }),
    )
}

pub fn remove_listener(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    address: SocketAddr,
    proxy: ListenerType,
) -> Result<(), anyhow::Error> {
    order_command(
        channel,
        timeout,
        ProxyRequestData::RemoveListener(RemoveListener { address, proxy }),
    )
}

pub fn activate_listener(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    address: SocketAddr,
    proxy: ListenerType,
) -> Result<(), anyhow::Error> {
    order_command(
        channel,
        timeout,
        ProxyRequestData::ActivateListener(ActivateListener {
            address,
            proxy,
            from_scm: false,
        }),
    )
}

pub fn deactivate_listener(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    address: SocketAddr,
    proxy: ListenerType,
) -> Result<(), anyhow::Error> {
    order_command(
        channel,
        timeout,
        ProxyRequestData::DeactivateListener(DeactivateListener {
            address,
            proxy,
            to_scm: false,
        }),
    )
}

pub fn query_application(
    mut channel: Channel<CommandRequest, CommandResponse>,
    json: bool,
    application_id: Option<String>,
    domain: Option<String>,
) -> Result<(), anyhow::Error> {
    if application_id.is_some() && domain.is_some() {
        bail!("Error: Either request an application ID or a domain name");
    }

    let command = if let Some(ref cluster_id) = application_id {
        CommandRequestData::Proxy(ProxyRequestData::Query(Query::Applications(
            QueryApplicationType::ClusterId(cluster_id.to_string()),
        )))
    } else if let Some(ref domain) = domain {
        let splitted: Vec<String> = domain.splitn(2, "/").map(|elem| elem.to_string()).collect();

        if splitted.len() == 0 {
            bail!("Domain can't be empty");
        }

        let query_domain = QueryApplicationDomain {
            hostname: splitted
                .get(0)
                .with_context(|| "Domain can't be empty")?
                .clone(),
            path: splitted.get(1).cloned().map(|path| format!("/{}", path)), // We add the / again because of the splitn removing it
        };

        CommandRequestData::Proxy(ProxyRequestData::Query(Query::Applications(
            QueryApplicationType::Domain(query_domain),
        )))
    } else {
        CommandRequestData::Proxy(ProxyRequestData::Query(Query::ApplicationsHashes))
    };

    let id = generate_id();
    channel.write_message(&CommandRequest::new(id.clone(), command, None));

    match channel.read_message() {
        None => {
            bail!("the proxy didn't answer");
        }
        Some(message) => {
            if id != message.id {
                bail!("received message with invalid id: {:?}", message);
            }
            match message.status {
                CommandStatus::Processing => {
                    // do nothing here
                    // for other messages, we would loop over read_message
                    // until an error or ok message was sent
                    Ok(())
                }
                CommandStatus::Error => {
                    if json {
                        print_json_response(&message.message)?;
                    }
                    bail!("could not query proxy state: {}", message.message);
                }
                CommandStatus::Ok => {
                    if let Some(needle) = application_id.or(domain) {
                        if let Some(CommandResponseData::Query(data)) = message.data {
                            if json {
                                return Ok(print_json_response(&data)?);
                            }

                            let application_headers =
                                vec!["id", "sticky_session", "https_redirect"];
                            let mut application_table =
                                create_queried_application_table(application_headers, &data);

                            let http_headers = vec!["id", "hostname", "path"];
                            let mut frontend_table =
                                create_queried_application_table(http_headers, &data);

                            let https_headers = vec!["id", "hostname", "path"];
                            let mut https_frontend_table =
                                create_queried_application_table(https_headers, &data);

                            let tcp_headers = vec!["id", "address"];
                            let mut tcp_frontend_table =
                                create_queried_application_table(tcp_headers, &data);

                            let backend_headers = vec!["backend id", "IP address", "Backup"];
                            let mut backend_table =
                                create_queried_application_table(backend_headers, &data);

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
                                        let entry =
                                            application_data.entry(app).or_insert(Vec::new());
                                        entry.push((*key).clone());

                                        for frontend in app.http_frontends.iter() {
                                            let entry =
                                                frontend_data.entry(frontend).or_insert(Vec::new());
                                            entry.push((*key).clone());
                                        }

                                        for frontend in app.https_frontends.iter() {
                                            let entry = https_frontend_data
                                                .entry(frontend)
                                                .or_insert(Vec::new());
                                            entry.push((*key).clone());
                                        }

                                        for frontend in app.tcp_frontends.iter() {
                                            let entry = tcp_frontend_data
                                                .entry(frontend)
                                                .or_insert(Vec::new());
                                            entry.push((*key).clone());
                                        }

                                        for backend in app.backends.iter() {
                                            let entry =
                                                backend_data.entry(backend).or_insert(Vec::new());
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
                                let backend_backup =
                                    key.backup.map(|b| if b { "X" } else { "" }).unwrap_or("");
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
                        Ok(())
                    } else {
                        if let Some(CommandResponseData::Query(data)) = message.data {
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
                                        (*(query_data.entry((*key).clone()).or_insert(Vec::new())))
                                            .push(*value);
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
                        Ok(())
                    }
                }
            }
        }
    }
}

pub fn query_certificate(
    mut channel: Channel<CommandRequest, CommandResponse>,
    json: bool,
    fingerprint: Option<String>,
    domain: Option<String>,
) -> Result<(), anyhow::Error> {
    let query = match (fingerprint, domain) {
        (None, None) => QueryCertificateType::All,
        (Some(f), None) => match hex::decode(f) {
            Err(e) => {
                bail!("invalid fingerprint: {:?}", e);
            }
            Ok(f) => QueryCertificateType::Fingerprint(f),
        },
        (None, Some(d)) => QueryCertificateType::Domain(d),
        (Some(_), Some(_)) => {
            bail!("Error: Either request a fingerprint or a domain name");
        }
    };

    let command = CommandRequestData::Proxy(ProxyRequestData::Query(Query::Certificates(query)));

    let id = generate_id();
    channel.write_message(&CommandRequest::new(id.clone(), command, None));

    match channel.read_message() {
        None => {
            bail!("the proxy didn't answer");
        }
        Some(message) => {
            if id != message.id {
                bail!("received message with invalid id: {:?}", message);
            }
            match message.status {
                CommandStatus::Processing => {
                    // do nothing here
                    // for other messages, we would loop over read_message
                    // until an error or ok message was sent
                    Ok(())
                }
                CommandStatus::Error => {
                    if json {
                        print_json_response(&message.message)?;
                        bail!("We received an error message");
                    } else {
                        bail!("could not query proxy state: {}", message.message);
                    }
                }
                CommandStatus::Ok => {
                    if let Some(CommandResponseData::Query(data)) = message.data {
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
                                            println!(
                                                "\t\t{}:\t{}",
                                                domain,
                                                hex::encode(fingerprint)
                                            );
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
                    } else {
                        bail!("unexpected response: {:?}", message.data);
                    }
                }
            }
        }
    }
}

pub fn query_metrics(
    mut channel: Channel<CommandRequest, CommandResponse>,
    json: bool,
    list: bool,
    refresh: Option<u32>,
    names: Vec<String>,
    clusters: Vec<String>,
    backends: Vec<(String, String)>,
) -> Result<(), anyhow::Error> {
    let query = if list {
        QueryMetricsType::List
    } else if !clusters.is_empty() && !backends.is_empty() {
        bail!("Error: Either request a list of clusters or a list of backends");
    } else {
        if !clusters.is_empty() {
            QueryMetricsType::Cluster {
                metrics: names,
                clusters,
                date: None,
            }
        } else {
            QueryMetricsType::Backend {
                metrics: names,
                backends,
                date: None,
            }
        }
    };

    let command = CommandRequestData::Proxy(ProxyRequestData::Query(Query::Metrics(query)));

    loop {
        let id = generate_id();
        channel.write_message(&CommandRequest::new(id.clone(), command.clone(), None));
        print!("{}", termion::cursor::Save);

        match channel.read_message() {
            None => {
                bail!("the proxy didn't answer");
            }
            Some(message) => {
                //println!("received message: {:?}", message);
                if id != message.id {
                    bail!("received message with invalid id: {:?}", message);
                }
                match message.status {
                    CommandStatus::Processing => {
                        // do nothing here
                        // for other messages, we would loop over read_message
                        // until an error or ok message was sent
                    }
                    CommandStatus::Error => {
                        if json {
                            return print_json_response(&message.message);
                        } else {
                            bail!("could not query proxy state: {}", message.message);
                        }
                    }
                    CommandStatus::Ok => {
                        if let Some(CommandResponseData::Query(data)) = message.data {
                            if json {
                                return print_json_response(&data);
                            }

                            //println!("got data: {:#?}", data);
                            if list {
                                let metrics: HashSet<_> = data
                                    .values()
                                    .filter_map(|value| match value {
                                        QueryAnswer::Metrics(QueryAnswerMetrics::List(v)) => {
                                            Some(v.iter())
                                        }
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

                            let data = data
                                .iter()
                                .filter_map(|(key, value)| match value {
                                    QueryAnswer::Metrics(QueryAnswerMetrics::Cluster(d)) => {
                                        let mut metrics = BTreeMap::new();
                                        for (cluster_id, cluster_metrics) in d.iter() {
                                            for (metric_key, value) in cluster_metrics.iter() {
                                                metrics.insert(
                                                    format!(
                                                        "{} {}",
                                                        cluster_id,
                                                        metric_key.replace("\t", ".")
                                                    ),
                                                    value.clone(),
                                                );
                                            }
                                        }
                                        Some((key.clone(), metrics))
                                    }
                                    QueryAnswer::Metrics(QueryAnswerMetrics::Backend(d)) => {
                                        let mut metrics = BTreeMap::new();
                                        for (cluster_id, cluster_metrics) in d.iter() {
                                            for (backend_id, backend_metrics) in
                                                cluster_metrics.iter()
                                            {
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
                            print_metrics("Result", &data);
                        }
                    }
                }
            }
        }

        match refresh {
            None => break,
            Some(seconds) => std::thread::sleep(std::time::Duration::from_secs(seconds as u64)),
        }

        print!(
            "{}{}",
            termion::cursor::Restore,
            termion::clear::BeforeCursor
        );
    }

    Ok(())
}

pub fn logging_filter(
    channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    filter: &str,
) -> Result<(), anyhow::Error> {
    order_command(
        channel,
        timeout,
        ProxyRequestData::Logging(String::from(filter)),
    )
}

pub fn events(mut channel: Channel<CommandRequest, CommandResponse>) -> Result<(), anyhow::Error> {
    let id = generate_id();
    channel.write_message(&CommandRequest::new(
        id.clone(),
        CommandRequestData::SubscribeEvents,
        None,
    ));

    loop {
        match channel.read_message() {
            None => {
                bail!("the proxy didn't answer");
            }
            Some(message) => match message.status {
                CommandStatus::Processing => {
                    if let Some(CommandResponseData::Event(event)) = message.data {
                        println!("got event from worker({}): {:?}", message.message, event);
                    }
                }
                CommandStatus::Error => {
                    bail!("could not get proxy events: {}", message.message);
                }
                CommandStatus::Ok => {
                    println!("{}", message.message);
                    return Ok(());
                }
            },
        }
    }
}

fn order_command(
    mut channel: Channel<CommandRequest, CommandResponse>,
    timeout: u64,
    order: ProxyRequestData,
) -> Result<(), anyhow::Error> {
    let id = generate_id();
    channel.write_message(&CommandRequest::new(
        id.clone(),
        CommandRequestData::Proxy(order.clone()),
        None,
    ));

    command_timeout!(timeout, {
        match channel.read_message() {
            None => {
                bail!("the proxy didn't answer");
            }
            Some(message) => {
                if id != message.id {
                    bail!("received message with invalid id: {:?}", message);
                }
                match message.status {
                    CommandStatus::Processing => {
                        // do nothing here
                        // for other messages, we would loop over read_message
                        // until an error or ok message was sent
                    }
                    CommandStatus::Error => bail!("could not execute order: {}", message.message),
                    CommandStatus::Ok => {
                        //deactivate success messages for now
                        /*
                        match order {
                          ProxyRequestData::AddCluster(_) => println!("application added : {}", message.message),
                          ProxyRequestData::RemoveCluster(_) => println!("application removed : {} ", message.message),
                          ProxyRequestData::AddBackend(_) => println!("backend added : {}", message.message),
                          ProxyRequestData::RemoveBackend(_) => println!("backend removed : {} ", message.message),
                          ProxyRequestData::AddCertificate(_) => println!("certificate added: {}", message.message),
                          ProxyRequestData::RemoveCertificate(_) => println!("certificate removed: {}", message.message),
                          ProxyRequestData::AddHttpFrontend(_) => println!("front added: {}", message.message),
                          ProxyRequestData::RemoveHttpFrontend(_) => println!("front removed: {}", message.message),
                          _ => {
                            // do nothing for now
                          }
                        }
                        */
                    }
                };
            }
        }
    });
    Ok(())
}

fn print_json_response<T: ::serde::Serialize>(input: &T) -> Result<(), anyhow::Error> {
    println!(
        "{}",
        serde_json::to_string_pretty(&input).context("Error while parsing response to JSON")?
    );
    Ok(())
}

fn load_full_certificate(
    certificate_path: &str,
    certificate_chain_path: &str,
    key_path: &str,
    versions: Vec<TlsVersion>,
) -> Result<Option<CertificateAndKey>, anyhow::Error> {
    match Config::load_file(certificate_path) {
        Err(e) => {
            bail!("could not load certificate: {:?}", e);
        }
        Ok(certificate) => {
            match Config::load_file(certificate_chain_path).map(split_certificate_chain) {
                Err(e) => {
                    bail!("could not load certificate chain: {:?}", e);
                }
                Ok(certificate_chain) => match Config::load_file(key_path) {
                    Err(e) => {
                        bail!("could not load key: {:?}", e);
                    }
                    Ok(key) => Ok(Some(CertificateAndKey {
                        certificate,
                        certificate_chain,
                        key,
                        versions,
                    })),
                },
            }
        }
    }
}

fn get_certificate_fingerprint(certificate_path: &str) -> Option<CertificateFingerprint> {
    match Config::load_file_bytes(certificate_path) {
        Ok(data) => match calculate_fingerprint(&data) {
            Some(fingerprint) => Some(CertificateFingerprint(fingerprint)),
            None => {
                eprintln!("could not calculate finrprint for certificate");
                exit(1);
            }
        },
        Err(e) => {
            eprintln!("could not load file: {:?}", e);
            exit(1);
        }
    }
}

fn create_queried_application_table(
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
