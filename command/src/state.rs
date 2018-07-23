use std::collections::{BTreeMap,HashMap,HashSet,BTreeSet};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use std::iter::FromIterator;
use certificate::calculate_fingerprint;

use messages::{Application,CertFingerprint,CertificateAndKey,Order,
  HttpFront,HttpsFront,TcpFront,Backend,QueryAnswerApplication,
  AddCertificate, RemoveCertificate, RemoveBackend};

pub type AppId = String;

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
pub struct HttpProxy {
  ip_address: String,
  port:       u16,
  fronts:     HashMap<AppId, Vec<HttpFront>>,
  backends:   HashMap<AppId, Vec<Backend>>,
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize, Deserialize)]
pub struct HttpsProxy {
  ip_address:   String,
  port:         u16,
  certificates: HashMap<CertFingerprint, CertificateAndKey>,
  fronts:       HashMap<AppId, Vec<HttpsFront>>,
  backends:     HashMap<AppId, Vec<Backend>>,
}

#[derive(Debug,Default,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct ConfigState {
  pub applications:    HashMap<AppId, Application>,
  pub backends:        HashMap<AppId, Vec<Backend>>,
  pub http_fronts:     HashMap<AppId, Vec<HttpFront>>,
  pub https_fronts:    HashMap<AppId, Vec<HttpsFront>>,
  pub tcp_fronts:      HashMap<AppId, Vec<TcpFront>>,
  // certificate and names
  pub certificates:    HashMap<CertFingerprint, (CertificateAndKey, Vec<String>)>,
  //ip, port
  pub http_addresses:  Vec<(String, u16)>,
  pub https_addresses: Vec<(String, u16)>,
  //tcp:
}

impl ConfigState {
  pub fn new() -> ConfigState {
    ConfigState {
      applications:    HashMap::new(),
      backends:        HashMap::new(),
      http_fronts:     HashMap::new(),
      https_fronts:    HashMap::new(),
      tcp_fronts:      HashMap::new(),
      certificates:    HashMap::new(),
      http_addresses:  Vec::new(),
      https_addresses: Vec::new(),
    }
  }

  pub fn add_http_address(&mut self, ip_address: String, port: u16) {
    self.http_addresses.push((ip_address, port))
  }

  pub fn add_https_address(&mut self, ip_address: String, port: u16) {
    self.https_addresses.push((ip_address, port))
  }

