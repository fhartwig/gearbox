use rand;
use rand::Rng;
use std::collections::BitVec;
use std::ops::Index;

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
    pub fn set_false(&mut self, index: PieceIndex) {
        self.bitv.set(index.0 as usize, false);
        self.piece_count -= 1;
    }

    /// panics if index is out of bounds
    pub fn set_true(&mut self, index: PieceIndex) {
        self.bitv.set(index.0 as usize, true);
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
    // FIXME: write tests for this
    pub fn pick_piece_from(&mut self, other: &Self) -> Option<PieceIndex> {
        let new_pieces = self.bitv.iter().zip(other.bitv.iter())
                             .filter(|&(a, b)| a && b).count();
        if new_pieces == 0 {
            return None;
        }
        let randint = rand::thread_rng().gen_range(0, new_pieces);
        let n = self.bitv.iter().zip(&other.bitv).enumerate()
                    // TODO: why did this work with == instead of &&?
                    //.filter(|&(_, t)| t.0 == t.1).nth(randint).unwrap().0;
                    .filter(|&(_, t)| t.0 && t.1).nth(randint).unwrap().0;
        let picked_index = PieceIndex(n as u32);

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

    fn index<'a>(&'a self, index: PieceIndex) -> &bool {
        &self.bitv[index.0 as usize]
    }
}
