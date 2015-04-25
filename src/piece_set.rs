use rand;
use rand::Rng;
use std::collections::BitVec;

use torrent_info::TorrentInfo;
use types::PieceIndex;

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
    pub fn get(&self, index: PieceIndex) -> bool {
        self.bitv[index as usize] 
    }

    /// panics if index is out of bounds
    pub fn set_false(&mut self, index: PieceIndex) {
        self.bitv.set(index as usize, false);
        self.piece_count -= 1;
    }

    /// panics if index is out of bounds
    pub fn set_true(&mut self, index: PieceIndex) {
        self.bitv.set(index as usize, true);
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
        let possible_pieces = 
            self.bitv.iter().zip(other.bitv.iter()).filter(|&(a, b)| a && b)
            .count();
        println!("Possible pieces: {}", possible_pieces);
        if possible_pieces == 0 {
            return None;
        }
        let randint = rand::thread_rng().gen_range(0, possible_pieces);
        println!("randint: {}", randint);
        let mut i = 0;
        let mut picked_index = 0u32;
        // TODO: use filter() and nth() here
        for (index, (a, b)) in self.bitv.iter().zip(other.bitv.iter()).enumerate() {
            if a && b {
                if i == randint {
                    picked_index = index as u32;
                    break;
                }
                i += 1;
            }
        }

        self.set_false(picked_index);
        println!("Picked: {}", picked_index);
        Some(picked_index)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.bitv.to_bytes()
    }
}
