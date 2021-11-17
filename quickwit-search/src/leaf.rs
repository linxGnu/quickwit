// Copyright (C) 2021 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use futures::future::try_join_all;
use itertools::{Either, Itertools};
use once_cell::sync::OnceCell;
use quickwit_common::quickwit_memory_usage;
use quickwit_directories::{CachingDirectory, HotDirectory, StorageDirectory};
use quickwit_index_config::IndexConfig;
use quickwit_proto::{
    LeafSearchResponse, SearchRequest, SplitIdAndFooterOffsets, SplitSearchError,
};
use quickwit_storage::{
    wrap_storage_with_long_term_cache, BundleStorage, MemorySizedCache, OwnedBytes, Storage,
};
use tantivy::collector::Collector;
use tantivy::directory::FileSlice;
use tantivy::query::Query;
use tantivy::{Index, ReloadPolicy, Searcher, Term};
use tokio::task::spawn_blocking;
use tracing::*;

use crate::collector::{make_collector_for_split, make_merge_collector, GenericQuickwitCollector};
use crate::{split_footer_cache_capacity, SearchError};

fn global_split_footer_cache() -> &'static MemorySizedCache<String> {
    static INSTANCE: OnceCell<MemorySizedCache<String>> = OnceCell::new();
    INSTANCE.get_or_init(|| {
        MemorySizedCache::with_capacity_in_bytes(split_footer_cache_capacity() as usize)
    })
}

async fn get_split_footer_from_cache_or_fetch(
    index_storage: Arc<dyn Storage>,
    split_and_footer_offsets: &SplitIdAndFooterOffsets,
) -> anyhow::Result<OwnedBytes> {
    {
        let possible_val = global_split_footer_cache().get(&split_and_footer_offsets.split_id);
        if let Some(footer_data) = possible_val {
            return Ok(footer_data);
        }
    }
    let split_file = PathBuf::from(format!("{}.split", split_and_footer_offsets.split_id));
    let footer_data_opt = index_storage
        .get_slice(
            &split_file,
            split_and_footer_offsets.split_footer_start as usize
                ..split_and_footer_offsets.split_footer_end as usize,
        )
        .await
        .with_context(|| {
            format!(
                "Failed to fetch hotcache and footer from {} for split `{}`",
                index_storage.uri(),
                split_and_footer_offsets.split_id
            )
        })?;

    global_split_footer_cache().put(
        split_and_footer_offsets.split_id.to_owned(),
        footer_data_opt.clone(),
    );

    Ok(footer_data_opt)
}

/// Opens a `tantivy::Index` for the given split.
///
/// The resulting index uses a dynamic and a static cache.
pub(crate) async fn open_index(
    index_storage: Arc<dyn Storage>,
    split_and_footer_offsets: &SplitIdAndFooterOffsets,
) -> anyhow::Result<Index> {
    let split_file = PathBuf::from(format!("{}.split", split_and_footer_offsets.split_id));
    let footer_data =
        get_split_footer_from_cache_or_fetch(index_storage.clone(), split_and_footer_offsets)
            .await?;

    let (hotcache_bytes, bundle_storage) = BundleStorage::open_from_split_data(
        index_storage,
        split_file,
        FileSlice::new(Box::new(footer_data)),
    )?;
    let bundle_storage_with_cache = wrap_storage_with_long_term_cache(Arc::new(bundle_storage));
    let directory = StorageDirectory::new(bundle_storage_with_cache);
    let caching_directory = CachingDirectory::new_with_unlimited_capacity(Arc::new(directory));
    let hot_directory = HotDirectory::open(caching_directory, hotcache_bytes.read_bytes()?)?;
    let index = Index::open(hot_directory)?;
    Ok(index)
}

/// Tantivy search does not make it possible to fetch data asynchronously during
/// search.
///
/// It is required to download all required information in advance.
/// This is the role of the `warmup` function.
///
/// The downloaded data depends on the query (which term's posting list is required,
/// are position required too), and the collector.
#[instrument(skip(searcher, query, fast_field_names))]
pub(crate) async fn warmup(
    searcher: &Searcher,
    query: &dyn Query,
    fast_field_names: &HashSet<String>,
) -> anyhow::Result<()> {
    warm_up_terms(searcher, query)
        .instrument(debug_span!("warm_up_terms"))
        .await?;
    warm_up_fastfields(searcher, fast_field_names)
        .instrument(debug_span!("warm_up_fastfields"))
        .await?;
    Ok(())
}

async fn warm_up_fastfields(
    searcher: &Searcher,
    fast_field_names: &HashSet<String>,
) -> anyhow::Result<()> {
    let mut fast_fields = Vec::new();
    for fast_field_name in fast_field_names.iter() {
        let fast_field = searcher
            .schema()
            .get_field(fast_field_name)
            .with_context(|| {
                format!(
                    "Couldn't get field named {:?} from schema.",
                    fast_field_name
                )
            })?;

        let field_entry = searcher.schema().get_field_entry(fast_field);
        if !field_entry.is_fast() {
            anyhow::bail!("Field {:?} is not a fast field.", fast_field_name);
        }
        fast_fields.push(fast_field);
    }

    let mut warm_up_futures = Vec::new();
    for field in fast_fields {
        for segment_reader in searcher.segment_readers() {
            let fast_field_slice = segment_reader.fast_fields().fast_field_data(field, 0)?;
            warm_up_futures.push(async move { fast_field_slice.read_bytes_async().await });
        }
    }
    try_join_all(warm_up_futures).await?;
    Ok(())
}

