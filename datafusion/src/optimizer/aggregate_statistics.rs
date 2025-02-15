// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Utilizing exact statistics from sources to avoid scanning data
use std::collections::HashMap;
use std::{sync::Arc, vec};

use crate::{
    execution::context::ExecutionProps,
    logical_plan::{col, DFField, DFSchema, Expr, LogicalPlan},
    physical_plan::aggregates::AggregateFunction,
    scalar::ScalarValue,
};

use super::{optimizer::OptimizerRule, utils};
use crate::error::Result;

/// Optimizer that uses available statistics for aggregate functions
pub struct AggregateStatistics {}

impl AggregateStatistics {
    #[allow(missing_docs)]
    pub fn new() -> Self {
        Self {}
    }
}

impl OptimizerRule for AggregateStatistics {
    fn optimize(
        &self,
        plan: &LogicalPlan,
        execution_props: &ExecutionProps,
    ) -> crate::error::Result<LogicalPlan> {
        match plan {
            // match only select count(*) from table_scan
            LogicalPlan::Aggregate {
                input,
                group_expr,
                aggr_expr,
                schema,
            } if group_expr.is_empty() => {
                // aggregations that can not be replaced
                // using statistics
                let mut agg = vec![];
                let mut max_values = HashMap::new();
                let mut min_values = HashMap::new();

                // expressions that can be replaced by constants
                let mut projections = vec![];
                if let Some(num_rows) = match input.as_ref() {
                    LogicalPlan::TableScan {
                        table_name, source, ..
                    } if source.has_exact_statistics() => {
                        let schema = source.schema();
                        let fields = schema.fields();
                        if let Some(column_statistics) =
                            source.statistics().column_statistics
                        {
                            if fields.len() == column_statistics.len() {
                                for (i, field) in fields.iter().enumerate() {
                                    if let Some(max_value) =
                                        column_statistics[i].max_value.clone()
                                    {
                                        let max_key =
                                            format!("{}.{}", table_name, field.name());
                                        max_values.insert(max_key, max_value);
                                    }
                                    if let Some(min_value) =
                                        column_statistics[i].min_value.clone()
                                    {
                                        let min_key =
                                            format!("{}.{}", table_name, field.name());
                                        min_values.insert(min_key, min_value);
                                    }
                                }
                            }
                        }

                        source.statistics().num_rows
                    }
                    _ => None,
                } {
                    for expr in aggr_expr {
                        match expr {
                            Expr::AggregateFunction {
                                fun: AggregateFunction::Count,
                                args,
                                distinct: false,
                            } if args
                                == &[Expr::Literal(ScalarValue::UInt8(Some(1)))] =>
                            {
                                projections.push(Expr::Alias(
                                    Box::new(Expr::Literal(ScalarValue::UInt64(Some(
                                        num_rows as u64,
                                    )))),
                                    "COUNT(Uint8(1))".to_string(),
                                ));
                            }
                            Expr::AggregateFunction {
                                fun: AggregateFunction::Max,
                                args,
                                ..
                            } => match &args[0] {
                                Expr::Column(c) => match max_values.get(&c.flat_name()) {
                                    Some(max_value) => {
                                        if !max_value.is_null() {
                                            let name = format!("MAX({})", c.name);
                                            projections.push(Expr::Alias(
                                                Box::new(Expr::Literal(
                                                    max_value.clone(),
                                                )),
                                                name,
                                            ));
                                        } else {
                                            agg.push(expr.clone());
                                        }
                                    }
                                    None => {
                                        agg.push(expr.clone());
                                    }
                                },
                                _ => {
                                    agg.push(expr.clone());
                                }
                            },
                            Expr::AggregateFunction {
                                fun: AggregateFunction::Min,
                                args,
                                ..
                            } => match &args[0] {
                                Expr::Column(c) => match min_values.get(&c.flat_name()) {
                                    Some(min_value) => {
                                        if !min_value.is_null() {
                                            let name = format!("MIN({})", c.name);
                                            projections.push(Expr::Alias(
                                                Box::new(Expr::Literal(
                                                    min_value.clone(),
                                                )),
                                                name,
                                            ));
                                        } else {
                                            agg.push(expr.clone());
                                        }
                                    }
                                    None => {
                                        agg.push(expr.clone());
                                    }
                                },
                                _ => {
                                    agg.push(expr.clone());
                                }
                            },
                            _ => {
                                agg.push(expr.clone());
                            }
                        }
                    }

                    return Ok(if agg.is_empty() {
                        // table scan can be entirely removed

                        LogicalPlan::Projection {
                            expr: projections,
                            input: Arc::new(LogicalPlan::EmptyRelation {
                                produce_one_row: true,
                                schema: Arc::new(DFSchema::empty()),
                            }),
                            schema: schema.clone(),
                        }
                    } else if projections.is_empty() {
                        // no replacements -> return original plan
                        plan.clone()
                    } else {
                        // Split into parts that can be supported and part that should stay in aggregate
                        let agg_fields = agg
                            .iter()
                            .map(|x| x.to_field(input.schema()))
                            .collect::<Result<Vec<DFField>>>()?;
                        let agg_schema = DFSchema::new(agg_fields)?;
                        let cols = agg
                            .iter()
                            .map(|e| e.name(&agg_schema))
                            .collect::<Result<Vec<String>>>()?;
                        projections.extend(cols.iter().map(|x| col(x)));
                        LogicalPlan::Projection {
                            expr: projections,
                            schema: schema.clone(),
                            input: Arc::new(LogicalPlan::Aggregate {
                                input: input.clone(),
                                group_expr: vec![],
                                aggr_expr: agg,
                                schema: Arc::new(agg_schema),
                            }),
                        }
                    });
                }
                Ok(plan.clone())
            }
            // Rest: recurse and find possible statistics
            _ => {
                let expr = plan.expressions();

                // apply the optimization to all inputs of the plan
                let inputs = plan.inputs();
                let new_inputs = inputs
                    .iter()
                    .map(|plan| self.optimize(plan, execution_props))
                    .collect::<Result<Vec<_>>>()?;

                utils::from_plan(plan, &expr, &new_inputs)
            }
        }
    }

