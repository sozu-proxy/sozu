use anyhow::{self, bail, Context};
use prettytable::Table;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use serde::Serialize;

use sozu_command_lib::{
    command::{Order, Request, Response, ResponseContent, ResponseStatus, RunState, WorkerInfo},
    worker::{QueryMetricsOptions, WorkerOrder},
};

use crate::ctl::{
    create_channel,
    display::{
        print_available_metrics, print_certificates, print_frontend_list, print_json_response,
        print_listeners, print_metrics, print_query_response_data, print_status,
    },
    CommandManager,
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
    format!("ID-{s}")
}

fn generate_tagged_id(tag: &str) -> String {
    let s: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(6)
        .map(|c| c as char)
        .collect();
    format!("{tag}-{s}")
}

impl CommandManager {
    fn send_request(&mut self, id: &str, command_request_order: Order) -> anyhow::Result<()> {
        let command_request = Request::new(id.to_string(), command_request_order, None);

        self.channel
            .write_message(&command_request)
            .with_context(|| "Could not write the request")
    }

    fn read_channel_message_with_timeout(&mut self) -> anyhow::Result<Response> {
        self.channel
            .read_message_blocking_timeout(Some(self.timeout))
            .with_context(|| "Command timeout. The proxy didn't send an answer")
    }

    pub fn order_command(&mut self, order: Order) -> Result<(), anyhow::Error> {
        self.order_command_with_worker_id(order, None, false)
    }

    pub fn order_command_with_json(
        &mut self,
        command_request_order: Order,
        json: bool,
    ) -> Result<(), anyhow::Error> {
        self.order_command_with_worker_id(command_request_order, None, json)
    }

