//! The worker half of the protected health-check protocol.
//!
//! The main process walks every worker through *prepare*, *commit* and, when
//! anything fails, *abort*, then *release* (`sozu_command::protected_transaction`).
//! This is what a worker does with each order:
//!
//! - *prepare* validates the order and stages it in the [`WorkerStore`]; the
//!   configuration, the `BackendMap` and the probes are untouched;
//! - *commit* applies the staged version: the cluster configuration in the
//!   state, and the configuration together with its values in the
//!   `BackendMap`, where the probes read them. The state before the commit is
//!   kept until the order to release it;
//! - *abort* drops a staged version, or restores exactly the state that
//!   preceded an applied one;
//! - *release* forgets that state once every worker applied the version.
//!
//! The `BackendMap` stays the single source the probes read. An ordinary change
//! of the cluster (`AddCluster`, `RemoveCluster`, `SetHealthCheck`,
//! `RemoveHealthCheck`) goes through [`ProtectedWorker::forget_cluster`], so
//! the store never keeps a value the `BackendMap` dropped.

use std::collections::HashMap;

use sozu_command::{
    protected_transaction::{Commit, Payload, WorkerStore},
    proto::command::{
        HealthCheckConfig, ProtectedHealthCheckOrder, ProtectedHealthCheckPhase,
        ProtectedHealthCheckReply, SetHealthCheck, WorkerResponse, request::RequestType,
        response_content::ContentType,
    },
    secret::{ProtectedHeaderSet, ProtectedHeaderVersion, ProtectedHealthCheck},
    state::ConfigState,
};

use crate::backends::BackendMap;

/// What a commit replaced, to put back on an abort.
struct Rollback {
    state: Option<HealthCheckConfig>,
    backend: Option<HealthCheckConfig>,
    headers: Option<ProtectedHeaderSet>,
}

/// The versioned protected headers one worker holds, and the state to restore
/// when an applied version is aborted. No serde, no `Clone`.
#[derive(Default)]
pub struct ProtectedWorker {
    store: WorkerStore,
    rollbacks: HashMap<(String, ProtectedHeaderVersion), Rollback>,
}

impl std::fmt::Debug for ProtectedWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtectedWorker")
            .field("store", &self.store)
            .field("rollbacks", &self.rollbacks.len())
            .finish()
    }
}

fn reply(
    request_id: &str,
    phase: ProtectedHealthCheckPhase,
    cluster: &str,
    version: u64,
) -> WorkerResponse {
    WorkerResponse::ok_with_content(
        request_id.to_owned(),
        ContentType::ProtectedHealthCheckReply(ProtectedHealthCheckReply {
            phase: phase as i32,
            cluster_id: cluster.to_owned(),
            version,
        })
        .into(),
    )
}

fn refuse(request_id: &str, reason: &'static str) -> WorkerResponse {
    WorkerResponse::error(request_id.to_owned(), reason)
}

impl ProtectedWorker {
    /// Answer a protected health-check request. Anything else is not for this
    /// module and yields `None`. The answer carries the phase, the cluster and
    /// the version, never a header name or a value, and every refusal is a
    /// static text.
    pub fn handle(
        &mut self,
        request_id: &str,
        request: &mut RequestType,
        state: &mut ConfigState,
        backends: &mut BackendMap,
    ) -> Option<WorkerResponse> {
        match request {
            RequestType::QueryProtectedHealthCheckSupport(_) => Some(reply(
                request_id,
                ProtectedHealthCheckPhase::ProtectedHealthCheckCapability,
                "",
                0,
            )),
            RequestType::ProtectedHealthCheckOrder(order) => {
                Some(self.order(request_id, order, state, backends))
            }
            _ => None,
        }
    }

    /// An ordinary change of `cluster` replaced or dropped what the
    /// `BackendMap` holds for it: nothing staged, kept or applied here may
    /// outlive that.
    pub fn forget_cluster(&mut self, cluster: &str) {
        self.store.remove_cluster(cluster);
        self.rollbacks.retain(|(held, _), _| held != cluster);
    }

