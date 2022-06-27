use crate::log::*;
use rand::Rng;
use std::{
  collections::HashMap,
  fs::File,
  io::{self, BufReader, Cursor, Read},
  path::PathBuf,
  sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
  },
};
use tokio_rustls::rustls::{Certificate, PrivateKey, ServerConfig};

pub struct Backend {
  pub app_name: String,
  pub server_name: String,
  pub reverse_proxy: ReverseProxy,

  // tls settings
  pub tls_cert_path: Option<PathBuf>,
  pub tls_cert_key_path: Option<PathBuf>,
  pub https_redirection: Option<bool>,
  pub server_config: Mutex<Option<ServerConfig>>,
}

#[derive(Debug, Clone)]
pub struct ReverseProxy {
  pub default_upstream: Option<Upstream>,
  pub upstream: HashMap<String, Upstream>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum LoadBalance {
  RoundRobin,
  Random,
}
impl Default for LoadBalance {
  fn default() -> Self {
    Self::RoundRobin
  }
}

#[derive(Debug, Clone)]
pub struct Upstream {
  pub uri: Vec<hyper::Uri>,
  pub lb: LoadBalance,
  pub cnt: UpstreamCount, // counter for load balancing
}

#[derive(Debug, Clone, Default)]
pub struct UpstreamCount(Arc<AtomicUsize>);

impl Upstream {
  pub fn get(&self) -> Option<&hyper::Uri> {
    match self.lb {
      LoadBalance::RoundRobin => {
        let idx = self.increment_cnt();
        self.uri.get(idx)
      }
      LoadBalance::Random => {
        let mut rng = rand::thread_rng();
        let max = self.uri.len() - 1;
        self.uri.get(rng.gen_range(0..max))
      }
    }
  }

  fn current_cnt(&self) -> usize {
    self.cnt.0.load(Ordering::Relaxed)
  }

  fn increment_cnt(&self) -> usize {
    if self.current_cnt() < self.uri.len() - 1 {
      self.cnt.0.fetch_add(1, Ordering::Relaxed)
    } else {
      self.cnt.0.fetch_and(0, Ordering::Relaxed)
    }
  }
}

impl Backend {
  pub fn get_tls_server_config(&self) -> Option<ServerConfig> {
    let lock = self.server_config.lock();
    if let Ok(opt) = lock {
      let opt_clone = opt.clone();
      if let Some(sc) = opt_clone {
        return Some(sc);
      }
    }
    None
  }
  pub async fn update_server_config(&self) -> io::Result<()> {
    debug!("Update TLS server config");
    let certs_path = self.tls_cert_path.as_ref().unwrap();
    let certs_keys_path = self.tls_cert_key_path.as_ref().unwrap();
    let certs: Vec<_> = {
      let certs_path_str = certs_path.display().to_string();
      let mut reader = BufReader::new(File::open(certs_path).map_err(|e| {
        io::Error::new(
          e.kind(),
          format!(
            "Unable to load the certificates [{}]: {}",
            certs_path_str, e
          ),
        )
      })?);
      rustls_pemfile::certs(&mut reader).map_err(|_| {
        io::Error::new(
          io::ErrorKind::InvalidInput,
          "Unable to parse the certificates",
        )
      })?
    }
    .drain(..)
    .map(Certificate)
    .collect();
    let certs_keys: Vec<_> = {
      let certs_keys_path_str = certs_keys_path.display().to_string();
      let encoded_keys = {
        let mut encoded_keys = vec![];
        File::open(certs_keys_path)
          .map_err(|e| {
            io::Error::new(
              e.kind(),
              format!(
                "Unable to load the certificate keys [{}]: {}",
                certs_keys_path_str, e
              ),
            )
          })?
          .read_to_end(&mut encoded_keys)?;
        encoded_keys
      };
      let mut reader = Cursor::new(encoded_keys);
      let pkcs8_keys = rustls_pemfile::pkcs8_private_keys(&mut reader).map_err(|_| {
        io::Error::new(
          io::ErrorKind::InvalidInput,
          "Unable to parse the certificates private keys (PKCS8)",
        )
      })?;
      reader.set_position(0);
      let mut rsa_keys = rustls_pemfile::rsa_private_keys(&mut reader).map_err(|_| {
        io::Error::new(
          io::ErrorKind::InvalidInput,
          "Unable to parse the certificates private keys (RSA)",
        )
      })?;
      let mut keys = pkcs8_keys;
      keys.append(&mut rsa_keys);
      if keys.is_empty() {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "No private keys found - Make sure that they are in PKCS#8/PEM format",
        ));
      }
      keys.drain(..).map(PrivateKey).collect()
    };

    let mut server_config = certs_keys
      .into_iter()
      .find_map(|certs_key| {
        let server_config_builder = ServerConfig::builder()
          .with_safe_defaults()
          .with_no_client_auth();
        if let Ok(found_config) = server_config_builder.with_single_cert(certs.clone(), certs_key) {
          Some(found_config)
        } else {
          None
        }
      })
      .ok_or_else(|| {
        io::Error::new(
          io::ErrorKind::InvalidInput,
          "Unable to find a valid certificate and key",
        )
      })?;
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    if let Ok(mut config_store) = self.server_config.lock() {
      *config_store = Some(server_config);
    } else {
      error!("Some thing wrong to write into mutex")
    }

    // server_config;
    Ok(())
  }
}
