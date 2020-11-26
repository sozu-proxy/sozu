use std::env;
use std::sync::{Arc, Mutex};
use sozu_command::logging::{Logger,LoggerBackend,target_to_backend};

lazy_static! {
  pub static ref MAIN_LOGGER: Arc<Mutex<Logger>> = Arc::new(Mutex::new(Logger::new()));
  pub static ref TAG:    String          = {
      let logger= MAIN_LOGGER.lock().unwrap();
      (*logger).tag.clone()
  };
}

#[macro_export]
macro_rules! log {
    (__inner__ $target:expr, $lvl:expr, $format:expr, $level_tag:expr,
     [$($transformed_args:ident),*], [$first_ident:ident $(, $other_idents:ident)*], $first_arg:expr $(, $other_args:expr)*) => ({
      let $first_ident = &$first_arg;
      log!(__inner__ $target, $lvl, $format, $level_tag, [$($transformed_args,)* $first_ident], [$($other_idents),*] $(, $other_args)*);
    });

    (__inner__ $target:expr, $lvl:expr, $format:expr, $level_tag:expr,
     [$($final_args:ident),*], [$($idents:ident),*]) => ({
      static _META: $crate::sozu_command::logging::Metadata = $crate::sozu_command::logging::Metadata {
          level:  $lvl,
          target: module_path!(),
      };
      {
        let mut logger = $crate::logging::MAIN_LOGGER.lock().unwrap();
        let pid = (*logger).pid;
        let tag = &*$crate::logging::TAG;

        let (now, precise_time) = $crate::sozu_command::logging::now();
        (*logger).log(
            &_META,
            format_args!(
                concat!("{} {} {} {} {}\t", $format, '\n'),
                now, precise_time, pid, tag,
                $level_tag $(, $final_args)*)
            );
      }
    });
    ($lvl:expr, $format:expr, $level_tag:expr $(, $args:expr)+) => {
      log!(__inner__ module_path!(), $lvl, $format, $level_tag, [], [a,b,c,d,e,f,g,h,i,j,k,l,m,n,o,p,q,r,s,t,u,v]
                  $(, $args)+)
    };
    ($lvl:expr, $format:expr, $level_tag:expr) => {
      log!(__inner__ module_path!(), $lvl, $format, $level_tag, [], [a,b,c,d,e,f,g,h,i,j,k,l,m,n,o,p,q,r,s,t,u,v])
    };
}

pub fn init(tag: String, spec: &str, backend: LoggerBackend, access_backend: Option<LoggerBackend>) {
  let directives = crate::sozu_command::logging::parse_logging_spec(spec);
  let mut logger = MAIN_LOGGER.lock().unwrap();
  if !logger.initialized {
      println!("initializing logger");
      logger.set_directives(directives);
      logger.backend        = backend;
      logger.access_backend = access_backend;
      logger.tag            = tag;
      logger.pid            = unsafe { libc::getpid() };
      logger.initialized    = true;

      //let _ = log::set_logger(&crate::sozu_command::logging::COMPAT_LOGGER).map_err(|e| println!("could not register compat logger: {:?}", e));
      log::set_max_level(log::LevelFilter::Info);
  }
}

pub fn setup(tag: String, level: &str, target: &str, access_target: Option<&str>) {
  let backend = target_to_backend(target);
  let access_backend = access_target.map(target_to_backend);

  if let Ok(log_level) = env::var("RUST_LOG") {
    init(tag, &log_level, backend, access_backend);
  } else {
    // We set the env variable so every worker can access it
    env::set_var("RUST_LOG", level);
    init(tag, level, backend, access_backend);
  }
  //initialize TAG here to avoid deadlocks
  let _ = &*TAG;
}

