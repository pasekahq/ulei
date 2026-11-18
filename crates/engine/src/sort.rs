//! Stable sorting through one shared instantiation of the standard slice sort: each call site
//! adds a small permutation step instead of its own copy of the sort.

use std::cmp::Ordering;

/// Sorts `items` as `<[T]>::sort_by(compare)` does, stable.
pub(crate) fn sort_by<T>(items: &mut [T], mut compare: impl FnMut(&T, &T) -> Ordering) {
    let order = {
        let items = &*items;
        stable_order(items.len(), &mut |a, b| compare(&items[a], &items[b]))
    };
    permute(items, order);
}

/// Sorts `items` as `<[T]>::sort` does.
pub(crate) fn sort<T: Ord>(items: &mut [T]) {
    sort_by(items, T::cmp);
}

/// The indices `0..len` in stable sorted order by `compare` of two indices.
fn stable_order(len: usize, compare: &mut dyn FnMut(usize, usize) -> Ordering) -> Vec<usize> {
    let mut order: Vec<usize> = (0..len).collect();
    order.sort_by(|&a, &b| compare(a, b));
    order
}

/// Moves `items[order[i]]` to position `i` for every `i`, following each cycle once.
fn permute<T>(items: &mut [T], mut order: Vec<usize>) {
    for start in 0..order.len() {
        let mut at = start;
        while order[at] != start {
            let from = order[at];
            items.swap(at, from);
            order[at] = at;
            at = from;
        }
        order[at] = at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_by_matches_the_standard_stable_sort() {
        let mut state = 0x2545_f491_u32;
        for len in 0..40 {
            let items: Vec<(u32, usize)> = (0..len)
                .map(|i| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    (state % 7, i)
                })
                .collect();
            let mut expected = items.clone();
            expected.sort_by_key(|a| std::cmp::Reverse(a.0));
            let mut actual = items;
            sort_by(&mut actual, |a, b| b.0.cmp(&a.0));
            assert_eq!(actual, expected);
        }
    }
}
