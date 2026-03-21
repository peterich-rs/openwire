use std::net::SocketAddr;
use std::time::Duration;

use hyper::Uri;
use hyper_util::client::legacy::connect::Connection;

use crate::{BoxFuture, CallContext, WireError};

pub trait ConnectionIo:
    hyper::rt::Read + hyper::rt::Write + Connection + Unpin + Send + 'static
{
}

impl<T> ConnectionIo for T where
    T: hyper::rt::Read + hyper::rt::Write + Connection + Unpin + Send + 'static
{
}

pub type BoxConnection = Box<dyn ConnectionIo>;

impl Connection for BoxConnection {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        (**self).connected()
    }
}

#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub id: crate::ConnectionId,
    pub remote_addr: Option<SocketAddr>,
    pub local_addr: Option<SocketAddr>,
    pub tls: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoalescingInfo {
    pub verified_server_names: Vec<String>,
}

impl CoalescingInfo {
    pub fn new(verified_server_names: Vec<String>) -> Self {
        Self {
            verified_server_names,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.verified_server_names.is_empty()
    }
}

pub trait DnsResolver: Send + Sync + 'static {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>>;
}

pub trait TcpConnector: Send + Sync + 'static {
    fn connect(
        &self,
        ctx: CallContext,
        addr: SocketAddr,
        timeout: Option<Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>>;
}

pub trait TlsConnector: Send + Sync + 'static {
    fn connect(
        &self,
        ctx: CallContext,
        uri: Uri,
        stream: BoxConnection,
    ) -> BoxFuture<Result<BoxConnection, WireError>>;
}
