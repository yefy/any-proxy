use super::domain_index::DomainIndex;
use super::SniContext;
use anyhow::Result;
use rustls::server::ClientHello;
use rustls::server::ResolvesServerCert;
use rustls::sign;
use std::collections::HashMap;
use std::fs;
use std::rc::Rc;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

/// 不验证证书
/// Dummy certificate verifier that treats any certificate as valid.
/// NOTE, such verification is vulnerable to MITM attacks, but convenient for testing.
pub struct SkipServerVerification;

impl SkipServerVerification {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

/// rustls sni对象
/// Something that resolves do different cert chains/keys based
/// on client-supplied server name (via SNI).
pub struct ResolvesServerCertUsingSNI {
    pub domain_index: std::sync::RwLock<Option<DomainIndex>>,
    pub by_name: std::sync::RwLock<Option<HashMap<i32, Arc<sign::CertifiedKey>>>>,
}

impl ResolvesServerCertUsingSNI {
    /// Create a new and empty (ie, knows no certificates) resolver.
    pub fn new(
        ctxs: &Vec<SniContext>,
        domain_index: DomainIndex,
    ) -> Result<ResolvesServerCertUsingSNI> {
        let mut sni_rustls_map = ResolvesServerCertUsingSNI {
            domain_index: std::sync::RwLock::new(Some(domain_index)),
            by_name: std::sync::RwLock::new(Some(HashMap::new())),
        };

        for ctx in ctxs.iter() {
            if ctx.ssl.is_none() {
                continue;
            }

            let ssl = ctx.ssl.as_ref().unwrap();

            let key = load_private_key(&ssl.key)
                .map_err(|e| anyhow::anyhow!("err:load key => key:{}, e{}", ssl.key, e))
                .and_then(|key| {
                    rustls::sign::any_supported_type(&key)
                        .map_err(|_| anyhow::anyhow!("err:load key => key:{}, e:{}", ssl.key, rustls::Error::General("invalid private key".into())))
                })?;

            let cert = load_certs(&ssl.cert)
                .map_err(|e| anyhow::anyhow!("err:load cert => cert:{}, e{}", ssl.cert, e))?;

            let certified_key = rustls::sign::CertifiedKey::new(cert, key);
            sni_rustls_map.add(ctx.index, certified_key)?;
        }

        Ok(sni_rustls_map)
    }

    /// Add a new `sign::CertifiedKey` to be used for the given SNI `name`.
    ///
    /// This function fails if `name` is not a valid DNS name, or if
    /// it's not valid for the supplied certificate, or if the certificate
    /// chain is syntactically faulty.
    pub fn add(&mut self, index: i32, ck: sign::CertifiedKey) -> Result<()> {
        // let checked_name = webpki::DNSNameRef::try_from_ascii_str(name)
        //     .map_err(|_| TLSError::General("Bad DNS name".into()))?;
        //
        // ck.cross_check_end_entity_cert(Some(checked_name))?;
        self.by_name
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .insert(index, Arc::new(ck));
        Ok(())
    }
    /// reload的时候，clone新配置
    pub fn take_from(&self, other: &ResolvesServerCertUsingSNI) {
        *self.domain_index.write().unwrap() = other.domain_index.write().unwrap().take();
        *self.by_name.write().unwrap() = other.by_name.write().unwrap().take();
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.read().unwrap().as_ref().unwrap().is_empty()
    }
}

impl ResolvesServerCert for ResolvesServerCertUsingSNI {
    fn resolve(&self, client_hello: ClientHello) -> Option<Arc<sign::CertifiedKey>> {
        if let Some(domain) = client_hello.server_name() {
            log::trace!("domain:{}", domain);
            let domain_index = match self
                .domain_index
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .index(domain)
            {
                Err(_) => return None,
                Ok(domain_index) => domain_index,
            };
            log::trace!("domain_index:{}", domain_index);
            self.by_name
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .get(&domain_index)
                .cloned()
        } else {
            // This kind of resolver requires SNI
            None
        }
    }
}

pub fn load_certs(filename: &str) -> Result<Vec<rustls::Certificate>> {
    use std::path::PathBuf;
    let cert_path = PathBuf::from(filename);
    let cert_chain = fs::read(cert_path.clone())
        .map_err(|_| anyhow::anyhow!("failed to read certificate chain"))?;
    let cert_chain = if cert_path.extension().map_or(false, |x| x == "der") {
        vec![rustls::Certificate(cert_chain)]
    } else {
        rustls_pemfile::certs(&mut &*cert_chain)
            .map_err(|_| anyhow::anyhow!("invalid PEM-encoded certificate"))?
            .into_iter()
            .map(rustls::Certificate)
            .collect()
    };
    Ok(cert_chain)
}

pub fn load_private_key(filename: &str) -> Result<rustls::PrivateKey> {
    use std::path::PathBuf;
    let key_path = PathBuf::from(filename);
    let key = fs::read(filename).map_err(|_| anyhow::anyhow!("failed to read private key"))?;
    let key = if key_path.extension().map_or(false, |x| x == "der") {
        rustls::PrivateKey(key)
    } else {
        let pkcs8 = rustls_pemfile::pkcs8_private_keys(&mut &*key)
            .map_err(|_| anyhow::anyhow!("malformed PKCS #8 private key"))?;
        match pkcs8.into_iter().next() {
            Some(x) => rustls::PrivateKey(x),
            None => {
                let rsa = rustls_pemfile::rsa_private_keys(&mut &*key)
                    .map_err(|_| anyhow::anyhow!("malformed PKCS #1 private key"))?;
                match rsa.into_iter().next() {
                    Some(x) => rustls::PrivateKey(x),
                    None => {
                        anyhow::bail!("no private keys found");
                    }
                }
            }
        }
    };
    Ok(key)
}

/// rustls accept
pub fn tls_acceptor(
    sni_rustls_map: std::sync::Arc<ResolvesServerCertUsingSNI>,
    protocols: &[Vec<u8>],
) -> Rc<TlsAcceptor> {
    let tls_cfg = {
        let mut server_crypto = rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_cert_resolver(sni_rustls_map);

        // Configure ALPN to accept HTTP/2, HTTP/1.1 in that order.
        //cfg.set_protocols(&[b"h2".to_vec(), b"http/1.1".to_vec()]);
        if protocols.len() > 0 {
            server_crypto.alpn_protocols = protocols.to_vec();
        }
        std::sync::Arc::new(server_crypto)
    };
    Rc::new(TlsAcceptor::from(tls_cfg))
}
