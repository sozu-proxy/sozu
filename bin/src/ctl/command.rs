use std::time::{Duration, Instant};

use sozu_command_lib::{
    logging::setup_logging_with_config,
    proto::command::{
        ListWorkers, ProtectedHealthCheckPhase, QueryMetricsOptions,
        QueryProtectedHealthCheckSupport, Request, Response, ResponseContent, ResponseStatus,
        RunState, UpgradeMain, request::RequestType, response_content::ContentType,
    },
};

use crate::ctl::{CommandManager, CtlError, create_channel};

/// Total time the main process has to confirm the protected health check
/// protocol. It is its own budget: the global command timeout, which `0`
/// disables, does not apply, and progress reports do not extend it.
const PROTECTED_CAPABILITY_DEADLINE: Duration = Duration::from_secs(5);

/// Whether `response` is exactly the capability reply of the protected health
/// check protocol: no cluster, version 0, and no other content.
fn confirms_protected_health_check_support(response: &Response) -> bool {
    matches!(
        &response.content,
        Some(ResponseContent {
            content_type: Some(ContentType::ProtectedHealthCheckReply(reply)),
        }) if reply.phase == ProtectedHealthCheckPhase::ProtectedHealthCheckCapability as i32
            && reply.cluster_id.is_empty()
            && reply.version == 0
    )
}

impl CommandManager {
    fn write_request_on_channel(&mut self, request: Request) -> Result<(), CtlError> {
        self.channel
            .write_message(&request)
            .map_err(CtlError::WriteRequest)
    }

    fn read_channel_message_with_timeout(&mut self) -> Result<Response, CtlError> {
        self.channel
            .read_message_blocking_timeout(self.timeout)
            .map_err(CtlError::ReadBlocking)
    }

    pub(super) fn send_request_get_response(
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

    /// Ask the main process whether every worker supports protected health
    /// check headers, within [`PROTECTED_CAPABILITY_DEADLINE`] counted from the
    /// request. Only an `Ok` carrying exactly the capability reply is a yes:
    /// a failure, any other answer, silence, a closed channel or the deadline
    /// refuse, and the caller then reads and sends nothing.
    pub(super) fn require_protected_health_check_support(&mut self) -> Result<(), CtlError> {
        let deadline = Instant::now() + PROTECTED_CAPABILITY_DEADLINE;
        self.write_request_on_channel(
            RequestType::QueryProtectedHealthCheckSupport(QueryProtectedHealthCheckSupport {})
                .into(),
        )?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let response = self
                .channel
                .read_message_blocking_timeout(Some(remaining))
                .map_err(|error| {
                    debug!("protected health check capability query: {}", error);
                    CtlError::ProtectedHealthCheckUnsupported
                })?;
            match response.status() {
                ResponseStatus::Processing => {}
                ResponseStatus::Failure => return Err(CtlError::Failure(response.message)),
                ResponseStatus::Ok if confirms_protected_health_check_support(&response) => {
                    return Ok(());
                }
                ResponseStatus::Ok => return Err(CtlError::ProtectedHealthCheckUnsupported),
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

        // The new main also keeps the workers still draining after their own
        // upgrade; they are replaced already and refuse another upgrade.
        for worker in workers.vec.into_iter().filter(|worker| {
            worker.run_state != RunState::Stopping as i32
                && worker.run_state != RunState::Stopped as i32
        }) {
            info!("trying to upgrade worker {}", worker.id);
            let config = self.config.clone();

            upgrade_jobs.push((
                worker.id,
                std::thread::spawn(move || {
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
                            return Err(e);
                        }
                    };

                    info!("created channel to upgrade worker {}", worker.id);

                    let mut command_manager = CommandManager {
                        channel,
                        timeout: Some(Duration::from_secs(60)), // overriden by upgrade_timeout anyway
                        config,
                        json: false,
                    };

                    let result = command_manager.upgrade_worker(worker.id);
                    match &result {
                        Ok(()) => info!("successfully upgraded worker {}", worker.id),
                        Err(e) => error!("error upgrading worker {}: {}", worker.id, e),
                    }
                    result
                }),
            ));
        }

        // Join every job even after a failure: the main has already changed
        // and the remaining workers still need their upgrade outcome reported.
        let mut failures = Vec::new();
        for (worker_id, job) in upgrade_jobs {
            match job.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("worker {worker_id}: {error}")),
                Err(error) => {
                    error!("an upgrading job panicked: {:?}", error);
                    failures.push(format!("worker {worker_id}: upgrade thread panicked"));
                }
            }
        }

        if !failures.is_empty() {
            return Err(CtlError::Failure(format!(
                "main upgraded, but some worker upgrades failed: {}",
                failures.join("; ")
            )));
        }

        info!("Finished upgrading");

        Ok(())
    }
}
