use std::{
	borrow::{Borrow, Cow},
	collections::HashSet,
	sync::Arc,
};

use anyhow::Context;
use either::Either;
use ironworks::excel;
use ironworks_schema::Schema;
use itertools::Itertools;
use serde::Deserialize;
use tokio::select;
use tokio_util::sync::CancellationToken;

use crate::{data::Data, version::VersionKey};

use super::{
	error::{Error, Result},
	internal_query::{post, pre, Normalizer},
	tantivy,
};

#[derive(Debug, Deserialize)]
pub struct Config {
	pagination: PaginationConfig,
	tantivy: tantivy::Config,
}

#[derive(Debug, Deserialize)]
struct PaginationConfig {
	limit_default: u32,
	limit_max: u32,
}

#[derive(Debug)]
pub struct SearchResult {
	pub score: f32,
	// TODO: `String` here necessitates a copy of the sheet name for every result, which seems wasteful.
	pub sheet: String,
	pub row_id: u32,
	pub subrow_id: u16,
}

pub struct Search {
	pagination_config: PaginationConfig,

	provider: Arc<tantivy::Provider>,

	data: Arc<Data>,
}

impl Search {
	pub fn new(config: Config, data: Arc<Data>) -> Result<Self> {
		Ok(Self {
			pagination_config: config.pagination,
			provider: Arc::new(tantivy::Provider::new(config.tantivy)?),
			data,
		})
	}

	pub async fn start(&self, cancel: CancellationToken) -> Result<()> {
		let mut receiver = self.data.subscribe();
		self.ingest(cancel.child_token(), receiver.borrow().clone())
			.await?;

		loop {
			select! {
				Ok(_) = receiver.changed() => {
					self.ingest(cancel.child_token(), receiver.borrow().clone()).await?
				}
				_ = cancel.cancelled() => break,
			}
		}

		Ok(())
	}

	async fn ingest(&self, cancel: CancellationToken, versions: Vec<VersionKey>) -> Result<()> {
		// Get a list of all sheets in the provided versions.
		// TODO: This has more `.collect`s than i'd like, but given it's a fairly cold path, probably isn't a problem.
		let sheets = versions
			.into_iter()
			.map(|version| -> Result<_> {
				let data_version = self.data.version(version).with_context(|| {
					format!("version {version} announced for ingestion but not provided")
				})?;
				let excel = data_version.excel();
				let list = excel.list()?;

				list.iter()
					.map(|sheet_name| Ok((version, excel.sheet(sheet_name.to_string())?)))
					.collect::<Result<Vec<_>>>()
			})
			.flatten_ok()
			.collect::<Result<Vec<_>>>()?;

		// Fire off the ingestion in the provider.
		Arc::clone(&self.provider).ingest(cancel, sheets).await?;

		Ok(())
	}

	pub fn search(
		&self,
		version: VersionKey,
		query: &pre::Node,
		language: excel::Language,
		sheet_filter: Option<HashSet<String>>,
		limit: Option<u32>,
		schema: &dyn Schema,
	) -> Result<Vec<SearchResult>> {
		// Get references to the game data we'll need.
		let excel = self
			.data
			.version(version)
			.with_context(|| format!("data for version {version} not ready"))?
			.excel();
		let list = excel.list()?;

		// Work out the actual result limit we'll use for this query.
		let result_limit = limit
			.unwrap_or(self.pagination_config.limit_default)
			.min(self.pagination_config.limit_max);

		// Build the helpers for this search call.
		let normalizer = Normalizer::new(&excel, schema);
		let executor = Executor {
			provider: &self.provider,
		};

		// Get an iterator over the provided sheet filter, falling back to the full list of sheets.
		let sheet_names = sheet_filter
			.map(|filter| Either::Left(filter.into_iter().map(Cow::from)))
			.unwrap_or_else(|| Either::Right(list.iter()));

		let normalized_queries = sheet_names
			.map(|name| {
				let normalized_query = normalizer.normalize(query, &name, language)?;
				Ok((name.to_string(), normalized_query))
			})
			// TODO: Much like the analogue in index, this is filtering out non-fatal errors. To raise as warnings, these will need to be split out at this point.
			.filter(|query| match query {
				Err(Error::Failure(_)) | Ok(_) => true,
				Err(_) => false,
			})
			.collect::<Result<Vec<_>>>()?;

		let results = executor.search(version, normalized_queries, Some(result_limit))?;

		Ok(results)
	}
}

// TODO: can probably store the number of search executions on this to feed into rate limiting
pub struct Executor<'a> {
	provider: &'a tantivy::Provider,
}

impl Executor<'_> {
	// TODO: The Option on limit is to represent the "no limit" case required for inner queries in relationships, where outer filtering may lead to any theoretical bounded inner query to be insufficient. For obvious reasons this is... _not_ a particulary efficient approach, though I'm not sure what better approaches exist. If nothing else, would be good to cache common queries in memory to avoid constant repetition of unbounded limits.
	pub fn search(
		&self,
		version: VersionKey,
		queries: impl IntoIterator<Item = (String, impl Borrow<post::Node>)>,
		limit: Option<u32>,
	) -> Result<Vec<SearchResult>> {
		self.provider.search(version, queries, limit, self)
	}
}
