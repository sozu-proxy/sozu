use clap::{App,Arg,SubCommand,ArgMatches};
use crate::worker::begin_worker_process;
use crate::upgrade::begin_new_main_process;

pub fn init<'a>() -> ArgMatches<'a> {
  App::new("sozu")
    .version(crate_version!())
    .about("hot reconfigurable proxy")
    .subcommand(SubCommand::with_name("start")
                .about("launch the main process")
                .arg(Arg::with_name("config")
                    .short("c")
                    .long("config")
                    .value_name("FILE")
                    .help("Sets a custom config file")
                    .takes_value(true)
                    .required(option_env!("SOZU_CONFIG").is_none())))
    .subcommand(SubCommand::with_name("worker")
                .about("start a worker (internal command, should not be used directly)")
                .arg(Arg::with_name("id").long("id")
                     .takes_value(true).required(true).help("worker identifier"))
                .arg(Arg::with_name("fd").long("fd")
                     .takes_value(true).required(true).help("IPC file descriptor"))
                .arg(Arg::with_name("scm").long("scm")
                     .takes_value(true).required(true).help("IPC SCM_RIGHTS file descriptor"))
                .arg(Arg::with_name("configuration-state-fd").long("configuration-state-fd")
                     .takes_value(true).required(true).help("configuration data file descriptor"))
                .arg(Arg::with_name("command-buffer-size").long("command-buffer-size")
                     .takes_value(true).required(true).help("Worker's channel buffer size"))
                .arg(Arg::with_name("max-command-buffer-size").long("max-command-buffer-size")
                     .takes_value(true).required(true).help("Worker's channel max buffer size")))
    .subcommand(SubCommand::with_name("upgrade")
                .about("start a new main process (internal command, should not be used directly)")
                .arg(Arg::with_name("fd").long("fd")
                     .takes_value(true).required(true).help("IPC file descriptor"))
                .arg(Arg::with_name("upgrade-fd").long("upgrade-fd")
                     .takes_value(true).required(true).help("upgrade data file descriptor"))
                .arg(Arg::with_name("command-buffer-size").long("command-buffer-size")
                     .takes_value(true).required(true).help("Main process command buffer size"))
                .arg(Arg::with_name("max-command-buffer-size").long("max-command-buffer-size")
                     .takes_value(true).required(false).help("Main process max command buffer size")))
    .get_matches()
}

pub fn get_fd<'a>(matches: &ArgMatches<'a>) -> i32 {
  matches.value_of("fd").expect("needs a file descriptor")
    .parse::<i32>().expect("the file descriptor must be a number")
}

pub fn get_scm<'a>(matches: &ArgMatches<'a>) -> i32 {
  matches.value_of("scm").expect("needs a file descriptor")
    .parse::<i32>().expect("the SCM_RIGHTS file descriptor must be a number")
}

pub fn get_configuration_state_fd<'a>(matches: &ArgMatches<'a>) -> i32 {
  matches.value_of("configuration-state-fd")
    .expect("needs a configuration state file descriptor")
    .parse::<i32>().expect("the file descriptor must be a number")
}

pub fn get_id<'a>(matches: &ArgMatches<'a>) -> i32 {
  matches.value_of("id").expect("needs a worker id")
    .parse::<i32>().expect("the worker id must be a number")
}

pub fn get_buffer_size<'a>(matches: &ArgMatches<'a>) -> usize {
  matches.value_of("command-buffer-size")
    .and_then(|size| size.parse::<usize>().ok())
    .unwrap_or(1_000_000)
}

pub fn get_max_buffer_size<'a>(matches: &ArgMatches<'a>, buffer_size: usize) -> usize {
  matches.value_of("max-command-buffer-size")
    .and_then(|size| size.parse::<usize>().ok())
    .unwrap_or(buffer_size * 2)
}

pub fn get_upgrade_fd<'a>(matches: &ArgMatches<'a>) -> i32 {
  matches.value_of("upgrade-fd").expect("needs an upgrade file descriptor")
    .parse::<i32>().expect("the file descriptor must be a number")
}

pub fn upgrade_worker<'a>(matches: &ArgMatches<'a>) -> Option<()> {
  matches.subcommand_matches("worker").and_then(|worker_matches| {
    let fd = get_fd(worker_matches);
    let scm = get_scm(worker_matches);
    let configuration_state_fd = get_configuration_state_fd(worker_matches);
    let id = get_id(worker_matches);
    let buffer_size = get_buffer_size(worker_matches);
    let max_buffer_size = get_max_buffer_size(worker_matches, buffer_size);

    begin_worker_process(fd, scm, configuration_state_fd, id, buffer_size, max_buffer_size);
    Some(())
  })
}

pub fn upgrade_main<'a>(matches: &ArgMatches<'a>) -> Option<()> {
 matches.subcommand_matches("upgrade").and_then(|upgrade_matches| {
    let fd = get_fd(upgrade_matches);
    let upgrade_fd = get_upgrade_fd(upgrade_matches);
    let buffer_size = get_buffer_size(upgrade_matches);
    let max_buffer_size = get_max_buffer_size(upgrade_matches, buffer_size);

    begin_new_main_process(fd, upgrade_fd, buffer_size, max_buffer_size);
    Some(())
  })
}
