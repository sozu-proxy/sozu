use anyhow::{self, bail, Context};
use prettytable::Table;

use sozu_command_lib::proto::command::{
    request::RequestType, response_content::ContentType, ListWorkers, QueryCertificatesFilters,
    QueryClusterByDomain, QueryClustersHashes, QueryMetricsOptions, Request, Response,
    ResponseContent, ResponseStatus, RunState, UpgradeMain,
};

use crate::ctl::{
    create_channel,
    display::{
        print_available_metrics, print_certificates_by_worker, print_certificates_with_validity,
        print_cluster_responses, print_frontend_list, print_json_response, print_listeners,
        print_metrics, print_request_counts, print_status,
    },
    CommandManager,
};

impl CommandManager {
    fn write_request_on_channel(&mut self, request: Request) -> anyhow::Result<()> {
        self.channel
            .write_message(&request)
            .with_context(|| "Could not write the request")
    }

    fn read_channel_message_with_timeout(&mut self) -> anyhow::Result<Response> {
        self.channel
            .read_message_blocking_timeout(Some(self.timeout))
            .with_context(|| "Command timeout. The proxy didn't send an answer")
    }

    pub fn send_request(&mut self, request: Request) -> Result<(), anyhow::Error> {
        self.channel
            .write_message(&request)
            .with_context(|| "Could not write the request")?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status() {
                ResponseStatus::Processing => {
                    debug!("Proxy is processing: {}", response.message);
                }
                ResponseStatus::Failure => bail!("Request failed: {}", response.message),
                ResponseStatus::Ok => {
                    println!("{}", response.message);

                    if let Some(response_content) = response.content {
                        match response_content.content_type {
                            Some(ContentType::RequestCounts(request_counts)) => {
                                print_request_counts(&request_counts, self.json)?;
                            }
                            Some(ContentType::FrontendList(frontends)) => {
                                print_frontend_list(frontends, self.json)?;
                            }
                            Some(ContentType::Workers(worker_infos)) => {
                                print_status(worker_infos, self.json)?;
                            }
                            Some(ContentType::ListenersList(list)) => {
                                print_listeners(list, self.json)?;
                            }
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
        info!("Preparing to upgrade proxy...");

        self.write_request_on_channel(RequestType::ListWorkers(ListWorkers {}).into())?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status() {
                ResponseStatus::Processing => {
                    debug!("Processing: {}", response.message);
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

                        self.write_request_on_channel(
                            RequestType::UpgradeMain(UpgradeMain {}).into(),
                        )?;

                        info!("Upgrading main process");

                        loop {
                            let response = self.read_channel_message_with_timeout()?;

                            match response.status() {
                                ResponseStatus::Processing => {
                                    debug!("Main process is upgrading");
                                }
                                ResponseStatus::Failure => {
                                    bail!(
                                        "Error: failed to upgrade the main: {}",
                                        response.message
                                    );
                                }
                                ResponseStatus::Ok => {
                                    info!("Main process upgrade succeeded: {}", response.message);
                                    break;
                                }
                            }
                        }

                        // Reconnect to the new main
                        info!("Reconnecting to new main process...");
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
                            info!(
                                "Upgrading worker {} (#{} out of {})",
                                worker.id,
                                i + 1,
                                running_count
                            );

                            self.upgrade_worker(worker.id)
                                .with_context(|| "Upgrading the worker failed")?;
                            //thread::sleep(Duration::from_millis(1000));
                        }

                        info!("Proxy successfully upgraded!");
                    } else {
                        info!("Received a response of the wrong kind: {:?}", response);
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn upgrade_worker(&mut self, worker_id: u32) -> Result<(), anyhow::Error> {
        trace!("upgrading worker {}", worker_id);

        //FIXME: we should be able to soft stop one specific worker
        self.write_request_on_channel(RequestType::UpgradeWorker(worker_id).into())?;

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
        list: bool,
        refresh: Option<u32>,
        metric_names: Vec<String>,
        cluster_ids: Vec<String>,
        backend_ids: Vec<String>,
    ) -> Result<(), anyhow::Error> {
        let request: Request = RequestType::QueryMetrics(QueryMetricsOptions {
            list,
            cluster_ids,
            backend_ids,
            metric_names,
        })
        .into();

        // a loop to reperform the query every refresh time
        loop {
            self.write_request_on_channel(request.clone())?;

            print!("{}", termion::cursor::Save);

            // a loop to process responses
            loop {
                let response = self.read_channel_message_with_timeout()?;

                match response.status() {
                    ResponseStatus::Processing => {
                        debug!("Proxy is processing: {}", response.message);
                    }
                    ResponseStatus::Failure => {
                        if self.json {
                            return print_json_response(&response.message);
                        } else {
                            bail!("could not query proxy state: {}", response.message);
                        }
                    }
                    ResponseStatus::Ok => {
                        if let Some(response_content) = response.content {
                            match response_content.content_type {
                                Some(ContentType::Metrics(aggregated_metrics_data)) => {
                                    print_metrics(aggregated_metrics_data, self.json)?
                                }
                                Some(ContentType::AvailableMetrics(available)) => {
                                    print_available_metrics(&available, self.json)?;
                                }
                                _ => {
                                    debug!("Wrong kind of response here");
                                }
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
        cluster_id: Option<String>,
        domain: Option<String>,
    ) -> Result<(), anyhow::Error> {
        if cluster_id.is_some() && domain.is_some() {
            bail!("Error: Either request an cluster ID or a domain name");
        }

        let request = if let Some(ref cluster_id) = cluster_id {
            RequestType::QueryClusterById(cluster_id.to_string()).into()
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

            RequestType::QueryClustersByDomain(query_domain).into()
        } else {
            RequestType::QueryClustersHashes(QueryClustersHashes {}).into()
        };

        self.write_request_on_channel(request)?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status() {
                ResponseStatus::Processing => {
                    debug!("Proxy is processing: {}", response.message);
                }
                ResponseStatus::Failure => {
                    if self.json {
                        print_json_response(&response.message)?;
                    }
                    bail!("could not query proxy state: {}", response.message);
                }
                ResponseStatus::Ok => {
                    match response.content {
                        Some(ResponseContent {
                            content_type: Some(ContentType::WorkerResponses(worker_responses)),
                        }) => print_cluster_responses(
                            cluster_id,
                            domain,
                            worker_responses,
                            self.json,
                        )?,
                        _ => bail!("Wrong response content"),
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn query_certificates(
        &mut self,
        fingerprint: Option<String>,
        domain: Option<String>,
        query_workers: bool,
    ) -> Result<(), anyhow::Error> {
        let filters = QueryCertificatesFilters {
            domain,
            fingerprint,
        };

        if query_workers {
            self.query_certificates_from_workers(filters)
        } else {
            self.query_certificates_from_the_state(filters)
        }
    }

    fn query_certificates_from_workers(
        &mut self,
        filters: QueryCertificatesFilters,
    ) -> Result<(), anyhow::Error> {
        self.write_request_on_channel(RequestType::QueryCertificatesFromWorkers(filters).into())?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status() {
                ResponseStatus::Processing => {
                    debug!("Proxy is processing: {}", response.message);
                }
                ResponseStatus::Failure => {
                    if self.json {
                        print_json_response(&response.message)?;
                    }
                    bail!("could not get certificate: {}", response.message);
                }
                ResponseStatus::Ok => {
                    info!("We did get a response from the proxy");
                    match response.content {
                        Some(ResponseContent {
                            content_type: Some(ContentType::WorkerResponses(worker_responses)),
                        }) => print_certificates_by_worker(worker_responses.map, self.json)?,
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
        filters: QueryCertificatesFilters,
    ) -> anyhow::Result<()> {
        self.write_request_on_channel(RequestType::QueryCertificatesFromTheState(filters).into())?;

        loop {
            let response = self.read_channel_message_with_timeout()?;

            match response.status() {
                ResponseStatus::Processing => {
                    debug!("Proxy is processing: {}", response.message);
                }
                ResponseStatus::Failure => {
                    bail!("could not get certificate: {}", response.message);
                }
                ResponseStatus::Ok => {
                    debug!("We did get a response from the proxy");
                    trace!("response message: {:?}\n", response.message);

                    if let Some(response_content) = response.content {
                        let certs = match response_content.content_type {
                            Some(ContentType::CertificatesWithFingerprints(certs)) => certs.certs,
                            _ => bail!(format!("Wrong response content {:?}", response_content)),
                        };
                        if certs.is_empty() {
                            bail!("No certificates match your request.");
                        }

                        if self.json {
                            print_json_response(&certs)?;
                        } else {
                            print_certificates_with_validity(certs)
                                .with_context(|| "Could not show certificate")?;
                        }
                    } else {
                        debug!("No response content.");
                    }

                    break;
                }
            }
        }
        Ok(())
    }
}
