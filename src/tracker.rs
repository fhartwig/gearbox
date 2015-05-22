use byteorder::{ReadBytesExt, WriteBytesExt, BigEndian};
use url::{Url, form_urlencoded};
use url::percent_encoding::{percent_encode, QUERY_ENCODE_SET};
use hyper::client::request::Request;
use hyper::method::Method;
use rand::random;

use bencode::{self, BValue, FromBValue, ConversionResult};
use bencode::ConversionError::OtherError;
use peer::PeerInfo;

use std::io::{Read, Write, Cursor};
use std::net::{SocketAddr, SocketAddrV4, Ipv4Addr, ToSocketAddrs, UdpSocket};

#[derive(Debug)]
pub struct TrackerRequest<'a> {
    pub info_hash: &'a [u8],
    pub peer_id: &'a [u8],
    pub event: Option<Event>,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub port: u16
}

#[derive(Debug)]
pub enum Tracker {
    Http(Url),
    Udp(SocketAddr)
}

impl <'a>FromBValue<'a> for Tracker {
    fn from_bvalue(bvalue: BValue<'a>) -> ConversionResult<Tracker> {
        let s = try!(FromBValue::from_bvalue(bvalue));
        let mut url = match Url::parse(s) {
            Ok(url) => url,
            Err(_) => return Err(OtherError) // TODO
        };

        let tracker = match &url.scheme[..] {
            "http" => Tracker::Http(url),
            "udp" => {
                // HACK: the url parser doesn't properly parse the host and port
                // if the url scheme is "udp", so we set the scheme to "http",
                // serialise the url and parse it again to get the socket addr
                url.scheme = String::from("http");
                let url_str = url.serialize();
                let url2 = Url::parse(&url_str).unwrap();
                let addr_str = format!("{}:{}", url2.host().unwrap(),
                                                url2.port().unwrap());
                // hack ends here
                let addr = ToSocketAddrs::to_socket_addrs(&addr_str[..]).unwrap().next().unwrap();
                Tracker::Udp(addr)
            }
            _ => panic!() // FIXME
        };
        Ok(tracker)
    }
}

impl Tracker {
    pub fn make_request(&self, request: &TrackerRequest)
                        -> Result<Vec<PeerInfo>, ()> {
        match self {
            &Tracker::Http(ref url) => make_http_request(url.clone(), request),
            &Tracker::Udp(ref addr) => make_udp_request(addr, request)
        }
    }
}

fn make_http_request(mut url: Url, request: &TrackerRequest)
                     -> Result<Vec<PeerInfo>, ()> {
    let downloaded_s = request.downloaded.to_string();
    let uploaded_s = request.uploaded.to_string();
    let left_s = request.left.to_string();
    let port_s = request.port.to_string();
    let mut params = vec![
        ("compact", "1"),
        ("uploaded", &uploaded_s),
        ("downloaded", &downloaded_s),
        ("left", &left_s),
        ("port", &port_s),
    ];
    if let Some(e) = request.event {
        params.push(("event", e.as_slice()));
    }
    let mut query = form_urlencoded::serialize(params.into_iter());
    let encoded_hash = percent_encode(request.info_hash, QUERY_ENCODE_SET);
    let encoded_id = percent_encode(request.peer_id, QUERY_ENCODE_SET);
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
                BValue::Dict(mut d, _) => {
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
        }
        Err(e) => panic!("Http error: {:?}", e)
    }
}

