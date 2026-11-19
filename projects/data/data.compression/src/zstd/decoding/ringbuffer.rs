use std::{
    collections::VecDeque,
    io::{self, Read},
};

pub(super) struct RingBuffer(VecDeque<u8>);

impl RingBuffer {
    pub fn new() -> Self {
        Self(VecDeque::new())
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }
    pub fn reserve(&mut self, n: usize) {
        self.0.reserve(n);
    }
    pub fn as_slices(&self) -> (&[u8], &[u8]) {
        self.0.as_slices()
    }
    pub fn extend(&mut self, bytes: &[u8]) {
        self.0.extend(bytes.iter().copied());
    }
    pub fn drop_first_n(&mut self, n: usize) {
        self.0.drain(..n);
    }

    pub fn extend_and_fill(&mut self, byte: u8, n: usize) {
        self.0.resize(self.0.len() + n, byte);
    }

    pub fn extend_from_reader(&mut self, mut reader: impl Read, mut n: usize) -> io::Result<()> {
        let mut chunk = [0; 8192];
        while n > 0 {
            let count = n.min(chunk.len());
            reader.read_exact(&mut chunk[..count])?;
            self.extend(&chunk[..count]);
            n -= count;
        }
        Ok(())
    }

    pub fn repeat(&mut self, offset: usize, n: usize) {
        self.0.reserve(n);
        let start = self.0.len() - offset;
        for pos in start..start + n {
            self.0.push_back(self.0[pos]);
        }
    }
}
