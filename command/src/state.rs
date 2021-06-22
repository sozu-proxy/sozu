use std::collections::{BTreeMap,HashMap,HashSet,BTreeSet};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::iter::{repeat,FromIterator};
use crate::certificate::calculate_fingerprint;

use crate::proxy::{Application,CertFingerprint,CertificateAndKey,ProxyRequestData,
  HttpFront,TcpFront,Backend,QueryAnswerApplication,
  AddCertificate, RemoveCertificate, RemoveBackend,
  HttpListener,HttpsListener,TcpListener,ListenerType,
  ActivateListener,RemoveListener, DeactivateListener};

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
  pub applications:    BTreeMap<AppId, Application>,
  pub backends:        BTreeMap<AppId, Vec<Backend>>,
  /// the bool indicates if it is active or not
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
      applications:    BTreeMap::new(),
      backends:        BTreeMap::new(),
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

        // we might be modifying the sticky id or load balancing parameters
        backend_vec.retain(|b| {
            b.backend_id != backend.backend_id
            || b.address != backend.address
        });
        backend_vec.push(backend.clone());
        backend_vec.sort();

        true
      },
      &ProxyRequestData::RemoveBackend(ref backend) => {
        if let Some(backend_list) = self.backends.get_mut(&backend.app_id) {
          let len = backend_list.len();
          backend_list.retain(|b| {
              b.backend_id != backend.backend_id
              || b.address != backend.address
          });
          backend_list.sort();
          backend_list.len() != len
        } else {
          false
        }
      },
      // This is to avoid the error message
      &ProxyRequestData::Logging(_) | &ProxyRequestData::Status | &ProxyRequestData::Query(_) => {false},
      o => {
        error!("state cannot handle order message: {:#?}", o);
        false
      }
    }
  }

  pub fn generate_orders(&self) -> Vec<ProxyRequestData> {
    let mut v = Vec::new();

    for &(ref listener, active) in self.http_listeners.values() {
      v.push(ProxyRequestData::AddHttpListener(listener.clone()));
      if active {
        v.push(ProxyRequestData::ActivateListener(ActivateListener {
          front: listener.front.clone(),
          proxy: ListenerType::HTTP,
          from_scm: false
        }));
      }
    }

    for &(ref listener, active) in self.https_listeners.values() {
      v.push(ProxyRequestData::AddHttpsListener(listener.clone()));
      if active {
        v.push(ProxyRequestData::ActivateListener(ActivateListener {
          front: listener.front.clone(),
          proxy: ListenerType::HTTPS,
          from_scm: false
        }));
      }
    }

    for &(ref listener, active) in self.tcp_listeners.values() {
      v.push(ProxyRequestData::AddTcpListener(listener.clone()));
      if active {
        v.push(ProxyRequestData::ActivateListener(ActivateListener {
          front: listener.front.clone(),
          proxy: ListenerType::TCP,
          from_scm: false
        }));
      }
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

    //pub tcp_listeners:   HashMap<SocketAddr, (TcpListener, bool)>,
    let my_tcp_listeners: HashSet<&SocketAddr> = self.tcp_listeners.keys().collect();
    let their_tcp_listeners: HashSet<&SocketAddr> = other.tcp_listeners.keys().collect();
    let removed_tcp_listeners = my_tcp_listeners.difference(&their_tcp_listeners);
    let added_tcp_listeners = their_tcp_listeners.difference(&my_tcp_listeners);

    let my_http_listeners: HashSet<&SocketAddr> = self.http_listeners.keys().collect();
    let their_http_listeners: HashSet<&SocketAddr> = other.http_listeners.keys().collect();
    let removed_http_listeners = my_http_listeners.difference(&their_http_listeners);
    let added_http_listeners = their_http_listeners.difference(&my_http_listeners);

    let my_https_listeners: HashSet<&SocketAddr> = self.https_listeners.keys().collect();
    let their_https_listeners: HashSet<&SocketAddr> = other.https_listeners.keys().collect();
    let removed_https_listeners = my_https_listeners.difference(&their_https_listeners);
    let added_https_listeners = their_https_listeners.difference(&my_https_listeners);

    let mut v = vec!();

    for address in removed_tcp_listeners {
      if self.tcp_listeners[address].1 {
        v.push(ProxyRequestData::DeactivateListener(DeactivateListener {
          front: **address,
          proxy: ListenerType::TCP,
          to_scm: false,
        }));
      }

      v.push(ProxyRequestData::RemoveListener(RemoveListener {
        front: **address,
        proxy: ListenerType::TCP
      }));
    }

    for address in added_tcp_listeners.clone() {
      v.push(ProxyRequestData::AddTcpListener(other.tcp_listeners[address].0.clone()));

      if other.tcp_listeners[address].1 {
        v.push(ProxyRequestData::ActivateListener(ActivateListener {
          front: **address,
          proxy: ListenerType::TCP,
          from_scm: false,
        }));
      }
    }

    for address in removed_http_listeners {
      if self.http_listeners[address].1 {
        v.push(ProxyRequestData::DeactivateListener(DeactivateListener {
          front: **address,
          proxy: ListenerType::HTTP,
          to_scm: false,
        }));
      }

      v.push(ProxyRequestData::RemoveListener(RemoveListener {
        front: **address,
        proxy: ListenerType::HTTP
      }));
    }

    for address in added_http_listeners.clone() {
      v.push(ProxyRequestData::AddHttpListener(other.http_listeners[address].0.clone()));

      if other.http_listeners[address].1 {
        v.push(ProxyRequestData::ActivateListener(ActivateListener {
          front: **address,
          proxy: ListenerType::HTTP,
          from_scm: false,
        }));
      }
    }

    for address in removed_https_listeners {
      if self.https_listeners[address].1 {
        v.push(ProxyRequestData::DeactivateListener(DeactivateListener {
          front: **address,
          proxy: ListenerType::HTTPS,
          to_scm: false,
        }));
      }

      v.push(ProxyRequestData::RemoveListener(RemoveListener {
        front: **address,
        proxy: ListenerType::HTTPS
      }));
    }

    for address in added_https_listeners.clone() {
      v.push(ProxyRequestData::AddHttpsListener(other.https_listeners[address].0.clone()));

      if other.https_listeners[address].1 {
        v.push(ProxyRequestData::ActivateListener(ActivateListener {
          front: **address,
          proxy: ListenerType::HTTPS,
          from_scm: false,
        }));
      }
    }

    for addr in my_tcp_listeners.intersection(&their_tcp_listeners) {
      let (my_listener, my_active) = &self.tcp_listeners[addr];
      let (their_listener, their_active) = &other.tcp_listeners[addr];

      if my_listener != their_listener {
        v.push(ProxyRequestData::RemoveListener(RemoveListener {
          front: **addr,
          proxy: ListenerType::TCP
        }));

        v.push(ProxyRequestData::AddTcpListener(their_listener.clone()));
      }

      if *my_active && !*their_active {
        v.push(ProxyRequestData::DeactivateListener(DeactivateListener {
          front: **addr,
          proxy: ListenerType::TCP,
          to_scm: false,
        }));
      }

      if !*my_active && *their_active {
        v.push(ProxyRequestData::ActivateListener(ActivateListener {
          front: **addr,
          proxy: ListenerType::TCP,
          from_scm: false,
        }));
      }
    }

    for addr in my_http_listeners.intersection(&their_http_listeners) {
      let (my_listener, my_active) = &self.http_listeners[addr];
      let (their_listener, their_active) = &other.http_listeners[addr];

      if my_listener != their_listener {
        v.push(ProxyRequestData::RemoveListener(RemoveListener {
          front: **addr,
          proxy: ListenerType::HTTP
        }));

        v.push(ProxyRequestData::AddHttpListener(their_listener.clone()));
      }

      if *my_active && !*their_active {
        v.push(ProxyRequestData::DeactivateListener(DeactivateListener {
          front: **addr,
          proxy: ListenerType::HTTP,
          to_scm: false,
        }));
      }

      if !*my_active && *their_active {
        v.push(ProxyRequestData::ActivateListener(ActivateListener {
          front: **addr,
          proxy: ListenerType::HTTP,
          from_scm: false,
        }));
      }
    }

    for addr in my_https_listeners.intersection(&their_https_listeners) {
      let (my_listener, my_active) = &self.https_listeners[addr];
      let (their_listener, their_active) = &other.https_listeners[addr];

      if my_listener != their_listener {
        v.push(ProxyRequestData::RemoveListener(RemoveListener {
          front: **addr,
          proxy: ListenerType::HTTPS
        }));

        v.push(ProxyRequestData::AddHttpsListener(their_listener.clone()));
      }

      if *my_active && !*their_active {
        v.push(ProxyRequestData::DeactivateListener(DeactivateListener {
          front: **addr,
          proxy: ListenerType::HTTPS,
          to_scm: false,
        }));
      }

      if !*my_active && *their_active {
        v.push(ProxyRequestData::ActivateListener(ActivateListener {
          front: **addr,
          proxy: ListenerType::HTTPS,
          from_scm: false,
        }));
      }
    }

    for (app_id, res) in diff_map(self.applications.iter(), other.applications.iter()) {
        match res {
            DiffResult::Added | DiffResult::Changed =>
                v.push(ProxyRequestData::AddApplication(other.applications.get(app_id)
                                                        .unwrap()
                                                        .clone())),
            DiffResult::Removed => v.push(ProxyRequestData::RemoveApplication(app_id.to_string())),
        }
    }


    for ((app_id, backend_id), res) in diff_map(
        self.backends.iter().map(|(app_id, v)| v.iter().map(move |backend| ((app_id, &backend.backend_id), backend))).flatten(),
        other.backends.iter().map(|(app_id, v)| v.iter().map(move |backend| ((app_id, &backend.backend_id), backend))).flatten(),
        ) {
        match res {
            DiffResult::Added => {
                let backend = other.backends.get(app_id)
                                  .and_then(|v| v.iter().find(|b| &b.backend_id == backend_id))
                                  .unwrap();
                v.push(ProxyRequestData::AddBackend(backend.clone()));
            },
            DiffResult::Removed => {
                let backend = self.backends.get(app_id).and_then(|v| v.iter().find(|b| &b.backend_id == backend_id)).unwrap();

                v.push(ProxyRequestData::RemoveBackend(RemoveBackend{
                    app_id:     backend.app_id.clone(),
                    backend_id: backend.backend_id.clone(),
                    address:    backend.address,
                }));
            },
            DiffResult::Changed => {
                let backend = self.backends.get(app_id).and_then(|v| v.iter().find(|b| &b.backend_id == backend_id)).unwrap();

                v.push(ProxyRequestData::RemoveBackend(RemoveBackend{
                    app_id:     backend.app_id.clone(),
                    backend_id: backend.backend_id.clone(),
                    address:    backend.address,
                }));

                let backend = other.backends.get(app_id)
                                  .and_then(|v| v.iter().find(|b| &b.backend_id == backend_id))
                                  .unwrap();
                v.push(ProxyRequestData::AddBackend(backend.clone()));
            }
        }
    }


    let mut my_http_fronts: HashSet<(&AppId, &HttpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in self.http_fronts.iter() {
      for ref front in front_list.iter() {
        my_http_fronts.insert((&app_id, &front));
      }
    }
    let mut their_http_fronts: HashSet<(&AppId, &HttpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in other.http_fronts.iter() {
      for ref front in front_list.iter() {
        their_http_fronts.insert((&app_id, &front));
      }
    }

    let removed_http_fronts = my_http_fronts.difference(&their_http_fronts);
    let added_http_fronts   = their_http_fronts.difference(&my_http_fronts);

    for &(_, front) in removed_http_fronts {
     v.push(ProxyRequestData::RemoveHttpFront(front.clone()));
    }

    for &(_, front) in added_http_fronts {
      v.push(ProxyRequestData::AddHttpFront(front.clone()));
    }

    let mut my_https_fronts: HashSet<(&AppId, &HttpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in self.https_fronts.iter() {
      for ref front in front_list.iter() {
        my_https_fronts.insert((&app_id, &front));
      }
    }
    let mut their_https_fronts: HashSet<(&AppId, &HttpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in other.https_fronts.iter() {
      for ref front in front_list.iter() {
        their_https_fronts.insert((&app_id, &front));
      }
    }

    let removed_https_fronts = my_https_fronts.difference(&their_https_fronts);
    let added_https_fronts   = their_https_fronts.difference(&my_https_fronts);

    for &(_, front) in removed_https_fronts {
     v.push(ProxyRequestData::RemoveHttpsFront(front.clone()));
    }

    for &(_, front) in added_https_fronts {
      v.push(ProxyRequestData::AddHttpsFront(front.clone()));
    }


    let mut my_tcp_fronts: HashSet<(&AppId, &TcpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in self.tcp_fronts.iter() {
      for ref front in front_list.iter() {
        my_tcp_fronts.insert((&app_id, &front));
      }
    }
    let mut their_tcp_fronts: HashSet<(&AppId, &TcpFront)> = HashSet::new();
    for (ref app_id, ref front_list) in other.tcp_fronts.iter() {
      for ref front in front_list.iter() {
        their_tcp_fronts.insert((&app_id, &front));
      }
    }

    let removed_tcp_fronts = my_tcp_fronts.difference(&their_tcp_fronts);
    let added_tcp_fronts   = their_tcp_fronts.difference(&my_tcp_fronts);

    for &(_, front) in removed_tcp_fronts {
     v.push(ProxyRequestData::RemoveTcpFront(front.clone()));
    }

    for &(_, front) in added_tcp_fronts {
      v.push(ProxyRequestData::AddTcpFront(front.clone()));
    }


    //pub certificates:    HashMap<SocketAddr, HashMap<CertFingerprint, (CertificateAndKey, Vec<String>)>>,
    let my_certificates:    HashSet<(SocketAddr, &CertFingerprint)> =
      HashSet::from_iter(self.certificates.iter().flat_map(|(addr, certs)| {
        repeat(*addr).zip(certs.keys())
      }));
    let their_certificates: HashSet<(SocketAddr, &CertFingerprint)> =
      HashSet::from_iter(other.certificates.iter().flat_map(|(addr, certs)| {
        repeat(*addr).zip(certs.keys())
      }));

    let removed_certificates = my_certificates.difference(&their_certificates);
    let added_certificates   = their_certificates.difference(&my_certificates);

    for  &(front, fingerprint) in removed_certificates {
      v.push(ProxyRequestData::RemoveCertificate(RemoveCertificate {
        front,
        fingerprint: fingerprint.clone(),
        names: Vec::new(),
      }));
    }

    for &(front, fingerprint) in added_certificates {
      if let Some((ref certificate_and_key, ref names)) = other.certificates.get(&front)
          .and_then(|certs| certs.get(fingerprint)) {
              v.push(ProxyRequestData::AddCertificate(AddCertificate{
                  front,
                  certificate: certificate_and_key.clone(),
                  names: names.clone(),
              }));
          }
    }


    for address in added_tcp_listeners {
      let listener = &other.tcp_listeners[address];
      if listener.1 {
        v.push(ProxyRequestData::ActivateListener(ActivateListener {
          front: listener.0.front.clone(),
          proxy: ListenerType::TCP,
          from_scm: false
        }));
      }
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

pub fn get_certificate(state: &ConfigState, fingerprint: &[u8]) -> Option<(String, Vec<String>)> {
  state.certificates.values().filter_map(|h| h.get(&CertFingerprint(fingerprint.to_vec())))
    .map(|(c, names)| (c.certificate.clone(), names.clone())).next()
}

struct DiffMap<'a, K:Ord, V, I1, I2> {
  my_it: I1,
  other_it: I2,
  my: Option<(K, &'a V)>,
  other: Option<(K, &'a V)>,
}

//fn diff_map<'a, K:Ord, V: PartialEq>(my: &'a BTreeMap<K,V>, other: &'a BTreeMap<K,V>) -> DiffMap<'a,K,V> {
fn diff_map<'a, K:Ord, V: PartialEq, I1: Iterator<Item=(K, &'a V)>, I2: Iterator<Item=(K, &'a V)>>(my: I1, other: I2) -> DiffMap<'a,K,V, I1, I2> {
    DiffMap { my_it: my, other_it: other, my: None, other: None }
}

enum DiffResult {
    Added,
    Removed,
    Changed,
}

// this will iterate over the keys of both iterators
// since keys are sorted, it should be easy to see which ones are in common or not
impl<'a, K:Ord, V:PartialEq, I1: Iterator<Item=(K, &'a V)>, I2: Iterator<Item=(K, &'a V)>> std::iter::Iterator for DiffMap<'a, K, V, I1, I2> {
    type Item = (K, DiffResult);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.my.is_none() {
                self.my = self.my_it.next();
            }
            if self.other.is_none() {
                self.other = self.other_it.next();
            }

            match (self.my.take(), self.other.take()) {
                // there are no more elements in my_it, all the next elements in other
                // should be added
                // if other was none, we will stop the iterator there
                (None, other) => return other.map(|(k,_)| (k, DiffResult::Added)),
                // there are no more elements in other_it, all the next elements in my
                // should be removed
                (Some((k,_)), None) =>  return Some((k, DiffResult::Removed)),
                (Some((k1,v1)), Some((k2, v2))) => {
                    // element is present in my but not other
                    if k1 < k2 {
                        self.other = Some((k2, v2));
                        return Some((k1, DiffResult::Removed));
                    // element is present in other byt not in my
                    } else if k1 > k2 {
                        self.my = Some((k1, v1));
                        return Some((k2, DiffResult::Added));
                    } else {
                        // key is present in both, if elements have changed
                        // return a value, otherwise go to the next key for both maps
                        if v1 != v2 {
                            return Some((k1, DiffResult::Changed));
                        }
                    }
                }

            }
        }
        //None
    }
}


#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::LoadBalancingAlgorithms;
  use crate::proxy::{ProxyRequestData,HttpFront,Backend,LoadBalancingParams,TlsProvider};

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
    state.handle_order(&ProxyRequestData::AddApplication(Application { app_id: String::from("app_2"), sticky_session: true, https_redirect: true, proxy_protocol: None, load_balancing: LoadBalancingAlgorithms::RoundRobin, load_metric: None, answer_503: None }));

    let mut state2:ConfigState = Default::default();
    state2.handle_order(&ProxyRequestData::AddHttpFront(HttpFront { app_id: String::from("app_1"), hostname: String::from("lolcatho.st:8080"), path_begin: String::from("/"), address: "0.0.0.0:8080".parse().unwrap() }));
    state2.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-0"), address: "127.0.0.1:1026".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state2.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-1"), address: "127.0.0.2:1027".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state2.handle_order(&ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-2"), address: "127.0.0.2:1028".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None  }));
    state2.handle_order(&ProxyRequestData::AddApplication(Application { app_id: String::from("app_3"), sticky_session: false, https_redirect: false, proxy_protocol: None, load_balancing: LoadBalancingAlgorithms::RoundRobin, load_metric: None, answer_503: None }));

   let e = vec!(
     ProxyRequestData::RemoveHttpFront(HttpFront { app_id: String::from("app_2"), hostname: String::from("test.local"), path_begin: String::from("/abc"), address: "0.0.0.0:8080".parse().unwrap() }),
     ProxyRequestData::RemoveBackend(RemoveBackend { app_id: String::from("app_2"), backend_id: String::from("app_2-0"), address: "192.167.1.2:1026".parse().unwrap() }),
     ProxyRequestData::AddBackend(Backend { app_id: String::from("app_1"), backend_id: String::from("app_1-2"), address: "127.0.0.2:1028".parse().unwrap(), load_balancing_parameters: Some(LoadBalancingParams::default()), sticky_id: None, backup: None }),
     ProxyRequestData::RemoveApplication(String::from("app_2")),
     ProxyRequestData::AddApplication(Application { app_id: String::from("app_3"), sticky_session: false, https_redirect: false, proxy_protocol: None, load_balancing: LoadBalancingAlgorithms::RoundRobin, load_metric: None, answer_503: None }),
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

  #[test]
  fn duplicate_backends() {
    let mut state:ConfigState = Default::default();
    state.handle_order(&ProxyRequestData::AddBackend(Backend {
      app_id: String::from("app_1"),
      backend_id: String::from("app_1-0"),
      address: "127.0.0.1:1026".parse().unwrap(),
      load_balancing_parameters: Some(LoadBalancingParams::default()),
      sticky_id: None,
      backup: None
    }));

    let b = Backend {
      app_id: String::from("app_1"),
      backend_id: String::from("app_1-0"),
      address: "127.0.0.1:1026".parse().unwrap(),
      load_balancing_parameters: Some(LoadBalancingParams::default()),
      sticky_id: Some("sticky".to_string()),
      backup: None
    };

    state.handle_order(&ProxyRequestData::AddBackend(b.clone()));

    assert_eq!(state.backends.get("app_1").unwrap(), &vec![b]);
  }

  #[test]
  fn listener_diff() {
    let mut state:ConfigState = Default::default();
    state.handle_order(&ProxyRequestData::AddTcpListener(TcpListener {
      front: "0.0.0.0:1234".parse().unwrap(),
      public_address: None,
      expect_proxy: false,
      front_timeout: 60,
      back_timeout: 30,
      connect_timeout: 3,
    }));
    state.handle_order(&ProxyRequestData::ActivateListener(ActivateListener {
      front: "0.0.0.0:1234".parse().unwrap(),
      proxy: ListenerType::TCP,
      from_scm: false,
    }));
    state.handle_order(&ProxyRequestData::AddHttpListener(HttpListener {
      front: "0.0.0.0:8080".parse().unwrap(),
      public_address: None,
      expect_proxy: false,
      answer_404: String::new(),
      answer_503: String::new(),
      sticky_name: String::new(),
      front_timeout: 60,
      request_timeout: 10,
      back_timeout: 30,
      connect_timeout: 3,
    }));
    state.handle_order(&ProxyRequestData::AddHttpsListener(HttpsListener {
      front: "0.0.0.0:8443".parse().unwrap(),
      public_address: None,
      expect_proxy: false,
      answer_404: String::new(),
      answer_503: String::new(),
      sticky_name: String::new(),
      versions: Vec::new(),
      cipher_list: String::new(),
      rustls_cipher_list: Vec::new(),
      tls_provider: TlsProvider::Openssl,
      certificate:         None,
      certificate_chain:   vec![],
      key:                 None,
      front_timeout: 60,
      request_timeout: 10,
      back_timeout: 30,
      connect_timeout: 3,
    }));
    state.handle_order(&ProxyRequestData::ActivateListener(ActivateListener {
      front: "0.0.0.0:8443".parse().unwrap(),
      proxy: ListenerType::HTTPS,
      from_scm: false,
    }));

    let mut state2:ConfigState = Default::default();
    state2.handle_order(&ProxyRequestData::AddTcpListener(TcpListener {
      front: "0.0.0.0:1234".parse().unwrap(),
      public_address: None,
      expect_proxy: true,
      front_timeout: 60,
      back_timeout: 30,
      connect_timeout: 3,
    }));
    state2.handle_order(&ProxyRequestData::AddHttpListener(HttpListener {
      front: "0.0.0.0:8080".parse().unwrap(),
      public_address: None,
      expect_proxy: false,
      answer_404: "test".to_string(),
      answer_503: String::new(),
      sticky_name: String::new(),
      front_timeout: 60,
      request_timeout: 10,
      back_timeout: 30,
      connect_timeout: 3,
    }));
    state2.handle_order(&ProxyRequestData::ActivateListener(ActivateListener {
      front: "0.0.0.0:8080".parse().unwrap(),
      proxy: ListenerType::HTTP,
      from_scm: false,
    }));
    state2.handle_order(&ProxyRequestData::AddHttpsListener(HttpsListener {
      front: "0.0.0.0:8443".parse().unwrap(),
      public_address: None,
      expect_proxy: false,
      answer_404: String::from("test"),
      answer_503: String::new(),
      sticky_name: String::new(),
      versions: Vec::new(),
      cipher_list: String::new(),
      rustls_cipher_list: Vec::new(),
      tls_provider: TlsProvider::Openssl,
      certificate:         None,
      certificate_chain:   vec![],
      key:                 None,
      front_timeout: 60,
      request_timeout: 10,
      back_timeout: 30,
      connect_timeout: 3,
    }));
    state2.handle_order(&ProxyRequestData::ActivateListener(ActivateListener {
      front: "0.0.0.0:8443".parse().unwrap(),
      proxy: ListenerType::HTTPS,
      from_scm: false,
    }));

    let e = vec!(
      ProxyRequestData::RemoveListener(RemoveListener {
        front: "0.0.0.0:1234".parse().unwrap(),
        proxy: ListenerType::TCP,
      }),
      ProxyRequestData::AddTcpListener(TcpListener {
        front: "0.0.0.0:1234".parse().unwrap(),
        public_address: None,
        expect_proxy: true,
        front_timeout: 60,
        back_timeout: 30,
        connect_timeout: 3,
      }),
      ProxyRequestData::DeactivateListener(DeactivateListener {
        front: "0.0.0.0:1234".parse().unwrap(),
        proxy: ListenerType::TCP,
        to_scm: false,
      }),
      ProxyRequestData::RemoveListener(RemoveListener {
        front: "0.0.0.0:8080".parse().unwrap(),
        proxy: ListenerType::HTTP,
      }),
      ProxyRequestData::AddHttpListener(HttpListener {
        front: "0.0.0.0:8080".parse().unwrap(),
        public_address: None,
        expect_proxy: false,
        answer_404: String::from("test"),
        answer_503: String::new(),
        sticky_name: String::new(),
        front_timeout: 60,
        request_timeout: 10,
        back_timeout: 30,
        connect_timeout: 3,
      }),
      ProxyRequestData::ActivateListener(ActivateListener {
        front: "0.0.0.0:8080".parse().unwrap(),
        proxy: ListenerType::HTTP,
        from_scm: false,
      }),
      ProxyRequestData::RemoveListener(RemoveListener {
        front: "0.0.0.0:8443".parse().unwrap(),
        proxy: ListenerType::HTTPS,
      }),
      ProxyRequestData::AddHttpsListener(HttpsListener {
        front: "0.0.0.0:8443".parse().unwrap(),
        public_address: None,
        expect_proxy: false,
        answer_404: String::from("test"),
        answer_503: String::new(),
        sticky_name: String::new(),
        versions: Vec::new(),
        cipher_list: String::new(),
        rustls_cipher_list: Vec::new(),
        tls_provider: TlsProvider::Openssl,
        certificate:         None,
        certificate_chain:   vec![],
        key:                 None,
        front_timeout: 60,
        request_timeout: 10,
        back_timeout: 30,
        connect_timeout: 3,
      }),
    );

    let diff = state.diff(&state2);
    //let diff: HashSet<&ProxyRequestData> = HashSet::from_iter(d.iter());
    println!("expected diff orders:\n{:#?}\n", e);
    println!("diff orders:\n{:#?}\n", diff);

    let hash1 = state.hash_state();
    let hash2 = state2.hash_state();

    assert_eq!(diff, e);
  }

}
