use std::io::prelude::*;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::SeekFrom;
use std::str;
use std::mem;
use std::fmt::Debug;

use page;
use page::{Page, PAGE_SIZE, HEADER_SIZE};
use util::{mem_move, deserialize, deserialize_kv};

use bincode;
use bincode::{serialize, deserialize as bin_deserialize,
              Bounded};
use serde::ser::Serialize;
use serde::de::{Deserialize, DeserializeOwned};

pub struct DbFile {
    path: String,
    // TODO: don't use separate cntrl file; use 0th page instead. This
    // will require an "address translation" mechanism since the
    // linear hashtable methods expect page 0 to be available.
    ctrl_file: File,
    file: File,
    ctrl_buffer: Page,
    pub buffer: Page,
    // which page is currently in `buffer`
    page_id: Option<usize>,
    tuples_per_page: usize,
    // changes made to `buffer`?
    dirty: bool,
    overflow_free: usize,
}

impl DbFile {
    pub fn new<K,V>(filename: &str) -> DbFile {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(filename);
        let file = match file {
            Ok(f) => f,
            Err(e) => panic!(e),
        };

        let mut ctrl_filename = String::from(filename);
        ctrl_filename.push_str("_ctrl");

        let ctrl_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(ctrl_filename);
        let ctrl_file = match ctrl_file {
            Ok(f) => f,
            Err(e) => panic!(e),
        };
        let keysize = mem::size_of::<K>();
        let valsize = mem::size_of::<V>();
        let total_size = keysize + valsize;
        let tuples_per_page = PAGE_SIZE / total_size;
        DbFile {
            path: String::from(filename),
            file: file,
            ctrl_file: ctrl_file,
            ctrl_buffer: Page::new(0, 0),
            buffer: Page::new(keysize, valsize),
            page_id: None,
            tuples_per_page: tuples_per_page,
            dirty: false,
            overflow_free: 50,
        }
    }

    // Control page layout:
    // | nbits | nitems | nbuckets | ....
    pub fn read_ctrlpage(&mut self) -> (usize, usize, usize) {
        self.get_ctrl_page();
        let nbits : usize =
            deserialize(&self.ctrl_buffer.storage[0..8]).unwrap();
        let nitems : usize =
            deserialize(&self.ctrl_buffer.storage[8..16]).unwrap();
        let nbuckets : usize =
            deserialize(&self.ctrl_buffer.storage[16..24]).unwrap();
        self.overflow_free =
            deserialize(&self.ctrl_buffer.storage[24..32]).unwrap();
        (nbits, nitems, nbuckets)
    }

    pub fn write_ctrlpage(&mut self,
                          (nbits, nitems, nbuckets):
                          (usize, usize, usize)) {
        self.get_ctrl_page();
        let nbits_bytes = &serialize(&nbits, Bounded(8)).unwrap();
        let nitems_bytes = &serialize(&nitems, Bounded(8)).unwrap();
        let nbuckets_bytes = &serialize(&nbuckets, Bounded(8)).unwrap();
        let overflow_free_bytes = &serialize(&self.overflow_free, Bounded(8)).unwrap();
        println!("nbits: {:?} nitems: {:?} nbuckets: {:?}", nbits_bytes,
                 nitems_bytes, nbuckets_bytes);
        mem_move(&mut self.ctrl_buffer.storage[0..8],
                 nbits_bytes);
        mem_move(&mut self.ctrl_buffer.storage[8..16],
                 nitems_bytes);
        mem_move(&mut self.ctrl_buffer.storage[16..24],
                 nbuckets_bytes);
        mem_move(&mut self.ctrl_buffer.storage[24..32],
                 overflow_free_bytes);
        DbFile::write_page(&mut self.ctrl_file,
                           0,
                           &self.ctrl_buffer.storage);
    }

    fn read_header(&mut self) {
        let num_tuples : usize = deserialize(&self.buffer.storage[0..8]).unwrap();
        let next : usize = deserialize(&self.buffer.storage[8..16]).unwrap();
        let prev : usize = deserialize(&self.buffer.storage[16..24]).unwrap();
        self.buffer.num_tuples = num_tuples;
        self.buffer.next = if next != 0 {
            Some(next)
        } else {
            None
        };
        self.buffer.prev = if prev != 0 {
            Some(prev)
        } else {
            None
        };
    }

    fn write_header(&mut self) {
        mem_move(&mut self.buffer.storage[0..8],
                 &serialize(&self.buffer.num_tuples, Bounded(8)).unwrap());
        mem_move(&mut self.buffer.storage[8..16],
                 &serialize(&self.buffer.next, Bounded(8)).unwrap());
        mem_move(&mut self.buffer.storage[16..24],
                 &serialize(&self.buffer.prev, Bounded(8)).unwrap());
    }

