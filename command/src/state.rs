use std::collections::{BTreeMap,HashMap,HashSet,BTreeSet};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::iter::{repeat,FromIterator};
use certificate::calculate_fingerprint;

use proxy::{Application,CertFingerprint,CertificateAndKey,ProxyRequestData,
  HttpFront,TcpFront,Backend,QueryAnswerApplication,
  AddCertificate, RemoveCertificate, RemoveBackend,
  HttpListener,HttpsListener,TcpListener,ListenerType,
  ActivateListener,RemoveListener};

pub type AppId = String;

#[derive(Debug,Clone,PartialEq,Eq, Serialize, Deserialize)]
pub struct HttpProxy {
  address:  SocketAddr,
  fronts:   HashMap<AppId, Vec<HttpFront>>,
  backends: HashMap<AppId, Vec<Backend>>,
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize, Deserialize)]
pub struct HttpsProxy {
  address:      SocketAddr,
  certificates: HashMap<CertFingerprint, CertificateAndKey>,
  fronts:       HashMap<AppId, Vec<HttpFront>>,
  backends:     HashMap<AppId, Vec<Backend>>,
}

#[derive(Debug,Default,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct ConfigState {
  pub applications:    HashMap<AppId, Application>,
  pub backends:        HashMap<AppId, Vec<Backend>>,
  pub http_listeners:  HashMap<SocketAddr, (HttpListener, bool)>,
  pub https_listeners: HashMap<SocketAddr, (HttpsListener, bool)>,
  pub tcp_listeners:   HashMap<SocketAddr, (TcpListener, bool)>,
  pub http_fronts:     HashMap<AppId, Vec<HttpFront>>,
  pub https_fronts:    HashMap<AppId, Vec<HttpFront>>,
  pub tcp_fronts:      HashMap<AppId, Vec<TcpFront>>,
  // certificate and names
  pub certificates:    HashMap<SocketAddr, HashMap<CertFingerprint, (CertificateAndKey, Vec<String>)>>,
  //ip, port
  pub http_addresses:  Vec<SocketAddr>,
  pub https_addresses: Vec<SocketAddr>,
  //tcp:
}

impl ConfigState {
  pub fn new() -> ConfigState {
    ConfigState {
      applications:    HashMap::new(),
      backends:        HashMap::new(),
      http_listeners:  HashMap::new(),
      https_listeners: HashMap::new(),
      tcp_listeners:   HashMap::new(),
      http_fronts:     HashMap::new(),
      https_fronts:    HashMap::new(),
      tcp_fronts:      HashMap::new(),
      certificates:    HashMap::new(),
      http_addresses:  Vec::new(),
      https_addresses: Vec::new(),
    }
  }

  pub fn add_http_address(&mut self, address: SocketAddr) {
    self.http_addresses.push(address)
  }

  pub fn add_https_address(&mut self, address: SocketAddr) {
    self.https_addresses.push(address)
  }

