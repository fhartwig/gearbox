use bencode::{BValue, FromBValue, ConversionResult};
use bencode::ConversionError::{OtherError, WrongBValueConstructor, KeyDoesNotExist};
use piece_set::PieceSet;
use types::PieceIndex;

use crypto::sha1::Sha1;
use crypto::digest::Digest;

use std::cmp::min;
use std::fmt::{Formatter, Error, Debug};
use std::io::{Read, ErrorKind};
use std::fs::File;
use std::path::PathBuf;
use std::iter::repeat;

pub struct FileInfo {
    pub path: PathBuf,
    pub length: u64, 
}

impl Debug for FileInfo {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(f,
               "FileInfo {{ path: \"{}\", length: \"{}\" }}",
               self.path.display(), self.length
        )
    }
}

pub struct TorrentInfo {
    name: String,
    piece_length: u32,
    hashes: Vec<u8>,
    files: Vec<FileInfo>,
    total_size: u64,
    info_hash: Vec<u8>
}

impl Debug for TorrentInfo {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        try!(write!(f,
            "TorrentInfo {{name: \"{}\", piece_length: {}, hashes: [{}], files: {:?}, total_size: {}}}",
            self.name,
            self.piece_length,
            self.hashes.len(),
            self.files,
            self.total_size));
            // TODO: hex-encoded info hash
        Ok(())
    }
}

impl TorrentInfo {
    pub fn new(name: String, piece_len: u32, piece_hashes: Vec<u8>,
            files: Vec<FileInfo>, dict_str: &[u8])
            -> Option<TorrentInfo> {
        let total_size = files.iter().map(|f| f.length).sum();
        if piece_hashes.len() % 20 != 0 {
            return None
        }
        //let piece_count = piece_hashes.len() / 20;
        // 0-length file list is an error
        if files.len() == 0 {
            return None
        }
        let mut hasher = Sha1::new();
        hasher.input(dict_str);
        let mut info_hash = vec![0;20];
        hasher.result(&mut info_hash[..]);
        // TODO: check that total_size, piece_length and piece_count make sense
            // together
        Some(
            TorrentInfo {
                name: name,
                piece_length: piece_len,
                hashes: piece_hashes,
                files: files,
                total_size: total_size,
                info_hash: info_hash
            }
        )
    }

    pub fn files(&self) -> &[FileInfo] {
        &self.files
    }

    pub fn info_hash(&self) -> &[u8] {
        &self.info_hash
    }

    /// panics when passed an invalid piece index
    pub fn get_piece_length(&self, piece_index: PieceIndex) -> u32 {
        debug_assert!(self.piece_count() > piece_index.0 as u32);
        min(self.piece_length,
            (self.total_size -
                (piece_index.0 as u64 * self.piece_length as u64))
                as u32
        )
    }

    pub fn piece_count(&self) -> u32 {
        self.hashes.len() as u32 / 20
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn map_block(&self, piece_index: PieceIndex, offset: u32, length: u32)
                     -> FileSectionIter {
        FileSectionIter::new(piece_index, offset, length, self)
    }

    pub fn get_piece_hash(&self, piece_index: PieceIndex) -> &[u8] {
        debug_assert!(self.piece_count() > piece_index.0 as u32);
        let offset = piece_index.0 as usize * 20;
        &self.hashes[offset..offset + 20]
    }

    pub fn check_downloaded_pieces(&self) -> PieceSet {
        let mut piece_set = PieceSet::new_empty(self);
        let mut buf: Vec<u8> = repeat(0u8).take(self.piece_length as usize).collect();
        let first_handle = match File::open(&self.files[0].path) {
            Ok(handle) => Some(handle),
            Err(ref e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => panic!("IoError when trying to check pieces: {:?}", e.kind())
        };
        let mut cur_file_index = 0;
        let mut cur_file_handle = first_handle;
        let mut piece_hash = [0;20];
        'pieces: for i in (0u32..self.piece_count()) {
            let piece_index = PieceIndex(i);
            let mut offset_in_piece = 0;
            for file_section in self.map_block(piece_index, 0,
                                        self.get_piece_length(piece_index)) {
                let buf_section = &mut buf[offset_in_piece as usize..
                                            // FIXME: add offset to end index
                                            // TODO: this seems to work, but why?
                                           file_section.length as usize];
                if cur_file_index < file_section.file_index {
                    cur_file_index += 1;
                    cur_file_handle = match File::open(&self.files[0].path) {
                        Ok(handle) => Some(handle),
                        Err(ref e) if e.kind() == ErrorKind::NotFound => None,
                        Err(e)=>
                            panic!("IoError when trying to check pieces: {:?}", e.kind())
                    }
                }

                let mut cur_file_finished = false;
                match cur_file_handle {
                    Some(ref mut handle) => {
                        let read = handle.read(buf_section).unwrap();
                        if read < file_section.length as usize {
                            // file is not long enough to be complete
                            cur_file_finished = true;
                        }
                    },
                    None => continue 'pieces // file does not exist
                }
                // handled down here to appease the borrow checker
                if cur_file_finished {
                    cur_file_handle = None;
                    continue 'pieces;
                }
                offset_in_piece += file_section.length;
            }
        
            let mut hasher = Sha1::new();
            hasher.input(&buf[..self.get_piece_length(piece_index) as usize]);
            hasher.result(&mut piece_hash);
            if piece_hash == self.get_piece_hash(piece_index) {
                piece_set.set_true(piece_index);
            }
        }
        piece_set
    }

    pub fn bytes_left_to_download(&self, pieces: &PieceSet) -> u64 {
        let mut downloaded_bytes =
            (pieces.count() as u64) * (self.piece_length as u64);
        let last_piece_index = PieceIndex(self.piece_count() - 1);
        if pieces[last_piece_index] {
            downloaded_bytes -= (self.piece_length -
                                 self.get_piece_length(last_piece_index)) as u64
        }
        self.total_size - downloaded_bytes
    }
}

struct FileSectionIter<'a> {
    torrent: &'a TorrentInfo,
    cur_file_index: usize,
    cur_file_offset: u64,
    piece_remaining_bytes: u32,
}

