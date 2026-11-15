//! The transport owns a dial address, never the database's TLS identity.
use super::{DbError, ServerConfig, SshTunnel, SsmTunnel, plain_error, ssh, ssm};
use std::{net::SocketAddr, sync::Arc};

pub(super) enum TunnelConfig<'a> {
    Ssh(&'a SshTunnel),
    Ssm(&'a SsmTunnel),
}

pub(super) fn selected<'a>(
    ssh: Option<&'a SshTunnel>,
    ssm: Option<&'a SsmTunnel>,
) -> Result<Option<TunnelConfig<'a>>, DbError> {
    match (ssh, ssm) {
        (Some(_), Some(_)) => Err(plain_error(
            "Choose either SSH or AWS SSM, not both.".into(),
        )),
        (Some(ssh), None) => Ok(Some(TunnelConfig::Ssh(ssh))),
        (None, Some(ssm)) => {
            ssm.validate().map_err(plain_error)?;
            Ok(Some(TunnelConfig::Ssm(ssm)))
        }
        (None, None) => Ok(None),
    }
}

pub(super) enum Tunnel {
    Ssh(ssh::Tunnel),
    Ssm(ssm::Tunnel),
}
impl Tunnel {
    pub fn open(config: TunnelConfig<'_>, host: &str, port: u16) -> Result<Self, DbError> {
        match config {
            TunnelConfig::Ssh(config) => ssh::Tunnel::open(config, host, port).map(Self::Ssh),
            TunnelConfig::Ssm(config) => ssm::Tunnel::open(config, host, port).map(Self::Ssm),
        }
    }
    pub fn dial(&self) -> Result<SocketAddr, DbError> {
        match self {
            Self::Ssh(t) => t.dial(),
            Self::Ssm(t) => t.dial(),
        }
    }
    pub fn explain(&self, error: DbError) -> DbError {
        match self {
            Self::Ssh(t) => t.explain(error),
            Self::Ssm(t) => t.explain(error),
        }
    }
}
pub(super) fn tunnelled<C>(
    server: &ServerConfig,
    default_port: u16,
    connect: impl FnOnce(Option<Arc<Tunnel>>) -> Result<C, DbError>,
) -> Result<C, DbError> {
    let Some(config) = selected(server.ssh.as_ref(), server.ssm.as_ref())? else {
        return connect(None);
    };
    let tunnel = Arc::new(Tunnel::open(
        config,
        &server.host,
        server.port.unwrap_or(default_port),
    )?);
    connect(Some(tunnel.clone())).map_err(|error| tunnel.explain(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ambiguous_transport_never_connects() {
        let server = ServerConfig {
            ssh: Some(SshTunnel::default()),
            ssm: Some(SsmTunnel::default()),
            ..Default::default()
        };
        let error = tunnelled(&server, 5432, |_| -> Result<(), DbError> {
            panic!("must not connect")
        })
        .unwrap_err();
        assert!(error.message.contains("not both"));
    }
    #[test]
    fn direct_transport_does_not_spawn() {
        tunnelled(&ServerConfig::default(), 5432, |t| {
            assert!(t.is_none());
            Ok(())
        })
        .unwrap();
    }
}
