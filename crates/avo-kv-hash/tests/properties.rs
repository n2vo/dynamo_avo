use avo_kv_hash::hash_sequence;
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
}