    pub fn order_command_with_worker_id(
        &mut self,
        command_request_order: Order,
        worker_id: Option<u32>,
        json: bool,
    ) -> Result<(), anyhow::Error> {
        let id = generate_id();

        let command_request = Request::new(id, command_request_order, worker_id);

        println!("Sending command : {command_request:?}");

        self.channel
            .write_message(&command_request)
            .with_context(|| "Could not write the request")?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if command_request.id != response.id {
                bail!(
                    "received message with invalid id. Was expecting id {}, got id {}:\n\t{:?}",
                    command_request.id,
                    response.id,
                    response
                );
            }
            match response.status {
                ResponseStatus::Processing => println!("Proxy is processing: {response}"),
                ResponseStatus::Error => bail!("Order failed: {}", response),
                ResponseStatus::Ok => {
                    if json {
                        // why do we need to print a success message in json?
                        print_json_response(&response)?;
                    } else {
                        println!("Success: {response}");
                    }
                    match response.content {
                        Some(response_content) => match response_content {
                            ResponseContent::AllWorkerCertificates(_)
                            | ResponseContent::AvailableMetrics(_)
                            | ResponseContent::AvailableWorkerMetrics(_)
                            | ResponseContent::CertificatesByDomain(_)
                            | ResponseContent::Event(_)
                            | ResponseContent::Metrics(_)
                            | ResponseContent::Query(_)
                            | ResponseContent::WorkerCertificates(_)
                            | ResponseContent::WorkerClusters(_)
                            | ResponseContent::WorkerClustersHashes(_)
                            | ResponseContent::WorkerMetrics(_)
                            | ResponseContent::Workers(_) => {}
                            ResponseContent::ListenersList(list) => print_listeners(list),
                            ResponseContent::State(state) => match json {
                                true => print_json_response(&state)?,
                                false => println!("{state:#?}"),
                            },
                            ResponseContent::FrontendList(frontends) => {
                                print_frontend_list(frontends)
                            }
                            ResponseContent::Status(worker_info_vec) => {
                                if json {
                                    print_json_response(&worker_info_vec)?;
                                } else {
                                    print_status(worker_info_vec);
                                }
                            }
                        },
                        None => {}
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

        self.send_request(&id, Order::ListWorkers)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("Error: received unexpected message: {:?}", response);
            }
            match response.status {
                ResponseStatus::Processing => {
                    println!("Processing: {response}");
                }
                ResponseStatus::Error => {
                    bail!("Error: failed to get the list of worker: {}", response);
                }
                ResponseStatus::Ok => {
                    if let Some(ResponseContent::Workers(ref workers)) = response.content {
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
                        self.send_request(&id, Order::UpgradeMain)?;

                        println!("Upgrading main process");

                        loop {
                            let response = self.read_channel_message_with_timeout()?;

                            if id != response.id {
                                bail!("Error: received unexpected message: {:?}", response);
                            }

                            match response.status {
                                ResponseStatus::Processing => {
                                    println!("Main process is upgrading");
                                }
                                ResponseStatus::Error => {
                                    bail!("Error: failed to upgrade the main: {}", response);
                                }
                                ResponseStatus::Ok => {
                                    println!("Main process upgrade succeeded: {response}");
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
                        println!("Received a response of the wrong kind: {response:?}");
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn upgrade_worker(&mut self, worker_id: u32) -> Result<(), anyhow::Error> {
        println!("upgrading worker {worker_id}");
        let id = generate_id();

        //FIXME: we should be able to soft stop one specific worker
        self.send_request(&id, Order::UpgradeWorker(worker_id))?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status {
                ResponseStatus::Processing => info!("Proxy is processing: {}", response),
                ResponseStatus::Error => {
                    bail!("could not stop the worker {}: {}", worker_id, response)
                }
                ResponseStatus::Ok => {
                    // this is necessary because we may receive responses about other workers
                    if id == response.id {
                        info!("Worker {} shut down: {}", worker_id, response);
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
        let command = Order::Worker(Box::new(WorkerOrder::QueryMetrics(QueryMetricsOptions {
            list,
            cluster_ids,
            backend_ids,
            metric_names,
        })));

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
                    ResponseStatus::Processing => {
                        println!("Proxy is processing: {response}");
                    }
                    ResponseStatus::Error => {
                        if json {
                            return print_json_response(&response.message);
                        } else {
                            bail!("could not query proxy state: {}", response);
                        }
                    }
                    ResponseStatus::Ok => {
                        match response.content {
                            Some(ResponseContent::Metrics(aggregated_metrics)) => {
                                print_metrics(aggregated_metrics, json)?
                            }
                            Some(ResponseContent::AvailableMetrics(available_metrics)) => {
                                print_available_metrics(&available_metrics)?;
                            }
                            _ => println!("Wrong kind of response here {response:?}"),
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

    pub fn query_cluster(
        &mut self,
        json: bool,
        cluster_id: Option<String>,
        domain: Option<String>,
    ) -> Result<(), anyhow::Error> {
        if cluster_id.is_some() && domain.is_some() {
            bail!("Error: Either request an cluster ID or a domain name");
        }

        let command = match (cluster_id.clone(), domain.clone()) {
            (Some(cluster_id), _) => {
                Order::Worker(Box::new(WorkerOrder::QueryClusterById { cluster_id }))
            }
            (None, Some(domain)) => {
                let splitted: Vec<String> =
                    domain.splitn(2, '/').map(|elem| elem.to_string()).collect();

                if splitted.is_empty() {
                    bail!("Domain can't be empty");
                }

                let hostname = splitted
                    .get(0)
                    .with_context(|| "Domain can't be empty")?
                    .clone();

                // We add the / again because of the splitn removing it
                let path = splitted.get(1).cloned().map(|path| format!("/{path}"));

                Order::Worker(Box::new(WorkerOrder::QueryClusterByDomain {
                    hostname,
                    path,
                }))
            }
            (None, None) => Order::Worker(Box::new(WorkerOrder::QueryClustersHashes)),
        };

        let id = generate_id();
        self.send_request(&id, command)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("received message with invalid id: {:?}", response);
            }
            match response.status {
                ResponseStatus::Processing => {
                    println!("Proxy is processing: {response}");
                }
                ResponseStatus::Error => {
                    if json {
                        print_json_response(&response.message)?;
                    }
                    bail!("could not query proxy state: {}", response);
                }
                ResponseStatus::Ok => {
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
        let order = match (fingerprint, domain) {
            (None, None) => WorkerOrder::QueryAllCertificates,
            (Some(f), None) => match hex::decode(f) {
                Err(e) => {
                    bail!("invalid fingerprint: {:?}", e);
                }
                Ok(f) => WorkerOrder::QueryCertificateByFingerprint(f),
            },
            (None, Some(d)) => WorkerOrder::QueryCertificateByDomain(d),
            (Some(_), Some(_)) => {
                bail!("Error: Either request a fingerprint or a domain name");
            }
        };

        let command = Order::Worker(Box::new(order));

        let id = generate_id();

        self.send_request(&id, command)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            if id != response.id {
                bail!("received message with invalid id: {:?}", response);
            }
            match response.status {
                ResponseStatus::Processing => {
                    println!("Proxy is processing: {response}");
                }
                ResponseStatus::Error => {
                    if json {
                        print_json_response(&response.message)?;
                        bail!("We received an error message");
                    } else {
                        bail!("could not query proxy state: {}", response);
                    }
                }
                ResponseStatus::Ok => {
                    match response.content {
                        Some(ResponseContent::Query(data)) => print_certificates(data, json)?,
                        _ => bail!("unexpected response: {:?}", response.content),
                    }
                    break;
                }
            }
        }
        Ok(())
    }
}
