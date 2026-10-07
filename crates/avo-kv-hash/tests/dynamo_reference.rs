//! Reference behavior of Dynamo's hashing contract (`dynamo-kv-hashing` 1.5.1).
//! These tests pin the upstream properties that this crate's implementation is compared against.

use dynamo_kv_hashing::Request;

const BLOCK_SIZE: u32 = 16;

fn plh(tokens: Vec<u32>, salt: Option<&str>) -> Vec<dynamo_kv_hashing::PositionalLineageHash> {
    Request::builder()
        .tokens(tokens)
        .salt(salt.map(str::to_string))
        .build()
        .expect("valid request")
        .positional_lineage_hashes(BLOCK_SIZE)
        .expect("hashing succeeds")
}

#[test]
fn only_complete_blocks_are_hashed() {
    assert_eq!(plh((0..47).collect(), None).len(), 2);
    assert_eq!(plh((0..48).collect(), None).len(), 3);
}

#[test]
fn shared_prefix_matches_until_first_divergent_block() {
    let mut a: Vec<u32> = (0..48).collect();
    let mut b = a.clone();
    a[40] = 9_000;
    b[40] = 9_001;
    let (ha, hb) = (plh(a, None), plh(b, None));
    assert_eq!(ha[..2], hb[..2]);
    assert_ne!(ha[2], hb[2]);
}

#[test]
fn different_salts_share_no_blocks() {
    let tokens: Vec<u32> = (0..48).collect();
    let (ha, hb) = (
        plh(tokens.clone(), Some("tenant-a")),
        plh(tokens, Some("tenant-b")),
    );
    assert!(ha.iter().zip(&hb).all(|(x, y)| x != y));
}
