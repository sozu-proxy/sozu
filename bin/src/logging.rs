use env;
use libc;
use time;
use log::{LogLevel,LogLevelFilter,LogRecord};
use env_logger::LogBuilder;

pub fn setup(level: Option<String>) {
  let pid = unsafe { libc::getpid() };
  let format = move |record: &LogRecord| {
    match record.level() {
    LogLevel::Debug | LogLevel::Trace => format!("{}\t{}\t{}\t{}\t{}\t|\t{}",
      time::now_utc().rfc3339(), time::precise_time_ns(), pid,
      record.level(), record.args(), record.location().module_path()),
    _ => format!("{}\t{}\t{}\t{}\t{}",
      time::now_utc().rfc3339(), time::precise_time_ns(), pid,
      record.level(), record.args())

    }
  };

  let mut builder = LogBuilder::new();
  builder.format(format).filter(None, LogLevelFilter::Info);

  if let Ok(log_level) = env::var("RUST_LOG") {
    builder.parse(&log_level);
  } else if let Some(log_level) = level {
    // We set the env variable so every worker can access it
    env::set_var("RUST_LOG", &log_level);
    builder.parse(&log_level);
  }

  builder.init().unwrap();
}
