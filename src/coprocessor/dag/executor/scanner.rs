// Copyright 2017 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use kvproto::coprocessor::KeyRange;

use coprocessor::codec::table::truncate_as_row_key;
use coprocessor::util;
use std::cmp::max;
use storage::txn::Result;
use storage::{Key, ScanMode, Snapshot, SnapshotStore, Statistics, StoreScanner, Value};
use util::escape;

const MIN_KEY_BUFFER_CAPACITY: usize = 256;

#[derive(Copy, Clone)]
pub enum ScanOn {
    Table,
    Index,
}

// `Scanner` is a helper struct to wrap all common scan operations
// for `TableScanExecutor` and `IndexScanExecutor`
pub struct Scanner<S: Snapshot> {
    scan_mode: ScanMode,
    scan_on: ScanOn,
    key_only: bool,
    seek_key: Vec<u8>,
    scanner: StoreScanner<S>,
    range: KeyRange,
    no_more: bool,
    // statistics_cache caches Statistics because
    // reset_range may re-initialize a StoreScanner.
    statistics_cache: Statistics,
}

impl<S: Snapshot> Scanner<S> {
    pub fn new(
        store: &SnapshotStore<S>,
        scan_on: ScanOn,
        desc: bool,
        key_only: bool,
        range: KeyRange,
    ) -> Result<Self> {
        let scan_mode = if desc {
            ScanMode::Backward
        } else {
            ScanMode::Forward
        };

        let seek_key = {
            let seek_key_slice = if desc {
                range.get_end()
            } else {
                range.get_start()
            };
            let mut buffer = Vec::with_capacity(max(MIN_KEY_BUFFER_CAPACITY, seek_key_slice.len()));
            buffer.extend_from_slice(seek_key_slice);
            buffer
        };

        let scanner = Self::range_scanner(store, scan_mode, key_only, &range)?;

        Ok(Self {
            scan_mode,
            scan_on,
            key_only,
            seek_key,
            scanner,
            range,
            no_more: false,
            statistics_cache: Statistics::default(),
        })
    }

    fn range_scanner(
        store: &SnapshotStore<S>,
        scan_mode: ScanMode,
        key_only: bool,
        range: &KeyRange,
    ) -> Result<StoreScanner<S>> {
        let lower_bound = Some(Key::from_raw(range.get_start()).take_encoded());
        let upper_bound = Some(Key::from_raw(range.get_end()).take_encoded());
        store.scanner(scan_mode, key_only, lower_bound, upper_bound)
    }

    pub fn reset_range(&mut self, range: KeyRange, store: &SnapshotStore<S>) -> Result<()> {
        self.range = range;
        self.no_more = false;
        unsafe {
            self.seek_key.set_len(0);
        }
        match self.scan_mode {
            ScanMode::Backward => self.seek_key.extend_from_slice(self.range.get_end()),
            ScanMode::Forward => self.seek_key.extend_from_slice(self.range.get_start()),
        };

        self.statistics_cache.add(&self.scanner.take_statistics());
        self.scanner = Self::range_scanner(store, self.scan_mode, self.key_only, &self.range)?;
        Ok(())
    }

    pub fn next_row(&mut self) -> Result<Option<(Vec<u8>, Value)>> {
        if self.no_more {
            return Ok(None);
        }

        let kv = self.scanner.next()?;

        let (key, value) = match kv {
            Some((k, v)) => (box_try!(k.raw()), v),
            None => {
                self.no_more = true;
                return Ok(None);
            }
        };

        if self.range.start > key || self.range.end <= key {
            panic!(
                "key: {} out of range [{}, {})",
                escape(&key),
                escape(self.range.get_start()),
                escape(self.range.get_end())
            );
        }

        {
            let seek_key_slice = match (self.scan_mode, self.scan_on) {
                (ScanMode::Forward, _) | (ScanMode::Backward, ScanOn::Index) => key.as_slice(),
                (ScanMode::Backward, ScanOn::Table) => box_try!(truncate_as_row_key(&key)),
            };
            unsafe {
                self.seek_key.set_len(0);
            }
            self.seek_key.extend_from_slice(seek_key_slice);
        }

        Ok(Some((key, value)))
    }

    pub fn start_scan(&self, range: &mut KeyRange) {
        assert!(!self.no_more);
        match self.scan_mode {
            ScanMode::Forward => range.set_start(self.seek_key.clone()),
            ScanMode::Backward => range.set_end(self.seek_key.clone()),
        };
    }

