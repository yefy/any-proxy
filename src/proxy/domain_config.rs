use super::proxy;
use super::stream_connect;
use super::stream_var;
use crate::config::config_toml;
use crate::config::config_toml::DomainListen;
use crate::config::config_toml::Listen;
use crate::config::config_toml::SSL;
use crate::quic;
use crate::quic::connect as quic_connect;
use crate::quic::endpoints;
use crate::quic::server as quic_server;
use crate::stream::connect;
use crate::stream::server::Server;
use crate::tcp::connect as tcp_connect;
use crate::tcp::server as tcp_server;
use crate::tunnel::connect as tunnel_connect;
use crate::tunnel2::connect as tunnel2_connect;
use crate::util;
use any_tunnel::client as tunnel_rs_client;
use any_tunnel2::client as tunnel2_rs_client;
use async_trait::async_trait;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

pub struct DomainConfigContext {
    pub common: config_toml::CommonConfig,
    pub tcp: config_toml::TcpConfig,
    pub quic: config_toml::QuicConfig,
    pub stream: config_toml::StreamConfig,
    pub access: Vec<config_toml::AccessConfig>,
    pub access_context: Vec<proxy::AccessContext>,
    pub domain: String,
    pub proxy_protocol: bool,
    pub connect: Rc<Box<dyn connect::Connect>>,
    pub endpoints: Arc<endpoints::Endpoints>,
    pub stream_var: Rc<stream_var::StreamVar>,
}

#[derive(Clone)]
pub struct DomainConfigListen {
    pub common: config_toml::CommonConfig,
    pub stream: config_toml::StreamConfig,
    pub listen_server: Rc<Box<dyn Server>>,
    pub domain_config_context_map: HashMap<i32, Rc<DomainConfigContext>>,
    pub domain_index: Rc<util::domain_index::DomainIndex>,
    quic_sni: Option<std::sync::Arc<util::rustls::ResolvesServerCertUsingSNI>>,
}

#[derive(Clone)]
pub struct DomainConfigListenMerge {
    pub key_type: DomainConfigKeyType,
    pub common: Option<config_toml::CommonConfig>,
    pub stream: Option<config_toml::StreamConfig>,
    pub tcp_config: Option<config_toml::TcpConfig>,
    pub quic_config: Option<config_toml::QuicConfig>,
    pub listen_addr: Option<SocketAddr>,
    pub listens: Vec<Listen>,
    pub domain_config_contexts: Vec<Rc<DomainConfigContext>>,
}

#[derive(Clone)]
pub enum DomainConfigKeyType {
    Tcp = 0,
    Udp = 1,
    Quic = 2,
}

pub struct DomainConfig {}

impl DomainConfig {
    pub fn new() -> anyhow::Result<DomainConfig> {
        Ok(DomainConfig {})
    }
    pub fn tcp_key_from_addr(addr: &str) -> anyhow::Result<String> {
        Ok("tcp".to_string() + &util::util::addr(addr)?.port().to_string())
    }
    pub fn udp_key_from_addr(addr: &str) -> anyhow::Result<String> {
        Ok("udp".to_string() + &util::util::addr(addr)?.port().to_string())
    }
    pub fn quic_key_from_addr(addr: &str) -> anyhow::Result<String> {
        Ok("quic".to_string() + &util::util::addr(addr)?.port().to_string())
    }

