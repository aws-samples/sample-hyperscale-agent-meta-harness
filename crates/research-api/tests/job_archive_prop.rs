//! Property test of the archive filter (JOB lane): for generated repositories what comes out is what went in.

#![allow(clippy::unwrap_used)]

mod job_support;

use harness_protocol::research::ResearchLimits;
use job_support::*;
use proptest::prelude::*;
use research_api::archive::filter_archive;
use std::collections::BTreeMap;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn generated_repositories_come_out_as_they_went_in(
        files in proptest::collection::btree_map("(x/)?[a-c]{1,3}\\.txt", proptest::collection::vec(32u8..127, 0..200), 1..8)
    ) {
        let map: BTreeMap<String, Vec<u8>> = files;
        let borrowed: Vec<(&str, &[u8])> = map.iter().map(|(n, d)| (n.as_str(), d.as_slice())).collect();
        let f = filter_archive(&archive_of(&borrowed), &[], &ResearchLimits::default()).unwrap();
        let out = read_output(&f.tar_gz);
        let got: BTreeMap<String, Vec<u8>> = out.into_iter().map(|o| (o.path, o.data)).collect();
        prop_assert_eq!(got, map.clone());
        prop_assert_eq!(f.files.len(), map.len());
        for m in &f.files {
            prop_assert_eq!(&m.sha256, &sha256_hex(&map[&m.path]));
        }
    }
}