async fn warm_up_terms(searcher: &Searcher, query: &dyn Query) -> anyhow::Result<()> {
    let mut terms: BTreeMap<Term, bool> = Default::default();
    query.query_terms(&mut terms);
    let grouped_terms = terms.iter().group_by(|term| term.0.field());
    let mut warm_up_futures = Vec::new();
    for (field, terms) in grouped_terms.into_iter() {
        let terms: Vec<(&Term, bool)> = terms
            .map(|(term, position_needed)| (term, *position_needed))
            .collect();
        for segment_reader in searcher.segment_readers() {
            let inv_idx = segment_reader.inverted_index(field)?;
            for (term, position_needed) in terms.iter().cloned() {
                let inv_idx_clone = inv_idx.clone();
                warm_up_futures
                    .push(async move { inv_idx_clone.warm_postings(term, position_needed).await });
            }
        }
    }
    try_join_all(warm_up_futures).await?;
    Ok(())
}

/// Apply a leaf search on a single split.
#[instrument(skip(search_request, storage, split, index_config))]
async fn leaf_search_single_split(
    search_request: &SearchRequest,
    storage: Arc<dyn Storage>,
    split: SplitIdAndFooterOffsets,
    index_config: Arc<dyn IndexConfig>,
) -> crate::Result<LeafSearchResponse> {
    // We need to make sure that quickwit behaves correctly when a lot of requests arrive.
    // We DO want to run a lot of leaf requests concurrently: quickwit is designed
    // to offer good performance when working with an object storage and object storage
    // typically have a low individual throughput, but can offer a overall high throughput
    // provided we run queries against it concurrently.
    //
    // Leaf requests run in two phases.
    // - the IO phase: quickwit fetches all of the data that we need for the query.
    // - the CPU phase: quickwit perform the actual computation over the data it fetched.
    //
    // The IO phase however runs on the regular tokio runtime.
    // The CPU phase is done on a thread pool sized against the number of cores
    // available.
    //
    // We want to work concurrently on a lot of requests for the IO phase, but
    // with no limitation at all, we could hit a memory limit.
    //
    // A simple semaphore on memory allocation is not a good solution at it would
    // easily lead to some deadlock.
    //
    // The solution we offer here is to only start running a leaf request IFF
    // the memory usage is below a given value.
    //
    // Ideally we would have preferred to allocate the memory necessary for a leaf request
    // upfront, but we do not know at this point how much memory will be needed (and
    // it would have made the code complicated).
    //
    // Instead, we allocate an arbitrary "cushion" at the beginning of phase 1.
    //
    // Once a leaf request got the green light, allocation will be tracked but will
    // not check if the capacity is available.
    let memory_cushion = quickwit_memory_usage()
        .use_memory_with_limit(
            crate::LEAF_REQUEST_MEMORY_CUSHION,
            crate::memory_threshold_to_start_leaf_request(),
        )
        .await;

    let split_id = split.split_id.to_string();
    let index = open_index(storage, &split).await?;
    let split_schema = index.schema();
    let quickwit_collector = make_collector_for_split(
        split_id.clone(),
        index_config.as_ref(),
        search_request,
        &split_schema,
    );
    let query = index_config.query(split_schema, search_request)?;
    let reader = index
        .reader_builder()
        .num_searchers(1)
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    let searcher = reader.searcher();
    warmup(&*searcher, &query, &quickwit_collector.fast_field_names()).await?;
    drop(memory_cushion);
    let leaf_search_response = crate::qspawn_blocking(move || {
        let span = info_span!( "search", split_id = %split.split_id);
        let _span_guard = span.enter();
        searcher.search(&query, &quickwit_collector)
    })
    .await
    .map_err(|_| {
        crate::SearchError::InternalError(format!("Leaf search panicked. split={}", split_id))
    })??;
    Ok(leaf_search_response)
}

/// `leaf` step of search.
///
/// The leaf search collects all kind of information, and returns a set of [PartialHit] candidates.
/// The root will be in charge to consolidate, identify the actual final top hits to display, and
/// fetch the actual documents to convert the partial hits into actual Hits.
pub async fn leaf_search(
    request: &SearchRequest,
    index_storage: Arc<dyn Storage>,
    splits: &[SplitIdAndFooterOffsets],
    index_config: Arc<dyn IndexConfig>,
) -> Result<LeafSearchResponse, SearchError> {
    let leaf_search_single_split_futures: Vec<_> = splits
        .iter()
        .map(|split| {
            let index_config_clone = index_config.clone();
            let index_storage_clone = index_storage.clone();
            async move {
                leaf_search_single_split(
                    request,
                    index_storage_clone,
                    split.clone(),
                    index_config_clone,
                )
                .await
                .map_err(|err| (split.split_id.clone(), err))
            }
        })
        .collect();
    let split_search_results = futures::future::join_all(leaf_search_single_split_futures).await;

    let (split_search_responses, errors): (Vec<LeafSearchResponse>, Vec<(String, SearchError)>) =
        split_search_results
            .into_iter()
            .partition_map(|split_search_res| match split_search_res {
                Ok(split_search_resp) => Either::Left(split_search_resp),
                Err(err) => Either::Right(err),
            });

    let merge_collector = make_merge_collector(request);
    let mut merged_search_response =
        spawn_blocking(move || merge_collector.merge_fruits(split_search_responses))
            .instrument(info_span!("merge_search_responses"))
            .await
            .context("Failed to merge split search responses.")??;

    merged_search_response
        .failed_splits
        .extend(errors.iter().map(|(split_id, err)| SplitSearchError {
            split_id: split_id.to_string(),
            error: format!("{:?}", err),
            retryable_error: true,
        }));
    Ok(merged_search_response)
}
