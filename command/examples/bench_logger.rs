//! This file is used to bench the logger of Sozu by logging random lines.
//! You can change the number of logs, log target, log filter and if they are colored with env
//! variables.

#[macro_use]
extern crate sozu_command_lib;

use std::time::Instant;

use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};

use sozu_command_lib::logging::setup_logging;

struct LogLine {
    arg0: u32,
    arg1: u32,
    arg2: u32,
    log_level: u32,
}

#[allow(dead_code)]
fn random_string(rng: &mut StdRng) -> String {
    (0..10)
        .map(|_| rng.sample(rand::distributions::Alphanumeric) as char)
        .collect()
}

fn random_log_line(rng: &mut StdRng) -> LogLine {
    LogLine {
        arg0: rng.next_u32(),
        arg1: rng.next_u32(),
        arg2: rng.next_u32(),
        log_level: rng.next_u32(),
    }
}

fn main() {
    let mut rng: StdRng = SeedableRng::seed_from_u64(54321);

    let n: usize = std::env::var("BENCH_LOG_ITERS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1000);
    let colored = std::env::var("BENCH_LOG_COLOR")
        .map(|v| v == "true")
        .unwrap_or(false);
    let pre_generate = std::env::var("BENCH_LOG_PREGEN")
        .map(|v| v == "true")
        .unwrap_or(false);
    let target = std::env::var("BENCH_LOG_TARGET").unwrap_or("stdout".to_string());
    let filter = std::env::var("BENCH_LOG_FILTER").unwrap_or("trace".to_string());

    eprintln!(
        "n={n}, pre_generate={pre_generate}, target={target}, colored={colored}, filter={filter}"
    );
    setup_logging(&target, colored, None, None, None, &filter, "WRK-01");

    let mut pre_generated_log_iterator;
    let mut log_iterator = std::iter::repeat(())
        .take(n)
        .map(|_| random_log_line(&mut rng));

    let log_iterator = if pre_generate {
        // the pre generated iterator allocates all LogLines to avoid generating them on the fly
        // generation takes between 15% and 20% of the time
        pre_generated_log_iterator = log_iterator.collect::<Vec<_>>().into_iter();
        &mut pre_generated_log_iterator as &mut dyn Iterator<Item = LogLine>
    } else {
        &mut log_iterator as &mut dyn Iterator<Item = LogLine>
    };

    let start = Instant::now();
    for LogLine {
        arg0,
        arg1,
        arg2,
        log_level,
    } in log_iterator.into_iter()
    {
        match log_level % 5 {
            0 => error!(
                "first argument: {}, second: {}, third: {}",
                arg0, arg1, arg2
            ),
            1 => warn!(
                "first argument: {}, second: {}, third: {}",
                arg0, arg1, arg2
            ),
            2 => info!(
                "first argument: {}, second: {}, third: {}",
                arg0, arg1, arg2
            ),
            3 => debug!(
                "first argument: {}, second: {}, third: {}",
                arg0, arg1, arg2
            ),
            _ => trace!(
                "first argument: {}, second: {}, third: {}",
                arg0,
                arg1,
                arg2
            ),
        }
    }
    eprintln!("logs took {:?}", start.elapsed());
}
