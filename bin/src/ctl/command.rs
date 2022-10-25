use anyhow::{self, bail, Context};
use prettytable::Table;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use serde::Serialize;

use sozu_command_lib::{
    command::{
        CommandRequest, CommandRequestOrder, CommandResponse, CommandResponseContent,
        CommandStatus, FrontendFilters, RunState, WorkerInfo,
    },
    proxy::{
        MetricsConfiguration, ProxyRequestOrder, Query, QueryCertificateType, QueryClusterDomain,
        QueryClusterType, QueryMetricsOptions,
    },
};

use crate::{
    cli::MetricsCmd,
    ctl::{
        create_channel,
        display::{
            print_available_metrics, print_certificates, print_frontend_list, print_json_response,
            print_metrics, print_query_response_data, print_status,
        },
        CommandManager,
    },
};

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

impl CommandManager {
    fn send_request(
        &mut self,
        id: &str,
        command_request_order: CommandRequestOrder,
    ) -> anyhow::Result<()> {
        let command_request = CommandRequest::new(id.to_string(), command_request_order, None);

        if !self.channel.write_message(&command_request) {
            bail!("Could not write the request");
        }
        Ok(())
    }

    fn read_channel_message_with_timeout(&mut self) -> anyhow::Result<CommandResponse> {
        self.channel
            .read_message_blocking_timeout(Some(self.timeout))
            .with_context(|| "Command timeout. The proxy didn't send an answer")
    }

    pub fn save_state(&mut self, path: String) -> Result<(), anyhow::Error> {
        let id = generate_id();

        self.send_request(&id, CommandRequestOrder::SaveState { path })?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("received message with invalid id: {:?}", response);
            }
            match response.status {
                CommandStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                CommandStatus::Error => {
                    bail!("could not save proxy state: {}", response.message)
                }
                CommandStatus::Ok => {
                    println!("{}", response.message);
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn load_state(&mut self, path: String) -> Result<(), anyhow::Error> {
        let id = generate_id();

        self.send_request(&id, CommandRequestOrder::LoadState { path: path.clone() })?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("received message with invalid id: {:?}", response);
            }
            match response.status {
                CommandStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                CommandStatus::Error => {
                    bail!("could not load proxy state: {}", response.message)
                }
                CommandStatus::Ok => {
                    println!("Proxy state loaded successfully from {}", path);
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn dump_state(&mut self, json: bool) -> Result<(), anyhow::Error> {
        let id = generate_id();

        self.send_request(&id.clone(), CommandRequestOrder::DumpState)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("received message with invalid id: {:?}", response);
            }
            match response.status {
                CommandStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                CommandStatus::Error => {
                    if json {
                        print_json_response(&response.message)?;
                    }
                    bail!("could not dump proxy state: {}", response.message);
                }
                CommandStatus::Ok => match response.content {
                    Some(CommandResponseContent::State(state)) => {
                        match json {
                            true => print_json_response(&state)?,
                            false => println!("{:#?}", state),
                        }
                        break;
                    }
                    _ => bail!("state dump was empty"),
                },
            }
        }
        Ok(())
    }

    pub fn soft_stop(&mut self, proxy_id: Option<u32>) -> Result<(), anyhow::Error> {
        println!("shutting down proxy");
        let id = generate_id();

        self.channel.write_message(&CommandRequest::new(
            id.clone(),
            CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::SoftStop)),
            proxy_id,
        ));

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("received message with invalid id: {:?}", response);
            }