  /// returns true if the order modified something
  pub fn handle_order(&mut self, order: &ProxyRequestData) -> bool {
    match order {
      &ProxyRequestData::AddApplication(ref application) => {
        let app = application.clone();
        self.applications.insert(app.app_id.clone(), app);
        true
      },
      &ProxyRequestData::RemoveApplication(ref app_id) => {
        self.applications.remove(app_id).is_some()
      },
      &ProxyRequestData::AddHttpListener(ref listener) => {
        if self.http_listeners.contains_key(&listener.front) {
          false
        } else {
          self.http_listeners.insert(listener.front, (listener.clone(), false));
          true
        }
      },
      &ProxyRequestData::AddHttpsListener(ref listener) => {
        if self.https_listeners.contains_key(&listener.front) {
          false
        } else {
          self.https_listeners.insert(listener.front, (listener.clone(), false));
          true
        }
      },
      &ProxyRequestData::AddTcpListener(ref listener) => {
        if self.tcp_listeners.contains_key(&listener.front) {
          false
        } else {
          self.tcp_listeners.insert(listener.front, (listener.clone(), false));
          true
        }
      },
      &ProxyRequestData::RemoveListener(ref remove) => {
        match remove.proxy {
          ListenerType::HTTP =>  self.http_listeners.remove(&remove.front).is_some(),
          ListenerType::HTTPS => self.https_listeners.remove(&remove.front).is_some(),
          ListenerType::TCP =>   self.tcp_listeners.remove(&remove.front).is_some(),
        }
      },
      &ProxyRequestData::ActivateListener(ref activate) => {
        match activate.proxy {
          ListenerType::HTTP =>  self.http_listeners.get_mut(&activate.front).map(|t| t.1 = true).is_some(),
          ListenerType::HTTPS => self.https_listeners.get_mut(&activate.front).map(|t| t.1 = true).is_some(),
          ListenerType::TCP =>   self.tcp_listeners.get_mut(&activate.front).map(|t| t.1 = true).is_some(),
        }
      },
      &ProxyRequestData::DeactivateListener(ref deactivate) => {
        match deactivate.proxy {
          ListenerType::HTTP =>  self.http_listeners.get_mut(&deactivate.front).map(|t| t.1 = false).is_some(),
          ListenerType::HTTPS => self.https_listeners.get_mut(&deactivate.front).map(|t| t.1 = false).is_some(),
          ListenerType::TCP =>   self.tcp_listeners.get_mut(&deactivate.front).map(|t| t.1 = false).is_some(),
        }
      },
      &ProxyRequestData::AddHttpFront(ref front) => {
        let front_vec = self.http_fronts.entry(front.app_id.clone()).or_insert_with(Vec::new);
        if !front_vec.contains(front) {
          front_vec.push(front.clone());
          true
        } else {
          false
        }
      },
      &ProxyRequestData::RemoveHttpFront(ref front) => {
        if let Some(front_list) = self.http_fronts.get_mut(&front.app_id) {
          let len = front_list.len();
          front_list.retain(|el| el.hostname != front.hostname || el.path_begin != front.path_begin);

          front_list.len() != len
        } else {
          false
        }
      },
      &ProxyRequestData::AddCertificate(ref add) => {
        let fingerprint = match calculate_fingerprint(&add.certificate.certificate.as_bytes()[..]) {
          Some(f)  => CertFingerprint(f),
          None => {
            error!("cannot obtain the certificate's fingerprint");
            return false;
          }
        };

        if !self.certificates.contains_key(&add.front) {
          self.certificates.insert(add.front.clone(), HashMap::new());
        }
        if !self.certificates.get(&add.front).unwrap().contains_key(&fingerprint) {
          self.certificates.get_mut(&add.front).unwrap().insert(fingerprint.clone(), (add.certificate.clone(), add.names.clone()));
          true
        } else {
          false
        }
      },
      &ProxyRequestData::RemoveCertificate(ref remove) => {
        self.certificates.get_mut(&remove.front).and_then(|certs| certs.remove(&remove.fingerprint)).is_some()
      },
      &ProxyRequestData::ReplaceCertificate(ref replace) => {
        let changed = self.certificates.get_mut(&replace.front).and_then(|certs| certs.remove(&replace.old_fingerprint)).is_some();

        let fingerprint = match calculate_fingerprint(&replace.new_certificate.certificate.as_bytes()[..]) {
          Some(f)  => CertFingerprint(f),
          None => {
            error!("cannot obtain the certificate's fingerprint");
            return changed;
          }
        };

        if !self.certificates.get(&replace.front).unwrap().contains_key(&fingerprint) {
          self.certificates.get_mut(&replace.front).map(|certs| certs.insert(fingerprint.clone(),
            (replace.new_certificate.clone(), replace.new_names.clone())));
          true
        } else {
          changed
        }
      },
      &ProxyRequestData::AddHttpsFront(ref front) => {
        let front_vec = self.https_fronts.entry(front.app_id.clone()).or_insert_with(Vec::new);
        if !front_vec.contains(front) {
          front_vec.push(front.clone());
          true
        } else {
          false
        }
      },
      &ProxyRequestData::RemoveHttpsFront(ref front) => {
        if let Some(front_list) = self.https_fronts.get_mut(&front.app_id) {
          let len = front_list.len();
          front_list.retain(|el| el.hostname != front.hostname || el.path_begin != front.path_begin);
          front_list.len() != len
        } else {
          false
        }
      },
      &ProxyRequestData::AddTcpFront(ref front) => {
        let front_vec = self.tcp_fronts.entry(front.app_id.clone()).or_insert_with(Vec::new);
        if !front_vec.contains(front) {
          front_vec.push(front.clone());
          true
        } else {
          false
        }
      },
      &ProxyRequestData::RemoveTcpFront(ref front) => {
        if let Some(front_list) = self.tcp_fronts.get_mut(&front.app_id) {
          let len = front_list.len();
          front_list.retain(|el| el.address != front.address);
          front_list.len() != len
        } else {
          false
        }
      },
      &ProxyRequestData::AddBackend(ref backend)  => {
        let backend_vec = self.backends.entry(backend.app_id.clone()).or_insert_with(Vec::new);
        if !backend_vec.contains(&backend) {
          backend_vec.push(backend.clone());
          true
        } else {
          false
        }
      },
      &ProxyRequestData::RemoveBackend(ref backend) => {
        if let Some(backend_list) = self.backends.get_mut(&backend.app_id) {
          let len = backend_list.len();
          backend_list.retain(|el| el.address != backend.address);
          backend_list.len() != len
        } else {
          false
        }
      },
      // This is to avoid the error message
      &ProxyRequestData::Logging(_) | &ProxyRequestData::Status => {false},
      o => {
        error!("state cannot handle order message: {:#?}", o);
        false
      }
    }
  }