  /// returns true if the order modified something
  pub fn handle_order(&mut self, order: &Order) -> bool {
    match order {
      &Order::AddApplication(ref application) => {
        let app = application.clone();
        self.applications.insert(app.app_id.clone(), app);
        true
      },
      &Order::RemoveApplication(ref app_id) => {
        self.applications.remove(app_id).is_some()
      },
      &Order::AddHttpFront(ref front) => {
        let front_vec = self.http_fronts.entry(front.app_id.clone()).or_insert(vec!());
        if !front_vec.contains(front) {
          front_vec.push(front.clone());
          true
        } else {
          false
        }
      },
      &Order::RemoveHttpFront(ref front) => {
        if let Some(front_list) = self.http_fronts.get_mut(&front.app_id) {
          let len = front_list.len();
          front_list.retain(|el| el.hostname != front.hostname || el.path_begin != front.path_begin);

          front_list.len() != len
        } else {
          false
        }
      },
      &Order::AddCertificate(ref add) => {
        let fingerprint = match calculate_fingerprint(&add.certificate.certificate.as_bytes()[..]) {
          Some(f)  => CertFingerprint(f),
          None => {
            error!("cannot obtain the certificate's fingerprint");
            return false;
          }
        };

        if !self.certificates.contains_key(&fingerprint) {
          self.certificates.insert(fingerprint.clone(), (add.certificate.clone(), add.names.clone()));
          true
        } else {
          false
        }
      },
      &Order::RemoveCertificate(ref remove) => {
        self.certificates.remove(&remove.fingerprint).is_some()
      },
      &Order::ReplaceCertificate(ref replace) => {
        let changed = self.certificates.remove(&replace.old_fingerprint).is_some();

        let fingerprint = match calculate_fingerprint(&replace.new_certificate.certificate.as_bytes()[..]) {
          Some(f)  => CertFingerprint(f),
          None => {
            error!("cannot obtain the certificate's fingerprint");
            return changed;
          }
        };

        if !self.certificates.contains_key(&fingerprint) {
          self.certificates.insert(fingerprint.clone(),
            (replace.new_certificate.clone(), replace.new_names.clone()));
          true
        } else {
          changed
        }
      },
      &Order::AddHttpsFront(ref front) => {
        let front_vec = self.https_fronts.entry(front.app_id.clone()).or_insert(vec!());
        if !front_vec.contains(front) {
          front_vec.push(front.clone());
          true
        } else {
          false
        }
      },
      &Order::RemoveHttpsFront(ref front) => {
        if let Some(front_list) = self.https_fronts.get_mut(&front.app_id) {
          let len = front_list.len();
          front_list.retain(|el| el.hostname != front.hostname || el.path_begin != front.path_begin);
          front_list.len() != len
        } else {
          false
        }
      },
      &Order::AddTcpFront(ref front) => {
        let front_vec = self.tcp_fronts.entry(front.app_id.clone()).or_insert(vec!());
        if !front_vec.contains(front) {
          front_vec.push(front.clone());
          true
        } else {
          false
        }
      },
      &Order::RemoveTcpFront(ref front) => {
        if let Some(front_list) = self.tcp_fronts.get_mut(&front.app_id) {
          let len = front_list.len();
          front_list.retain(|el| el.ip_address != front.ip_address || el.port != front.port);
          front_list.len() != len
        } else {
          false
        }
      },
      &Order::AddBackend(ref backend)  => {
        let backend_vec = self.backends.entry(backend.app_id.clone()).or_insert(vec!());
        if !backend_vec.contains(&backend) {
          backend_vec.push(backend.clone());
          true
        } else {
          false
        }
      },
      &Order::RemoveBackend(ref backend) => {
        if let Some(backend_list) = self.backends.get_mut(&backend.app_id) {
          let len = backend_list.len();
          backend_list.retain(|el| el.ip_address != backend.ip_address || el.port != backend.port);
          backend_list.len() != len
        } else {
          false
        }
      },
      // This is to avoid the error message
      &Order::Logging(_) | &Order::Status => {false},
      o => {
        error!("state cannot handle order message: {:#?}", o);
        false
      }
    }
  }

  pub fn generate_orders(&self) -> Vec<Order> {
    let mut v = Vec::new();

    for app in self.applications.values() {
      v.push(Order::AddApplication(app.clone()));
    }

    for front_list in self.http_fronts.values() {
      for front in front_list {
        v.push(Order::AddHttpFront(front.clone()));
      }
    }

    for &(ref certificate_and_key, ref names) in self.certificates.values() {
      v.push(Order::AddCertificate(AddCertificate{
        certificate: certificate_and_key.clone(),
        names: names.clone(),
      }));
    }

    for front_list in self.https_fronts.values() {
      for front in front_list {
        v.push(Order::AddHttpsFront(front.clone()));
      }
    }

    for front_list in self.tcp_fronts.values() {
      for front in front_list {
        v.push(Order::AddTcpFront(front.clone()));
      }
    }

    for backend_list in self.backends.values() {
      for backend in backend_list {
        v.push(Order::AddBackend(backend.clone()));
      }
    }

    v
  }

