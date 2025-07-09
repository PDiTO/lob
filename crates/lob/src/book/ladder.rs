//! One side of the book: a sorted array of `(price, level index)` pairs.
//!
//! Entries are kept sorted so the best price is always the *last* element. Almost
//! all activity in a real book happens within a few ticks of the touch, so:
//!
//! * reading or removing the best level is `Vec::last` / `Vec::pop`, O(1);
//! * inserting a new level near the touch shifts only the handful of entries on the
//!   better side of it;
//! * finding an arbitrary level is a binary search over a contiguous array of
//!   16-byte entries, which stays in a few cache lines for typical depths.
//!
//! To store both sides with the same "best is last, ascending order" layout, asks
//! are keyed by `!price` (bitwise not), which reverses the order without the
//! overflow edge case of negation.

use crate::types::{Price, Side};

#[derive(Clone, Debug)]
pub(crate) struct Ladder {
    side: Side,
    entries: Vec<(i64, u32)>,
}

impl Ladder {
    pub(crate) fn new(side: Side) -> Self {
        Self {
            side,
            entries: Vec::with_capacity(256),
        }
    }

    #[inline]
    fn key(&self, price: Price) -> i64 {
        match self.side {
            Side::Bid => price,
            Side::Ask => !price,
        }
    }

    #[inline]
    fn price(&self, key: i64) -> Price {
        match self.side {
            Side::Bid => key,
            Side::Ask => !key,
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Best price and its level index.
    #[inline]
    pub(crate) fn best(&self) -> Option<(Price, u32)> {
        self.entries.last().map(|&(k, l)| (self.price(k), l))
    }

    /// Position of `price`: `Ok(i)` if present, `Err(i)` for where it would go.
    ///
    /// Checks the best entry first because new orders overwhelmingly join or
    /// improve the touch.
    #[inline]
    fn search(&self, price: Price) -> Result<usize, usize> {
        let key = self.key(price);
        match self.entries.last() {
            None => return Err(0),
            Some(&(k, _)) if key > k => return Err(self.entries.len()),
            Some(&(k, _)) if key == k => return Ok(self.entries.len() - 1),
            _ => {}
        }
        self.entries.binary_search_by_key(&key, |&(k, _)| k)
    }

    #[inline]
    pub(crate) fn find(&self, price: Price) -> Option<u32> {
        self.search(price).ok().map(|i| self.entries[i].1)
    }

    /// Returns the level at `price`, or calls `make` to create one and inserts it.
    #[inline]
    pub(crate) fn find_or_insert_with(&mut self, price: Price, make: impl FnOnce() -> u32) -> u32 {
        match self.search(price) {
            Ok(i) => self.entries[i].1,
            Err(i) => {
                let level = make();
                let key = self.key(price);
                self.entries.insert(i, (key, level));
                level
            }
        }
    }

    /// Removes the level at `price`, returning its index.
    #[inline]
    pub(crate) fn remove(&mut self, price: Price) -> Option<u32> {
        let key = self.key(price);
        if let Some(&(k, l)) = self.entries.last()
            && k == key
        {
            self.entries.pop();
            return Some(l);
        }
        match self.entries.binary_search_by_key(&key, |&(k, _)| k) {
            Ok(i) => Some(self.entries.remove(i).1),
            Err(_) => None,
        }
    }

    /// Levels from best to worst.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (Price, u32)> + '_ {
        self.entries.iter().rev().map(|&(k, l)| (self.price(k), l))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bids_best_is_highest() {
        let mut l = Ladder::new(Side::Bid);
        for (i, p) in [100, 102, 101, 99].into_iter().enumerate() {
            l.find_or_insert_with(p, || i as u32);
        }
        assert_eq!(l.best(), Some((102, 1)));
        let prices: Vec<_> = l.iter().map(|(p, _)| p).collect();
        assert_eq!(prices, vec![102, 101, 100, 99]);
    }

    #[test]
    fn asks_best_is_lowest_including_negative_prices() {
        let mut l = Ladder::new(Side::Ask);
        for (i, p) in [5, -3, 0, i64::MIN, i64::MAX].into_iter().enumerate() {
            l.find_or_insert_with(p, || i as u32);
        }
        let prices: Vec<_> = l.iter().map(|(p, _)| p).collect();
        assert_eq!(prices, vec![i64::MIN, -3, 0, 5, i64::MAX]);
        assert_eq!(l.remove(-3), Some(1));
        assert_eq!(l.remove(-3), None);
        assert_eq!(l.find(0), Some(2));
        assert_eq!(l.len(), 4);
    }

    #[test]
    fn existing_level_is_reused() {
        let mut l = Ladder::new(Side::Bid);
        assert_eq!(l.find_or_insert_with(10, || 7), 7);
        assert_eq!(l.find_or_insert_with(10, || unreachable!()), 7);
        assert_eq!(l.len(), 1);
    }
}
