use bencode::{BValue, FromBValue, ConversionResult};
use bencode::BValue::BString;
use bencode::ConversionError::{WrongBValueConstructor, OtherError};

use std::net::{SocketAddrV4, Ipv4Addr};

#[derive(Debug)]
pub struct PeerInfo {pub addr: SocketAddrV4}

// TODO: handle non-compact representation
impl <'a>FromBValue<'a> for Vec<PeerInfo> {
    fn from_bvalue(bvalue: BValue<'a>) -> ConversionResult<Vec<PeerInfo>> {
        match bvalue {
            // compact representation
            BString(bytes) => {
                if bytes.len() % 6 != 0 { return Err(OtherError) } // TODO
                let mut peers = Vec::with_capacity(bytes.len() / 6);
                for chunk in bytes.chunks(6) {
                    peers.push(PeerInfo{addr: SocketAddrV4::new(
                            Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]),
                            (chunk[4] as u16) << 8 | chunk[5] as u16
                    )});
                }
                Ok(peers)
            }
            _ => Err(WrongBValueConstructor)
        }
    }
}