    pub fn stop_scan(&mut self, range: &mut KeyRange) -> bool {
        if self.no_more {
            return false;
        }

        match self.scan_mode {
            ScanMode::Forward => {
                // Increase seek_key, so that stop_scan returns a key that is exclusive, producing
                // a half-close range
                util::convert_to_prefix_next(&mut self.seek_key);
                range.set_end(self.seek_key.clone())
            }
            ScanMode::Backward => range.set_start(self.seek_key.clone()),
        };
        true
    }

    pub fn collect_statistics_into(&mut self, stats: &mut Statistics) {
        stats.add(&self.statistics_cache);
        stats.add(&self.scanner.take_statistics());
    }
}

#[cfg(test)]
pub mod test {
    use std::i64;

    use kvproto::kvrpcpb::{Context, IsolationLevel};
    use tipb::schema::ColumnInfo;

    use coprocessor::codec::datum::{self, Datum};
    use coprocessor::codec::mysql::types;
    use coprocessor::codec::table;
    use coprocessor::util;
    use storage::engine::{self, Engine, Modify, RocksEngine, RocksSnapshot, TEMP_DIR};
    use storage::mvcc::MvccTxn;
    use storage::{Key, Mutation, Options, SnapshotStore, ALL_CFS};
    use util::collections::HashMap;

    use super::*;

    pub fn new_col_info(cid: i64, tp: u8) -> ColumnInfo {
        let mut col_info = ColumnInfo::new();
        col_info.set_tp(i32::from(tp));
        col_info.set_column_id(cid);
        col_info
    }

    pub struct Data {
        pub kv_data: Vec<(Vec<u8>, Vec<u8>)>,
        // expect_rows[row_id][column_id]=>value
        pub expect_rows: Vec<HashMap<i64, Vec<u8>>>,
        pub cols: Vec<ColumnInfo>,
    }

    impl Data {
        pub fn get_prev_2_cols(&self) -> Vec<ColumnInfo> {
            let col1 = self.cols[0].clone();
            let col2 = self.cols[1].clone();
            vec![col1, col2]
        }

        pub fn get_col_pk(&self) -> ColumnInfo {
            let mut pk_col = new_col_info(0, types::LONG);
            pk_col.set_pk_handle(true);
            pk_col
        }
    }

    pub fn prepare_table_data(key_number: usize, table_id: i64) -> Data {
        let cols = vec![
            new_col_info(1, types::LONG_LONG),
            new_col_info(2, types::VARCHAR),
            new_col_info(3, types::NEW_DECIMAL),
        ];

        let mut kv_data = Vec::new();
        let mut expect_rows = Vec::new();

        for handle in 0..key_number {
            let row = map![
                1 => Datum::I64(handle as i64),
                2 => Datum::Bytes(b"abc".to_vec()),
                3 => Datum::Dec(10.into())
            ];
            let mut expect_row = HashMap::default();
            let col_ids: Vec<_> = row.iter().map(|(&id, _)| id).collect();
            let col_values: Vec<_> = row
                .iter()
                .map(|(cid, v)| {
                    let f = table::flatten(v.clone()).unwrap();
                    let value = datum::encode_value(&[f]).unwrap();
                    expect_row.insert(*cid, value);
                    v.clone()
                })
                .collect();

            let value = table::encode_row(col_values, &col_ids).unwrap();
            let key = table::encode_row_key(table_id, handle as i64);
            expect_rows.push(expect_row);
            kv_data.push((key, value));
        }
        Data {
            kv_data,
            expect_rows,
            cols,
        }
    }

    const START_TS: u64 = 10;
    const COMMIT_TS: u64 = 20;

    pub struct TestStore {
        snapshot: RocksSnapshot,
        ctx: Context,
        engine: RocksEngine,
    }

    impl TestStore {
        pub fn new(kv_data: &[(Vec<u8>, Vec<u8>)]) -> TestStore {
            let engine = engine::new_local_engine(TEMP_DIR, ALL_CFS).unwrap();
            let ctx = Context::new();
            let snapshot = engine.snapshot(&ctx).unwrap();
            let mut store = TestStore {
                snapshot,
                ctx,
                engine,
            };
            store.init_data(kv_data);
            store
        }

