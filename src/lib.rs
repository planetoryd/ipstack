pub use error::{IpStackError, Result};
use packet::{NetworkPacket, NetworkTuple};
use std::collections::{
    hash_map::Entry::{Occupied, Vacant},
    HashMap,
};
use stream::IpStackStream;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    select,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tracing::{error, info, trace, warn};

use crate::{
    packet::IpStackPacketProtocol,
    stream::{IpStackTcpStream, IpStackUdpStream},
};
mod error;
mod packet;
pub mod stream;

const DROP_TTL: u8 = 0;

#[cfg(not(target_os = "windows"))]
const TTL: u8 = 64;

#[cfg(target_os = "windows")]
const TTL: u8 = 128;

#[cfg(any(target_os = "linux", target_os = "macos"))]
const TUN_FLAGS: [u8; 2] = [0x00, 0x00];

#[cfg(target_os = "linux")]
const TUN_PROTO_IP6: [u8; 2] = [0x86, 0xdd];
#[cfg(target_os = "linux")]
const TUN_PROTO_IP4: [u8; 2] = [0x08, 0x00];

#[cfg(target_os = "macos")]
const TUN_PROTO_IP6: [u8; 2] = [0x00, 0x02];
#[cfg(target_os = "macos")]
const TUN_PROTO_IP4: [u8; 2] = [0x00, 0x02];

pub struct IpStack {
    accept_receiver: UnboundedReceiver<IpStackStream>,
}

impl IpStack {
    pub fn new<D>(mut device: D, mtu: u16, packet_info: bool) -> IpStack
    where
        D: AsyncRead + AsyncWrite + std::marker::Unpin + std::marker::Send + 'static,
    {
        let (accept_sender, accept_receiver) = mpsc::unbounded_channel::<IpStackStream>();

        tokio::spawn(async move {
            let mut streams: HashMap<NetworkTuple, UnboundedSender<NetworkPacket>> = HashMap::new();
            let mut buffer = [0u8; u16::MAX as usize];

            let (pkt_sender, mut pkt_receiver) = mpsc::unbounded_channel::<NetworkPacket>();
            loop {
                select! {
                    Ok(n) = device.read(&mut buffer) => {
                        let offset = if packet_info && cfg!(not(target_os = "windows")) {4} else {0};
                        let content = &buffer[offset..n];
                        let res = NetworkPacket::parse(content);
                        let Ok(packet) = res else {
                            warn!("parse error {:?}", res.unwrap_err());
                            continue;
                        };
                        match streams.entry(packet.network_tuple()){
                            Occupied(entry) =>{
                                let t = packet.transport_protocol();
                                if let Err(_x) = entry.get().send(packet){
                                    warn!("after device-read, cannot send {}", _x);
                                    match t {
                                        IpStackPacketProtocol::Tcp(_t) => {
                                            // dbg!(t.flags());
                                        }
                                        IpStackPacketProtocol::Udp => {
                                            // dbg!("udp");
                                        }
                                    }

                                }
                            }
                            Vacant(entry) => {
                                match packet.transport_protocol(){
                                    IpStackPacketProtocol::Tcp(h) => {
                                        match IpStackTcpStream::new(packet.src_addr(),packet.dst_addr(),h, pkt_sender.clone(),mtu).await{
                                            Ok(stream) => {
                                                entry.insert(stream.stream_sender());
                                                accept_sender.send(IpStackStream::Tcp(stream))?;
                                            }
                                            Err(e) => {
                                                error!("after device-read, create tcp stream failed, {}",e);
                                            }
                                        }
                                    }
                                    IpStackPacketProtocol::Udp => {
                                        let stream = IpStackUdpStream::new(packet.src_addr(),packet.dst_addr(),packet.payload, pkt_sender.clone(),mtu);
                                        entry.insert(stream.stream_sender());
                                        accept_sender.send(IpStackStream::Udp(stream))?;
                                    }
                                }
                            }
                        }
                    }
                    Some(packet) = pkt_receiver.recv() => {
                        if packet.ttl() == 0{
                            let reverse = packet.reverse_network_tuple();
                            warn!("ttl 0 remove stream {:?}", &reverse);
                            streams.remove(&reverse);
                            continue;
                        }
                        #[allow(unused_mut)]
                        let Ok(mut packet_byte) = packet.to_bytes() else {
                            trace!("to_bytes error");
                            continue;
                        };
                        #[cfg(any(target_os = "macos", target_os = "linux"))]
                        if packet_info {
                            if packet.src_addr().is_ipv4(){
                                packet_byte.splice(0..0, [TUN_FLAGS, TUN_PROTO_IP4].concat());
                            } else{
                                packet_byte.splice(0..0, [TUN_FLAGS, TUN_PROTO_IP6].concat());
                            }
                        }
                        device.write_all(&packet_byte).await?;
                        // device.flush().await.unwrap();
                    }
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), IpStackError>(())
        });

        IpStack { accept_receiver }
    }
    pub async fn accept(&mut self) -> Result<IpStackStream, IpStackError> {
        if let Some(s) = self.accept_receiver.recv().await {
            Ok(s)
        } else {
            Err(IpStackError::AcceptError)
        }
    }
}
