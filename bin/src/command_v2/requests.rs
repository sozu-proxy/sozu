use std::collections::BTreeMap;

use mio::Token;
use sozu_command_lib::{
    proto::command::{
        request::RequestType, response_content::ContentType, ClusterHashes, ClusterInformations,
        FrontendFilters, Request, ResponseContent, ResponseStatus, WorkerInfo, WorkerInfos,
        WorkerResponses,
    },
    response::WorkerResponse,
};

use crate::command_v2::{
    server::{DefaultGatherer, Gatherer, GatheringTask, Server},
    ClientSession,
};

impl Server {
    pub fn handle_request(&mut self, client: &mut ClientSession, request: Request) {
        let request_type = request.request_type.unwrap();
        match request_type {
            RequestType::SaveState(_) => todo!(),
            RequestType::LoadState(_) => todo!(),
            RequestType::ListWorkers(_) => list_workers(self, client),
            RequestType::ListFrontends(inner) => {
                list_frontend_command(self, client, inner);
            }
            RequestType::ListListeners(_) => todo!(),
            RequestType::LaunchWorker(_) => todo!(),
            RequestType::UpgradeMain(_) => todo!(),
            RequestType::UpgradeWorker(_) => todo!(),
            RequestType::SubscribeEvents(_) => todo!(),
            RequestType::ReloadConfiguration(_) => todo!(),
            RequestType::Status(_) => todo!(),
            RequestType::AddCluster(_)
            | RequestType::ActivateListener(_)
            | RequestType::AddBackend(_)
            | RequestType::AddCertificate(_)
            | RequestType::AddHttpFrontend(_)
            | RequestType::AddHttpListener(_)
            | RequestType::AddHttpsFrontend(_)
            | RequestType::AddHttpsListener(_)
            | RequestType::AddTcpFrontend(_)
            | RequestType::AddTcpListener(_)
            | RequestType::DeactivateListener(_)
            | RequestType::RemoveBackend(_)
            | RequestType::RemoveCertificate(_)
            | RequestType::RemoveCluster(_)
            | RequestType::RemoveHttpFrontend(_)
            | RequestType::RemoveHttpsFrontend(_)
            | RequestType::RemoveListener(_)
            | RequestType::RemoveTcpFrontend(_)
            | RequestType::ReplaceCertificate(_) => {
                worker_request(self, client, request_type);
            }
            RequestType::QueryClustersHashes(_)
            | RequestType::QueryClustersByDomain(_)
            | RequestType::QueryClusterById(_) => {
                query_clusters(self, client, request_type);
            }
            RequestType::QueryMetrics(_) => todo!(),
            RequestType::SoftStop(_) => todo!(),
            RequestType::HardStop(_) => todo!(),
            RequestType::ConfigureMetrics(_) => todo!(),
            RequestType::Logging(_) => todo!(),
            RequestType::ReturnListenSockets(_) => todo!(),
            RequestType::QueryCertificatesFromTheState(_) => todo!(),
            RequestType::QueryCertificatesFromWorkers(_) => todo!(),
            RequestType::CountRequests(_) => todo!(),
        }

        self.tick_later(client.token);
    }

