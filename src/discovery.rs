use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{net::UdpSocket, time};
use uuid::Uuid;

use crate::{
    config::Identity,
    protocol::{Announcement, DISCOVERY_GROUP, DISCOVERY_MAGIC, DISCOVERY_PORT, PROTOCOL_VERSION},
};

#[derive(Clone, Debug)]
pub struct Peer {
    pub id: Uuid,
    pub name: String,
    pub addr: SocketAddr,
}

pub struct Discovery {
    identity: Identity,
}

impl Discovery {
    pub fn new(identity: Identity) -> Result<Self> {
        Ok(Self { identity })
    }

    pub async fn run_announcer(self) -> Result<()> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .context("绑定发现广播套接字失败")?;
        socket.set_multicast_ttl_v4(1)?;
        let target: SocketAddr = format!("{DISCOVERY_GROUP}:{DISCOVERY_PORT}").parse()?;
        let payload = serde_json::to_vec(&Announcement {
            magic: DISCOVERY_MAGIC.into(),
            version: PROTOCOL_VERSION,
            id: self.identity.id,
            name: self.identity.name,
            transfer_port: self.identity.transfer_port,
        })?;

        loop {
            socket.send_to(&payload, target).await?;
            time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub async fn listen(duration: Duration) -> Result<Vec<Peer>> {
        let group: Ipv4Addr = DISCOVERY_GROUP.parse()?;
        let socket = multicast_listener(group)?;
        let mut buffer = [0_u8; 2048];
        let deadline = Instant::now() + duration;
        let mut peers = HashMap::<Uuid, Peer>::new();

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let result = time::timeout(remaining, socket.recv_from(&mut buffer)).await;
            let Ok(Ok((length, source))) = result else {
                break;
            };
            let Ok(announcement) = serde_json::from_slice::<Announcement>(&buffer[..length]) else {
                continue;
            };
            if announcement.magic != DISCOVERY_MAGIC || announcement.version != PROTOCOL_VERSION {
                continue;
            }
            peers.insert(
                announcement.id,
                Peer {
                    id: announcement.id,
                    name: announcement.name,
                    addr: SocketAddr::new(source.ip(), announcement.transfer_port),
                },
            );
        }

        let mut peers: Vec<_> = peers.into_values().collect();
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(peers)
    }
}

fn multicast_listener(group: Ipv4Addr) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT).into())?;
    socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_nonblocking(true)?;
    let std_socket: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(std_socket).context("创建异步发现套接字失败")
}

#[allow(dead_code)]
fn _ipv4_only(ip: IpAddr) -> Option<Ipv4Addr> {
    match ip {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(_) => None,
    }
}
