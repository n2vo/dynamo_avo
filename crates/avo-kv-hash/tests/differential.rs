use avo_kv_hash::hash_sequence;
use dynamo_kv_hashing::Request;
use proptest::prelude::*;
use proptest::proptest;

const BLOCK_SIZE: u32 = 16;

#[test]
fn sequence_hashes_match_dynamo_bit_for_bit() {
    let tokens: Vec<u32> = (0..48).collect();

    let dynamo: Vec<u64> = Request::builder()
        .tokens(tokens.clone())
        .salt(None)
        .build()
        .expect("valid request")
        .sequence_hashes(BLOCK_SIZE)
        .expect("hashing succeeds");

    let ours: Vec<u64> = hash_sequence(&tokens, BLOCK_SIZE as usize, None)
        .into_iter()
        .map(|h| h.as_u64())
        .collect();

    assert_eq!(ours, dynamo);
}

proptest! {
    #[test]
    fn random_sequences_match_dynamo(
        tokens in prop::collection::vec(any::<u32>(), 0..512),
    ) {
        let dynamo: Vec<u64> = Request::builder()
            .tokens(tokens.clone())
            .salt(None)
            .build()
            .expect("valid request")
            .sequence_hashes(BLOCK_SIZE)
            .expect("hashing succeeds");

        let ours: Vec<u64> = hash_sequence(&tokens, BLOCK_SIZE as usize, None)
            .into_iter()
            .map(|h| h.as_u64())
            .collect();

        prop_assert_eq!(ours, dynamo);
    }
}