    fn query_main(&self, request: &RequestType) -> Result<Option<ResponseContent>, ()> {
        match request {
            // RequestType::SaveState(_) => todo!(),
            // RequestType::LoadState(_) => todo!(),
            // RequestType::ListWorkers(_) => todo!(),
            // RequestType::ListFrontends(_) => todo!(),
            // RequestType::ListListeners(_) => todo!(),
            // RequestType::LaunchWorker(_) => todo!(),
            // RequestType::UpgradeMain(_) => todo!(),
            // RequestType::UpgradeWorker(_) => todo!(),
            // RequestType::SubscribeEvents(_) => todo!(),
            // RequestType::ReloadConfiguration(_) => todo!(),
            // RequestType::Status(_) => todo!(),
            // RequestType::AddCluster(_) => todo!(),
            // RequestType::RemoveCluster(_) => todo!(),
            // RequestType::AddHttpFrontend(_) => todo!(),
            // RequestType::RemoveHttpFrontend(_) => todo!(),
            // RequestType::AddHttpsFrontend(_) => todo!(),
            // RequestType::RemoveHttpsFrontend(_) => todo!(),
            // RequestType::AddCertificate(_) => todo!(),
            // RequestType::ReplaceCertificate(_) => todo!(),
            // RequestType::RemoveCertificate(_) => todo!(),
            // RequestType::AddTcpFrontend(_) => todo!(),
            // RequestType::RemoveTcpFrontend(_) => todo!(),
            // RequestType::AddBackend(_) => todo!(),
            // RequestType::RemoveBackend(_) => todo!(),
            // RequestType::AddHttpListener(_) => todo!(),
            // RequestType::AddHttpsListener(_) => todo!(),
            // RequestType::AddTcpListener(_) => todo!(),
            // RequestType::RemoveListener(_) => todo!(),
            // RequestType::ActivateListener(_) => todo!(),
            // RequestType::DeactivateListener(_) => todo!(),
            RequestType::QueryClusterById(cluster_id) => Ok(Some(
                ContentType::Clusters(ClusterInformations {
                    vec: self.state.cluster_state(cluster_id).into_iter().collect(),
                })
                .into(),
            )),
            RequestType::QueryClustersByDomain(domain) => {
                let cluster_ids = self
                    .state
                    .get_cluster_ids_by_domain(domain.hostname.clone(), domain.path.clone());
                let vec = cluster_ids
                    .iter()
                    .filter_map(|cluster_id| self.state.cluster_state(cluster_id))
                    .collect();
                Ok(Some(
                    ContentType::Clusters(ClusterInformations { vec }).into(),
                ))
            }
            RequestType::QueryClustersHashes(_) => Ok(Some(
                ContentType::ClusterHashes(ClusterHashes {
                    map: self.state.hash_state(),
                })
                .into(),
            )),
            // RequestType::QueryMetrics(_) => todo!(),
            // RequestType::SoftStop(_) => todo!(),
            // RequestType::HardStop(_) => todo!(),
            // RequestType::ConfigureMetrics(_) => todo!(),
            // RequestType::Logging(_) => todo!(),
            // RequestType::ReturnListenSockets(_) => todo!(),
            // RequestType::QueryCertificatesFromTheState(_) => todo!(),
            // RequestType::QueryCertificatesFromWorkers(_) => todo!(),
            // RequestType::CountRequests(_) => todo!(),
            _ => Ok(None),
        }
    }
}

//===============================================
// List frontends

pub fn list_frontend_command(
    server: &mut Server,
    client: &mut ClientSession,
    filters: FrontendFilters,
) {
    let response = server
        .query_main(&RequestType::ListFrontends(filters).into())
        .unwrap();
    client.finish_ok(response);
}

fn list_workers(server: &mut Server, client: &mut ClientSession) {
    let vec: Vec<WorkerInfo> = server
        .workers
        .iter()
        .map(|(_, worker_session)| WorkerInfo {
            id: worker_session.id,
            pid: worker_session.pid,
            run_state: worker_session.run_state as i32,
        })
        .collect();

    debug!("workers: {:#?}", vec);

    client.finish_ok(Some(ContentType::Workers(WorkerInfos { vec }).into()));
}

//===============================================
// Query clusters

#[derive(Debug)]
pub struct QueryClustersCommand {
    pub client_token: Token,
    pub request_type: RequestType,
    pub gatherer: DefaultGatherer,
    main_process_response: Option<ResponseContent>,
}

pub fn query_clusters(
    server: &mut Server,
    client: &mut ClientSession,
    request_content: RequestType,
) {
    let task = Box::new(QueryClustersCommand {
        client_token: client.token,
        request_type: request_content.clone(),
        gatherer: DefaultGatherer::default(),
        main_process_response: server.query_main(&request_content).unwrap(),
    });
    client.return_processing("Querying cluster hashes...");

    server.scatter(request_content.into(), task)
}

