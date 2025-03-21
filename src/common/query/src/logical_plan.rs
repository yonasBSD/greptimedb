// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub mod accumulator;
mod expr;
mod udaf;

use std::sync::Arc;

use datafusion::catalog::CatalogProviderList;
use datafusion::error::Result as DatafusionResult;
use datafusion::logical_expr::{LogicalPlan, LogicalPlanBuilder};
use datafusion_common::Column;
use datafusion_expr::col;
pub use expr::{build_filter_from_timestamp, build_same_type_ts_filter};

pub use self::accumulator::{Accumulator, AggregateFunctionCreator, AggregateFunctionCreatorRef};
pub use self::udaf::AggregateFunction;
use crate::error::Result;
use crate::logical_plan::accumulator::*;
use crate::signature::{Signature, Volatility};

pub fn create_aggregate_function(
    name: String,
    args_count: u8,
    creator: Arc<dyn AggregateFunctionCreator>,
) -> AggregateFunction {
    let return_type = make_return_function(creator.clone());
    let accumulator = make_accumulator_function(creator.clone());
    let state_type = make_state_function(creator.clone());
    AggregateFunction::new(
        name,
        Signature::any(args_count as usize, Volatility::Immutable),
        return_type,
        accumulator,
        state_type,
        creator,
    )
}

/// Rename columns by applying a new projection. Returns an error if the column to be
/// renamed does not exist. The `renames` parameter is a `Vector` with elements
/// in the form of `(old_name, new_name)`.
pub fn rename_logical_plan_columns(
    enable_ident_normalization: bool,
    plan: LogicalPlan,
    renames: Vec<(&str, &str)>,
) -> DatafusionResult<LogicalPlan> {
    let mut projection = Vec::with_capacity(renames.len());

    for (old_name, new_name) in renames {
        let old_column: Column = if enable_ident_normalization {
            Column::from_qualified_name(old_name)
        } else {
            Column::from_qualified_name_ignore_case(old_name)
        };

        let (qualifier_rename, field_rename) =
            plan.schema().qualified_field_from_column(&old_column)?;

        for (qualifier, field) in plan.schema().iter() {
            if qualifier.eq(&qualifier_rename) && field.as_ref() == field_rename {
                projection.push(col(Column::from((qualifier, field))).alias(new_name));
            }
        }
    }

    LogicalPlanBuilder::from(plan).project(projection)?.build()
}

/// The datafusion `[LogicalPlan]` decoder.
#[async_trait::async_trait]
pub trait SubstraitPlanDecoder {
    /// Decode the [`LogicalPlan`] from bytes with the [`CatalogProviderList`].
    /// When `optimize` is true, it will do the optimization for decoded plan.
    ///
    /// TODO(dennis): It's not a good design for an API to do many things.
    /// The `optimize` was introduced because of `query` and `catalog` cyclic dependency issue
    /// I am happy to refactor it if we have a better solution.
    async fn decode(
        &self,
        message: bytes::Bytes,
        catalog_list: Arc<dyn CatalogProviderList>,
        optimize: bool,
    ) -> Result<LogicalPlan>;
}

pub type SubstraitPlanDecoderRef = Arc<dyn SubstraitPlanDecoder + Send + Sync>;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use datafusion_expr::builder::LogicalTableSource;
    use datafusion_expr::lit;
    use datatypes::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
    use datatypes::prelude::*;
    use datatypes::vectors::VectorRef;

    use super::*;
    use crate::error::Result;
    use crate::function::AccumulatorCreatorFunction;
    use crate::signature::TypeSignature;

    #[derive(Debug)]
    struct DummyAccumulator;

    impl Accumulator for DummyAccumulator {
        fn state(&self) -> Result<Vec<Value>> {
            Ok(vec![])
        }

        fn update_batch(&mut self, _values: &[VectorRef]) -> Result<()> {
            Ok(())
        }

        fn merge_batch(&mut self, _states: &[VectorRef]) -> Result<()> {
            Ok(())
        }

        fn evaluate(&self) -> Result<Value> {
            Ok(Value::Int32(0))
        }
    }

    #[derive(Debug)]
    struct DummyAccumulatorCreator;

    impl AggrFuncTypeStore for DummyAccumulatorCreator {
        fn input_types(&self) -> Result<Vec<ConcreteDataType>> {
            Ok(vec![ConcreteDataType::float64_datatype()])
        }

        fn set_input_types(&self, _: Vec<ConcreteDataType>) -> Result<()> {
            Ok(())
        }
    }

    impl AggregateFunctionCreator for DummyAccumulatorCreator {
        fn creator(&self) -> AccumulatorCreatorFunction {
            Arc::new(|_| Ok(Box::new(DummyAccumulator)))
        }

        fn output_type(&self) -> Result<ConcreteDataType> {
            Ok(self.input_types()?.into_iter().next().unwrap())
        }

        fn state_types(&self) -> Result<Vec<ConcreteDataType>> {
            Ok(vec![
                ConcreteDataType::float64_datatype(),
                ConcreteDataType::uint32_datatype(),
            ])
        }
    }

    fn mock_plan() -> LogicalPlan {
        let schema = Schema::new(vec![
            Field::new("id", DataType::Int32, true),
            Field::new("name", DataType::Utf8, true),
        ]);
        let table_source = LogicalTableSource::new(SchemaRef::new(schema));

        let projection = None;

        let builder =
            LogicalPlanBuilder::scan("person", Arc::new(table_source), projection).unwrap();

        builder
            .filter(col("id").gt(lit(500)))
            .unwrap()
            .build()
            .unwrap()
    }

    #[test]
    fn test_rename_logical_plan_columns() {
        let plan = mock_plan();
        let new_plan =
            rename_logical_plan_columns(true, plan, vec![("id", "a"), ("name", "b")]).unwrap();

        assert_eq!(
            r#"
Projection: person.id AS a, person.name AS b
  Filter: person.id > Int32(500)
    TableScan: person"#,
            format!("\n{}", new_plan)
        );
    }

    #[test]
    fn test_create_udaf() {
        let creator = DummyAccumulatorCreator;
        let udaf = create_aggregate_function("dummy".to_string(), 1, Arc::new(creator));
        assert_eq!("dummy", udaf.name);

        let signature = udaf.signature;
        assert_eq!(TypeSignature::Any(1), signature.type_signature);
        assert_eq!(Volatility::Immutable, signature.volatility);

        assert_eq!(
            Arc::new(ConcreteDataType::float64_datatype()),
            (udaf.return_type)(&[ConcreteDataType::float64_datatype()]).unwrap()
        );
        assert_eq!(
            Arc::new(vec![
                ConcreteDataType::float64_datatype(),
                ConcreteDataType::uint32_datatype(),
            ]),
            (udaf.state_type)(&ConcreteDataType::float64_datatype()).unwrap()
        );
    }
}