            match response.status {
                CommandStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                CommandStatus::Error => {
                    bail!("could not stop the proxy: {}", response.message);
                }
                CommandStatus::Ok => {
                    println!("Proxy shut down with message: \"{}\"", response.message);
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn hard_stop(&mut self, proxy_id: Option<u32>) -> Result<(), anyhow::Error> {
        println!("shutting down proxy");
        let id = generate_id();
        self.channel.write_message(&CommandRequest::new(
            id.clone(),
            CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::HardStop)),
            proxy_id,
        ));
        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status {
                CommandStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                CommandStatus::Error => {
                    bail!("could not stop the proxy: {}", response.message);
                }
                CommandStatus::Ok => {
                    if id == response.id {
                        println!("Proxy shut down: {}", response.message);
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    // 1. Request a list of workers
    // 2. Send an UpgradeMain
    // 3. Send an UpgradeWorker to each worker
    pub fn upgrade_main(&mut self) -> Result<(), anyhow::Error> {
        println!("Preparing to upgrade proxy...");

        let id = generate_tagged_id("LIST-WORKERS");

        self.send_request(&id, CommandRequestOrder::ListWorkers)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("Error: received unexpected message: {:?}", response);
            }
            match response.status {
                CommandStatus::Processing => {
                    println!("Processing: {}", response.message);
                }
                CommandStatus::Error => {
                    bail!(
                        "Error: failed to get the list of worker: {}",
                        response.message
                    );
                }
                CommandStatus::Ok => {
                    if let Some(CommandResponseContent::Workers(ref workers)) = response.content {
                        let mut table = Table::new();
                        table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
                        table.add_row(row!["Worker", "pid", "run state"]);
                        for worker in workers.iter() {
                            let run_state = format!("{:?}", worker.run_state);
                            table.add_row(row![worker.id, worker.pid, run_state]);
                        }
                        println!();
                        table.printstd();
                        println!();

                        let id = generate_tagged_id("UPGRADE-MAIN");
                        self.send_request(&id, CommandRequestOrder::UpgradeMain)?;

                        println!("Upgrading main process");

                        loop {
                            let response = self.read_channel_message_with_timeout()?;

                            if id != response.id {
                                bail!("Error: received unexpected message: {:?}", response);
                            }

                            match response.status {
                                CommandStatus::Processing => {
                                    println!("Main process is upgrading");
                                }
                                CommandStatus::Error => {
                                    bail!(
                                        "Error: failed to upgrade the main: {}",
                                        response.message
                                    );
                                }
                                CommandStatus::Ok => {
                                    println!(
                                        "Main process upgrade succeeded: {}",
                                        response.message
                                    );
                                    break;
                                }
                            }
                        }

                        // Reconnect to the new main
                        println!("Reconnecting to new main process...");
                        self.channel = create_channel(&self.config)
                            .with_context(|| "could not reconnect to the command unix socket")?;

                        // Do a rolling restart of the workers
                        let running_workers = workers
                            .iter()
                            .filter(|worker| worker.run_state == RunState::Running)
                            .collect::<Vec<_>>();
                        let running_count = running_workers.len();
                        for (i, worker) in running_workers.iter().enumerate() {
                            println!(
                                "Upgrading worker {} (#{} out of {})",
                                worker.id,
                                i + 1,
                                running_count
                            );

                            self.upgrade_worker(worker.id)
                                .with_context(|| "Upgrading the worker failed")?;
                            //thread::sleep(Duration::from_millis(1000));
                        }

                        println!("Proxy successfully upgraded!");
                    } else {
                        println!("Received a response of the wrong kind: {:?}", response);
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn upgrade_worker(&mut self, worker_id: u32) -> Result<(), anyhow::Error> {
        println!("upgrading worker {}", worker_id);
        let id = generate_id();

        //FIXME: we should be able to soft stop one specific worker
        self.send_request(&id, CommandRequestOrder::UpgradeWorker(worker_id))?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status {
                CommandStatus::Processing => info!("Proxy is processing: {}", response.message),
                CommandStatus::Error => bail!(
                    "could not stop the worker {}: {}",
                    worker_id,
                    response.message
                ),
                CommandStatus::Ok => {
                    if id == response.id {
                        info!("Worker {} shut down: {}", worker_id, response.message);
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn status(&mut self, json: bool) -> anyhow::Result<()> {
        let request_id = generate_id();

        self.send_request(&request_id, CommandRequestOrder::Status)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if request_id != response.id {
                bail!("received message with invalid id: {:?}", response);
            }

            match response.status {
                CommandStatus::Processing => {
                    println!("server is processing: {}", response.message);
                }
                CommandStatus::Error => {
                    if json {
                        print_json_response(&response.message)?;
                    }
                    bail!("could not get the worker list: {}", response.message);
                }
                CommandStatus::Ok => match response.content {
                    Some(CommandResponseContent::Status(worker_info_vec)) => {
                        print_status(worker_info_vec);
                        break;
                    }
                    Some(_) => {
                        bail!("Received the wrong kind of response data from the command server")
                    }
                    None => bail!("No data in the response"),
                },
            }
        }
        Ok(())
    }

    pub fn configure_metrics(&mut self, cmd: MetricsCmd) -> Result<(), anyhow::Error> {
        let id = generate_id();
        //println!("will send message for metrics with id {}", id);

        let configuration = match cmd {
            MetricsCmd::Enable => MetricsConfiguration::Enabled(true),
            MetricsCmd::Disable => MetricsConfiguration::Enabled(false),
            MetricsCmd::Clear => MetricsConfiguration::Clear,
            _ => bail!("The command passed to the configure_metrics function is wrong."),
        };

        self.send_request(
            &id,
            CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::ConfigureMetrics(
                configuration,
            ))),
        )?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status {
                CommandStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                CommandStatus::Error => {
                    bail!("Error with metrics command: {}", response.message);
                }
                CommandStatus::Ok => {
                    if id == response.id {
                        println!("Successfull metrics command: {}", response.message);
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn get_metrics(
        &mut self,
        json: bool,
        list: bool,
        refresh: Option<u32>,
        metric_names: Vec<String>,
        cluster_ids: Vec<String>,
        backend_ids: Vec<String>,
    ) -> Result<(), anyhow::Error> {
        let command = CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::Query(
            Query::Metrics(QueryMetricsOptions {
                list,
                cluster_ids,
                backend_ids,
                metric_names,
            }),
        )));

        // a loop to reperform the query every refresh time
        loop {
            let id = generate_id();
            self.send_request(&id, command.clone())?;

            print!("{}", termion::cursor::Save);

            // a loop to process responses
            loop {
                let response = self.read_channel_message_with_timeout()?;

                if id != response.id {
                    bail!("received message with invalid id: {:?}", response);
                }
                match response.status {
                    CommandStatus::Processing => {
                        println!("Proxy is processing: {}", response.message);
                    }
                    CommandStatus::Error => {
                        if json {
                            return print_json_response(&response.message);
                        } else {
                            bail!("could not query proxy state: {}", response.message);
                        }
                    }
                    CommandStatus::Ok => {
                        match response.content {
                            Some(CommandResponseContent::Metrics(aggregated_metrics_data)) => {
                                print_metrics(aggregated_metrics_data, json)?
                            }
                            Some(CommandResponseContent::Query(lists_of_metrics)) => {
                                print_available_metrics(&lists_of_metrics)?;
                            }
                            _ => println!("Wrong kind of response here"),
                        }
                        break;
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

    pub fn reload_configuration(
        &mut self,
        path: Option<String>,
        json: bool,
    ) -> Result<(), anyhow::Error> {
        let id = generate_id();

        self.send_request(&id, CommandRequestOrder::ReloadConfiguration { path })?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("received message with invalid id: {:?}", response);
            }
            match response.status {
                CommandStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                CommandStatus::Error => {
                    if json {
                        return print_json_response(&response.message);
                    }
                    bail!("could not get the worker list: {}", response.message);
                }
                CommandStatus::Ok => {
                    match json {
                        true => print_json_response(&response.message)?,
                        false => println!("Reloaded configuration: {}", response.message),
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn list_frontends(
        &mut self,
        http: bool,
        https: bool,
        tcp: bool,
        domain: Option<String>,
    ) -> Result<(), anyhow::Error> {
        let command = CommandRequestOrder::ListFrontends(FrontendFilters {
            http,
            https,
            tcp,
            domain,
        });

        let id = generate_id();
        self.send_request(&id, command)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("received message with invalid id: {:?}", response);
            }
            match response.status {
                CommandStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                CommandStatus::Error => {
                    println!("could not query proxy state: {}", response.message)
                }
                CommandStatus::Ok => {
                    match response.content {
                        Some(CommandResponseContent::FrontendList(frontends)) => {
                            print_frontend_list(frontends)
                        }
                        _ => println!("Received a response of the wrong kind: {:?}", response),
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn query_cluster(
        &mut self,
        json: bool,
        cluster_id: Option<String>,
        domain: Option<String>,
    ) -> Result<(), anyhow::Error> {
        if cluster_id.is_some() && domain.is_some() {
            bail!("Error: Either request an cluster ID or a domain name");
        }

        let command = if let Some(ref cluster_id) = cluster_id {
            CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::Query(Query::Clusters(
                QueryClusterType::ClusterId(cluster_id.to_string()),
            ))))
        } else if let Some(ref domain) = domain {
            let splitted: Vec<String> =
                domain.splitn(2, '/').map(|elem| elem.to_string()).collect();

            if splitted.is_empty() {
                bail!("Domain can't be empty");
            }

            let query_domain = QueryClusterDomain {
                hostname: splitted
                    .get(0)
                    .with_context(|| "Domain can't be empty")?
                    .clone(),
                path: splitted.get(1).cloned().map(|path| format!("/{}", path)), // We add the / again because of the splitn removing it
            };

            CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::Query(Query::Clusters(
                QueryClusterType::Domain(query_domain),
            ))))
        } else {
            CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::Query(Query::ClustersHashes)))
        };

        let id = generate_id();
        self.send_request(&id, command)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("received message with invalid id: {:?}", response);
            }
            match response.status {
                CommandStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                CommandStatus::Error => {
                    if json {
                        print_json_response(&response.message)?;
                    }
                    bail!("could not query proxy state: {}", response.message);
                }
                CommandStatus::Ok => {
                    print_query_response_data(cluster_id, domain, response.content, json)?;
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn query_certificate(
        &mut self,
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

        let command = CommandRequestOrder::Proxy(Box::new(ProxyRequestOrder::Query(
            Query::Certificates(query),
        )));

        let id = generate_id();

        self.send_request(&id, command)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("received message with invalid id: {:?}", response);
            }
            match response.status {
                CommandStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                CommandStatus::Error => {
                    if json {
                        print_json_response(&response.message)?;
                        bail!("We received an error message");
                    } else {
                        bail!("could not query proxy state: {}", response.message);
                    }
                }
                CommandStatus::Ok => {
                    match response.content {
                        Some(CommandResponseContent::Query(data)) => {
                            print_certificates(data, json)?
                        }
                        _ => bail!("unexpected response: {:?}", response.content),
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn events(&mut self) -> Result<(), anyhow::Error> {
        let id = generate_id();

        self.send_request(&id, CommandRequestOrder::SubscribeEvents)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;
            match response.status {
                CommandStatus::Processing => match response.content {
                    Some(CommandResponseContent::Event(event)) => {
                        println!("got event from worker({}): {:?}", response.message, event)
                    }
                    _ => {
                        println!("Received an unexpected response: {:?}", response)
                    }
                },
                CommandStatus::Error => {
                    bail!("could not get proxy events: {}", response.message);
                }
                CommandStatus::Ok => {
                    println!("{}", response.message);
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn order_command(&mut self, order: ProxyRequestOrder) -> Result<(), anyhow::Error> {
        let id = generate_id();

        let request_order = CommandRequestOrder::Proxy(Box::new(order));
        println!("Sending request order: {:?}", request_order);
        self.send_request(&id, request_order)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("received message with invalid id: {:?}", response);
            }
            match response.status {
                CommandStatus::Processing => println!("Proxy is processing: {}", response.message),
                CommandStatus::Error => bail!("could not execute order: {}", response.message),
                CommandStatus::Ok => {
                    println!("Success: {}", response.message);
                    break;
                }
            }
        }
        Ok(())
    }
}