    fn name(&self) -> &str {
        "aggregate_statistics"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::datatypes::{DataType, Field, Schema};

    use crate::error::Result;
    use crate::execution::context::ExecutionProps;
    use crate::logical_plan::LogicalPlan;
    use crate::optimizer::aggregate_statistics::AggregateStatistics;
    use crate::optimizer::optimizer::OptimizerRule;
    use crate::scalar::ScalarValue;
    use crate::{
        datasource::{
            datasource::{ColumnStatistics, Statistics},
            TableProvider,
        },
        logical_plan::Expr,
    };

    struct TestTableProvider {
        num_rows: usize,
        column_statistics: Vec<ColumnStatistics>,
        is_exact: bool,
    }

    impl TableProvider for TestTableProvider {
        fn as_any(&self) -> &dyn std::any::Any {
            unimplemented!()
        }
        fn schema(&self) -> arrow::datatypes::SchemaRef {
            Arc::new(Schema::new(vec![Field::new("a", DataType::Int64, false)]))
        }

        fn scan(
            &self,
            _projection: &Option<Vec<usize>>,
            _batch_size: usize,
            _filters: &[Expr],
            _limit: Option<usize>,
        ) -> Result<std::sync::Arc<dyn crate::physical_plan::ExecutionPlan>> {
            unimplemented!()
        }
        fn statistics(&self) -> Statistics {
            Statistics {
                num_rows: Some(self.num_rows),
                total_byte_size: None,
                column_statistics: Some(self.column_statistics.clone()),
            }
        }
        fn has_exact_statistics(&self) -> bool {
            self.is_exact
        }
    }

    #[test]
    fn optimize_count_using_statistics() -> Result<()> {
        use crate::execution::context::ExecutionContext;
        let mut ctx = ExecutionContext::new();
        ctx.register_table(
            "test",
            Arc::new(TestTableProvider {
                num_rows: 100,
                column_statistics: Vec::new(),
                is_exact: true,
            }),
        )
        .unwrap();

        let plan = ctx
            .create_logical_plan("select count(*) from test")
            .unwrap();
        let expected = "\
            Projection: #COUNT(UInt8(1))\
            \n  Projection: UInt64(100) AS COUNT(Uint8(1))\
            \n    EmptyRelation";

        assert_optimized_plan_eq(&plan, expected);
        Ok(())
    }

    #[test]
    fn optimize_count_not_exact() -> Result<()> {
        use crate::execution::context::ExecutionContext;
        let mut ctx = ExecutionContext::new();
        ctx.register_table(
            "test",
            Arc::new(TestTableProvider {
                num_rows: 100,
                column_statistics: Vec::new(),
                is_exact: false,
            }),
        )
        .unwrap();

        let plan = ctx
            .create_logical_plan("select count(*) from test")
            .unwrap();
        let expected = "\
            Projection: #COUNT(UInt8(1))\
            \n  Aggregate: groupBy=[[]], aggr=[[COUNT(UInt8(1))]]\
            \n    TableScan: test projection=None";

        assert_optimized_plan_eq(&plan, expected);
        Ok(())
    }

    #[test]
    fn optimize_count_sum() -> Result<()> {
        use crate::execution::context::ExecutionContext;
        let mut ctx = ExecutionContext::new();
        ctx.register_table(
            "test",
            Arc::new(TestTableProvider {
                num_rows: 100,
                column_statistics: Vec::new(),
                is_exact: true,
            }),
        )
        .unwrap();

        let plan = ctx
            .create_logical_plan("select sum(a)/count(*) from test")
            .unwrap();
        let expected = "\
            Projection: #SUM(test.a) Divide #COUNT(UInt8(1))\
            \n  Projection: UInt64(100) AS COUNT(Uint8(1)), #SUM(test.a)\
            \n    Aggregate: groupBy=[[]], aggr=[[SUM(#test.a)]]\
            \n      TableScan: test projection=None";

        assert_optimized_plan_eq(&plan, expected);
        Ok(())
    }

    #[test]
    fn optimize_count_group_by() -> Result<()> {
        use crate::execution::context::ExecutionContext;
        let mut ctx = ExecutionContext::new();
        ctx.register_table(
            "test",
            Arc::new(TestTableProvider {
                num_rows: 100,
                column_statistics: Vec::new(),
                is_exact: true,
            }),
        )
        .unwrap();

        let plan = ctx
            .create_logical_plan("SELECT count(*), a FROM test GROUP BY a")
            .unwrap();
        let expected = "\
            Projection: #COUNT(UInt8(1)), #test.a\
            \n  Aggregate: groupBy=[[#test.a]], aggr=[[COUNT(UInt8(1))]]\
            \n    TableScan: test projection=None";

        assert_optimized_plan_eq(&plan, expected);
        Ok(())
    }

    #[test]
    fn optimize_count_filter() -> Result<()> {
        use crate::execution::context::ExecutionContext;
        let mut ctx = ExecutionContext::new();
        ctx.register_table(
            "test",
            Arc::new(TestTableProvider {
                num_rows: 100,
                column_statistics: Vec::new(),
                is_exact: true,
            }),
        )
        .unwrap();

        let plan = ctx
            .create_logical_plan("SELECT count(*) FROM test WHERE a < 5")
            .unwrap();
        let expected = "\
            Projection: #COUNT(UInt8(1))\
            \n  Aggregate: groupBy=[[]], aggr=[[COUNT(UInt8(1))]]\
            \n    Filter: #test.a Lt Int64(5)\
            \n      TableScan: test projection=None";

        assert_optimized_plan_eq(&plan, expected);
        Ok(())
    }

    #[test]
    fn optimize_max_min_using_statistics() -> Result<()> {
        use crate::execution::context::ExecutionContext;
        let mut ctx = ExecutionContext::new();

        let column_statistic = ColumnStatistics {
            null_count: None,
            max_value: Some(ScalarValue::from(100_i64)),
            min_value: Some(ScalarValue::from(1_i64)),
            distinct_count: None,
        };
        let column_statistics = vec![column_statistic];

        ctx.register_table(
            "test",
            Arc::new(TestTableProvider {
                num_rows: 100,
                column_statistics,
                is_exact: true,
            }),
        )
        .unwrap();

        let plan = ctx
            .create_logical_plan("select max(a), min(a) from test")
            .unwrap();
        let expected = "\
            Projection: #MAX(test.a), #MIN(test.a)\
            \n  Projection: Int64(100) AS MAX(a), Int64(1) AS MIN(a)\
            \n    EmptyRelation";

        assert_optimized_plan_eq(&plan, expected);
        Ok(())
    }

    #[test]
    fn optimize_max_min_not_using_statistics() -> Result<()> {
        use crate::execution::context::ExecutionContext;
        let mut ctx = ExecutionContext::new();
        ctx.register_table(
            "test",
            Arc::new(TestTableProvider {
                num_rows: 100,
                column_statistics: Vec::new(),
                is_exact: true,
            }),
        )
        .unwrap();

        let plan = ctx
            .create_logical_plan("select max(a), min(a) from test")
            .unwrap();
        let expected = "\
            Projection: #MAX(test.a), #MIN(test.a)\
            \n  Aggregate: groupBy=[[]], aggr=[[MAX(#test.a), MIN(#test.a)]]\
            \n    TableScan: test projection=None";

        assert_optimized_plan_eq(&plan, expected);
        Ok(())
    }

    fn assert_optimized_plan_eq(plan: &LogicalPlan, expected: &str) {
        let opt = AggregateStatistics::new();
        let optimized_plan = opt.optimize(plan, &ExecutionProps::new()).unwrap();
        let formatted_plan = format!("{:?}", optimized_plan);
        assert_eq!(formatted_plan, expected);
        assert_eq!(plan.schema(), plan.schema());
    }
}
