//! The full upgrade must expose a partial worker failure to automation.
//! A scripted command peer isolates CLI aggregation from process handoff.

use std::{
    fs,
    os::unix::net::UnixListener,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use mio::net::UnixStream;
use sozu_command_lib::{
    channel::Channel,
    proto::command::{
        Request, Response, ResponseContent, ResponseStatus, RunState, WorkerInfo, WorkerInfos,
        request::RequestType, response_content::ContentType,
    },
};

fn accept(listener: &UnixListener) -> Channel<Response, Request> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let mut channel = Channel::new(UnixStream::from_std(stream), 16_384, 163_840);
                channel.blocking().expect("blocking command peer");
                return channel;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "CLI did not connect");
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("accept command peer: {error}"),
        }
    }
}

fn request(channel: &mut Channel<Response, Request>) -> RequestType {
    channel
        .read_message_blocking_timeout(Some(Duration::from_secs(10)))
        .expect("CLI request")
        .request_type
        .expect("request variant")
}

fn full_upgrade(failing_worker: Option<u32>) -> std::process::Output {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("command.sock");
    let config = dir.path().join("config.toml");
    fs::write(
        &config,
        format!(
            "command_socket = {:?}\ncommand_buffer_size = 16384\nmax_command_buffer_size = 163840\nlog_target = \"stdout\"\n",
            socket.to_str().expect("socket path")
        ),
    )
    .expect("config");
    let listener = UnixListener::bind(&socket).expect("command listener");
    listener.set_nonblocking(true).expect("nonblocking accept");
    let peer = thread::spawn(move || {
        let mut main = accept(&listener);
        assert!(matches!(request(&mut main), RequestType::UpgradeMain(_)));
        main.write_message(&Response::new(
            ResponseStatus::Ok,
            "main upgraded".into(),
            None,
        ))
        .expect("main response");
        let mut reconnected = accept(&listener);
        assert!(matches!(
            request(&mut reconnected),
            RequestType::ListWorkers(_)
        ));
        reconnected
            .write_message(&Response::new(
                ResponseStatus::Ok,
                "workers".into(),
                Some(ResponseContent {
                    content_type: Some(ContentType::Workers(WorkerInfos {
                        vec: [
                            RunState::Running,
                            RunState::NotAnswering,
                            RunState::Stopping,
                            RunState::Stopped,
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(id, run_state)| WorkerInfo {
                            id: id as u32,
                            pid: 100 + id as i32,
                            run_state: run_state as i32,
                        })
                        .collect(),
                    })),
                }),
            ))
            .expect("worker list");
        let mut upgraded = Vec::new();
        for _ in 0..2 {
            let mut worker = accept(&listener);
            let RequestType::UpgradeWorker(id) = request(&mut worker) else {
                panic!("expected worker upgrade");
            };
            upgraded.push(id);
            let (status, message) = if failing_worker == Some(id) {
                (ResponseStatus::Failure, "new worker activation refused")
            } else {
                (ResponseStatus::Ok, "worker upgraded")
            };
            worker
                .write_message(&Response::new(status, message.into(), None))
                .expect("worker response");
        }
        upgraded.sort_unstable();
        assert_eq!(
            upgraded,
            [0, 1],
            "attempt every active worker, skip draining workers"
        );
    });
    let output = Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args([
            "-c",
            config.to_str().expect("config path"),
            "-t",
            "2000",
            "upgrade",
        ])
        .output()
        .expect("run CLI");
    peer.join().expect("scripted command peer");
    output
}

#[test]
fn full_upgrade_fails_if_any_worker_upgrade_fails() {
    let output = full_upgrade(Some(0));
    assert!(
        !output.status.success(),
        "partial upgrade must fail: {output:?}"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("new worker activation refused"));
}

#[test]
fn full_upgrade_succeeds_when_every_active_worker_upgrades() {
    let output = full_upgrade(None);
    assert!(output.status.success(), "successful upgrade: {output:?}");
}
