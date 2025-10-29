use std::time::Duration;

use sozu_command_lib::{
    logging::setup_logging_with_config,
    proto::command::{
        ListWorkers, QueryMetricsOptions, Request, Response, ResponseContent, ResponseStatus,
        UpgradeMain, request::RequestType, response_content::ContentType,
    },
};

use crate::ctl::{CommandManager, CtlError, create_channel};

impl CommandManager {
    fn write_request_on_channel(&mut self, request: Request) -> Result<(), CtlError> {
        self.channel
            .write_message(&request)
            .map_err(CtlError::WriteRequest)
    }

    fn read_channel_message_with_timeout(&mut self) -> Result<Response, CtlError> {
        self.channel
            .read_message_blocking_timeout(Some(self.timeout))
            .map_err(CtlError::ReadBlocking)
    }

    fn send_request_get_response(
        &mut self,
        request: Request,
        timeout: bool,
    ) -> Result<Response, CtlError> {
        self.channel
            .write_message(&request)
            .map_err(CtlError::WriteRequest)?;

        loop {
            let response = if timeout {
                self.read_channel_message_with_timeout()?
            } else {
                self.channel
                    .read_message_blocking_timeout(None)
                    .map_err(CtlError::ReadBlocking)?
            };

            match response.status() {
                ResponseStatus::Processing => {
                    if !self.json {
                        debug!("Processing: {}", response.message);
                    }
                    if let Some(ResponseContent {
                        content_type: Some(ContentType::Event(event)),
                    }) = response.content
                    {
                        info!("{}, {}", response.message, event);
                    }
                }
                ResponseStatus::Failure => return Err(CtlError::Failure(response.message)),
                ResponseStatus::Ok => return Ok(response),
            }
        }
    }

    fn send_request_display_response(
        &mut self,
        request: Request,
        timeout: bool,
    ) -> Result<(), CtlError> {
        self.send_request_get_response(request, timeout)?
            .display(self.json)
            .map_err(CtlError::Display)
    }

    pub fn send_request(&mut self, request: Request) -> Result<(), CtlError> {
        self.send_request_display_response(request, true)
    }

    pub fn send_request_no_timeout(&mut self, request: Request) -> Result<(), CtlError> {
        self.send_request_display_response(request, false)
    }

    pub fn get_metrics(
        &mut self,
        list: bool,
        refresh: Option<u32>,
        metric_names: Vec<String>,
        cluster_ids: Vec<String>,
        backend_ids: Vec<String>,
        no_clusters: bool,
        workers: bool,
    ) -> Result<(), CtlError> {
        let request: Request = RequestType::QueryMetrics(QueryMetricsOptions {
            list,
            cluster_ids,
            backend_ids,
            metric_names,
            no_clusters,
            workers,
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
                        if !self.json {
                            debug!("Processing: {}", response.message);
                        }
                    }
                    ResponseStatus::Failure | ResponseStatus::Ok => {
                        response.display(self.json).map_err(CtlError::Display)?;
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

    pub fn upgrade_main(&mut self) -> Result<(), CtlError> {
        debug!("updating main process");
        self.send_request(RequestType::UpgradeMain(UpgradeMain {}).into())?;

        info!("recreating a channel to reconnect with the new main process...");
        self.channel = create_channel(&self.config)?;

        info!("requesting the list of workers from the new main");
        let response =
            self.send_request_get_response(RequestType::ListWorkers(ListWorkers {}).into(), true)?;

        let workers = match response.content {
            Some(ResponseContent {
                content_type: Some(ContentType::Workers(worker_infos)),
            }) => worker_infos,
            _ => return Err(CtlError::WrongResponse(response)),
        };

        info!("About to upgrade these workers: {:?}", workers);

        let mut upgrade_jobs = Vec::new();

        for worker in workers.vec {
            info!("trying to upgrade worker {}", worker.id);
            let config = self.config.clone();

            upgrade_jobs.push(std::thread::spawn(move || {
                if let Err(e) =
                    setup_logging_with_config(&config, &format!("UPGRADE-WRK-{}", worker.id))
                {
                    error!("Could not setup logging: {}", e);
                }

                info!("creating channel to upgrade worker {}", worker.id);
                let channel = match create_channel(&config) {
                    Ok(channel) => channel,
                    Err(e) => {
                        error!(
                            "could not create channel to worker {}, this is critical: {}",
                            worker.id, e
                        );
                        return;
                    }
                };

                info!("created channel to upgrade worker {}", worker.id);

                let mut command_manager = CommandManager {
                    channel,
                    timeout: Duration::from_secs(60), // overriden by upgrade_timeout anyway
                    config,
                    json: false,
                };

                match command_manager.upgrade_worker(worker.id) {
                    Ok(()) => info!("successfully upgraded worker {}", worker.id),
                    Err(e) => error!("error upgrading worker {}: {}", worker.id, e),
                }
            }));
        }

        for job in upgrade_jobs {
            if let Err(e) = job.join() {
                error!("an upgrading job panicked: {:?}", e)
            }
        }

        info!("Finished upgrading");

        Ok(())
    }
}