  pub fn generate_orders(&self) -> Vec<ProxyRequestData> {
    let mut v = Vec::new();

    for &(ref listener, _) in self.http_listeners.values() {
      v.push(ProxyRequestData::AddHttpListener(listener.clone()));
    }

    for &(ref listener, _) in self.https_listeners.values() {
      v.push(ProxyRequestData::AddHttpsListener(listener.clone()));
    }

    for &(ref listener, _) in self.tcp_listeners.values() {
      v.push(ProxyRequestData::AddTcpListener(listener.clone()));
    }

    for app in self.applications.values() {
      v.push(ProxyRequestData::AddApplication(app.clone()));
    }

    for front_list in self.http_fronts.values() {
      for front in front_list {
        v.push(ProxyRequestData::AddHttpFront(front.clone()));
      }
    }

    for (ref front, ref certs) in self.certificates.iter() {
      for &(ref certificate_and_key, ref names) in certs.values() {
        v.push(ProxyRequestData::AddCertificate(AddCertificate{
          front: **front,
          certificate: certificate_and_key.clone(),
          names: names.clone(),
        }));
      }
    }

    for front_list in self.https_fronts.values() {
      for front in front_list {
        v.push(ProxyRequestData::AddHttpsFront(front.clone()));
      }
    }

    for front_list in self.tcp_fronts.values() {
      for front in front_list {
        v.push(ProxyRequestData::AddTcpFront(front.clone()));
      }
    }

    for backend_list in self.backends.values() {
      for backend in backend_list {
        v.push(ProxyRequestData::AddBackend(backend.clone()));
      }
    }

    v
  }

  pub fn generate_activate_orders(&self) -> Vec<ProxyRequestData> {
    let mut v = Vec::new();
    for front in self.http_listeners.iter().filter(|(_,t)| t.1).map(|(k,_)| k) {
      v.push(ProxyRequestData::ActivateListener(ActivateListener {
        front: *front,
        proxy: ListenerType::HTTP,
        from_scm: false,
      }));
    }

    for front in self.https_listeners.iter().filter(|(_,t)| t.1).map(|(k,_)| k) {
      v.push(ProxyRequestData::ActivateListener(ActivateListener {
        front: *front,
        proxy: ListenerType::HTTPS,
        from_scm: false,
      }));
    }
    for front in self.tcp_listeners.iter().filter(|(_,t)| t.1).map(|(k,_)| k) {
      v.push(ProxyRequestData::ActivateListener(ActivateListener {
        front: *front,
        proxy: ListenerType::TCP,
        from_scm: false,
      }));
    }

    v
  }

