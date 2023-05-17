use anyhow::{self, bail, Context};
use prettytable::Table;
use serde::Serialize;

use sozu_command_lib::proto::command::{
    request::RequestType, response_content::ContentType, ListWorkers, QueryCertificatesFilters,
    QueryClusterByDomain, QueryClustersHashes, QueryMetricsOptions, Request, Response,
    ResponseContent, ResponseStatus, RunState, UpgradeMain, WorkerInfo,
};

use crate::ctl::{
    create_channel,
    display::{
        print_available_metrics, print_certificates_by_worker, print_certificates_with_validity,
        print_frontend_list, print_json_response, print_listeners, print_metrics,
        print_query_response_data, print_status,
    },
    CommandManager,
};

// Used to display the JSON response of the status command
#[derive(Serialize, Debug)]
struct WorkerStatus<'a> {
    pub worker: &'a WorkerInfo,
    pub status: &'a String,
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
        println!("Sending request : {request:?}");

        self.channel
            .write_message(&request)
            .with_context(|| "Could not write the request")?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status() {
                ResponseStatus::Processing => println!("Proxy is processing: {}", response.message),
                ResponseStatus::Failure => bail!("Request failed: {}", response.message),
                ResponseStatus::Ok => {
                    if json {
                        // why do we need to print a success message in json?
                        print_json_response(&response.message)?;
                    } else {
                        println!("Success: {}", response.message);
                    }
                    if let Some(response_content) = response.content {
                        match response_content.content_type {
                            Some(ContentType::FrontendList(frontends)) => {
                                print_frontend_list(frontends)
                            }
                            Some(ContentType::Workers(worker_infos)) => {
                                if json {
                                    print_json_response(&worker_infos)?;
                                } else {
                                    print_status(worker_infos);
                                }
                            }
                            Some(ContentType::ListenersList(list)) => print_listeners(list),
                            _ => {}
                        }
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

        self.send_request(Request {
            request_type: Some(RequestType::ListWorkers(ListWorkers {})),
        })?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status() {
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
                    if let Some(ResponseContent {
                        content_type: Some(ContentType::Workers(ref worker_infos)),
                    }) = response.content
                    {
                        let mut table = Table::new();
                        table.set_format(*prettytable::format::consts::FORMAT_BOX_CHARS);
                        table.add_row(row!["Worker", "pid", "run state"]);
                        for worker in worker_infos.vec.iter() {
                            let run_state = format!("{:?}", worker.run_state);
                            table.add_row(row![worker.id, worker.pid, run_state]);
                        }
                        println!();
                        table.printstd();
                        println!();

                        self.send_request(Request {
                            request_type: Some(RequestType::UpgradeMain(UpgradeMain {})),
                        })?;

                        println!("Upgrading main process");

                        loop {
                            let response = self.read_channel_message_with_timeout()?;

                            match response.status() {
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
                        let running_workers = worker_infos
                            .vec
                            .iter()
                            .filter(|worker| worker.run_state == RunState::Running as i32)
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
        self.send_request(Request {
            request_type: Some(RequestType::UpgradeWorker(worker_id)),
        })?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status() {
                ResponseStatus::Processing => info!("Proxy is processing: {}", response.message),
                ResponseStatus::Failure => bail!(
                    "could not stop the worker {}: {}",
                    worker_id,
                    response.message
                ),
                ResponseStatus::Ok => {
                    info!("Success: {}", response.message);
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
        let request = Request {
            request_type: Some(RequestType::QueryMetrics(QueryMetricsOptions {
                list,
                cluster_ids,
                backend_ids,
                metric_names,
            })),
        };

        // a loop to reperform the query every refresh time
        loop {
            self.send_request(request.clone())?;

            print!("{}", termion::cursor::Save);

            // a loop to process responses
            loop {
                let response = self.read_channel_message_with_timeout()?;

                match response.status() {
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
                        if let Some(response_content) = response.content {
                            match response_content.content_type {
                                Some(ContentType::Metrics(aggregated_metrics_data)) => {
                                    print_metrics(aggregated_metrics_data, json)?
                                }
                                Some(ContentType::AvailableMetrics(available)) => {
                                    print_available_metrics(&available)?;
                                }
                                _ => println!("Wrong kind of response here"),
                            }
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

        let request = if let Some(ref cluster_id) = cluster_id {
            Request {
                request_type: Some(RequestType::QueryClusterById(cluster_id.to_string())),
            }
        } else if let Some(ref domain) = domain {
            let splitted: Vec<String> =
                domain.splitn(2, '/').map(|elem| elem.to_string()).collect();

            if splitted.is_empty() {
                bail!("Domain can't be empty");
            }

            let query_domain = QueryClusterByDomain {
                hostname: splitted
                    .get(0)
                    .with_context(|| "Domain can't be empty")?
                    .clone(),
                path: splitted.get(1).cloned().map(|path| format!("/{path}")), // We add the / again because of the splitn removing it
            };

            Request {
                request_type: Some(RequestType::QueryClustersByDomain(query_domain)),
            }
        } else {
            Request {
                request_type: Some(RequestType::QueryClustersHashes(QueryClustersHashes {})),
            }
        };

        self.send_request(request)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status() {
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
                    match response.content {
                        Some(content) => {
                            print_query_response_data(cluster_id, domain, content, json)?
                        }
                        None => println!("No content in the response"),
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn query_certificates(
        &mut self,
        json: bool,
        fingerprint: Option<String>,
        domain: Option<String>,
        query_workers: bool,
    ) -> Result<(), anyhow::Error> {
        let filters = QueryCertificatesFilters {
            domain,
            fingerprint,
        };

        if query_workers {
            self.query_certificates_from_workers(json, filters)
        } else {
            self.query_certificates_from_the_state(json, filters)
        }
    }

    fn query_certificates_from_workers(
        &mut self,
        json: bool,
        filters: QueryCertificatesFilters,
    ) -> Result<(), anyhow::Error> {
        self.send_request(Request {
            request_type: Some(RequestType::QueryCertificatesFromWorkers(filters)),
        })?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status() {
                ResponseStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                ResponseStatus::Failure => {
                    if json {
                        print_json_response(&response.message)?;
                        bail!("We received an error message");
                    } else {
                        bail!("could not get certificate: {}", response.message);
                    }
                }
                ResponseStatus::Ok => {
                    info!("We did get a response from the proxy");
                    match response.content {
                        Some(ResponseContent {
                            content_type: Some(ContentType::WorkerResponses(worker_responses)),
                        }) => print_certificates_by_worker(worker_responses.map, json)?,
                        _ => bail!("unexpected response: {:?}", response.content),
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    fn query_certificates_from_the_state(
        &mut self,
        json: bool,
        filters: QueryCertificatesFilters,
    ) -> anyhow::Result<()> {
        self.send_request(Request {
            request_type: Some(RequestType::QueryCertificatesFromTheState(filters)),
        })?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status() {
                ResponseStatus::Processing => {
                    println!("Proxy is processing: {}", response.message);
                }
                ResponseStatus::Failure => {
                    bail!("could not get certificate: {}", response.message);
                }
                ResponseStatus::Ok => {
                    info!("We did get a response from the proxy");
                    println!("response message: {:?}\n", response.message);

                    if let Some(response_content) = response.content {
                        let certs = match response_content.content_type {
                            Some(ContentType::CertificatesWithFingerprints(certs)) => certs.certs,
                            _ => bail!(format!("Wrong response content {:?}", response_content)),
                        };
                        if certs.is_empty() {
                            bail!("No certificates match your request.");
                        }

                        if json {
                            print_json_response(&certs)
                                .with_context(|| "Could not print certificates in JSON")?;
                        } else {
                            print_certificates_with_validity(certs)
                                .with_context(|| "Could not show certificate")?;
                        }
                    } else {
                        println!("No response content.");
                    }

                    break;
                }
            }
        }
        Ok(())
    }
}
