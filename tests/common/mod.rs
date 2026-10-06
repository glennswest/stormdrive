//! A node CA, a serving pair for 127.0.0.1 and client pairs, minted for a
//! test (#19) — what stormcert-agent writes to /data/stormcert on a node.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use rcgen::{BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair};

pub struct Ca {
    pub cert: Certificate,
    pub key: KeyPair,
}

impl Ca {
    pub fn new(name: &str) -> Ca {
        let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.distinguished_name.push(DnType::CommonName, name);
        let key = KeyPair::generate().unwrap();
        let cert = p.self_signed(&key).unwrap();
        Ca { cert, key }
    }

    pub fn pem(&self) -> String {
        self.cert.pem()
    }

    /// (certificate PEM, key PEM) for `cn`, each `org` an O; a server pair
    /// names 127.0.0.1 and localhost.
    pub fn issue(&self, cn: &str, orgs: &[&str], server: bool) -> (String, String) {
        let sans: Vec<String> = if server { vec!["127.0.0.1".into(), "localhost".into()] } else { vec![] };
        let mut p = CertificateParams::new(sans).unwrap();
        p.distinguished_name.push(DnType::CommonName, cn);
        for o in orgs {
            p.distinguished_name.push(DnType::OrganizationName, *o);
        }
        p.extended_key_usages = vec![if server { ExtendedKeyUsagePurpose::ServerAuth } else { ExtendedKeyUsagePurpose::ClientAuth }];
        let key = KeyPair::generate().unwrap();
        let cert = p.signed_by(&key, &self.cert, &self.key).unwrap();
        (cert.pem(), key.serialize_pem())
    }
}

/// A node's /data/stormcert in `dir`: ca.crt, stormdrive.crt, stormdrive.key.
pub fn node_certs(dir: &Path, ca: &Ca) -> (PathBuf, PathBuf, PathBuf) {
    let (crt, key) = ca.issue("stormdrive", &[], true);
    let paths = (dir.join("ca.crt"), dir.join("stormdrive.crt"), dir.join("stormdrive.key"));
    std::fs::write(&paths.0, ca.pem()).unwrap();
    std::fs::write(&paths.1, crt).unwrap();
    std::fs::write(&paths.2, key).unwrap();
    paths
}

/// The `[api]` lines that point stormdrive at them.
pub fn api_toml(ca: &Path, crt: &Path, key: &Path) -> String {
    format!(
        "tls_cert_file = \"{}\"\ntls_key_file = \"{}\"\nclient_ca_files = [\"{}\"]\n",
        crt.display(),
        key.display(),
        ca.display()
    )
}

/// A client that trusts `ca`, presenting `identity` (cert + key PEM) if given.
pub fn client(ca: &Ca, identity: Option<&(String, String)>) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .add_root_certificate(reqwest::Certificate::from_pem(ca.pem().as_bytes()).unwrap());
    if let Some((c, k)) = identity {
        b = b.identity(reqwest::Identity::from_pem(format!("{c}\n{k}").as_bytes()).unwrap());
    }
    b.build().unwrap()
}