  pub fn diff(&self, other:&ConfigState) -> Vec<ProxyRequestData> {
    let my_apps: HashSet<&AppId>    = self.applications.keys().collect();
    let their_apps: HashSet<&AppId> = other.applications.keys().collect();

    let removed_apps = my_apps.difference(&their_apps);
    let added_apps: Vec<&Application> = their_apps.difference(&my_apps).filter_map(|app_id| other.applications.get(app_id.as_str())).collect();

    //pub tcp_listeners:   HashMap<SocketAddr, (TcpListener, bool)>,
    let my_tcp_listeners: HashSet<&SocketAddr> = self.tcp_listeners.keys().collect();
    let their_tcp_listeners: HashSet<&SocketAddr> = self.tcp_listeners.keys().collect();
    let removed_tcp_listeners = my_tcp_listeners.difference(&their_tcp_listeners);
    let added_tcp_listeners: Vec<&SocketAddr> = their_tcp_listeners.difference(&my_tcp_listeners)
      .cloned().collect();

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

    let mut my_fronts: HashSet<(&AppId, &HttpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in self.https_fronts.iter() {
      for ref front in front_list.iter() {
        my_fronts.insert((&app_id, &front));
      }
    }
    let mut their_fronts: HashSet<(&AppId, &HttpFront)> = HashSet::new();
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

    let my_certificates:    HashSet<(SocketAddr, &CertFingerprint, &(CertificateAndKey, Vec<String>))> =
      HashSet::from_iter(self.certificates.iter().flat_map(|(addr, certs)| {
        certs.iter().zip(repeat(*addr)).map(|((k, v), addr)| (addr, k, v))
      }));
    let their_certificates: HashSet<(SocketAddr, &CertFingerprint, &(CertificateAndKey, Vec<String>))> =
      HashSet::from_iter(other.certificates.iter().flat_map(|(addr, certs)| {
        certs.iter().zip(repeat(*addr)).map(|((k, v), addr)| (addr, k, v))
      }));

    let removed_certificates = my_certificates.difference(&their_certificates);
    let added_certificates   = their_certificates.difference(&my_certificates);

    let mut v = vec!();

    for address in removed_tcp_listeners {
      v.push(ProxyRequestData::RemoveListener(RemoveListener {
        front: **address,
        proxy: ListenerType::TCP
      }));
    }

    for address in added_tcp_listeners {
      v.push(ProxyRequestData::AddTcpListener(other.tcp_listeners[address].0.clone()));
    }

    for app_id in removed_apps {
      v.push(ProxyRequestData::RemoveApplication(app_id.to_string()));
    }

    for app in added_apps {
      v.push(ProxyRequestData::AddApplication(app.clone()));
    }

    for &(front, _, &(ref certificate_and_key, ref names)) in added_certificates {
      v.push(ProxyRequestData::AddCertificate(AddCertificate{
        front,
        certificate: certificate_and_key.clone(),
        names: names.clone(),
      }));
    }

    for &(_, front) in removed_http_fronts {
     v.push(ProxyRequestData::RemoveHttpFront(front.clone()));
    }

    for &(_, front) in removed_https_fronts {
     v.push(ProxyRequestData::RemoveHttpsFront(front.clone()));
    }

    for &(_, front) in removed_tcp_fronts {
     v.push(ProxyRequestData::RemoveTcpFront(front.clone()));
    }

    for &(_, backend) in added_backends {
      v.push(ProxyRequestData::AddBackend(backend.clone()));
    }

    for &(_, backend) in removed_backends {
      v.push(ProxyRequestData::RemoveBackend(RemoveBackend{
        app_id: backend.app_id.clone(),
        backend_id: backend.backend_id.clone(),
        address:    backend.address,
      }));
    }

    for &(_, front) in added_http_fronts {
      v.push(ProxyRequestData::AddHttpFront(front.clone()));
    }

    for &(_, front) in added_https_fronts {
      v.push(ProxyRequestData::AddHttpsFront(front.clone()));
    }

    for &(_, front) in added_tcp_fronts {
      v.push(ProxyRequestData::AddTcpFront(front.clone()));
    }

    for  &(front, fingerprint, _) in removed_certificates {
      v.push(ProxyRequestData::RemoveCertificate(RemoveCertificate {
        front,
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
      http_frontends:  self.http_fronts.get(app_id).cloned().unwrap_or_else(Vec::new),
      https_frontends: self.https_fronts.get(app_id).cloned().unwrap_or_else(Vec::new),
      tcp_frontends:   self.tcp_fronts.get(app_id).cloned().unwrap_or_else(Vec::new),
      backends:        self.backends.get(app_id).cloned().unwrap_or_else(Vec::new),
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
    if hostname != front_hostname {
      return false;
    }

    match path_begin {
      &Some(ref path_begin) => path_begin == front_path_begin,
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
  use proxy::{ProxyRequestData,HttpFront,Backend,LoadBalancingParams};

  #[test]
  fn serialize() {
    let mut state:ConfigState = Default::default();
    state.handle_order(&ProxyRequestData::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/"), address: "0.0.0.0:8080".parse().unwrap() }));
    state.handle_order(&ProxyRequestData::AddHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc"), address: "0.0.0.0:8080".parse().unwrap() }));
    state.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-0"), address: "127.0.0.1:1026".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-1"), address: "127.0.0.2:1027".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_2"), backend_id: String::from("app_2-0"), address: "192.167.1.2:1026".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-3"), address: "192.168.1.3:1027".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()) , sticky_id: None, backup: None }));
    state.handle_order(&ProxyRequestData::RemoveBackend(RemoveBackend { app_id: String::from("app_1"), backend_id: String::from("app_1-3"), address: "192.168.1.3:1027".parse().unwrap() }));

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
    state.handle_order(&ProxyRequestData::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/"), address: "0.0.0.0:8080".parse().unwrap() }));
    state.handle_order(&ProxyRequestData::AddHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc"), address: "0.0.0.0:8080".parse().unwrap() }));
    state.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-0"), address: "127.0.0.1:1026".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-1"), address: "127.0.0.2:1027".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_2"), backend_id: String::from("app_2-0"), address: "192.167.1.2:1026".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state.handle_order(&ProxyRequestData::AddApplication(Application { app_id: String::from("app_2"), sticky_session: true, https_redirect: true, proxy_protocol: None, load_balancing_policy: LoadBalancingAlgorithms::RoundRobin, answer_503: None }));

    let mut state2:ConfigState = Default::default();
    state2.handle_order(&ProxyRequestData::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/"), address: "0.0.0.0:8080".parse().unwrap() }));
    state2.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-0"), address: "127.0.0.1:1026".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state2.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-1"), address: "127.0.0.2:1027".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state2.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-2"), address: "127.0.0.2:1028".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state2.handle_order(&ProxyRequestData::AddApplication(Application { app_id: String::from("app_3"), sticky_session: false, https_redirect: false, proxy_protocol: None, load_balancing_policy: LoadBalancingAlgorithms::RoundRobin, answer_503: None }));

   let e = vec!(
     ProxyRequestData::RemoveHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc"), address: "0.0.0.0:8080".parse().unwrap() }),
     ProxyRequestData::RemoveBackend(RemoveBackend { app_id: String::from("app_2"), backend_id: String::from("app_2-0"), address: "192.167.1.2:1026".parse().unwrap() }),
     ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-2"), address: "127.0.0.2:1028".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None }),
     ProxyRequestData::RemoveApplication(String::from("app_2")),
     ProxyRequestData::AddApplication(Application { app_id: String::from("app_3"), sticky_session: false, https_redirect: false, proxy_protocol: None, load_balancing_policy: LoadBalancingAlgorithms::RoundRobin, answer_503: None }),
   );
   let expected_diff:HashSet<&ProxyRequestData> = HashSet::from_iter(e.iter());

   let d = state.diff(&state2);
   let diff = HashSet::from_iter(d.iter());
   println!("diff orders:\n{:#?}\n", diff);
   println!("expected diff orders:\n{:#?}\n", expected_diff);

   let hash1 = state.hash_state();
   let hash2 = state2.hash_state();
   let mut state3 = state.clone();
   state3.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-2"), address: "127.0.0.2:1028".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None }));
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
      path_begin: String::from(""),
      address: "0.0.0.0:8080".parse().unwrap(),
    };

    let https_front_app1 = HttpFront {
      app_id: String::from("MyApp_1"),
      hostname: String::from("lolcatho.st"),
      path_begin: String::from(""),
      address: "0.0.0.0:8443".parse().unwrap(),
    };

    let http_front_app2 = HttpFront {
      app_id: String::from("MyApp_2"),
      hostname: String::from("lolcatho.st"),
      path_begin: String::from("/api"),
      address: "0.0.0.0:8080".parse().unwrap(),
    };

    let https_front_app2 = HttpFront {
      app_id: String::from("MyApp_2"),
      hostname: String::from("lolcatho.st"),
      path_begin: String::from("/api"),
      address: "0.0.0.0:8443".parse().unwrap(),
    };

    let add_http_front_order_app1 = ProxyRequestData::AddHttpFront(http_front_app1);
    let add_http_front_order_app2 = ProxyRequestData::AddHttpFront(http_front_app2);
    let add_https_front_order_app1 = ProxyRequestData::AddHttpsFront(https_front_app1);
    let add_https_front_order_app2 = ProxyRequestData::AddHttpsFront(https_front_app2);
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