fn make_udp_request(tracker_addr: &SocketAddr, request: &TrackerRequest)
                    -> Result<Vec<PeerInfo>, ()> {
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    let mut buf = [0u8;1600];
    let connection_id = make_connect_request(&socket, &tracker_addr, &mut buf[..]);

    let transaction_id: u32 = random();
    let response = make_announce_request(&socket, &tracker_addr, &mut buf[..],
                                         request, transaction_id,
                                         connection_id);
    let mut response_reader = Cursor::new(response);
    let response_action = response_reader.read_u32::<BigEndian>().unwrap();
    assert_eq!(response_action, 1);
    let response_trans_id = response_reader.read_u32::<BigEndian>().unwrap();
    assert_eq!(response_trans_id, transaction_id);
    let _interval = response_reader.read_u32::<BigEndian>().unwrap();
    let _leechers = response_reader.read_u32::<BigEndian>().unwrap();
    let _seeders = response_reader.read_u32::<BigEndian>().unwrap();

    let peers_count = (response.len() as u64 - response_reader.position()) / 6;
    let mut peers = Vec::with_capacity(peers_count as usize);
    for _ in (0..peers_count) {
        let ip_byte0 = response_reader.read_u8().unwrap();
        let ip_byte1 = response_reader.read_u8().unwrap();
        let ip_byte2 = response_reader.read_u8().unwrap();
        let ip_byte3 = response_reader.read_u8().unwrap();
        let ip = Ipv4Addr::new(ip_byte0, ip_byte1, ip_byte2, ip_byte3);
        let port = response_reader.read_u16::<BigEndian>().unwrap();
        peers.push(PeerInfo { addr: SocketAddrV4::new(ip, port) });
    }
    Ok(peers)
}

fn make_connect_request(socket: &UdpSocket, addr: &SocketAddr, buf: &mut [u8])
                        -> u64 {
    let init_connection_id: u64 = 0x41727101980;
    let transaction_id: u32 = random();
    let action: u32 = 0; // connect
    {
        let mut writer = Cursor::new(&mut buf[..]);
        writer.write_u64::<BigEndian>(init_connection_id).unwrap();
        writer.write_u32::<BigEndian>(action).unwrap();
        writer.write_u32::<BigEndian>(transaction_id).unwrap();
    }
    socket.send_to(&buf[..16], addr).unwrap();
    let (bytes_received, _) = socket.recv_from(buf).unwrap();
    let response = &buf[..bytes_received];
    let mut response_reader = Cursor::new(response);
    let response_action = response_reader.read_u32::<BigEndian>().unwrap();
    let response_trans_id = response_reader.read_u32::<BigEndian>().unwrap();
    // FIXME: check this properly
    assert_eq!(response_action, 0);
    assert_eq!(response_trans_id, transaction_id);
    response_reader.read_u64::<BigEndian>().unwrap()
}

fn make_announce_request<'a>(socket: &UdpSocket, addr: &SocketAddr,
                         buf: &'a mut [u8], request: &TrackerRequest,
                         transaction_id: u32, connection_id: u64)
                         -> &'a [u8] {
    {
        let mut writer = Cursor::new(&mut buf[..]);
        writer.write_u64::<BigEndian>(connection_id).unwrap();
        let action = 1; // announce
        writer.write_u32::<BigEndian>(action).unwrap();
        writer.write_u32::<BigEndian>(transaction_id).unwrap();
        writer.write(request.info_hash).unwrap();
        writer.write(request.peer_id).unwrap();
        writer.write_u64::<BigEndian>(request.downloaded).unwrap();
        writer.write_u64::<BigEndian>(request.left).unwrap();
        writer.write_u64::<BigEndian>(request.uploaded).unwrap();
        writer.write_u32::<BigEndian>(event_to_id(request.event)).unwrap();
        writer.write_u32::<BigEndian>(0).unwrap(); // default value for ip address
        writer.write_u32::<BigEndian>(0).unwrap(); // FIXME: no idea what this is for
        writer.write_i32::<BigEndian>(-1).unwrap(); // num_want default
        writer.write_u16::<BigEndian>(request.port).unwrap();
    }

    socket.send_to(&buf[..98], addr).unwrap();
    let (bytes_received, _) = socket.recv_from(buf).unwrap();
    &buf[..bytes_received]

}

#[derive(Debug, Copy, Clone)]
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

fn event_to_id(event: Option<Event>) -> u32 {
    match event {
        None => 0,
        Some(Event::Started) => 2,
        Some(Event::Completed) => 1,
        Some(Event::Stopped) => 3
    }
}
