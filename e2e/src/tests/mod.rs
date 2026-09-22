//! End-to-end test surface.
//!
//! Tests that toggle the process-wide `e2e-hooks` AtomicBools (e.g.
//! `force_h2_client_failure`) MUST carry
//! `#[serial_test::serial(force_h2_client_failure)]` to avoid cross-test
//! interference. Canonical group:
//! `e2e/src/tests/h2_security_session.rs:239, 622`.

#![allow(clippy::clone_on_copy)]
#![allow(clippy::explicit_counter_loop)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::manual_unwrap_or_default)]
#![allow(clippy::module_inception)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::redundant_field_names)]
#![allow(clippy::single_match)]
#![allow(clippy::type_complexity)]
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::unnecessary_map_or)]
#![allow(clippy::useless_conversion)]
#![allow(clippy::useless_format)]
#![allow(clippy::useless_vec)]

mod cluster_ip_limit_tests;
mod command_channel_security_tests;
mod eviction_tests;
mod fuzz_tests;
mod h1_security_tests;
mod h2_clock_tests;
mod h2_correctness_tests;
mod h2_log_context_tests;
mod h2_priority_rearm_tests;
mod h2_security_header_injection;
mod h2_security_parser;
mod h2_security_session;
mod h2_security_sni;
mod h2_security_tests;
mod h2_tests;
pub(crate) mod h2_utils;
mod hsts_tests;
mod listener_reactivation_tests;
mod listener_update_tests;
mod metrics_lifecycle_tests;
mod mux_tests;
mod protocol_pair_matrix;
mod proxy_protocol_local_tests;
mod redirect_rewrite_auth_tests;
mod router_hostname_tests;
mod router_path_rule_tests;
mod socket_log_context_tests;
mod tcp_sni_tests;
mod tcp_tests;
mod tests;
mod tls_tests;
mod udp_tests;

use std::{io::stdin, net::SocketAddr};

use sozu_command_lib::{
    config::ListenerBuilder,
    proto::command::{
        ActivateListener, Cluster, ListenerType, Request, ServerConfig, request::RequestType,
    },
    scm_socket::Listeners,
    state::ConfigState,
};

use self::tests::create_local_address;
use crate::{
    http_utils::http_ok_response,
    mock::{
        aggregator::SimpleAggregator, async_backend::BackendHandle as AsyncBackend,
        sync_backend::Backend as SyncBackend,
    },
    port_registry::{
        attach_reserved_http_listener, provide_port as reserve_port,
        provide_unbound_port as issue_unbound_port,
    },
    sozu::worker::Worker,
};

#[derive(PartialEq, Eq, Debug)]
pub enum State {
    Success,
    Fail,
    Undecided,
}

fn provide_port() -> u16 {
    reserve_port()
}

fn provide_unbound_port() -> u16 {
    issue_unbound_port()
}

/// Setup a Sozu worker with
/// - `config`
/// - `listeners`
/// - 1 active HttpListener on `front_address`
/// - 1 cluster ("cluster_0")
/// - 1 HttpFrontend for "cluster_0" on `front_address`
/// - n backends ("cluster_0-{0..n}")
pub fn setup_test<S: Into<String>>(
    name: S,
    config: ServerConfig,
    mut listeners: Listeners,
    state: ConfigState,
    front_address: SocketAddr,
    nb_backends: usize,
    should_stick: bool,
) -> (Worker, Vec<SocketAddr>) {
    if listeners.http.is_empty() {
        attach_reserved_http_listener(&mut listeners, front_address);
    }
    let mut worker = Worker::start_new_worker_owned(name, config, listeners, state);

    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpListener(
            ListenerBuilder::new_http(front_address.into())
                .to_http(None)
                .unwrap(),
        )),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::ActivateListener(ActivateListener {
            address: front_address.into(),
            proxy: ListenerType::Http.into(),
            from_scm: false,
        })),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddCluster(Cluster {
            sticky_session: should_stick,
            ..Worker::default_cluster("cluster_0")
        })),
    });
    worker.send_proxy_request(Request {
        request_type: Some(RequestType::AddHttpFrontend(Worker::default_http_frontend(
            "cluster_0",
            front_address,
        ))),
    });

    let mut backends = Vec::new();
    for i in 0..nb_backends {
        let back_address = create_local_address();

        worker.send_proxy_request(
            RequestType::AddBackend(Worker::default_backend(
                "cluster_0",
                format!("cluster_0-{i}"),
                back_address,
                if should_stick {
                    Some(format!("sticky_cluster_0-{i}"))
                } else {
                    None
                },
            ))
            .into(),
        );
        backends.push(back_address);
    }

    worker.read_to_last();
    (worker, backends)
}