        fn init_data(&mut self, kv_data: &[(Vec<u8>, Vec<u8>)]) {
            if kv_data.is_empty() {
                return;
            }

            // do prewrite.
            let txn_motifies = {
                let mut txn = MvccTxn::new(self.snapshot.clone(), START_TS, true).unwrap();
                let mut pk = vec![];
                for &(ref key, ref value) in kv_data {
                    if pk.is_empty() {
                        pk = key.clone();
                    }
                    txn.prewrite(
                        Mutation::Put((Key::from_raw(key), value.to_vec())),
                        &pk,
                        &Options::default(),
                    ).unwrap();
                }
                txn.into_modifies()
            };
            self.write_modifies(txn_motifies);

            // do commit
            let txn_modifies = {
                let mut txn = MvccTxn::new(self.snapshot.clone(), START_TS, true).unwrap();
                for &(ref key, _) in kv_data {
                    txn.commit(&Key::from_raw(key), COMMIT_TS).unwrap();
                }
                txn.into_modifies()
            };
            self.write_modifies(txn_modifies);
        }

        #[inline]
        fn write_modifies(&mut self, txn: Vec<Modify>) {
            self.engine.write(&self.ctx, txn).unwrap();
            self.snapshot = self.engine.snapshot(&self.ctx).unwrap()
        }

        pub fn get_snapshot(&mut self) -> (RocksSnapshot, u64) {
            (self.snapshot.clone(), COMMIT_TS + 1)
        }
    }

    #[inline]
    pub fn get_range(table_id: i64, start: i64, end: i64) -> KeyRange {
        let mut key_range = KeyRange::new();
        key_range.set_start(table::encode_row_key(table_id, start));
        key_range.set_end(table::encode_row_key(table_id, end));
        key_range
    }

    pub fn get_point_range(table_id: i64, handle: i64) -> KeyRange {
        let start_key = table::encode_row_key(table_id, handle);
        let mut end = start_key.clone();
        util::convert_to_prefix_next(&mut end);
        let mut key_range = KeyRange::new();
        key_range.set_start(start_key);
        key_range.set_end(end);
        key_range
    }

    #[test]
    fn test_scan() {
        let table_id = 1;
        let pk = table::encode_row_key(table_id, 1);
        let pv = b"value1";
        let test_data = vec![
            (pk.clone(), pv.to_vec()),
            (table::encode_row_key(table_id, 2), b"value2".to_vec()),
        ];
        let mut test_store = TestStore::new(&test_data);
        let (snapshot, start_ts) = test_store.get_snapshot();
        let store = SnapshotStore::new(snapshot, start_ts, IsolationLevel::SI, true);
        let range = get_range(table_id, i64::MIN, i64::MAX);
        let mut scanner = Scanner::new(&store, ScanOn::Table, false, false, range).unwrap();
        for &(ref k, ref v) in &test_data {
            let (key, value) = scanner.next_row().unwrap().unwrap();
            assert_eq!(k, &key);
            assert_eq!(*v, value);
        }
        assert!(scanner.next_row().unwrap().is_none());
    }

    #[test]
    fn test_reverse_scan() {
        let table_id = 1;
        let key_number = 10;
        let mut data = prepare_table_data(key_number, table_id);
        let mut test_store = TestStore::new(&data.kv_data);
        let (snapshot, start_ts) = test_store.get_snapshot();
        let store = SnapshotStore::new(snapshot, start_ts, IsolationLevel::SI, true);
        let range = get_range(table_id, i64::MIN, i64::MAX);
        let mut scanner = Scanner::new(&store, ScanOn::Table, true, false, range).unwrap();
        data.kv_data.reverse();
        for &(ref k, ref v) in &data.kv_data {
            let (key, value) = scanner.next_row().unwrap().unwrap();
            assert_eq!(*k, key);
            assert_eq!(*v, value);
        }
        assert!(scanner.next_row().unwrap().is_none());
    }

    #[test]
    fn test_scan_key_only() {
        let table_id = 1;
        let pk = table::encode_row_key(table_id, 1);
        let pv = b"value1";
        let test_data = vec![
            (pk.clone(), pv.to_vec()),
            (table::encode_row_key(table_id, 2), b"value2".to_vec()),
        ];
        let mut test_store = TestStore::new(&test_data);
        let (snapshot, start_ts) = test_store.get_snapshot();
        let store = SnapshotStore::new(snapshot, start_ts, IsolationLevel::SI, true);
        let range = get_range(table_id, i64::MIN, i64::MAX);
        let mut scanner = Scanner::new(&store, ScanOn::Table, false, true, range).unwrap();
        let (_, value) = scanner.next_row().unwrap().unwrap();
        assert!(value.is_empty());
    }