struct FileSection {
    pub file_index: usize,
    pub offset: u64,
    pub length: u32
}

impl <'a> Iterator for FileSectionIter<'a> {

    type Item = FileSection;

    fn next(&mut self) -> Option<<Self as Iterator>::Item> {
        if self.piece_remaining_bytes == 0
                || self.cur_file_index >= self.torrent.files.len() {
            return None;
        }
        let cur_file_offset = self.cur_file_offset;
        let cur_file_index = self.cur_file_index;
        let cur_file = &self.torrent.files[cur_file_index];
        let section_length = min((cur_file.length - cur_file_offset) as u32,
                                 self.piece_remaining_bytes);
        self.cur_file_index += 1;
        self.cur_file_offset = 0; // only the first section of a piece can
                                  // start at a file offset > 0
        self.piece_remaining_bytes -= section_length;
        Some(
            FileSection {
                file_index: cur_file_index,
                offset: cur_file_offset,
                length: section_length
            }
        )
    }
}

impl <'a> FileSectionIter<'a> {
    fn new(piece_index: PieceIndex, offset: u32, length: u32,
           torrent: &'a TorrentInfo) -> FileSectionIter<'a> {
        let mut remaining_offset =
            piece_index.0 as u64 * torrent.piece_length as u64 + offset as u64;
        let mut file_index = 0;
        loop {
            let cur_file_length = torrent.files[file_index].length;
            if remaining_offset < cur_file_length {
                return FileSectionIter {
                    torrent: torrent,
                    cur_file_index: file_index,
                    cur_file_offset: remaining_offset,
                    piece_remaining_bytes: length
                };
            } else {
                remaining_offset -= cur_file_length;
            }
            file_index += 1;
        }
    }
}

impl <'a>FromBValue<'a> for FileInfo {
    fn from_bvalue(bvalue: BValue<'a>) -> ConversionResult<FileInfo> {
        match bvalue {
            BValue::Dict(mut dict, _) => {
                match (dict.remove(&b"path"[..]),
                       dict.remove(&b"length"[..])) {
                    (Some(bv_path), Some(bv_length)) => {
                        let path = try!(FromBValue::from_bvalue(bv_path));
                        let length = try!(FromBValue::from_bvalue(bv_length));
                        Ok(FileInfo{path: path, length: length})
                    }
                    _ => Err(KeyDoesNotExist)
                }
            }
            _ => Err(WrongBValueConstructor)
        }
    }
}

impl <'a> FromBValue<'a> for PathBuf {
    fn from_bvalue(bvalue: BValue<'a>) -> ConversionResult<PathBuf> {
        match bvalue {
            BValue::List(l) => {
                let mut path = PathBuf::new();
                for segment in l {
                    let seg: &str = try!(FromBValue::from_bvalue(segment));
                    path.push(seg);
                }
                Ok(path)
            },
            _ => Err(WrongBValueConstructor)
        }
    }
}

impl <'a> FromBValue<'a> for TorrentInfo {
    fn from_bvalue(info_dict: BValue<'a>) -> ConversionResult<TorrentInfo> {
        match info_dict {
            BValue::Dict(mut dict, dict_str) => {
                match (dict.remove(&b"piece length"[..]),
                        dict.remove(&b"pieces"[..]),
                        dict.remove(&b"name"[..])) {
                    (Some(bv_piece_len), Some(bv_hashes), Some(bv_name)) => {
                        let name: String = try!(FromBValue::from_bvalue(bv_name));
                        let files = match dict.remove(&b"files"[..]) {
                            Some(bv_files) => try!(FromBValue::from_bvalue(bv_files)),
                            None => {
                                match dict.remove(&b"length"[..]) {
                                    Some(bv_length) => {
                                        let length = try!(FromBValue::from_bvalue(bv_length));
                                        vec![FileInfo {
                                                path: PathBuf::from(&name),
                                                length: length
                                        }]
                                    },
                                    None => return Err(KeyDoesNotExist)
                                }
                            }
                        };
                        let hashes = try!(FromBValue::from_bvalue(bv_hashes));
                        let piece_len_usize: usize =
                            try!(FromBValue::from_bvalue(bv_piece_len));
                        let piece_len = piece_len_usize as u32;
                        // TODO: make error handling more sensible
                        match TorrentInfo::new(name, piece_len, hashes, files, dict_str) {
                            Some(ti) => Ok(ti),
                            None => Err(OtherError)
                        }
                    },
                    _ => Err(KeyDoesNotExist)
                }
            },
            _ => Err(WrongBValueConstructor)
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use piece_set::PieceSet;
    use types::PieceIndex;

    #[test]
    fn test_bytes_left_to_download() {
        let file_info = FileInfo{path: PathBuf::from("foo"), length: 10};
        let torrent = TorrentInfo::new("".to_string(), 3, vec![0;4 * 20],
                                       vec![file_info], b"").unwrap();
        let mut pieces = PieceSet::new_empty(&torrent);
        assert_eq!(torrent.bytes_left_to_download(&pieces), 10);

        pieces.set_true(PieceIndex(0));
        assert_eq!(torrent.bytes_left_to_download(&pieces), 7);

        pieces.set_true(PieceIndex(3));
        assert_eq!(torrent.bytes_left_to_download(&pieces), 6);

        pieces.set_true(PieceIndex(1));
        assert_eq!(torrent.bytes_left_to_download(&pieces), 3);
    }
}