pub fn setup_async_test<S: Into<String>>(
    name: S,
    config: ServerConfig,
    listeners: Listeners,
    state: ConfigState,
    front_address: SocketAddr,
    nb_backends: usize,
    should_stick: bool,
) -> (Worker, Vec<AsyncBackend<SimpleAggregator>>) {
    let (worker, backends) = setup_test(
        name,
        config,
        listeners,
        state,
        front_address,
        nb_backends,
        should_stick,
    );
    let backends = backends
        .into_iter()
        .enumerate()
        .map(|(i, back_address)| {
            let aggregator = SimpleAggregator {
                requests_received: 0,
                responses_sent: 0,
            };
            AsyncBackend::spawn_detached_backend(
                format!("BACKEND_{i}"),
                back_address,
                aggregator,
                AsyncBackend::http_handler(format!("pong{i}")),
            )
        })
        .collect::<Vec<_>>();
    (worker, backends)
}

pub fn setup_sync_test<S: Into<String>>(
    name: S,
    config: ServerConfig,
    listeners: Listeners,
    state: ConfigState,
    front_address: SocketAddr,
    nb_backends: usize,
    should_stick: bool,
) -> (Worker, Vec<SyncBackend>) {
    let (worker, backends) = setup_test(
        name,
        config,
        listeners,
        state,
        front_address,
        nb_backends,
        should_stick,
    );
    let backends = backends
        .into_iter()
        .enumerate()
        .map(|(i, back_address)| {
            SyncBackend::new(
                format!("BACKEND_{i}"),
                back_address,
                http_ok_response(format!("pong{i}")),
            )
        })
        .collect::<Vec<_>>();
    (worker, backends)
}

/// The requirement `total` expresses, phrased so it stays true and
/// grammatical at `total == 1`, where one run is not a consecutiveness
/// property at all.
fn stability_check_requirement(total: usize) -> String {
    if total == 1 {
        "a single clean run is required".to_owned()
    } else {
        format!("{total} consecutive successes are required")
    }
}

/// Message for the iteration that broke the run of consecutive successes.
fn stability_check_failure_message(iteration: usize, total: usize) -> String {
    format!(
        "stability check FAILED: iteration {iteration} of {total} did not succeed ({})",
        stability_check_requirement(total)
    )
}

/// Message for the iteration that was interrupted ([`State::Undecided`])
/// before it could resolve to success or failure.
fn stability_check_interrupted_message(iteration: usize, total: usize) -> String {
    format!(
        "stability check INTERRUPTED: iteration {iteration} of {total} was undecided ({})",
        stability_check_requirement(total)
    )
}

/// Message for a check whose every iteration succeeded.
///
/// Shares the `stability check` vocabulary of the two other outcomes, so one
/// `grep 'stability check'` over a run log finds the passes as well as the
/// failures, and so no line says a test "succeeded after N iterations" — the
/// phrasing that made the old failure line read as an exhausted retry budget.
///
/// The `total == 1` arm is spelled out here instead of routing through
/// `stability_check_requirement`, which the two other outcomes share: that
/// function returns a *requirement* clause ("a single clean run is required"),
/// and a pass line reports what happened rather than what was asked for, so
/// splicing one into the other yields either nonsense or a third phrasing of
/// the same fact. What the two functions have in common is the degenerate
/// arity test `total == 1` — structural, and not free to drift — not a string.
fn stability_check_success_message(total: usize) -> String {
    if total == 1 {
        "stability check PASSED: the single required run succeeded".to_owned()
    } else {
        format!("stability check PASSED: all {total} consecutive iterations succeeded")
    }
}

/// Runs the stability check and returns its outcome together with the line
/// that describes it, printing nothing.
///
/// [`repeat_until_error_or`] is a thin printing wrapper around this. The split
/// exists so a unit test can drive the real loop — the iteration counter, the
/// stop-at-the-first-bad-trial rule, and the `(iteration, total)` argument
/// order — rather than only calling the message builders, which cannot catch a
/// regression in any of the three.
fn run_stability_check<F>(times: usize, test: F) -> (State, String)
where
    F: Fn() -> State + Sized,
{
    for i in 1..=times {
        match test() {
            State::Success => {}
            State::Fail => return (State::Fail, stability_check_failure_message(i, times)),
            State::Undecided => {
                return (
                    State::Undecided,
                    stability_check_interrupted_message(i, times),
                );
            }
        }
    }
    (State::Success, stability_check_success_message(times))
}

/// Runs `test` up to `times` times and reports whether it is *stable*.
///
/// This is a **stability check, not a retry helper**: it loops WHILE `test`
/// keeps returning [`State::Success`] and stops at the first iteration that
/// does not, so `repeat_until_error_or(n, ..)` requires `n` **consecutive**
/// clean runs — a single bad trial fails the whole check, it does not spend
/// a retry budget. Choose `n` for what the test is trying to prove:
///
/// * a property that must hold on every run (a timing budget, a race guard,
///   a reaper cadence) genuinely wants several consecutive clean passes;
/// * a property a single clean delivery already proves (a crafted frame is
///   rejected, a header is stripped) gets no extra assurance from repeating
///   it, only more exposure to unrelated per-trial harness flake — see
///   issue #1410.
///
/// `times == 1` is the degenerate case: one run, no consecutiveness to claim,
/// and the reported line says so instead of asking for "1 consecutive
/// successes".
pub fn repeat_until_error_or<F>(times: usize, test_description: &str, test: F) -> State
where
    F: Fn() -> State + Sized,
{
    println!("{test_description}");
    let (state, message) = run_stability_check(times, test);
    println!("------------------------------------------------------------------");
    println!("{message}");
    state
}

