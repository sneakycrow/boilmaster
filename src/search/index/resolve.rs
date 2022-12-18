use tantivy::{
	query::{BooleanQuery, Query, TermQuery, TermSetQuery},
	schema::{Field, IndexRecordOption, Schema, Type},
	Term,
};

use crate::search::{
	error::{FieldTypeError, SchemaMismatchError, SearchError},
	query::post::{Group, Leaf, Node, Operation, Relation, Value},
	version::Executor,
};

use super::schema::column_field_name;

pub struct QueryResolver<'a> {
	pub schema: &'a Schema,
	pub executor: &'a Executor,
}

impl QueryResolver<'_> {
	pub fn resolve(&self, node: &Node) -> Result<Box<dyn Query>, SearchError> {
		match node {
			Node::Group(group) => self.resolve_clause(group),
			Node::Leaf(leaf) => self.resolve_leaf(leaf),
		}
	}

	fn resolve_clause(&self, group: &Group) -> Result<Box<dyn Query>, SearchError> {
		let subqueries = group
			.clauses
			.iter()
			.map(|(occur, node)| {
				use crate::search::query::post::Occur as BOccur;
				use tantivy::query::Occur as TOccur;
				let tantivy_occur = match occur {
					BOccur::Must => TOccur::Must,
					BOccur::Should => TOccur::Should,
					BOccur::MustNot => TOccur::MustNot,
				};

				Ok((tantivy_occur, self.resolve(node)?))
			})
			.collect::<Result<Vec<_>, SearchError>>()?;

		Ok(Box::new(BooleanQuery::new(subqueries)))
	}

	fn resolve_leaf(&self, leaf: &Leaf) -> Result<Box<dyn Query>, SearchError> {
		// TODO: this should use a schema-provided name fetcher or something, this is not stable
		let field_name = column_field_name(&leaf.field);
		let field = self.schema.get_field(&field_name).ok_or_else(|| {
			SearchError::SchemaMismatch(SchemaMismatchError {
				field: format!("field {field_name}"),
				reason: "field does not exist in search index".into(),
			})
		})?;

		match &leaf.operation {
			Operation::Relation(relation) => self.resolve_relation(relation, field),
			Operation::Equal(value) => {
				let term = self.value_to_term(value, field)?;
				Ok(Box::new(TermQuery::new(term, IndexRecordOption::Basic)))
			}
		}
	}

	fn resolve_relation(
		&self,
		relation: &Relation,
		field: Field,
	) -> Result<Box<dyn Query>, SearchError> {
		// Run the inner query on the target index.
		let results = self
			.executor
			.search(&relation.target.sheet, &relation.query)?;

		// Map the results to terms for the query we're building.
		// TODO: I'm ignoring the subrow here - is that sane? AFAIK subrow relations act as a pivot table, many:many - I don't _think_ it references the subrow anywhere?
		// TODO: I have access to a score from the inside here. I should propagate that, somehow.
		let terms = results
			.map(|result| self.value_to_term(&Value::U64(result.row_id.into()), field))
			.collect::<Result<Vec<_>, _>>()?;

		if relation.target.condition.is_some() {
			todo!("handle relationship conditions")
		}

		Ok(Box::new(TermSetQuery::new(terms)))
	}

	fn value_to_term(&self, value: &Value, field: Field) -> Result<Term, SearchError> {
		let field_entry = self.schema.get_field_entry(field);
		let field_type = field_entry.field_type().value_type();

		(|| -> Option<_> {
			Some(match field_type {
				Type::U64 => Term::from_field_u64(field, self.value_to_u64(value)?),
				Type::I64 => Term::from_field_i64(field, self.value_to_i64(value)?),
				other => todo!("{other:#?}"),
			})
		})()
		.ok_or_else(|| {
			SearchError::FieldType(FieldTypeError {
				field: format!("field {}", self.schema.get_field_name(field)),
				expected: field_type.name().to_string(),
				got: format!("{value:?}"),
			})
		})
	}

	fn value_to_u64(&self, value: &Value) -> Option<u64> {
		match value {
			Value::U64(inner) => Some(*inner),
		}
	}

	fn value_to_i64(&self, value: &Value) -> Option<i64> {
		match value {
			Value::U64(inner) => (*inner).try_into().ok(),
		}
	}
}