    #[test]
    fn test_seek_key() {
        let table_id = 1;
        let pk = table::encode_row_key(table_id, 1);
        let pv = b"value1";
        let test_data = vec![(pk.clone(), pv.to_vec())];
        let mut test_store = TestStore::new(&test_data);
        let (snapshot, start_ts) = test_store.get_snapshot();
        let store = SnapshotStore::new(snapshot, start_ts, IsolationLevel::SI, true);
        let range = get_range(table_id, i64::MIN, i64::MAX);

        // 1. desc scan
        let scanner = Scanner::new(&store, ScanOn::Table, true, false, range.clone()).unwrap();
        assert_eq!(scanner.seek_key, range.get_end());

        // 2.asc scan
        let scanner = Scanner::new(&store, ScanOn::Table, false, false, range.clone()).unwrap();
        assert_eq!(scanner.seek_key, range.get_start());
    }

    #[test]
    fn test_scan_start_stop() {
        let table_id = 1;
        let pks = vec![1, 2, 3, 4, 5, 7, 10, 15, 20, 25, 26, 27];
        let values: Vec<_> = pks
            .iter()
            .map(|pk| format!("value{}", pk).into_bytes())
            .collect();
        let test_data: Vec<_> = pks
            .into_iter()
            .map(|pk| table::encode_row_key(table_id, pk))
            .zip(values.into_iter())
            .collect();
        let mut test_store = TestStore::new(&test_data);
        let (snapshot, start_ts) = test_store.get_snapshot();
        let store = SnapshotStore::new(snapshot, start_ts, IsolationLevel::SI, true);

        // `test_take` is used to take `count` keys from the scanner. It calls `start_scan` at
        // beginning, `stop_scan` in the end, producing a range. the range will be cheched against
        // `expect_start_pk` and `expect_end_pk`. Pass -1 as pk means the end.
        let test_take = |scanner: &mut Scanner<_>, count, expect_start_pk, expect_end_pk| {
            let mut range = KeyRange::new();
            scanner.start_scan(&mut range);

            let mut keys = Vec::new();
            for _ in 0..count {
                if let Some((key, _)) = scanner.next_row().unwrap() {
                    keys.push(key);
                } else {
                    break;
                }
            }

            let has_more = scanner.stop_scan(&mut range);
            if has_more || scanner.scan_mode == ScanMode::Forward {
                assert_eq!(
                    range.get_start(),
                    table::encode_row_key(table_id, expect_start_pk).as_slice()
                );
            } else {
                assert_eq!(expect_start_pk, -1);
            }
            if has_more || scanner.scan_mode == ScanMode::Backward {
                assert_eq!(
                    range.get_end(),
                    table::encode_row_key(table_id, expect_end_pk).as_slice()
                );
            } else {
                assert_eq!(expect_end_pk, -1);
            }

            keys
        };

        let range = get_range(table_id, 1, 26);
        let mut scanner = Scanner::new(&store, ScanOn::Table, false, true, range.clone()).unwrap();
        let mut res = test_take(&mut scanner, 3, 1, 4);
        res.append(&mut test_take(&mut scanner, 3, 4, 8));
        res.append(&mut test_take(&mut scanner, 3, 8, 21));
        res.append(&mut test_take(&mut scanner, 10, 21, -1));

        let expect_keys: Vec<_> = [1, 2, 3, 4, 5, 7, 10, 15, 20, 25]
            .iter()
            .map(|pk| table::encode_row_key(table_id, *pk))
            .collect();
        assert_eq!(res, expect_keys);

        let mut scanner = Scanner::new(&store, ScanOn::Table, true, true, range).unwrap();
        let mut res = test_take(&mut scanner, 3, 15, 26);
        res.append(&mut test_take(&mut scanner, 3, 5, 15));
        res.append(&mut test_take(&mut scanner, 10, -1, 5));
        assert_eq!(res, expect_keys.into_iter().rev().collect::<Vec<_>>());
    }
}