pub fn wait_input<S: Into<String>>(s: S) {
    println!("==================================================================");
    println!("{}", s.into());
    println!("==================================================================");
    let mut buf = String::new();
    stdin().read_line(&mut buf).expect("bad input");
}

#[cfg(test)]
mod repeat_until_error_or_tests {
    use super::*;
    use std::cell::Cell;

    /// Red-then-green regression for issue #1410: the failure message must
    /// name which iteration failed *and* the total it was measured against,
    /// so it cannot be misread as an exhausted retry budget.
    #[test]
    fn failure_message_names_the_failing_iteration_and_the_total() {
        let message = stability_check_failure_message(3, 5);
        assert!(
            message.contains("iteration 3 of 5"),
            "expected the failure message to name the failing iteration out of \
             the total (\"iteration 3 of 5\"), got: {message:?}"
        );
    }

    #[test]
    fn interrupted_message_names_the_interrupted_iteration_and_the_total() {
        let message = stability_check_interrupted_message(2, 5);
        assert!(
            message.contains("iteration 2 of 5"),
            "expected the interrupted message to name the interrupted iteration \
             out of the total (\"iteration 2 of 5\"), got: {message:?}"
        );
    }

    /// `n == 1` boundary. Four live call sites pass 1
    /// (`e2e/src/tests/redirect_rewrite_auth_tests.rs`, `e2e/src/tests/h2_tests.rs`),
    /// and one run has no consecutiveness property to require, so no message
    /// may ask for "1 consecutive successes".
    #[test]
    fn single_run_messages_claim_no_consecutiveness_property() {
        assert_eq!(
            stability_check_failure_message(1, 1),
            "stability check FAILED: iteration 1 of 1 did not succeed \
             (a single clean run is required)"
        );
        assert_eq!(
            stability_check_interrupted_message(1, 1),
            "stability check INTERRUPTED: iteration 1 of 1 was undecided \
             (a single clean run is required)"
        );
        assert_eq!(
            stability_check_success_message(1),
            "stability check PASSED: the single required run succeeded"
        );
        for message in [
            stability_check_failure_message(1, 1),
            stability_check_interrupted_message(1, 1),
            stability_check_success_message(1),
        ] {
            assert!(
                !message.contains("consecutive"),
                "a one-run check must not claim consecutiveness, got: {message:?}"
            );
        }
    }

    /// Drives the real loop rather than the message builders alone, so the
    /// iteration counter and the `(iteration, total)` argument order are
    /// covered. Injecting either regression reported on issue #1410 —
    /// `1..=times` becoming `0..times`, or the two arguments swapped — turns
    /// this red, while the message-only tests above stay green.
    #[test]
    fn a_failing_third_trial_of_five_is_reported_as_iteration_three_of_five() {
        let trials = Cell::new(0usize);
        let (state, message) = run_stability_check(5, || {
            trials.set(trials.get() + 1);
            if trials.get() == 3 {
                State::Fail
            } else {
                State::Success
            }
        });
        assert_eq!(state, State::Fail);
        assert_eq!(
            trials.get(),
            3,
            "the check must stop at the first bad trial, not run the remaining two"
        );
        assert_eq!(
            message,
            "stability check FAILED: iteration 3 of 5 did not succeed \
             (5 consecutive successes are required)"
        );
    }

    #[test]
    fn an_undecided_second_trial_of_three_stops_the_check() {
        let trials = Cell::new(0usize);
        let (state, message) = run_stability_check(3, || {
            trials.set(trials.get() + 1);
            if trials.get() == 2 {
                State::Undecided
            } else {
                State::Success
            }
        });
        assert_eq!(state, State::Undecided);
        assert_eq!(trials.get(), 2);
        assert_eq!(
            message,
            "stability check INTERRUPTED: iteration 2 of 3 was undecided \
             (3 consecutive successes are required)"
        );
    }

    #[test]
    fn a_clean_check_runs_exactly_n_trials_and_reports_the_pass() {
        let trials = Cell::new(0usize);
        let (state, message) = run_stability_check(5, || {
            trials.set(trials.get() + 1);
            State::Success
        });
        assert_eq!(state, State::Success);
        assert_eq!(trials.get(), 5, "a clean check runs exactly n trials");
        assert_eq!(
            message,
            "stability check PASSED: all 5 consecutive iterations succeeded"
        );
    }

    #[test]
    fn a_failing_single_run_check_is_reported_without_a_consecutiveness_claim() {
        let (state, message) = run_stability_check(1, || State::Fail);
        assert_eq!(state, State::Fail);
        assert_eq!(
            message,
            "stability check FAILED: iteration 1 of 1 did not succeed \
             (a single clean run is required)"
        );
    }
}
