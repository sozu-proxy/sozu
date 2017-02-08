use env;
use sozu::logging::Logger;

pub fn setup(level: Option<String>) {
  if let Ok(log_level) = env::var("RUST_LOG") {
    Logger::init(&log_level);
  } else if let Some(log_level) = level {
    // We set the env variable so every worker can access it
    env::set_var("RUST_LOG", &log_level);
    Logger::init(&log_level);
  }
}