    fn order(
        &mut self,
        request_id: &str,
        order: &mut ProtectedHealthCheckOrder,
        state: &mut ConfigState,
        backends: &mut BackendMap,
    ) -> WorkerResponse {
        let Ok(phase) = ProtectedHealthCheckPhase::try_from(order.phase) else {
            return refuse(request_id, "unknown protected health check phase");
        };
        let version = ProtectedHeaderVersion::from_wire(order.version);
        let cluster = order.cluster_id.to_owned();
        let outcome = match phase {
            ProtectedHealthCheckPhase::ProtectedHealthCheckCapability => {
                Err("a capability query is not an order")
            }
            ProtectedHealthCheckPhase::ProtectedHealthCheckPrepare => {
                self.prepare(order, version, state)
            }
            ProtectedHealthCheckPhase::ProtectedHealthCheckCommit => {
                self.commit(&cluster, version, state, backends)
            }
            ProtectedHealthCheckPhase::ProtectedHealthCheckAbort => {
                self.abort(&cluster, version, state, backends);
                Ok(())
            }
            ProtectedHealthCheckPhase::ProtectedHealthCheckRelease => {
                self.store.release(&cluster, version);
                self.rollbacks.remove(&(cluster.to_owned(), version));
                Ok(())
            }
        };
        match outcome {
            Ok(()) => reply(request_id, phase, &cluster, order.version),
            Err(reason) => refuse(request_id, reason),
        }
    }

    fn prepare(
        &mut self,
        order: &mut ProtectedHealthCheckOrder,
        version: ProtectedHeaderVersion,
        state: &ConfigState,
    ) -> Result<(), &'static str> {
        let prepare = order
            .prepare
            .as_mut()
            .ok_or("a prepare order carries no health check")?;
        if prepare.cluster_id != order.cluster_id {
            return Err("the prepared cluster is not the ordered one");
        }
        let protected =
            ProtectedHealthCheck::from_trusted(prepare).map_err(|error| match error {
                sozu_command::secret::ProtectedHealthCheckError::Invalid(reason) => reason,
                _ => "the protected health check order is invalid",
            })?;
        if !state.clusters.contains_key(protected.cluster_id()) {
            return Err("the cluster is not known to this worker");
        }
        let metadata = RequestType::SetHealthCheck(SetHealthCheck {
            cluster_id: protected.cluster_id().to_owned(),
            config: protected.config().to_owned(),
        });
        if state.health_check_port_unix_conflict(&metadata).is_some() {
            return Err("a health check port and a unix backend cannot share a cluster");
        }
        self.store
            .prepare(
                protected.cluster_id(),
                version,
                Some(Payload {
                    config: protected.config().to_owned(),
                    headers: protected.headers().to_owned(),
                }),
            )
            .map_err(|_| "the protected health check version is stale")
    }

    fn commit(
        &mut self,
        cluster: &str,
        version: ProtectedHeaderVersion,
        state: &mut ConfigState,
        backends: &mut BackendMap,
    ) -> Result<(), &'static str> {
        if self.rollbacks.contains_key(&(cluster.to_owned(), version)) {
            // A repeated commit of the version already applied.
            return Ok(());
        }
        match self.store.commit(cluster, version) {
            Commit::Applied => {}
            Commit::Superseded => return Err("a newer protected health check is already applied"),
            Commit::Unknown => return Err("the protected health check version was not staged"),
        }
        let Some(payload) = self.store.effective(cluster).cloned() else {
            self.store.abort(cluster, version);
            return Err("the protected health check version is a removal");
        };
        let rollback = Rollback {
            state: state
                .clusters
                .get(cluster)
                .and_then(|held| held.health_check.to_owned()),
            backend: backends.health_check_configs.get(cluster).cloned(),
            headers: backends
                .protected_health_check_headers
                .get(cluster)
                .cloned(),
        };
        let applied = state.dispatch(
            &RequestType::SetHealthCheck(SetHealthCheck {
                cluster_id: cluster.to_owned(),
                config: payload.config.to_owned(),
            })
            .into(),
        );
        if applied.is_err() {
            self.store.abort(cluster, version);
            self.store.remove_cluster(cluster);
            return Err("the worker state refused the protected health check");
        }
        backends.set_protected_health_check(cluster, payload.config, payload.headers);
        self.rollbacks
            .insert((cluster.to_owned(), version), rollback);
        Ok(())
    }

    fn abort(
        &mut self,
        cluster: &str,
        version: ProtectedHeaderVersion,
        state: &mut ConfigState,
        backends: &mut BackendMap,
    ) {
        self.store.abort(cluster, version);
        if let Some(rollback) = self.rollbacks.remove(&(cluster.to_owned(), version)) {
            if let Some(held) = state.clusters.get_mut(cluster) {
                held.health_check = rollback.state;
            }
            backends.restore_health_check(cluster, rollback.backend, rollback.headers);
            // The `BackendMap` is the truth again: no copy of the aborted or
            // the restored payload stays in the store.
            self.store.remove_cluster(cluster);
        }
    }
}