  pub fn diff(&self, other:&ConfigState) -> Vec<Order> {
    let my_apps: HashSet<&AppId>    = self.applications.keys().collect();
    let their_apps: HashSet<&AppId> = other.applications.keys().collect();

    let removed_apps = my_apps.difference(&their_apps);
    let added_apps: Vec<&Application> = their_apps.difference(&my_apps).filter_map(|app_id| other.applications.get(app_id.as_str())).collect();

    let mut my_fronts: HashSet<(&AppId, &HttpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in self.http_fronts.iter() {
      for ref front in front_list.iter() {
        my_fronts.insert((&app_id, &front));
      }
    }
    let mut their_fronts: HashSet<(&AppId, &HttpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in other.http_fronts.iter() {
      for ref front in front_list.iter() {
        their_fronts.insert((&app_id, &front));
      }
    }

    let removed_http_fronts = my_fronts.difference(&their_fronts);
    let added_http_fronts   = their_fronts.difference(&my_fronts);

    let mut my_fronts: HashSet<(&AppId, &HttpsFront)> = HashSet::new();
    for (ref app_id, ref front_list) in self.https_fronts.iter() {
      for ref front in front_list.iter() {
        my_fronts.insert((&app_id, &front));
      }
    }
    let mut their_fronts: HashSet<(&AppId, &HttpsFront)> = HashSet::new();
    for (ref app_id, ref front_list) in other.https_fronts.iter() {
      for ref front in front_list.iter() {
        their_fronts.insert((&app_id, &front));
      }
    }

    let removed_https_fronts = my_fronts.difference(&their_fronts);
    let added_https_fronts   = their_fronts.difference(&my_fronts);

    let mut my_fronts: HashSet<(&AppId, &TcpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in self.tcp_fronts.iter() {
      for ref front in front_list.iter() {
        my_fronts.insert((&app_id, &front));
      }
    }
    let mut their_fronts: HashSet<(&AppId, &TcpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in other.tcp_fronts.iter() {
      for ref front in front_list.iter() {
        their_fronts.insert((&app_id, &front));
      }
    }

    let removed_tcp_fronts = my_fronts.difference(&their_fronts);
    let added_tcp_fronts   = their_fronts.difference(&my_fronts);

    let mut my_backends: HashSet<(&AppId, &Backend)> = HashSet::new();
    for (ref app_id, ref backend_list) in self.backends.iter() {
      for ref backend in backend_list.iter() {
        my_backends.insert((&app_id, &backend));
      }
    }
    let mut their_backends: HashSet<(&AppId, &Backend)> = HashSet::new();
    for (ref app_id, ref backend_list) in other.backends.iter() {
      for ref backend in backend_list.iter() {
        their_backends.insert((&app_id, &backend));
      }
    }

    let removed_backends = my_backends.difference(&their_backends);
    let added_backends   = their_backends.difference(&my_backends);

    let my_certificates:    HashSet<(&CertFingerprint, &(CertificateAndKey, Vec<String>))> =
      HashSet::from_iter(self.certificates.iter());
    let their_certificates: HashSet<(&CertFingerprint, &(CertificateAndKey, Vec<String>))> =
      HashSet::from_iter(other.certificates.iter());

    let removed_certificates = my_certificates.difference(&their_certificates);
    let added_certificates   = their_certificates.difference(&my_certificates);

    let mut v = vec!();

    for app_id in removed_apps {
      v.push(Order::RemoveApplication(app_id.to_string()));
    }

    for app in added_apps {
      v.push(Order::AddApplication(app.clone()));
    }

    for &(_, &(ref certificate_and_key, ref names)) in added_certificates {
      v.push(Order::AddCertificate(AddCertificate{
        certificate: certificate_and_key.clone(),
        names: names.clone(),
      }));
    }

    for &(_, front) in removed_http_fronts {
     v.push(Order::RemoveHttpFront(front.clone()));
    }

    for &(_, front) in removed_https_fronts {
     v.push(Order::RemoveHttpsFront(front.clone()));
    }

    for &(_, front) in removed_tcp_fronts {
     v.push(Order::RemoveTcpFront(front.clone()));
    }

    for &(_, backend) in added_backends {
      v.push(Order::AddBackend(backend.clone()));
    }

    for &(_, backend) in removed_backends {
      v.push(Order::RemoveBackend(RemoveBackend{
        app_id: backend.app_id.clone(),
        backend_id: backend.backend_id.clone(),
        ip_address: backend.ip_address.clone(),
        port: backend.port,
      }));
    }

    for &(_, front) in added_http_fronts {
      v.push(Order::AddHttpFront(front.clone()));
    }

    for &(_, front) in added_https_fronts {
      v.push(Order::AddHttpsFront(front.clone()));
    }

    for &(_, front) in added_tcp_fronts {
      v.push(Order::AddTcpFront(front.clone()));
    }

    for  &(fingerprint, _) in removed_certificates {
      v.push(Order::RemoveCertificate(RemoveCertificate {
        fingerprint: fingerprint.clone(),
        names: Vec::new(),
      }));
    }
    v
  }

  pub fn hash_state(&self) -> BTreeMap<AppId, u64> {
    self.applications.keys().map(|app_id| {
      let mut s = DefaultHasher::new();
      self.applications.get(app_id).hash(&mut s);
      self.backends.get(app_id).map(|ref v| v.iter().collect::<BTreeSet<_>>().hash(&mut s));
      self.http_fronts.get(app_id).map(|ref v| v.iter().collect::<BTreeSet<_>>().hash(&mut s));
      self.https_fronts.get(app_id).map(|ref v| v.iter().collect::<BTreeSet<_>>().hash(&mut s));
      self.tcp_fronts.get(app_id).map(|ref v| v.iter().collect::<BTreeSet<_>>().hash(&mut s));

      (app_id.to_string(), s.finish())

    }).collect()
  }

  pub fn application_state(&self, app_id: &str) -> QueryAnswerApplication {
    QueryAnswerApplication {
      configuration:   self.applications.get(app_id).cloned(),
      http_frontends:  self.http_fronts.get(app_id).cloned().unwrap_or(vec!()),
      https_frontends: self.https_fronts.get(app_id).cloned().unwrap_or(vec!()),
      tcp_frontends:   self.tcp_fronts.get(app_id).cloned().unwrap_or(vec!()),
      backends:        self.backends.get(app_id).cloned().unwrap_or(vec!()),
    }
  }

  pub fn count_backends(&self) -> usize {
    self.backends.values().fold(0, |acc, v| acc + v.len())
  }

  pub fn count_frontends(&self) -> usize {
    self.http_fronts.values().fold(0, |acc, v| acc + v.len()) +
    self.https_fronts.values().fold(0, |acc, v| acc + v.len()) +
    self.tcp_fronts.values().fold(0, |acc, v| acc + v.len())
  }
}

pub fn get_application_ids_by_domain(state: &ConfigState, hostname: String, path_begin: Option<String>) -> HashSet<AppId> {
  let domain_check = |front_hostname: &str, front_path_begin: &str, hostname: &str, path_begin: &Option<String>| -> bool {
    let domain_matches = if hostname == front_hostname { true } else { false };
    if !domain_matches {
      return false;
    }

    match path_begin {
      &Some(ref path_begin) => if path_begin == front_path_begin { true } else { false }
      &None => true
    }
  };

  let mut app_ids: HashSet<AppId> = HashSet::new();

  state.http_fronts.values()
    .for_each(|fronts| {
      fronts
        .iter()
        .filter(|front| domain_check(&front.hostname, &front.path_begin, &hostname, &path_begin))
        .for_each(|front| { app_ids.insert(front.app_id.clone()); });
    });

  state.https_fronts.values()
    .for_each(|fronts| {
      fronts
        .iter()
        .filter(|front| domain_check(&front.hostname, &front.path_begin, &hostname, &path_begin))
        .for_each(|front| { app_ids.insert(front.app_id.clone()); });
    });

  app_ids
}

#[cfg(test)]
mod tests {
  use super::*;
  use config::LoadBalancingAlgorithms;
  use messages::{Order,HttpFront,Backend,LoadBalancingParams};

  #[test]
  fn serialize() {
    let mut state:ConfigState = Default::default();
    state.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/") }));
    state.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc") }));
    state.handle_order(&Order::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-0"), ip_address: String::from("127.0.0.1"), port: 1026, load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state.handle_order(&Order::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-1"), ip_address: String::from("127.0.0.2"), port: 1027, load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state.handle_order(&Order::AddBackend(Backend { app_id: String::from("app_2"), backend_id: String::from("app_2-0"), ip_address: String::from("192.167.1.2"), port: 1026, load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state.handle_order(&Order::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-3"),ip_address: String::from("192.168.1.3"), port: 1027, load_balancing_parameters: Some(LoadBalancingParams::default()) , sticky_id: None, backup: None }));
    state.handle_order(&Order::RemoveBackend(RemoveBackend { app_id: String::from("app_1"), backend_id: String::from("app_1-3"), ip_address: String::from("192.168.1.3"), port: 1027 }));

    /*
    let encoded = state.encode();
    println!("serialized:\n{}", encoded);

    let new_state: Option<HttpProxy> = decode_str(&encoded);
    println!("deserialized:\n{:?}", new_state);
    assert_eq!(new_state, Some(state));
    */
    //assert!(false);
  }

  #[test]
  fn diff() {
    let mut state:ConfigState = Default::default();
    state.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/") }));
    state.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc") }));
    state.handle_order(&Order::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-0"), ip_address: String::from("127.0.0.1"), port: 1026, load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state.handle_order(&Order::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-1"), ip_address: String::from("127.0.0.2"), port: 1027, load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state.handle_order(&Order::AddBackend(Backend { app_id: String::from("app_2"), backend_id: String::from("app_2-0"), ip_address: String::from("192.167.1.2"), port: 1026, load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state.handle_order(&Order::AddApplication(Application { app_id: String::from("app_2"), sticky_session: true, https_redirect: true, proxy_protocol: None, load_balancing_policy: LoadBalancingAlgorithms::RoundRobin }));

    let mut state2:ConfigState = Default::default();
    state2.handle_order(&Order::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/") }));
    state2.handle_order(&Order::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-0"), ip_address: String::from("127.0.0.1"), port: 1026, load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state2.handle_order(&Order::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-1"), ip_address: String::from("127.0.0.2"), port: 1027, load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state2.handle_order(&Order::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-2"), ip_address: String::from("127.0.0.2"), port: 1028, load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state2.handle_order(&Order::AddApplication(Application { app_id: String::from("app_3"), sticky_session: false, https_redirect: false, proxy_protocol: None, load_balancing_policy: LoadBalancingAlgorithms::RoundRobin }));

   let e = vec!(
     Order::RemoveHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc") }),
     Order::RemoveBackend(RemoveBackend { app_id: String::from("app_2"), backend_id: String::from("app_2-0"), ip_address: String::from("192.167.1.2"), port: 1026 }),
     Order::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-2"), ip_address: String::from("127.0.0.2"), port: 1028, load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None }),
     Order::RemoveApplication(String::from("app_2")),
     Order::AddApplication(Application { app_id: String::from("app_3"), sticky_session: false, https_redirect: false, proxy_protocol: None, load_balancing_policy: LoadBalancingAlgorithms::RoundRobin }),
   );
   let expected_diff:HashSet<&Order> = HashSet::from_iter(e.iter());

   let d = state.diff(&state2);
   let diff = HashSet::from_iter(d.iter());
   println!("diff orders:\n{:#?}\n", diff);
   println!("expected diff orders:\n{:#?}\n", expected_diff);

   let hash1 = state.hash_state();
   let hash2 = state2.hash_state();
   let mut state3 = state.clone();
   state3.handle_order(&Order::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-2"), ip_address: String::from("127.0.0.2"), port: 1028, load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None }));
   let hash3 = state3.hash_state();
   println!("state 1 hashes: {:#?}", hash1);
   println!("state 2 hashes: {:#?}", hash2);
   println!("state 3 hashes: {:#?}", hash3);

   assert_eq!(diff, expected_diff);
  }

  #[test]
  fn application_ids_by_domain() {
    let mut config = ConfigState::new();
    let http_front_app1 = HttpFront {
      app_id: String::from("MyApp_1"),
      hostname: String::from("lolcatho.st"),
      path_begin: String::from("")
    };

    let https_front_app1 = HttpsFront {
      app_id: String::from("MyApp_1"),
      hostname: String::from("lolcatho.st"),
      path_begin: String::from(""),
      fingerprint: CertFingerprint(vec!(0x00))
    };

    let http_front_app2 = HttpFront {
      app_id: String::from("MyApp_2"),
      hostname: String::from("lolcatho.st"),
      path_begin: String::from("/api")
    };

    let https_front_app2 = HttpsFront {
      app_id: String::from("MyApp_2"),
      hostname: String::from("lolcatho.st"),
      path_begin: String::from("/api"),
      fingerprint: CertFingerprint(vec!(0x00))
    };

    let add_http_front_order_app1 = Order::AddHttpFront(http_front_app1);
    let add_http_front_order_app2 = Order::AddHttpFront(http_front_app2);
    let add_https_front_order_app1 = Order::AddHttpsFront(https_front_app1);
    let add_https_front_order_app2 = Order::AddHttpsFront(https_front_app2);
    config.handle_order(&add_http_front_order_app1);
    config.handle_order(&add_http_front_order_app2);
    config.handle_order(&add_https_front_order_app1);
    config.handle_order(&add_https_front_order_app2);

    let mut app1_app2: HashSet<AppId> = HashSet::new();
    app1_app2.insert(String::from("MyApp_1"));
    app1_app2.insert( String::from("MyApp_2"));

    let mut app2: HashSet<AppId> = HashSet::new();
    app2.insert(String::from("MyApp_2"));

    let empty: HashSet<AppId> = HashSet::new();
    assert_eq!(get_application_ids_by_domain(&config, String::from("lolcatho.st"), None), app1_app2);
    assert_eq!(get_application_ids_by_domain(&config, String::from("lolcatho.st"), Some(String::from("/api"))), app2);
    assert_eq!(get_application_ids_by_domain(&config, String::from("lolcathost"), None), empty);
    assert_eq!(get_application_ids_by_domain(&config, String::from("lolcathost"), Some(String::from("/sozu"))), empty);
  }
}
