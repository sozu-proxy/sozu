use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use rusty_ulid::Ulid;
use sozu_command_lib::{
    config::MetricDetailLevel, logging::LogContext, proto::command::filtered_metrics,
};
use sozu_lib::{
    SessionMetrics,
    metrics::{METRICS, names},
};

#[test]
fn public_session_metrics_literal_and_registration_remain_supported() {
    // `SessionMetrics` has historically been constructible by downstream
    // crates. Keep this literal exhaustive so adding a private field is a
    // compile-time API regression rather than an unobserved source break.
    let metrics = SessionMetrics {
        start: None,
        start_wall: None,
        service_time: Duration::ZERO,
        wait_time: Duration::ZERO,
        bin: 0,
        bout: 0,
        service_start: None,
        wait_start: Instant::now(),
        backend_id: None::<Rc<str>>,
        backend_start: None,
        backend_connected: None,
        backend_headers_received: None,
        backend_stop: None,
        backend_bin: 0,
        backend_bout: 0,
    };

    // The public method is also a current-state emission surface for
    // embedders. At process detail its labelled access-log event is folded
    // into the proxy aggregate; a fail-closed delayed-owner path would drop
    // it instead.
    METRICS.with(|aggregator| {
        let mut aggregator = aggregator.borrow_mut();
        aggregator.clear_local();
        aggregator.set_up_detail(MetricDetailLevel::Process);
    });

    let context = LogContext {
        session_id: Ulid::generate(),
        request_id: None,
        cluster_id: Some("external-cluster"),
        backend_id: Some("external-backend"),
    };
    metrics.register_end_of_session(&context);

    METRICS.with(|aggregator| {
        let mut aggregator = aggregator.borrow_mut();
        let dumped = aggregator.dump_local_proxy_metrics();
        assert_eq!(
            dumped
                .get(names::access_logs::COUNT)
                .and_then(|metric| metric.inner.as_ref()),
            Some(&filtered_metrics::Inner::Count(1)),
            "the historical public method must still record its labelled event",
        );
        aggregator.clear_local();
        aggregator.set_up_detail(MetricDetailLevel::Cluster);
    });
}
