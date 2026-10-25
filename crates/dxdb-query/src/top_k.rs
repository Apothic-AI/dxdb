//! Skip/limit selection: `src/rx-query-helper.ts:371-576`.
//!
//! A query with a limit needs only the first `skip + limit` documents of the sorted
//! result. Sorting all `n` and slicing costs `n log n` comparisons; selecting the `k`
//! smallest with a bounded max-heap costs about `n + k log k log(n/k)`, which is much
//! cheaper when `k` is small. That matters for the common "show the first 20 of a
//! large collection" query.
//!
//! The selection is exact rather than approximate because RxDB always appends the
//! primary key to the sort order after normalization, so there are no ties: the `k`
//! smallest documents are exactly the first `k` of the full sort.
//!
//! When `k` is close to `n` the heap bookkeeping costs more than it saves, so the
//! full sort is used. [`TOP_K_SELECTION_FACTOR`] is the crossover, tuned by benchmark
//! upstream (`rx-query-helper.test.ts:254`). Reproducing the factor keeps the
//! crossover — and therefore the performance profile — identical, and the ported
//! tests assert the selection at exactly the boundary.

use std::cmp::Ordering;

use serde_json::Value;

use crate::sort::SortComparator;

/// `TOP_K_SELECTION_FACTOR` (`:387`): use top-k when `k * factor < n`.
pub const TOP_K_SELECTION_FACTOR: usize = 2;

/// `getSkipPlusLimit` (`:389`): `skip + limit`, or `None` for "unbounded".
///
/// Upstream returns `Infinity`; `None` is the same information without a float NaN
/// hazard in the arithmetic below.
pub fn get_skip_plus_limit(skip: u64, limit: Option<u64>) -> Option<u64> {
    limit.map(|limit| skip + limit)
}

/// `shouldUseTopKSelection` (`:404`).
pub fn should_use_top_k_selection(documents_amount: usize, skip_plus_limit: Option<u64>) -> bool {
    match skip_plus_limit {
        Some(k) => (k as u128) * (TOP_K_SELECTION_FACTOR as u128) < documents_amount as u128,
        None => false,
    }
}

/// `sortAndSliceDocuments` (`:484`).
///
/// Returns the documents from `skip` to `skip + limit` in sort order. The input is
/// consumed; upstream may mutate the array it is given, and taking ownership makes
/// that explicit instead of a comment.
pub fn sort_and_slice_documents(
    mut documents: Vec<Value>,
    comparator: &SortComparator,
    skip: u64,
    limit: Option<u64>,
) -> Vec<Value> {
    let skip = skip as usize;
    let Some(skip_plus_limit) = get_skip_plus_limit(skip as u64, limit) else {
        // No limit: a full sort is the answer.
        if skip >= documents.len() {
            return Vec::new();
        }
        documents.sort_by(|a, b| comparator.compare(a, b));
        return documents.into_iter().skip(skip).collect();
    };
    let skip_plus_limit = skip_plus_limit as usize;
    if skip_plus_limit == 0 || skip >= documents.len() {
        return Vec::new();
    }
    if !should_use_top_k_selection(documents.len(), Some(skip_plus_limit as u64)) {
        documents.sort_by(|a, b| comparator.compare(a, b));
        if skip == 0 && documents.len() <= skip_plus_limit {
            return documents;
        }
        return documents
            .into_iter()
            .skip(skip)
            .take(skip_plus_limit - skip)
            .collect();
    }
    let mut collector = TopKDocumentsCollector::new(comparator, skip as u64, limit);
    for document in documents {
        collector.push(document);
    }
    collector.get_result()
}

/// A bounded max-heap: the root is the largest retained document, which is the one
/// dropped first when a smaller one arrives (`BoundedMaxHeap`, `:419`).
struct BoundedMaxHeap {
    heap: Vec<Value>,
    k: usize,
}

impl BoundedMaxHeap {
    fn new(k: usize) -> Self {
        Self {
            heap: Vec::with_capacity(k.min(1024)),
            k,
        }
    }

