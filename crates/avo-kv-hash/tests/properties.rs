use avo_kv_hash::{hash_sequence, IncrementalHasher};
use proptest::prelude::*;

const BLOCK_SIZE: usize = 16;

#[test]
fn only_complete_blocks_are_hashed() {
    let tokens: Vec<u32> = (0..47).collect();
    let hashes = hash_sequence(&tokens, BLOCK_SIZE, None);
    assert_eq!(hashes.len(), 2);
}

proptest! {
    #[test]
    fn block_count_is_floor_of_len_over_block_size(
        tokens in prop::collection::vec(any::<u32>(), 0..200),
        block_size in 1usize..32,
    ) {
        let hashes = hash_sequence(&tokens, block_size, None);
        prop_assert_eq!(hashes.len(), tokens.len() / block_size);
    }

    #[test]
    fn prefix_hashes_are_stable(
        tokens in prop::collection::vec(any::<u32>(), 0..200),
        cut in 0usize..200,
    ) {
        let cut = cut.min(tokens.len());
        let full = hash_sequence(&tokens, BLOCK_SIZE, None);
        let partial = hash_sequence(&tokens[..cut], BLOCK_SIZE, None);
        prop_assert_eq!(&full[..partial.len()], &partial[..]);
    }

    #[test]
    fn incremental_matches_one_pass(
        tokens in prop::collection::vec(any::<u32>(), 0..300),
        block_size in 1usize..32,
        salt in prop::option::of("[a-z]{0..8}"),
    ) {
        let one_pass = hash_sequence(&tokens, block_size, salt.as_deref());

        let mut incremental = IncrementalHasher::new(block_size, salt.as_deref());
        for &token in &tokens {
            incremental.push(token);
        }

        prop_assert_eq!(incremental.hashes(), &one_pass[..]);
    }
}
