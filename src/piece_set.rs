use bit_vec::BitVec;
use std::ops::Index;

use torrent_info::TorrentInfo;
use types::PieceIndex;

use rand::{thread_rng, Rng};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceSet {
    bitv: BitVec,
    piece_count: u32
}

impl PieceSet {
    fn from_elem(torrent: &TorrentInfo, elem: bool) -> PieceSet {
        let pieces = torrent.piece_count();
        PieceSet {
            bitv: BitVec::from_elem(pieces as usize, elem),
            piece_count: if elem {pieces} else {0}
        }
    }

    pub fn new_empty(torrent: &TorrentInfo) -> PieceSet {
        PieceSet::from_elem(torrent, false)
    }

    pub fn inverse(&self) -> PieceSet {
        let mut new_vec = self.bitv.clone();
        new_vec.negate();
        PieceSet {
            bitv: new_vec,
            piece_count: self.bitv.len() as u32 - self.piece_count
        }
    }

    pub fn count(&self) -> u32 {
        self.piece_count
    }

    /// panics if index is out of bounds
    pub fn set_false(&mut self, index: PieceIndex) {
        self.bitv.set(u32::from(index) as usize, false);
        self.piece_count -= 1;
    }

    /// panics if index is out of bounds
    pub fn set_true(&mut self, index: PieceIndex) {
        self.bitv.set(u32::from(index) as usize, true);
        self.piece_count += 1;
    }

    pub fn is_complete(&self) -> bool {
        self.bitv.capacity() as u32 == self.piece_count
    }

    pub fn is_empty(&self) -> bool {
        self.piece_count == 0
    }

    /// picks a random index that is set in both input PieceSets, removing
    /// the piece index from self
    pub fn pick_piece_from(&mut self, other: &Self) -> Option<PieceIndex> {
        let new_pieces = self.bitv.iter().zip(other.bitv.iter())
                             .filter(|&(a, b)| a && b).count();
        if new_pieces == 0 {
            return None;
        }
        let randint = thread_rng().gen_range(0, new_pieces);
        let n = self.bitv.iter().zip(&other.bitv).enumerate()
                    .filter(|&(_, t)| t.0 && t.1).nth(randint).unwrap().0;
        let picked_index = PieceIndex::from(n as u32);

        self.set_false(picked_index);
        info!("Picked: {:?}", picked_index);
        Some(picked_index)
    }

    pub fn has_new_pieces(&self, other: &Self) -> bool {
        self.bitv.iter().zip(other.bitv.iter()).any(|(a, b)| a && b)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.bitv.to_bytes()
    }
}

impl Index<PieceIndex> for PieceSet {
    type Output = bool;

    fn index(&self, index: PieceIndex) -> &bool {
        &self.bitv[u32::from(index) as usize]
    }
}

#[cfg(test)]
mod tests {
    use torrent_info::{TorrentInfo, FileInfo};
    use super::PieceSet;
    use types::PieceIndex;

    #[test]
    fn test_pick_piece_from() {
        let dummy_file =
            FileInfo { path: From::from("abcd"), length: 512 * 1024 * 1024 };
        let torrent = TorrentInfo::new("dummy_name".to_string(), 1,
                                       vec![0u8;60],
                                       vec![dummy_file], b"").unwrap();
        let mut to_download = PieceSet::new_empty(&torrent).inverse();
        let peers_pieces = PieceSet::new_empty(&torrent).inverse();
        assert!(to_download.pick_piece_from(&peers_pieces).is_some());

        let mut to_download = PieceSet::new_empty(&torrent);
        to_download.set_true(PieceIndex::from(0));
        assert_eq!(to_download.pick_piece_from(&peers_pieces),
                   Some(PieceIndex::from(0)));
        assert!(to_download.pick_piece_from(&peers_pieces).is_none());
    }
}