    pub async fn parse_config(
        &self,
        config: &config_toml::ConfigToml,
        tunnel_client: tunnel_rs_client::Client,
        tunnel2_client: tunnel2_rs_client::Client,
    ) -> anyhow::Result<HashMap<String, DomainConfigListen>> {
        let mut access_map = HashMap::new();
        let mut key_map = HashMap::new();
        let mut domain_config_listen_map = HashMap::new();
        let mut domain_config_listen_merge_map = HashMap::new();

        let quic = config._domain.as_ref().unwrap().quic.as_ref().unwrap();
        let common = &config.common;
        let endpoints = Arc::new(endpoints::Endpoints::new(quic, common.reuseport)?);

        for domain_server_config in config._domain.as_ref().unwrap()._server.iter() {
            let stream_var = stream_var::StreamVar::new();
            let stream_connect = stream_connect::StreamConnect::new(
                "tcp".to_string(),
                SocketAddr::from(([127, 0, 0, 1], 8080)),
                SocketAddr::from(([127, 0, 0, 1], 18080)),
            );
            let access_context = if domain_server_config.access.is_some() {
                let mut access_context = Vec::new();
                for access in domain_server_config.access.as_ref().unwrap() {
                    let ret: anyhow::Result<util::var::Var> = async {
                        let access_format_vars =
                            util::var::Var::new(&access.access_format, Some("-"))?;
                        let mut access_format_var = util::var::Var::copy(&access_format_vars)?;
                        access_format_var.for_each(|var| {
                            let var_name = util::var::Var::var_name(var);
                            let value = stream_var.find(var_name, &stream_connect)?;
                            Ok(value)
                        })?;
                        let _ = access_format_var.join()?;
                        Ok(access_format_vars)
                    }
                    .await;
                    let access_format_vars = ret.map_err(|e| {
                        anyhow::anyhow!(
                            "err:access_format => access_format:{}, e:{}",
                            access.access_format,
                            e
                        )
                    })?;

                    let ret: anyhow::Result<()> = async {
                        let path = Path::new(&access.access_log_file);
                        let canonicalize = path
                            .canonicalize()
                            .map_err(|e| anyhow::anyhow!("err:path.canonicalize() => e:{}", e))?;
                        let path = canonicalize
                            .to_str()
                            .ok_or(anyhow::anyhow!("err:{}", access.access_log_file))?
                            .to_string();
                        let access_log_file = match access_map.get(&path).cloned() {
                            Some(access_log_file) => access_log_file,
                            None => {
                                let access_log_file = std::fs::OpenOptions::new()
                                    .append(true)
                                    .create(true)
                                    .open(&access.access_log_file)
                                    .map_err(|e| {
                                        anyhow::anyhow!(
                                            "err::open {} => e:{}",
                                            access.access_log_file,
                                            e
                                        )
                                    })?;
                                let access_log_file = std::sync::Arc::new(access_log_file);
                                access_map.insert(path, access_log_file.clone());
                                access_log_file
                            }
                        };

                        access_context.push(proxy::AccessContext {
                            access_format_vars,
                            access_log_file,
                        });
                        Ok(())
                    }
                    .await;
                    ret.map_err(|e| {
                        anyhow::anyhow!(
                            "err:access_log_file => access_log_file:{}, e:{}",
                            access.access_log_file,
                            e
                        )
                    })?;
                }
                Some(access_context)
            } else {
                None
            };

            let (endpoints, quic) = if domain_server_config.quic.is_some() {
                let quic = domain_server_config.quic.as_ref().unwrap();
                let common = domain_server_config.common.as_ref().unwrap();
                let endpoints = Arc::new(endpoints::Endpoints::new(quic, common.reuseport)?);
                (endpoints, quic)
            } else {
                (endpoints.clone(), quic)
            };

            let (address, connect): (String, Rc<Box<dyn connect::Connect>>) =
                match &domain_server_config.proxy_pass {
                    config_toml::ProxyPass::Tcp(tcp) => (
                        tcp.address.clone(),
                        Rc::new(Box::new(tcp_connect::Connect::new(
                            tcp.address.clone(),
                            tokio::time::Duration::from_secs(
                                domain_server_config
                                    .stream
                                    .as_ref()
                                    .unwrap()
                                    .stream_connect_timeout,
                            ),
                            domain_server_config.tcp.clone().take().unwrap(),
                        )?)),
                    ),
                    config_toml::ProxyPass::Quic(quic) => (
                        quic.address.clone(),
                        Rc::new(Box::new(quic_connect::Connect::new(
                            quic.address.clone(),
                            quic.ssl_domain.clone(),
                            tokio::time::Duration::from_secs(
                                domain_server_config
                                    .stream
                                    .as_ref()
                                    .unwrap()
                                    .stream_connect_timeout,
                            ),
                            endpoints.clone(),
                        )?)),
                    ),
                    config_toml::ProxyPass::Tunnel(tunnel) => match tunnel {
                        config_toml::ProxyPassTunnel::Tcp(tcp) => (
                            tcp.address.clone(),
                            Rc::new(Box::new(tunnel_connect::Connect::new(
                                tunnel_client.clone(),
                                Box::new(tunnel_connect::PeerStreamConnectTcp::new(
                                    tcp.address.clone(),
                                    tokio::time::Duration::from_secs(
                                        domain_server_config
                                            .stream
                                            .as_ref()
                                            .unwrap()
                                            .stream_connect_timeout,
                                    ),
                                    domain_server_config.tcp.clone().take().unwrap(),
                                    tcp.tunnel_max_connect,
                                )),
                            )?)),
                        ),
                        config_toml::ProxyPassTunnel::Quic(quic) => (
                            quic.address.clone(),
                            Rc::new(Box::new(tunnel_connect::Connect::new(
                                tunnel_client.clone(),
                                Box::new(tunnel_connect::PeerStreamConnectQuic::new(
                                    quic.address.clone(),
                                    quic.ssl_domain.clone(),
                                    tokio::time::Duration::from_secs(
                                        domain_server_config
                                            .stream
                                            .as_ref()
                                            .unwrap()
                                            .stream_connect_timeout,
                                    ),
                                    endpoints.clone(),
                                    quic.tunnel_max_connect,
                                )),
                            )?)),
                        ),
                        config_toml::ProxyPassTunnel::Upstrem(upstream) => {
                            return Err(anyhow::anyhow!("err:not support upstream{}", upstream));
                        }
                    },
                    config_toml::ProxyPass::Tunnel2(tunnel) => match tunnel {
                        config_toml::ProxyPassTunnel2::Tcp(tcp) => (
                            tcp.address.clone(),
                            Rc::new(Box::new(tunnel2_connect::Connect::new(
                                tunnel2_client.clone(),
                                Box::new(tunnel2_connect::PeerStreamConnectTcp::new(
                                    tcp.address.clone(),
                                    tokio::time::Duration::from_secs(
                                        domain_server_config
                                            .stream
                                            .as_ref()
                                            .unwrap()
                                            .stream_connect_timeout,
                                    ),
                                    domain_server_config.tcp.clone().take().unwrap(),
                                )),
                            )?)),
                        ),
                        config_toml::ProxyPassTunnel2::Quic(quic) => (
                            quic.address.clone(),
                            Rc::new(Box::new(tunnel2_connect::Connect::new(
                                tunnel2_client.clone(),
                                Box::new(tunnel2_connect::PeerStreamConnectQuic::new(
                                    quic.address.clone(),
                                    quic.ssl_domain.clone(),
                                    tokio::time::Duration::from_secs(
                                        domain_server_config
                                            .stream
                                            .as_ref()
                                            .unwrap()
                                            .stream_connect_timeout,
                                    ),
                                    endpoints.clone(),
                                )),
                            )?)),
                        ),
                        config_toml::ProxyPassTunnel2::Upstrem(upstream) => {
                            return Err(anyhow::anyhow!("err:not support upstream{}", upstream));
                        }
                    },
                    config_toml::ProxyPass::Upstrem(upstream) => {
                        return Err(anyhow::anyhow!("err:not support upstream{}", upstream));
                    }
                };
            let _ = util::util::lookup_host(tokio::time::Duration::from_secs(10), &address)
                .await
                .map_err(|e| anyhow::anyhow!("err:lookup_host => address:{} e:{}", address, e))?;

            let domain_config_context = Rc::new(DomainConfigContext {
                common: domain_server_config.common.clone().take().unwrap(),
                tcp: domain_server_config.tcp.clone().take().unwrap(),
                quic: quic.clone(),
                stream: domain_server_config.stream.clone().take().unwrap(),
                access: domain_server_config.access.clone().take().unwrap(),
                access_context: access_context.unwrap(),
                domain: domain_server_config.domain.clone(),
                proxy_protocol: domain_server_config.proxy_protocol,
                //proxy_pass: domain_server_config.proxy_pass.clone(),
                connect: connect.clone(),
                endpoints,
                stream_var: Rc::new(stream_var::StreamVar::new()),
            });

            for listen in domain_server_config.listen.as_ref().unwrap().iter() {
                match listen {
                    DomainListen::Tcp(listen) => {
                        let ret: anyhow::Result<()> = async {
                            let addrs = util::util::str_addrs(&listen.address)?;
                            let sock_addrs = util::util::addrs(&addrs)?;
                            for (index, addr) in addrs.iter().enumerate() {
                                let key = DomainConfig::tcp_key_from_addr(addr)?;
                                if domain_config_listen_merge_map.get(&key).is_none() {
                                    if key_map.get(&key).is_some() {
                                        return Err(anyhow::anyhow!(
                                            "err:key is exist => key:{}",
                                            key
                                        ));
                                    }
                                    key_map.insert(key.clone(), true);
                                    domain_config_listen_merge_map.insert(
                                        key.clone(),
                                        DomainConfigListenMerge {
                                            common: None,
                                            stream: None,
                                            tcp_config: None,
                                            quic_config: None,
                                            key_type: DomainConfigKeyType::Tcp,
                                            listen_addr: None,
                                            listens: Vec::new(),
                                            domain_config_contexts: Vec::new(),
                                        },
                                    );
                                }

                                let mut values =
                                    domain_config_listen_merge_map.get_mut(&key).unwrap();
                                let listen = config_toml::Listen {
                                    address: addr.clone(),
                                    ssl: None,
                                };

                                values.common = Some(domain_config_context.common.clone());
                                values.stream = Some(domain_config_context.stream.clone());
                                if values.tcp_config.is_none() {
                                    values.tcp_config = Some(domain_config_context.tcp.clone());
                                    values.quic_config = Some(domain_config_context.quic.clone());
                                }
                                values.listens.push(listen);
                                values.listen_addr = Some(sock_addrs[index]);
                                values
                                    .domain_config_contexts
                                    .push(domain_config_context.clone());
                            }
                            Ok(())
                        }
                        .await;
                        ret.map_err(|e| {
                            anyhow::anyhow!("err:address => address:{}, e:{}", listen.address, e)
                        })?;
                    }
                    DomainListen::Quic(listen) => {
                        let ret: anyhow::Result<()> = async {
                            let addrs = util::util::str_addrs(&listen.address)?;
                            let sock_addrs = util::util::addrs(&addrs)?;
                            for (index, addr) in addrs.iter().enumerate() {
                                let udp_key = DomainConfig::udp_key_from_addr(addr)?;
                                let quic_key = DomainConfig::quic_key_from_addr(addr)?;
                                if domain_config_listen_merge_map.get(&quic_key).is_none() {
                                    if key_map.get(&udp_key).is_some() {
                                        return Err(anyhow::anyhow!(
                                            "err:udp_key is exist => key:{}",
                                            udp_key
                                        ));
                                    }
                                    key_map.insert(udp_key.clone(), true);
                                    domain_config_listen_merge_map.insert(
                                        quic_key.clone(),
                                        DomainConfigListenMerge {
                                            common: None,
                                            stream: None,
                                            tcp_config: None,
                                            quic_config: None,
                                            key_type: DomainConfigKeyType::Quic,
                                            listen_addr: None,
                                            listens: Vec::new(),
                                            domain_config_contexts: Vec::new(),
                                        },
                                    );
                                }

                                let mut values =
                                    domain_config_listen_merge_map.get_mut(&quic_key).unwrap();

                                let ssl = SSL {
                                    ssl_domain: domain_server_config.domain.clone(),
                                    cert: listen.ssl.cert.clone(),
                                    key: listen.ssl.key.clone(),
                                    tls: listen.ssl.tls.clone(),
                                };
                                let listen = config_toml::Listen {
                                    address: addr.clone(),
                                    ssl: Some(ssl),
                                };

                                values.common = Some(domain_config_context.common.clone());
                                values.stream = Some(domain_config_context.stream.clone());
                                if values.tcp_config.is_none() {
                                    values.tcp_config = Some(domain_config_context.tcp.clone());
                                    values.quic_config = Some(domain_config_context.quic.clone());
                                }
                                values.listen_addr = Some(sock_addrs[index]);
                                values.listens.push(listen);
                                values
                                    .domain_config_contexts
                                    .push(domain_config_context.clone());
                            }
                            Ok(())
                        }
                        .await;
                        ret.map_err(|e| {
                            anyhow::anyhow!("err:address => address:{}, e:{}", listen.address, e)
                        })?;
                    }
                }
            }
        }

        for (key, value) in domain_config_listen_merge_map.iter_mut() {
            match &value.key_type {
                &DomainConfigKeyType::Tcp => {
                    let mut domain_config_context_map = HashMap::new();
                    let mut index_map = HashMap::new();
                    let mut index = 0;
                    for domain_config_context in value.domain_config_contexts.iter() {
                        index += 1;
                        index_map.insert(index, (domain_config_context.domain.clone(), index));
                        domain_config_context_map.insert(index, domain_config_context.clone());

                        let mut index_map_test = HashMap::new();
                        index_map_test.insert(index, (domain_config_context.domain.clone(), index));
                        util::domain_index::DomainIndex::new(&index_map_test).map_err(|e| {
                            anyhow::anyhow!(
                                "err:domain => domain:{:?}, e:{}",
                                domain_config_context.domain,
                                e
                            )
                        })?;
                    }
                    let domain_index = Rc::new(
                        util::domain_index::DomainIndex::new(&index_map).map_err(|e| {
                            anyhow::anyhow!("err:domain => index_map:{:?}, e:{}", index_map, e)
                        })?,
                    );

                    let listen_server: Rc<Box<dyn Server>> =
                        Rc::new(Box::new(tcp_server::Server::new(
                            value.listen_addr.clone().unwrap(),
                            value.common.as_ref().unwrap().reuseport,
                            value.tcp_config.clone().unwrap(),
                        )?));

                    domain_config_listen_map.insert(
                        key.clone(),
                        DomainConfigListen {
                            common: value.common.clone().unwrap(),
                            stream: value.stream.clone().unwrap(),
                            listen_server,
                            domain_config_context_map,
                            domain_index,
                            quic_sni: None,
                        },
                    );
                }
                &DomainConfigKeyType::Udp => {
                    continue;
                }
                &DomainConfigKeyType::Quic => {
                    let mut domain_config_context_map = HashMap::new();
                    let mut index_map = HashMap::new();
                    let mut index = 0;
                    for domain_config_context in value.domain_config_contexts.iter() {
                        index += 1;
                        index_map.insert(index, (domain_config_context.domain.clone(), index));
                        domain_config_context_map.insert(index, domain_config_context.clone());

                        let mut index_map_test = HashMap::new();
                        index_map_test.insert(index, (domain_config_context.domain.clone(), index));
                        util::domain_index::DomainIndex::new(&index_map_test).map_err(|e| {
                            anyhow::anyhow!(
                                "err:domain => domain:{:?}, e:{}",
                                domain_config_context.domain,
                                e
                            )
                        })?;
                    }
                    let domain_index = Rc::new(
                        util::domain_index::DomainIndex::new(&index_map).map_err(|e| {
                            anyhow::anyhow!("err:domain => index_map:{:?}, e:{}", index_map, e)
                        })?,
                    );
                    let sni = std::sync::Arc::new(quic::util::sni(&value.listens)?);

                    let listen_server: Rc<Box<dyn Server>> =
                        Rc::new(Box::new(quic_server::Server::new(
                            value.listen_addr.clone().unwrap(),
                            value.common.as_ref().unwrap().reuseport,
                            value.quic_config.clone().unwrap(),
                            sni.clone(),
                        )?));

                    domain_config_listen_map.insert(
                        key.clone(),
                        DomainConfigListen {
                            common: value.common.clone().unwrap(),
                            stream: value.stream.clone().unwrap(),
                            listen_server,
                            domain_config_context_map,
                            domain_index,
                            quic_sni: Some(sni),
                        },
                    );
                }
            }
        }

        return Ok(domain_config_listen_map);
    }

    pub fn merger(
        old_domain_config_listen: &DomainConfigListen,
        mut new_domain_config_listen: DomainConfigListen,
    ) -> anyhow::Result<DomainConfigListen> {
        if old_domain_config_listen.quic_sni.is_some()
            && new_domain_config_listen.quic_sni.is_some()
        {
            let old_sni = old_domain_config_listen.quic_sni.as_ref().unwrap();
            old_sni.take_from(new_domain_config_listen.quic_sni.as_ref().unwrap());
            new_domain_config_listen.quic_sni = Some(old_sni.clone());
        }
        Ok(new_domain_config_listen)
    }
}

#[async_trait(?Send)]
impl proxy::Config for DomainConfig {
    async fn parse(
        &self,
        config: &config_toml::ConfigToml,
        tunnel_client: tunnel_rs_client::Client,
        tunnel2_client: tunnel2_rs_client::Client,
    ) -> anyhow::Result<()> {
        if config._domain.is_none() {
            return Ok(());
        }

        let _ = self
            .parse_config(config, tunnel_client, tunnel2_client)
            .await?;
        Ok(())
    }
}
