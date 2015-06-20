use bencode::{BValue, FromBValue, ConversionResult, ConversionError};

use std::net::{SocketAddrV4, Ipv4Addr};

#[derive(Debug)]
pub struct PeerInfo { pub addr: SocketAddrV4 }

// newtype to avoid overlapping instance of FromBValue for Vec<T>
pub struct PeerInfoList { pub peers: Vec<PeerInfo> }

impl <'a>FromBValue<'a> for PeerInfoList {
    fn from_bvalue(bvalue: BValue<'a>) -> ConversionResult<PeerInfoList> {
        let peers: Vec<PeerInfo> = match bvalue {
            // compact representation
            BValue::String(bytes) => {
                if bytes.len() % 6 != 0 {
                    return Err(ConversionError::OtherError); // TODO
                }
                let mut peers = Vec::with_capacity(bytes.len() / 6);
                for chunk in bytes.chunks(6) {
                    peers.push(PeerInfo{addr: SocketAddrV4::new(
                            Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]),
                            (chunk[4] as u16) << 8 | chunk[5] as u16
                    )});
                }
                peers
            },
            // bencoded dict list representation
            l@BValue::List(_) => try!(FromBValue::from_bvalue(l),
            _ => return Err(ConversionError::WrongBValueConstructor)
        };
        Ok(PeerInfoList { peers: peers })
    }
}

impl <'a> FromBValue<'a> for PeerInfo {
    fn from_bvalue(bvalue: BValue<'a>) -> ConversionResult<PeerInfo> {
        match bvalue {
            BValue::Dict(mut dict, _) => {
                let peer_id = dict.remove(&b"peer id"[..]);
                let ip = dict.remove(&b"ip"[..]);
                let port = dict.remove(&b"port"[..]);
                match (peer_id, ip, port) {
                    (Some(_id), Some(ip), Some(port)) => {
                        let ip = try!(FromBValue::from_bvalue(ip));
                        let port = try!(FromBValue::from_bvalue(port));
                        Ok(PeerInfo { addr: SocketAddrV4::new(ip, port) })
                    },
                    _ => Err(ConversionError::KeyDoesNotExist)
                }
            },
            _ => Err(ConversionError::WrongBValueConstructor)
        }
    }
}