    /// `heapPush` (`:424`).
    fn push(&mut self, comparator: &SortComparator, document: Value) {
        if self.k == 0 {
            return;
        }
        if self.heap.len() < self.k {
            self.heap.push(document);
            let mut child = self.heap.len() - 1;
            while child > 0 {
                let parent = (child - 1) / 2;
                if comparator.compare(&self.heap[child], &self.heap[parent]) == Ordering::Greater {
                    self.heap.swap(child, parent);
                    child = parent;
                } else {
                    break;
                }
            }
            return;
        }
        if comparator.compare(&document, &self.heap[0]) != Ordering::Less {
            return;
        }
        self.heap[0] = document;
        let length = self.heap.len();
        let mut index = 0;
        loop {
            let left = 2 * index + 1;
            if left >= length {
                break;
            }
            let right = left + 1;
            let mut largest = left;
            if right < length
                && comparator.compare(&self.heap[right], &self.heap[left]) == Ordering::Greater
            {
                largest = right;
            }
            if comparator.compare(&self.heap[largest], &self.heap[index]) == Ordering::Greater {
                self.heap.swap(index, largest);
                index = largest;
            } else {
                break;
            }
        }
    }

    /// The retained documents, sorted.
    fn into_sorted(mut self, comparator: &SortComparator) -> Vec<Value> {
        self.heap.sort_by(|a, b| comparator.compare(a, b));
        self.heap
    }
}

/// A streaming collector: `createTopKDocumentsCollector` (`:532`).
///
/// Buffers until it has seen enough documents to make the heap worth its
/// bookkeeping, then switches to bounded retention. This is what a storage uses while
/// iterating rows, so it never holds more than `skip + limit` documents for a large
/// result.
pub struct TopKDocumentsCollector<'a> {
    comparator: &'a SortComparator,
    skip: usize,
    skip_plus_limit: Option<u64>,
    buffer_limit: u64,
    buffer: Vec<Value>,
    heap: Option<BoundedMaxHeap>,
}

impl<'a> TopKDocumentsCollector<'a> {
    /// Builds a collector for a `skip`/`limit` pair.
    pub fn new(comparator: &'a SortComparator, skip: u64, limit: Option<u64>) -> Self {
        let skip_plus_limit = get_skip_plus_limit(skip, limit);
        let buffer_limit = match skip_plus_limit {
            Some(k) => k.saturating_mul(TOP_K_SELECTION_FACTOR as u64),
            None => u64::MAX,
        };
        Self {
            comparator,
            skip: skip as usize,
            skip_plus_limit,
            buffer_limit,
            buffer: Vec::new(),
            heap: None,
        }
    }

    /// Adds a document that matched the query.
    pub fn push(&mut self, document: Value) {
        let Some(skip_plus_limit) = self.skip_plus_limit else {
            self.buffer.push(document);
            return;
        };
        if skip_plus_limit == 0 {
            return;
        }
        if let Some(heap) = &mut self.heap {
            heap.push(self.comparator, document);
            return;
        }
        self.buffer.push(document);
        if self.buffer.len() as u64 > self.buffer_limit {
            let mut heap = BoundedMaxHeap::new(skip_plus_limit as usize);
            for buffered in self.buffer.drain(..) {
                heap.push(self.comparator, buffered);
            }
            self.heap = Some(heap);
        }
    }

    /// The sorted documents from `skip` to `skip + limit`. Call once.
    pub fn get_result(self) -> Vec<Value> {
        if let Some(heap) = self.heap {
            let Some(skip_plus_limit) = self.skip_plus_limit else {
                return Vec::new();
            };
            if skip_plus_limit == 0 || self.skip >= heap.heap.len() {
                return Vec::new();
            }
            let sorted = heap.into_sorted(self.comparator);
            return sorted.into_iter().skip(self.skip).collect();
        }
        let limit = match self.skip_plus_limit {
            Some(skip_plus_limit) => Some(skip_plus_limit.saturating_sub(self.skip as u64)),
            None => None,
        };
        sort_and_slice_documents(self.buffer, self.comparator, self.skip as u64, limit)
    }
}

/// `createTopKDocumentsCollector` (`:532`) as a free function, for callers that
/// prefer it.
pub fn create_top_k_documents_collector<'a>(
    comparator: &'a SortComparator,
    skip: u64,
    limit: Option<u64>,
) -> TopKDocumentsCollector<'a> {
    TopKDocumentsCollector::new(comparator, skip, limit)
}
