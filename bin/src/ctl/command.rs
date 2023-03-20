use anyhow::{self, bail, Context};
use prettytable::Table;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use serde::Serialize;

use sozu_command_lib::{
    request::{
        QueryCertificateType, QueryClusterDomain, QueryClusterType, QueryMetricsOptions, Request,
    },
    response::{Response, ResponseContent, ResponseStatus, RunState, WorkerInfo},
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

/*
fn generate_id() -> String {
    let s: String = thread_rng()
    .sample_iter(&Alphanumeric)
    .take(6)
    .map(|c| c as char)
    .collect();
format!("ID-{s}")
}
*/

fn generate_tagged_id(tag: &str) -> String {
    let s: String = thread_rng()
        .sample_iter(&Alphanumeric)
        .take(6)
        .map(|c| c as char)
        .collect();
    format!("{tag}-{s}")
}

impl CommandManager {
    fn send_request(&mut self, request: Request) -> anyhow::Result<()> {
        self.channel
            .write_message(&request)
            .with_context(|| "Could not write the request")
    }

    fn read_channel_message_with_timeout(&mut self) -> anyhow::Result<Response> {
        self.channel
            .read_message_blocking_timeout(Some(self.timeout))
            .with_context(|| "Command timeout. The proxy didn't send an answer")
    }

    pub fn order_request(&mut self, request: Request) -> Result<(), anyhow::Error> {
        self.order_request_to_all_workers(request, false)
    }

    pub fn order_request_to_all_workers(
        &mut self,
        request: Request,
        json: bool,
    ) -> Result<(), anyhow::Error> {
        // let id = generate_id();

        println!("Sending request : {request:?}");

        self.channel
            .write_message(&request)
            .with_context(|| "Could not write the request")?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status {
                ResponseStatus::Processing => println!("Proxy is processing: {}", response.message),
                ResponseStatus::Failure => bail!("Request failed: {}", response.message),
                ResponseStatus::Ok => {
                    if json {
                        // why do we need to print a success message in json?
                        print_json_response(&response.message)?;
                    } else {
                        println!("Success: {}", response.message);
                    }
                    match response.content {
                        Some(response_content) => match response_content {
                            ResponseContent::Workers(_)
                            | ResponseContent::Metrics(_)
                            | ResponseContent::WorkerResponses(_)
                            | ResponseContent::WorkerMetrics(_)
                            | ResponseContent::ClustersHashes(_)
                            | ResponseContent::Clusters(_)
                            | ResponseContent::Certificates(_)
                            | ResponseContent::AvailableMetrics(_)
                            | ResponseContent::Event(_) => {}
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
                            ResponseContent::ListenersList(list) => print_listeners(list),
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

        self.send_request(Request::ListWorkers)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status {
                ResponseStatus::Processing => {
                    println!("Processing: {}", response.message);
                }
                ResponseStatus::Failure => {
                    bail!(
                        "Error: failed to get the list of worker: {}",
                        response.message
                    );
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
                        self.send_request(Request::UpgradeMain)?;

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
                                ResponseStatus::Failure => {
                                    bail!(
                                        "Error: failed to upgrade the main: {}",
                                        response.message
                                    );
                                }
                                ResponseStatus::Ok => {
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

        //FIXME: we should be able to soft stop one specific worker
        self.send_request(Request::UpgradeWorker(worker_id))?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status {
                ResponseStatus::Processing => info!("Proxy is processing: {}", response.message),
                ResponseStatus::Failure => bail!(
                    "could not stop the worker {}: {}",
                    worker_id,
                    response.message
                ),
                ResponseStatus::Ok => {
                    // this is necessary because we may receive responses about other workers
                    // TODO: is it though?
                    // if id == response.id {
                    info!("Success: {}", response.message);
                    // }
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
        let request = Request::QueryMetrics(QueryMetricsOptions {
            list,
            cluster_ids,
            backend_ids,
            metric_names,
        });

        // a loop to reperform the query every refresh time
        loop {
            self.send_request(request.clone())?;

            print!("{}", termion::cursor::Save);

            // a loop to process responses
            loop {
                let response = self.read_channel_message_with_timeout()?;

                match response.status {
                    ResponseStatus::Processing => {
                        println!("Proxy is processing: {}", response.message);
                    }
                    ResponseStatus::Failure => {
                        if json {
                            return print_json_response(&response.message);
                        } else {
                            bail!("could not query proxy state: {}", response.message);
                        }
                    }
                    ResponseStatus::Ok => {
                        match response.content {
                            Some(ResponseContent::Metrics(aggregated_metrics_data)) => {
                                print_metrics(aggregated_metrics_data, json)?
                            }
                            Some(ResponseContent::AvailableMetrics(available)) => {
                                print_available_metrics(&available)?;
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
            Request::QueryClusters(QueryClusterType::ClusterId(cluster_id.to_string()))
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
                path: splitted.get(1).cloned().map(|path| format!("/{path}")), // We add the / again because of the splitn removing it
            };

            Request::QueryClusters(QueryClusterType::Domain(query_domain))
        } else {
            Request::QueryClustersHashes
        };

        self.send_request(command)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status {
                ResponseStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                ResponseStatus::Failure => {
                    if json {
                        print_json_response(&response.message)?;
                    }
                    bail!("could not query proxy state: {}", response.message);
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

        let request = Request::QueryCertificates(query);

        self.send_request(request)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status {
                ResponseStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                ResponseStatus::Failure => {
                    if json {
                        print_json_response(&response.message)?;
                        bail!("We received an error message");
                    } else {
                        bail!("could not query proxy state: {}", response.message);
                    }
                }
                ResponseStatus::Ok => {
                    match response.content {
                        Some(ResponseContent::WorkerResponses(data)) => {
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
}
