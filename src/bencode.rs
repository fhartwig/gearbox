use std::collections::HashMap;
use std::str::from_utf8;
use std::fmt;

pub use self::ConversionError::*;
pub use self::ParseError::*;

pub type ParseResult<T> = Result<T, ParseError>;

pub struct Parser<'a> {
    buf: &'a [u8],
    next_pos: usize
}

impl <'a> Parser<'a> {
    fn new(input: &[u8]) -> ParseResult<Parser> {
        if input.len() == 0 {
            return Err(EmptyInput);
        }
        Ok(Parser {
            buf: input,
            next_pos: 0
        })
    }

    fn next_byte(&mut self) -> ParseResult<u8>{
        if self.next_pos >= self.buf.len() {
            return Err(EOF);
        }
        let byte = self.buf[self.next_pos];
        self.next_pos += 1;
        Ok(byte)
    }

    fn peek(&mut self) -> ParseResult<u8> {
        if self.next_pos >= self.buf.len() {
            return Err(EOF);
        }
        Ok(self.buf[self.next_pos])
    }

    fn take_next_bytes(&mut self, n: usize) -> ParseResult<&'a [u8]> {
        let start = self.next_pos;
        let end = self.next_pos + n;
        if end > self.buf.len() {
            return Err(EOF);
        }
        self.next_pos += n;
        Ok(&self.buf.as_ref()[start..end])
        
    }

    fn read_str_length(&mut self) -> ParseResult<usize> {
        let mut result = 0;
        loop {
            let c = try!(self.next_byte()) as char;
            match c.to_digit(10) {
                Some(n) => result = result * 10 + n as usize,
                None if c == ':'=> break,
                _ => return Err(UnexpectedToken(c as u8))
            }
        }
        Ok(result)
    }

    fn parse_string(&mut self) -> ParseResult<&'a [u8]> {
        let len = try!(self.read_str_length());
        self.take_next_bytes(len)
    }
    
    fn accept_byte(&mut self, expected: u8) -> ParseResult<()>{
        let next = try!(self.next_byte());
        if next != expected {Err(UnexpectedToken(next))} else {Ok(())}
    }

    // TODO: should probably return at least an i64
    fn parse_int(&mut self) -> ParseResult<isize> {
        try!(self.accept_byte('i' as u8));
        let mut neg = false;
        let mut result = 0;
        let mut cur_char = try!(self.next_byte()) as char;
        if cur_char == '-' {
            neg = true;
            cur_char = try!(self.next_byte()) as char;
        }
        loop {
            match cur_char.to_digit(10) {
                Some(n) => result = result * 10 + n as isize,
                None => {
                    if cur_char == 'e' {
                        break;
                    } else {
                        return Err(UnexpectedToken(cur_char as u8));
                    }
                }
            }
            cur_char = try!(self.next_byte()) as char;
        }
        Ok(if neg {-result} else {result})
    }

    fn parse_list(&mut self) -> ParseResult<Vec<BValue<'a>>> {
        try!(self.accept_byte('l' as u8));
        let mut list = Vec::new();
        loop {
            let b = try!(self.peek());
            if b as char == 'e' {
                let _ = self.next_byte();
                break
            }
            let v = try!(self.parse_value());
            list.push(v);
        }
        Ok(list)
    }

    fn parse_dict(&mut self) -> ParseResult<HashMap<&'a [u8], BValue<'a>>>{
        try!(self.accept_byte('d' as u8));
        let mut dict = HashMap::new();
        loop {
            let b = try!(self.peek());
            if b as char == 'e' {
                let _ = self.next_byte();
                break
            }
            let key = try!(self.parse_string());
            let value = try!(self.parse_value());
            dict.insert(key, value);
        }
        Ok(dict)
    }

    fn parse_value(&mut self) -> ParseResult<BValue<'a>> {
        match try!(self.peek()) as char {
            'i' => {
                let n = try!(self.parse_int());
                Ok(BValue::Int(n))
            },
            'l' => {
                let list = try!(self.parse_list());
                Ok(BValue::List(list))
            },
            'd' => {
                let dict_start = self.next_pos;
                let dict = try!(self.parse_dict());
                let dict_end = self.next_pos; // one past the end of the dict
                let dict_str = &self.buf.as_ref()[dict_start..dict_end];
                Ok(BValue::Dict(dict, dict_str))
            },
            c if c.is_digit(10) => {
                let string = try!(self.parse_string());
                Ok(BValue::String(string))
            },
            c => Err(UnexpectedToken(c as u8))
        }
    }
}

pub fn parse_bvalue<'a>(input: &'a [u8]) -> ParseResult<BValue<'a>> {
    let mut parser = try!(Parser::new(input));
    parser.parse_value()
}