    pub fn get_ctrl_page(&mut self) {
        self.ctrl_file.seek(SeekFrom::Start(0))
            .expect("Could not seek to offset");
        self.ctrl_file.read(&mut self.ctrl_buffer.storage)
            .expect("Could not read file");
    }

    // Reads page to self.buffer
    pub fn get_page(&mut self, page_id: usize) {
        match self.page_id {
            Some(p) if p == page_id => (),
            Some(_) | None => {
                self.dirty = false;
                let offset = (page_id * PAGE_SIZE) as u64;
                self.file.seek(SeekFrom::Start(offset))
                    .expect("Could not seek to offset");
                self.file.read(&mut self.buffer.storage)
                    .expect("Could not read file");
                self.page_id = Some(page_id);
                self.buffer.id = page_id;
                self.read_header();
            },
        }
    }

    // Writes data in self.buffer into page `page_id`
    pub fn write_page(mut file: &File, page_id: usize, data: &[u8]) {
        let offset = (page_id * PAGE_SIZE) as u64;
        file.seek(SeekFrom::Start(offset))
            .expect("Could not seek to offset");
        println!("wrote {:?} bytes from offset {}",
                 file.write(data), offset);
        file.flush().expect("flush failed");
    }

    pub fn write_tuple<K, V>(&mut self, page_id: usize, row_num: usize, key: K, val: V)
        where K: Serialize,
              V: Serialize {
        self.get_page(page_id);

        // The maximum sizes of the encoded key and val.
        let key_limit = Bounded(mem::size_of::<K>() as u64);
        let val_limit = Bounded(mem::size_of::<V>() as u64);

        self.dirty = true;
        self.buffer.write_tuple(row_num,
                                &serialize(&key, key_limit).unwrap(),
                                &serialize(&val, val_limit).unwrap());
        self.write_buffer();
    }

    pub fn search_bucket<K, V>(&mut self, page_id: usize, key: K) -> (Option<usize>, Option<V>)
        where K: Serialize + Debug,
              V: DeserializeOwned + Debug {
        println!("[get] page_id: {}", page_id);
        self.get_page(page_id);
        let key_size = mem::size_of::<K>() as u64;
        let key_bytes = serialize(&key, Bounded(key_size)).unwrap();

        match self.buffer.search_bucket(&key_bytes) {
            (Some(index), Some(val_bytes)) =>
                (Some(index), Some(deserialize(&val_bytes).unwrap())),
            (Some(index), None) => (Some(index), None),
            _ => (None, None),
        }
    }

    pub fn put<K,V>(&mut self, page_id: usize, key: K, val: V)
        where K: Serialize,
              V: Serialize {
        println!("[put] page_id: {}", page_id);
        self.get_page(page_id);
        let key_size = mem::size_of::<K>() as u64;
        let val_size = mem::size_of::<V>() as u64;
        let key_bytes = serialize(&key, Bounded(key_size)).unwrap();
        let val_bytes = serialize(&val, Bounded(val_size)).unwrap();
        self.dirty = true;
        self.buffer.put(&key_bytes, &val_bytes);
        // TODO: avoid writing to file after every update. ie. only
        // write to file once the page needs to get evicted from
        // `buffer`.
        self.write_buffer();
    }

    /// Write out page in `buffer` to file.
    pub fn write_buffer(&mut self) {
        self.dirty = false;
        self.write_header();
        DbFile::write_page(&mut self.file,
                           self.page_id.expect("No page buffered"),
                           &self.buffer.storage);
    }

    /// Returns a vector with all tuples in the block
    fn all_tuples_in_page<K, V>(&mut self, page_id: usize)
                                    -> Vec<(K,V)>
        where K: DeserializeOwned + Debug,
              V: DeserializeOwned + Debug {
        self.get_page(page_id);
        let mut records = Vec::new();
        for i in 0..self.buffer.num_tuples {
            let (k, v) = self.buffer.read_tuple(i);
            let (dk, dv) : (K, V) = deserialize_kv::<K,V>(&k, &v);
            records.push((dk, dv));
        }

        records
    }

    /// Clear out block `page_id` in disk. Returns a list of all
    /// tuples that were present in the block.
    pub fn allocate_new_page<K,V>(&mut self, page_id: usize) -> Vec<(K,V)>
        where K: DeserializeOwned + Debug,
              V: DeserializeOwned + Debug {
        let keysize = mem::size_of::<K>();
        let valsize = mem::size_of::<V>();
        let new_page = Page::new(keysize, valsize);
        let tuples = self.all_tuples_in_page::<K,V>(page_id);
        mem::replace(&mut self.buffer, new_page);
        self.buffer.id = page_id;
        self.page_id = Some(page_id);
        self.dirty = false;
        self.write_buffer();

        tuples
    }
}
