use url::{Url, form_urlencoded};
use url::percent_encoding::{percent_encode, QUERY_ENCODE_SET};
use hyper::client::request::Request;
use hyper::method::Method;

use bencode::{BValue, FromBValue, ConversionResult};
use bencode::BValue::BDict;
use bencode::ConversionError::OtherError;
use bencode;
use peer::PeerInfo;

use std::io::Read;

// TODO: make tracker a sum type to distinguish between http and udp?
#[derive(Debug)]
pub struct Tracker {url: Url}

impl <'a>FromBValue<'a> for Tracker {
    fn from_bvalue(bvalue: BValue<'a>) -> ConversionResult<Tracker> {
        let s = try!(FromBValue::from_bvalue(bvalue));
        match Url::parse(s) {
            Ok(url) => Ok(Tracker::new(url)),
            Err(_) => Err(OtherError) // TODO
        }
    }
}

impl Tracker {
    pub fn new(url: Url) -> Tracker {
        Tracker{url: url}
    }

    pub fn make_request(&self, info_hash: &[u8], peer_id: &[u8],
            event: Option<Event>, uploaded: u64, downloaded: u64,
            left: u64, port: u16) -> Result<Vec<PeerInfo>, ()> {
        match &self.url.scheme[..] {
            "http" => {
                let mut url = self.url.clone();
                let downloaded_s = downloaded.to_string();
                let uploaded_s = uploaded.to_string();
                let left_s = left.to_string();
                let port_s = port.to_string();
                let mut params = vec![
                    ("compact", "1"),
                    ("uploaded", &uploaded_s),
                    ("downloaded", &downloaded_s),
                    ("left", &left_s),
                    ("port", &port_s),
                ];
                if let Some(e) = event {
                    params.push(("event", e.as_slice()));
                }
                let mut query = form_urlencoded::serialize(params.into_iter());
                let encoded_hash = percent_encode(info_hash, QUERY_ENCODE_SET);
                let encoded_id = percent_encode(peer_id, QUERY_ENCODE_SET);
                query.push_str("&info_hash=");
                query.push_str(&encoded_hash[..]);
                query.push_str("&peer_id=");
                query.push_str(&encoded_id[..]);
                url.query = Some(query);
                match Request::new(Method::Get, url).unwrap().start().unwrap().send() {
                    Ok(mut resp) => {
                        let mut data = Vec::new();
                        resp.read_to_end(&mut data).unwrap();
                        let bvalue = bencode::parse_bvalue(&data).unwrap(); // TODO
                        match bvalue {
                            BDict(mut d, _) => {
                                if let Some(f) =
                                        d.remove(&b"failure reason"[..]) {
                                    let err_str: &str = FromBValue::from_bvalue(f).unwrap();
                                    println!("Failure reason: {}", err_str);
                                    return Err(())
                                }
                                match d.remove(&b"peers"[..]) {
                                    Some(peers) => Ok(FromBValue::from_bvalue(peers).unwrap()),
                                    _ => panic!("could not parse peers")
                                }
                            }
                            _ => {println!("Unexpected constructor"); Err(())}
                        }
                        /*match bencode::parse_value(data.as_slice()) {
                            Some(peer_list) => Ok(peer_list),
                            None => panic!("error parsing peer list")
                        }*/
                    }
                    Err(e) => panic!("Http error: {:?}", e)
                }
            },
            "udp" => {
                Err(()) // TODO
            },
            _ => panic!("bad network scheme")
        }
    }
}

pub enum Event { Started, Completed, Stopped }

impl Event {
    fn as_slice(&self) -> &'static str {
        match *self {
            Event::Started => "started",
            Event::Completed => "completed",
            Event::Stopped => "stopped"
        }
    }
}