#[derive(Debug)]
enum ParseError {
    EOF,
    EmptyInput,
    UnexpectedToken(u8)
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            EOF => write!(f, "End of file"),
            EmptyInput => write!(f, "No input given"),
            UnexpectedToken(c) => write!(f, "Unexpected byte: {}", c)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum BValue<'a> {
    Int(isize), // TODO: should be i64 (technically, it's arbitrary-precision,
        // but I can't think of a use case for anything larger than i64
    String(&'a [u8]),
    List(Vec<BValue<'a>>),
    Dict(HashMap<&'a [u8], BValue<'a>>, &'a [u8])
}

impl <'a> BValue<'a> {
    pub fn get(&mut self, key: &'a [u8]) -> ConversionResult<BValue<'a>> {
        match *self {
            BValue::Dict(ref mut d, _) => {
                match d.remove(&key) {
                    Some(bv) => Ok(bv),
                    None => Err(KeyDoesNotExist)
                }
            }
            _ => Err(WrongBValueConstructor)
        }
    }
}

#[derive(Debug)]
pub enum ConversionError {
    BadEncoding,
    WrongBValueConstructor,
    KeyDoesNotExist,
    OtherError // TODO
}


impl fmt::Display for ConversionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            BadEncoding => write!(f, "String is not utf8-encoded"),
            WrongBValueConstructor => write!(f, "Unexpected Constructor"),
            KeyDoesNotExist => write!(f, "Key does not exist"),
            OtherError => write!(f, "Other error")
        }
    }
}

pub type ConversionResult<T> = Result<T, ConversionError>;

pub trait FromBValue<'a> {
    fn from_bvalue(BValue<'a>) -> ConversionResult<Self>;
}

impl <'a>FromBValue<'a> for &'a [u8] {
    fn from_bvalue(bvalue: BValue<'a>) -> ConversionResult<&'a [u8]>{
        match bvalue {
            BValue::String(s) => Ok(s),
            _ => Err(WrongBValueConstructor)
        }
    }
}

impl <'a>FromBValue<'a> for Vec<u8> {
    fn from_bvalue(bvalue: BValue<'a>) -> ConversionResult<Vec<u8>>{
        match bvalue {
            BValue::String(s) => {
                let mut v = Vec::with_capacity(s.len());
                v.push_all(s);
                Ok(v)
            }
            _ => Err(WrongBValueConstructor)
        }
    }
}

impl <'a> FromBValue<'a> for &'a str {
    fn from_bvalue(bvalue: BValue<'a>) -> ConversionResult<&'a str> {
        match bvalue {
            BValue::String(bytes) => match from_utf8(bytes) {
                Ok(s) => Ok(s),
                Err(_) => Err(BadEncoding)
            },
            _ => Err(WrongBValueConstructor)
        }
    }
}

impl <'a>FromBValue<'a> for isize {
    fn from_bvalue(bvalue: BValue) -> ConversionResult<isize> {
        match bvalue {
            BValue::Int(n) => Ok(n),
            _ => Err(WrongBValueConstructor)
        }
    }
}

impl <'a>FromBValue<'a> for usize {
    fn from_bvalue(bvalue: BValue) -> ConversionResult<usize> {
        match bvalue {
            BValue::Int(n) if n >= 0 => Ok(n as usize),
            _ => Err(WrongBValueConstructor) // TODO: better error
        }
    }
}

impl <'a>FromBValue<'a> for u64 {
    fn from_bvalue(bvalue: BValue) -> ConversionResult<u64> {
        match bvalue {
            BValue::Int(n) if n >= 0 => Ok(n as u64),
            _ => Err(WrongBValueConstructor)
        }
    }
}

impl <'a> FromBValue<'a> for String {
    fn from_bvalue(bvalue: BValue<'a>) -> ConversionResult<String> {
        match bvalue {
            BValue::String(bytes) => {
                let mut v = Vec::with_capacity(bytes.len());
                v.push_all(bytes);
                match String::from_utf8(v) {
                    Ok(s) => Ok(s),
                    Err(_) => Err(BadEncoding)
                }
            },
            _ => Err(WrongBValueConstructor)
        }
    }
}

impl <'a, T: FromBValue<'a>> FromBValue<'a> for Vec<T> {
    fn from_bvalue(bvalue: BValue<'a>) -> ConversionResult<Vec<T>> {
        match bvalue {
            BValue::List(v) => {
                // TODO: why does this not work?
                //Ok(v.into_iter().map(|e| {
                //    FromBValue::from_bvalue(e).unwrap_or(return Err(()))
                //}).collect())
                let mut result = Vec::with_capacity(v.len());
                for e in v {
                    result.push(try!(FromBValue::from_bvalue(e)));
                }
                Ok(result)
            }
            _ => Err(WrongBValueConstructor)
        }
    }
}
/*
impl <'a, T: FromBValue<'a>> FromBValue<'a> for HashMap<&'a [u8], T> {
    fn from_bvalue(bvalue: BValue<'a>) ->
            ConversionResult<HashMap<&'a [u8], T>> {
        match bvalue {
            BDict(vec) => {
                let mut map = HashMap::new();
                for (k, v) in vec.into_iter() {
                    map.insert(k, try!(FromBValue::from_bvalue(v)));
                }
                Ok(map)
            }
            _ => Err(WrongBValueConstructor)
        }
    }
}*/

#[cfg(test)]
mod tests {
    use std::iter::FromIterator;
    use super::*;
    #[test]
    fn test_string_parsing() {
        let s = b"3:cow";
        let mut parser = Parser::new(s).unwrap();
        assert_eq!(b"cow", parser.parse_string().unwrap());
        assert_eq!(parser.next_pos, 5);
    }

    #[test]
    fn test_int_parsing() {
        let s = b"i123e";
        assert_eq!(parse_bvalue(s).unwrap(), BValue::Int(123isize));
    }

    #[test]
    fn test_list_parsing() {
        let s = b"l4:spam4:eggse";
        let mut parser = Parser::new(s).unwrap();
        let v = parser.parse_value().unwrap();
        assert_eq!(v, BValue::List(vec![BValue::String(b"spam"),
                                        BValue::String(b"eggs")]));
    }

    #[test]
    fn test_dict_parsing() {
        let s = b"d3:cow3:moo4:spami3ee";
        let bv = parse_bvalue(s).unwrap();
        assert_eq!(bv, BValue::Dict(FromIterator::from_iter(
            vec![(&b"cow"[..], BValue::String(&b"moo"[..])),
                 (&b"spam"[..], BValue::Int(3))].into_iter()),
            s));
    }
}