impl GatheringTask for QueryClustersCommand {
    fn client_token(&self) -> Option<Token> {
        Some(self.client_token)
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn on_finish(self: Box<Self>, _server: &mut Server, client: &mut ClientSession) {
        let mut worker_responses: BTreeMap<String, ResponseContent> = self
            .gatherer
            .responses
            .into_iter()
            .filter_map(|(worker_id, proxy_response)| {
                proxy_response
                    .content
                    .map(|response_content| (worker_id.to_string(), response_content))
            })
            .collect();

        if let Some(main_response) = &self.main_process_response {
            worker_responses.insert(String::from("main"), main_response.clone());
        }

        client.finish_ok(Some(
            ContentType::WorkerResponses(WorkerResponses {
                map: worker_responses,
            })
            .into(),
        ));
    }
}

//===============================================
// Load static configuration

#[derive(Debug)]
struct LoadStaticConfig {
    pub ok: usize,
    pub error: usize,
    pub expected_responses: usize,
}

pub fn load_static_config(server: &mut Server) {
    let callback = Box::new(LoadStaticConfig {
        ok: 0,
        error: 0,
        expected_responses: 0,
    });
    let task_id = server.new_task(callback);
    for (request_index, message) in server
        .config
        .generate_config_messages()
        .unwrap()
        .into_iter()
        .enumerate()
    {
        let request = message.content;
        if let Err(e) = server.state.dispatch(&request) {
            error!("Could not execute request on state: {:#}", e);
        }

        if let &Some(RequestType::AddCertificate(_)) = &request.request_type {
            debug!("config generated AddCertificate( ... )");
        } else {
            debug!("config generated {:?}", request);
        }

        server.scatter_on(request, task_id, request_index);
    }
}

impl GatheringTask for LoadStaticConfig {
    fn client_token(&self) -> Option<Token> {
        None
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        self
    }

    fn on_finish_no_client(self: Box<Self>, server: &mut Server) {
        if self.error == 0 {
            info!("loading state: {} ok messages, 0 errors", self.ok);
        } else {
            error!(
                "loading state: {} ok messages, {} errors",
                self.ok, self.error
            );
        }
        server.update_counts();
    }
}

impl Gatherer for LoadStaticConfig {
    fn inc_expected_responses(&mut self, count: usize) {
        self.expected_responses += count;
    }

    fn on_message_no_client(
        &mut self,
        _server: &mut Server,
        _worker_id: u32,
        message: WorkerResponse,
    ) -> bool {
        match message.status {
            ResponseStatus::Ok => {
                self.ok += 1;
            }
            ResponseStatus::Processing => {
                info!("processing");
            }
            ResponseStatus::Failure => {
                error!(
                    "error handling configuration message {}: {}",
                    message.id, message.message
                );
                self.error += 1;
            }
        }
        self.ok + self.error >= self.expected_responses
    }
}

// =========================================================
// Worker request

#[derive(Debug)]
struct WorkerRequest {
    pub client_token: Token,
    pub gatherer: DefaultGatherer,
}

pub fn worker_request(
    server: &mut Server,
    client: &mut ClientSession,
    request_content: RequestType,
) {
    let request: Request = request_content.into();

    if let Err(e) = server.state.dispatch(&request) {
        return client.finish_failure(&format!(
            "could not dispatch request on the main process state: {e}",
        ));
    }

    server.scatter(
        request,
        Box::new(WorkerRequest {
            client_token: client.token,
            gatherer: DefaultGatherer::default(),
        }),
    )
}

impl GatheringTask for WorkerRequest {
    fn client_token(&self) -> Option<Token> {
        Some(self.client_token)
    }

    fn get_gatherer(&mut self) -> &mut dyn Gatherer {
        &mut self.gatherer
    }

    fn on_finish(self: Box<Self>, _server: &mut Server, client: &mut ClientSession) {
        let mut messages = vec![];
        let mut has_error = false;

        for (worker_id, response) in self.gatherer.responses {
            match response.status {
                ResponseStatus::Failure => {
                    messages.push(format!("{}: {}", worker_id, response.message));
                    has_error = true;
                }
                _ => messages.push(format!("{}: OK", worker_id)),
            }
        }

        if has_error {
            client.finish_failure(messages.join(", "));
        } else {
            client.finish_ok(None);
        }
    }
}
