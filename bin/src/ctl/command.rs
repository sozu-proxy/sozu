use anyhow::{self, bail, Context};

use sozu_command_lib::proto::command::{
    request::RequestType, response_content::ContentType, ListWorkers, QueryMetricsOptions, Request,
    Response, ResponseContent, ResponseStatus, RunState, UpgradeMain,
};

use crate::ctl::{create_channel, CommandManager};

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
                    response.display(self.json)?;
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
                        // display worker status
                        response.display(false)?;

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
                    ResponseStatus::Failure | ResponseStatus::Ok => {
                        response.display(self.json)?;
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
}
