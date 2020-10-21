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

//! SQL Query Planner (produces logical plan from SQL AST)

use std::str::FromStr;
use std::sync::Arc;

use crate::logical_plan::Expr::Alias;
use crate::logical_plan::{
    lit, Expr, LogicalPlan, LogicalPlanBuilder, Operator, PlanType, StringifiedPlan,
};
use crate::scalar::ScalarValue;
use crate::{
    error::{DataFusionError, Result},
    physical_plan::udaf::AggregateUDF,
};
use crate::{
    physical_plan::udf::ScalarUDF,
    physical_plan::{aggregates, functions},
    sql::parser::{CreateExternalTable, FileType, Statement as DFStatement},
};

use arrow::datatypes::*;

use super::parser::ExplainPlan;
use itertools::Itertools;
use sqlparser::ast::{
    BinaryOperator, DataType as SQLDataType, Expr as SQLExpr, Query, Select, SelectItem,
    SetExpr, SetOperator, TableFactor, TableWithJoins, UnaryOperator, Value,
};
use sqlparser::ast::{ColumnDef as SQLColumnDef, ColumnOption};
use sqlparser::ast::{OrderByExpr, Statement};
use std::collections::{HashMap, HashSet};

/// The SchemaProvider trait allows the query planner to obtain meta-data about tables and
/// functions referenced in SQL statements
pub trait SchemaProvider {
    /// Getter for a field description
    fn get_table_meta(&self, name: &str) -> Option<SchemaRef>;
    /// Getter for a UDF description
    fn get_function_meta(&self, name: &str) -> Option<Arc<ScalarUDF>>;
    /// Getter for a UDAF description
    fn get_aggregate_meta(&self, name: &str) -> Option<Arc<AggregateUDF>>;
}

/// SQL query planner
pub struct SqlToRel<'a, S: SchemaProvider> {
    schema_provider: &'a S,
}

impl<'a, S: SchemaProvider> SqlToRel<'a, S> {
    /// Create a new query planner
    pub fn new(schema_provider: &'a S) -> Self {
        SqlToRel { schema_provider }
    }

    /// Generate a logical plan from an DataFusion SQL statement
    pub fn statement_to_plan(&self, statement: &DFStatement) -> Result<LogicalPlan> {
        match statement {
            DFStatement::CreateExternalTable(s) => self.external_table_to_plan(&s),
            DFStatement::Statement(s) => self.sql_statement_to_plan(&s),
            DFStatement::Explain(s) => self.explain_statement_to_plan(&(*s)),
        }
    }

    /// Generate a logical plan from an SQL statement
    pub fn sql_statement_to_plan(&self, sql: &Statement) -> Result<LogicalPlan> {
        match sql {
            Statement::Query(query) => self.query_to_plan(&query),
            _ => Err(DataFusionError::NotImplemented(
                "Only SELECT statements are implemented".to_string(),
            )),
        }
    }

    fn query_to_plan(&self, query: &Query) -> Result<LogicalPlan> {
        self.query_to_plan_with_alias(query, &None)
    }

    /// Generate a logic plan from an SQL query
    pub fn query_to_plan_with_alias(
        &self,
        query: &Query,
        alias: &Option<String>,
    ) -> Result<LogicalPlan> {
        let set_expr = &query.body;
        let plan = self.set_expr_to_plan(set_expr, alias)?;

        let plan = self.order_by(&plan, &query.order_by)?;

        self.limit(&plan, &query.limit)
    }

    fn set_expr_to_plan(
        &self,
        set_expr: &SetExpr,
        alias: &Option<String>,
    ) -> Result<LogicalPlan> {
        match set_expr {
            SetExpr::Select(s) => self.select_to_plan(s.as_ref()),
            SetExpr::SetOperation {
                op,
                left,
                right,
                all,
            } => match (op, all) {
                (SetOperator::Union, true) => {
                    let left_plan = self.set_expr_to_plan(left.as_ref(), &None)?;
                    let right_plan = self.set_expr_to_plan(right.as_ref(), &None)?;
                    let inputs = vec![left_plan, right_plan]
                        .into_iter()
                        .flat_map(|p| match p {
                            LogicalPlan::Union { inputs, .. } => inputs.clone(),
                            x => vec![Arc::new(x)],
                        })
                        .collect::<Vec<_>>();
                    if inputs.len() == 0 {
                        return Err(ExecutionError::ExecutionError(format!(
                            "Empty UNION: {}",
                            set_expr
                        )));
                    }
                    if !inputs.iter().all(|s| s.schema() == inputs[0].schema()) {
                        return Err(ExecutionError::ExecutionError(format!(
                            "UNION ALL schema expected to be the same across selects"
                        )));
                    }
                    Ok(LogicalPlan::Union {
                        schema: inputs[0].schema().clone(),
                        inputs,
                        alias: alias.clone(),
                    })
                }
                _ => Err(ExecutionError::NotImplemented(
                    format!("Only UNION ALL is supported: {}", set_expr).to_owned(),
                )),
            },
            _ => Err(DataFusionError::NotImplemented(
                format!("Query {} not implemented yet", set_expr).to_owned(),
            )),
        }
    }

    /// Generate a logical plan from a CREATE EXTERNAL TABLE statement
    pub fn external_table_to_plan(
        &self,
        statement: &CreateExternalTable,
    ) -> Result<LogicalPlan> {
        let CreateExternalTable {
            name,
            columns,
            file_type,
            has_header,
            location,
            ..
        } = statement;

        // semantic checks
        match *file_type {
            FileType::CSV => {
                if columns.is_empty() {
                    return Err(DataFusionError::Plan(
                        "Column definitions required for CSV files. None found".into(),
                    ));
                }
            }
            FileType::Parquet => {
                if !columns.is_empty() {
                    return Err(DataFusionError::Plan(
                        "Column definitions can not be specified for PARQUET files."
                            .into(),
                    ));
                }
            }
            FileType::NdJson => {}
        };

        let schema = SchemaRef::new(self.build_schema(&columns)?);

        Ok(LogicalPlan::CreateExternalTable {
            schema,
            name: name.to_string(),
            location: location.clone(),
            file_type: file_type.clone(),
            has_header: has_header.clone(),
        })
    }

    /// Generate a plan for EXPLAIN ... that will print out a plan
    ///
    pub fn explain_statement_to_plan(
        &self,
        explain_plan: &ExplainPlan,
    ) -> Result<LogicalPlan> {
        let verbose = explain_plan.verbose;
        let plan = self.statement_to_plan(&explain_plan.statement)?;

        let stringified_plans = vec![StringifiedPlan::new(
            PlanType::LogicalPlan,
            format!("{:#?}", plan),
        )];

        let schema = LogicalPlan::explain_schema();
        let plan = Arc::new(plan);

        Ok(LogicalPlan::Explain {
            verbose,
            plan,
            stringified_plans,
            schema,
        })
    }

    fn build_schema(&self, columns: &Vec<SQLColumnDef>) -> Result<Schema> {
        let mut fields = Vec::new();

        for column in columns {
            let data_type = self.make_data_type(&column.data_type)?;
            let allow_null = column
                .options
                .iter()
                .any(|x| x.option == ColumnOption::Null);
            fields.push(Field::new(&column.name.value, data_type, allow_null));
        }

        Ok(Schema::new(fields))
    }

    /// Maps the SQL type to the corresponding Arrow `DataType`
    fn make_data_type(&self, sql_type: &SQLDataType) -> Result<DataType> {
        match sql_type {
            SQLDataType::BigInt => Ok(DataType::Int64),
            SQLDataType::Int => Ok(DataType::Int32),
            SQLDataType::SmallInt => Ok(DataType::Int16),
            SQLDataType::Char(_) | SQLDataType::Varchar(_) | SQLDataType::Text => {
                Ok(DataType::Utf8)
            }
            SQLDataType::Decimal(_, _) => Ok(DataType::Float64),
            SQLDataType::Float(_) => Ok(DataType::Float32),
            SQLDataType::Real | SQLDataType::Double => Ok(DataType::Float64),
            SQLDataType::Boolean => Ok(DataType::Boolean),
            SQLDataType::Date => Ok(DataType::Date64(DateUnit::Day)),
            SQLDataType::Time => Ok(DataType::Time64(TimeUnit::Millisecond)),
            SQLDataType::Timestamp => Ok(DataType::Date64(DateUnit::Millisecond)),
            _ => Err(DataFusionError::NotImplemented(format!(
                "The SQL data type {:?} is not implemented",
                sql_type
            ))),
        }
    }

    fn from_join_to_plan(&self, from: &Vec<TableWithJoins>) -> Result<LogicalPlan> {
        if from.len() == 0 {
            return Ok(LogicalPlanBuilder::empty().build()?);
        }
        if from.len() != 1 {
            return Err(DataFusionError::NotImplemented(
                "FROM with multiple tables is still not implemented".to_string(),
            ));
        };
        let relation = &from[0].relation;
        match relation {
            TableFactor::Table { name, alias, .. } => {
                let name = name.to_string();
                match self.schema_provider.get_table_meta(&name) {
                    Some(schema) => Ok(LogicalPlanBuilder::scan(
                        "default",
                        &name,
                        schema.as_ref(),
                        None,
                        alias.as_ref().map(|i| i.name.value.to_string()),
                    )?
                    .build()?),
                    None => Err(DataFusionError::Plan(format!(
                        "no schema found for table {}",
                        name
                    ))),
                }
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => self.query_to_plan_with_alias(
                &subquery,
                &alias.as_ref().map(|a| a.name.value.to_string()),
            ),
            _ => Err(DataFusionError::NotImplemented(
                "Subqueries are still not supported".to_string(),
            )),
        }
    }

    /// Generate a logic plan from an SQL select
    fn select_to_plan(&self, select: &Select) -> Result<LogicalPlan> {
        if select.having.is_some() {
            return Err(DataFusionError::NotImplemented(
                "HAVING is not implemented yet".to_string(),
            ));
        }

        let plan = self.from_join_to_plan(&select.from)?;

        // filter (also known as selection) first
        let plan = self.filter(&plan, &select.selection)?;

        let projection_expr: Vec<Expr> = select
            .projection
            .iter()
            .map(|e| self.sql_select_to_rex(&e, &plan.schema(), &plan.aliased_schema()))
            .collect::<Result<Vec<Expr>>>()?;

        let aggr_expr: Vec<Expr> = projection_expr
            .iter()
            .filter(|e| is_aggregate_expr(e))
            .flat_map(|e| collect_aggregate_expr(e, vec![]))
            .map(|e| -> Result<(String, Expr)> { Ok((e.name(plan.schema())?, e)) })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .unique_by(|(name, _)| name.to_string())
            .map(|(_, e)| e)
            .collect();

        // apply projection or aggregate
        let plan = if (select.group_by.len() > 0) | (aggr_expr.len() > 0) {
            self.aggregate(&plan, projection_expr, &select.group_by, aggr_expr)?
        } else {
            self.project(&plan, projection_expr)?
        };
        Ok(plan)
    }

    /// Apply a filter to the plan
    fn filter(
        &self,
        plan: &LogicalPlan,
        predicate: &Option<SQLExpr>,
    ) -> Result<LogicalPlan> {
        match *predicate {
            Some(ref predicate_expr) => LogicalPlanBuilder::from(&plan)
                .filter(self.sql_to_rex(
                    predicate_expr,
                    &plan.schema(),
                    &plan.aliased_schema(),
                )?)?
                .build(),
            _ => Ok(plan.clone()),
        }
    }

    /// Wrap a plan in a projection
    fn project(&self, input: &LogicalPlan, expr: Vec<Expr>) -> Result<LogicalPlan> {
        LogicalPlanBuilder::from(input).project(expr)?.build()
    }

    /// Wrap a plan in an aggregate
    fn aggregate(
        &self,
        input: &LogicalPlan,
        projection_expr: Vec<Expr>,
        group_by: &Vec<SQLExpr>,
        aggr_expr: Vec<Expr>,
    ) -> Result<LogicalPlan> {
        let group_expr: Vec<Expr> = group_by
            .iter()
            .map(|e| {
                match e {
                    SQLExpr::Value(Value::Number(n)) => match n.parse::<usize>() {
                        Ok(n) => {
                            if n - 1 < projection_expr.len() && n >= 1 {
                                if is_aggregate_expr(&projection_expr[n - 1]) {
                                    Err(ExecutionError::General(format!("Can't group by aggregate function: {:?}", projection_expr[n - 1])))
                                } else {
                                    Ok(projection_expr[n - 1].clone())
                                }
                            } else {
                                Err(ExecutionError::General(format!("Select column reference should be within 1..{} but found {}", projection_expr.len(), n)))
                            }
                        },
                        Err(_) => Err(ExecutionError::General(format!("Can't parse {} as number", n))),
                    }
                    _ => self.sql_to_rex(&e, &input.schema(), &input.aliased_schema())
                }
            })
            .collect::<Result<Vec<Expr>>>()?;

        let non_aggr_projection = projection_expr
            .iter()
            .filter(|e| !is_aggregate_expr(e))
            .collect::<Vec<_>>();
        let mut group_expr_names = group_expr
            .iter()
            .map(|e| e.name(input.schema()))
            .collect::<Result<Vec<_>>>()?;
        group_expr_names.sort();
        let mut non_aggr_projection_names = non_aggr_projection
            .iter()
            .map(|e| e.name(input.schema()))
            .collect::<Result<Vec<_>>>()?;
        non_aggr_projection_names.sort();

        if group_expr_names != non_aggr_projection_names {
            return Err(DataFusionError::Plan(
                "Projection references non-aggregate values".to_owned(),
            ));
        }

        let plan = LogicalPlanBuilder::from(&input)
            .aggregate(group_expr, aggr_expr)?
            .build()?;

        // optionally wrap in projection to preserve final order of fields
        let columns = plan
            .schema()
            .fields()
            .iter()
            .map(|f| Expr::Column(f.name().clone()))
            .collect::<Vec<_>>();
        let expected_columns = projection_expr
            .iter()
            .map(|e| {
                replace_aggregate_expr_in_projection(
                    e,
                    input.schema(),
                    &plan
                        .schema()
                        .fields()
                        .iter()
                        .map(|f| f.name().clone())
                        .collect::<HashSet<_>>(),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        if expected_columns
            .iter()
            .map(|c| c.name(input.schema()))
            .collect::<Result<Vec<_>>>()?
            != columns
                .iter()
                .map(|c| c.name(input.schema()))
                .collect::<Result<Vec<_>>>()?
        {
            self.project(&plan, expected_columns)
        } else {
            Ok(plan)
        }
    }

    /// Wrap a plan in a limit
    fn limit(&self, input: &LogicalPlan, limit: &Option<SQLExpr>) -> Result<LogicalPlan> {
        match *limit {
            Some(ref limit_expr) => {
                let n = match self.sql_to_rex(
                    &limit_expr,
                    &input.schema(),
                    &input.aliased_schema(),
                )? {
                    Expr::Literal(ScalarValue::Int64(Some(n))) => Ok(n as usize),
                    _ => Err(DataFusionError::Plan(
                        "Unexpected expression for LIMIT clause".to_string(),
                    )),
                }?;

                LogicalPlanBuilder::from(&input).limit(n)?.build()
            }
            _ => Ok(input.clone()),
        }
    }

    /// Wrap the logical in a sort
    fn order_by(
        &self,
        plan: &LogicalPlan,
        order_by: &Vec<OrderByExpr>,
    ) -> Result<LogicalPlan> {
        if order_by.len() == 0 {
            return Ok(plan.clone());
        }

        let input_schema = plan.schema();
        let order_by_rex: Result<Vec<Expr>> = order_by
            .iter()
            .map(|e| {
                Ok(Expr::Sort {
                    expr: Box::new(
                        self.sql_to_rex(&e.expr, &input_schema, &plan.aliased_schema())
                            .unwrap(),
                    ),
                    // by default asc
                    asc: e.asc.unwrap_or(true),
                    // by default nulls first to be consistent with spark
                    nulls_first: e.nulls_first.unwrap_or(true),
                })
            })
            .collect();

        LogicalPlanBuilder::from(&plan).sort(order_by_rex?)?.build()
    }

    /// Generate a relational expression from a select SQL expression
    fn sql_select_to_rex(
        &self,
        sql: &SelectItem,
        schema: &Schema,
        aliased_schema: &HashMap<String, SchemaRef>,
    ) -> Result<Expr> {
        match sql {
            SelectItem::UnnamedExpr(expr) => {
                self.sql_to_rex(expr, schema, aliased_schema)
            }
            SelectItem::ExprWithAlias { expr, alias } => Ok(Alias(
                Box::new(self.sql_to_rex(&expr, schema, aliased_schema)?),
                alias.value.clone(),
            )),
            SelectItem::Wildcard => Ok(Expr::Wildcard),
            SelectItem::QualifiedWildcard(_) => Err(DataFusionError::NotImplemented(
                "Qualified wildcards are not supported".to_string(),
            )),
        }
    }

    /// Generate a relational expression from a SQL expression
    pub fn sql_to_rex(
        &self,
        sql: &SQLExpr,
        schema: &Schema,
        aliased_schema: &HashMap<String, SchemaRef>,
    ) -> Result<Expr> {
        match sql {
            SQLExpr::Value(Value::Number(n)) => match n.parse::<i64>() {
                Ok(n) => Ok(lit(n)),
                Err(_) => Ok(lit(n.parse::<f64>().unwrap())),
            },
            SQLExpr::Value(Value::SingleQuotedString(ref s)) => Ok(lit(s.clone())),

            SQLExpr::Identifier(ref id) => {
                if &id.value[0..1] == "@" {
                    let var_names = vec![id.value.clone()];
                    Ok(Expr::ScalarVariable(var_names))
                } else {
                    match schema.field_with_name(&id.value) {
                        Ok(field) => Ok(Expr::Column(field.name().clone())),
                        Err(_) => Err(DataFusionError::Plan(format!(
                            "Invalid identifier '{}' for schema {}",
                            id,
                            schema.to_string()
                        ))),
                    }
                }
            }

            SQLExpr::CompoundIdentifier(ids) => {
                let mut var_names = vec![];
                for i in 0..ids.len() {
                    let id = ids[i].clone();
                    var_names.push(id.value);
                }
                if &var_names[0][0..1] == "@" {
                    Ok(Expr::ScalarVariable(var_names))
                } else if aliased_schema.contains_key(&var_names[0]) {
                    match schema.field_with_name(&var_names[1]) {
                        Ok(field) => Ok(Expr::Column(field.name().clone())),
                        Err(_) => Err(ExecutionError::ExecutionError(format!(
                            "Invalid identifier '{}' for schema {}",
                            &var_names[1],
                            schema.to_string()
                        ))),
                    }
                } else {
                    Err(DataFusionError::Plan(format!(
                        "Invalid compound identifier '{:?}'. Alias not found among: {:?}",
                        var_names,
                        aliased_schema.keys().collect::<Vec<_>>()
                    )))
                }
            }

            SQLExpr::Wildcard => Ok(Expr::Wildcard),

            SQLExpr::Cast {
                ref expr,
                ref data_type,
            } => Ok(Expr::Cast {
                expr: Box::new(self.sql_to_rex(&expr, schema, aliased_schema)?),
                data_type: convert_data_type(data_type)?,
            }),

            SQLExpr::IsNull(ref expr) => Ok(Expr::IsNull(Box::new(self.sql_to_rex(
                expr,
                schema,
                aliased_schema,
            )?))),

            SQLExpr::IsNotNull(ref expr) => Ok(Expr::IsNotNull(Box::new(
                self.sql_to_rex(expr, schema, aliased_schema)?,
            ))),

            SQLExpr::UnaryOp { ref op, ref expr } => match *op {
                UnaryOperator::Not => Ok(Expr::Not(Box::new(self.sql_to_rex(
                    expr,
                    schema,
                    aliased_schema,
                )?))),
                _ => Err(DataFusionError::Internal(format!(
                    "SQL binary operator cannot be interpreted as a unary operator"
                ))),
            },

            SQLExpr::BinaryOp {
                ref left,
                ref op,
                ref right,
            } => {
                let operator = match *op {
                    BinaryOperator::Gt => Ok(Operator::Gt),
                    BinaryOperator::GtEq => Ok(Operator::GtEq),
                    BinaryOperator::Lt => Ok(Operator::Lt),
                    BinaryOperator::LtEq => Ok(Operator::LtEq),
                    BinaryOperator::Eq => Ok(Operator::Eq),
                    BinaryOperator::NotEq => Ok(Operator::NotEq),
                    BinaryOperator::Plus => Ok(Operator::Plus),
                    BinaryOperator::Minus => Ok(Operator::Minus),
                    BinaryOperator::Multiply => Ok(Operator::Multiply),
                    BinaryOperator::Divide => Ok(Operator::Divide),
                    BinaryOperator::Modulus => Ok(Operator::Modulus),
                    BinaryOperator::And => Ok(Operator::And),
                    BinaryOperator::Or => Ok(Operator::Or),
                    BinaryOperator::Like => Ok(Operator::Like),
                    BinaryOperator::NotLike => Ok(Operator::NotLike),
                    _ => Err(DataFusionError::NotImplemented(format!(
                        "Unsupported SQL binary operator {:?}",
                        op
                    ))),
                }?;

                Ok(Expr::BinaryExpr {
                    left: Box::new(self.sql_to_rex(&left, &schema, aliased_schema)?),
                    op: operator,
                    right: Box::new(self.sql_to_rex(&right, &schema, aliased_schema)?),
                })
            }

            SQLExpr::Function(function) => {
                // TODO parser should do lowercase?
                let name: String = function.name.to_string().to_lowercase();

                // first, scalar built-in
                if let Ok(fun) = functions::BuiltinScalarFunction::from_str(&name) {
                    let args = function
                        .args
                        .iter()
                        .map(|a| self.sql_to_rex(a, schema, aliased_schema))
                        .collect::<Result<Vec<Expr>>>()?;

                    return Ok(Expr::ScalarFunction { fun, args });
                };

                if name.to_lowercase() == "nullif" {
                    if let Ok(if_fn) = functions::BuiltinScalarFunction::from_str("if") {
                        if function.args.len() != 2 {
                            return Err(ExecutionError::General(format!(
                                "nullif expects 2 arguments but found: {:?}",
                                function.args
                            )));
                        }
                        return Ok(Expr::ScalarFunction {
                            fun: if_fn,
                            args: vec![
                                Expr::BinaryExpr {
                                    left: Box::new(self.sql_to_rex(
                                        &function.args[0],
                                        &schema,
                                        aliased_schema,
                                    )?),
                                    op: Operator::NotEq,
                                    right: Box::new(self.sql_to_rex(
                                        &function.args[1],
                                        &schema,
                                        aliased_schema,
                                    )?),
                                },
                                self.sql_to_rex(
                                    &function.args[0],
                                    &schema,
                                    aliased_schema,
                                )?,
                            ],
                        });
                    }
                }

                // next, aggregate built-ins
                if let Ok(fun) = aggregates::AggregateFunction::from_str(&name) {
                    let args = if fun == aggregates::AggregateFunction::Count {
                        function
                            .args
                            .iter()
                            .map(|a| match a {
                                SQLExpr::Value(Value::Number(_)) => Ok(lit(1_u8)),
                                SQLExpr::Wildcard => Ok(lit(1_u8)),
                                _ => self.sql_to_rex(a, schema, aliased_schema),
                            })
                            .collect::<Result<Vec<Expr>>>()?
                    } else {
                        function
                            .args
                            .iter()
                            .map(|a| self.sql_to_rex(a, schema, aliased_schema))
                            .collect::<Result<Vec<Expr>>>()?
                    };

                    return Ok(Expr::AggregateFunction {
                        fun,
                        distinct: function.distinct,
                        args,
                    });
                };

                // finally, user-defined functions (UDF) and UDAF
                match self.schema_provider.get_function_meta(&name).or_else(|| {
                    self.schema_provider.get_function_meta(&name.to_uppercase())
                }) {
                    Some(fm) => {
                        let args = function
                            .args
                            .iter()
                            .map(|a| self.sql_to_rex(a, schema, aliased_schema))
                            .collect::<Result<Vec<Expr>>>()?;

                        Ok(Expr::ScalarUDF {
                            fun: fm.clone(),
                            args,
                        })
                    }
                    None => match self.schema_provider.get_aggregate_meta(&name).or_else(
                        || {
                            self.schema_provider
                                .get_aggregate_meta(&name.to_uppercase())
                        },
                    ) {
                        Some(fm) => {
                            let args = function
                                .args
                                .iter()
                                .map(|a| self.sql_to_rex(a, schema, aliased_schema))
                                .collect::<Result<Vec<Expr>>>()?;

                            Ok(Expr::AggregateUDF {
                                fun: fm.clone(),
                                args,
                            })
                        }
                        _ => Err(DataFusionError::Plan(format!(
                            "Invalid function '{}'",
                            name
                        ))),
                    },
                }
            }

            SQLExpr::Nested(e) => self.sql_to_rex(&e, &schema, aliased_schema),

            _ => Err(DataFusionError::NotImplemented(format!(
                "Unsupported ast node {:?} in sqltorel",
                sql
            ))),
        }
    }
}

/// Determine if an expression is an aggregate expression or not
fn is_aggregate_expr(e: &Expr) -> bool {
    match e {
        Expr::AggregateFunction { .. } | Expr::AggregateUDF { .. } => true,
        Expr::Alias(expr, _) => is_aggregate_expr(expr),
        Expr::BinaryExpr { left, right, .. } => {
            is_aggregate_expr(left) || is_aggregate_expr(right)
        }
        Expr::ScalarFunction { args, .. } => args.iter().any(|e| is_aggregate_expr(e)),
        _ => false,
    }
}

/// Collect Aggregate expressions hierarchically
fn collect_aggregate_expr(e: &Expr, result: Vec<Expr>) -> Vec<Expr> {
    let mut next_result = result;
    match e {
        Expr::AggregateFunction { .. } | Expr::AggregateUDF { .. } => {
            next_result.push(e.clone());
        }
        Expr::Alias(expr, _) => next_result = collect_aggregate_expr(expr, next_result),
        Expr::BinaryExpr { left, right, .. } => {
            next_result = collect_aggregate_expr(left, next_result);
            next_result = collect_aggregate_expr(right, next_result);
        }
        Expr::ScalarFunction { args, .. } => {
            for arg in args.iter() {
                next_result = collect_aggregate_expr(arg, next_result);
            }
        }
        _ => (),
    };
    next_result
}

fn replace_aggregate_expr_in_projection(
    expr: &Expr,
    input_schema: &Schema,
    aggregate_expr: &HashSet<String>,
) -> Result<Expr> {
    let name = expr.name(input_schema)?;
    if aggregate_expr.contains(&name) {
        return Ok(Expr::Column(name));
    }
    match expr {
        Expr::Alias(expr, alias) => Ok(Expr::Alias(
            Box::new(replace_aggregate_expr_in_projection(
                expr,
                input_schema,
                aggregate_expr,
            )?),
            alias.to_string(),
        )),
        Expr::BinaryExpr { left, right, op } => Ok(Expr::BinaryExpr {
            left: Box::new(replace_aggregate_expr_in_projection(
                left,
                input_schema,
                aggregate_expr,
            )?),
            right: Box::new(replace_aggregate_expr_in_projection(
                right,
                input_schema,
                aggregate_expr,
            )?),
            op: op.clone(),
        }),
        Expr::ScalarFunction { args, fun } => Ok(Expr::ScalarFunction {
            fun: fun.clone(),
            args: args
                .iter()
                .map(|e| {
                    replace_aggregate_expr_in_projection(e, input_schema, aggregate_expr)
                })
                .collect::<Result<Vec<_>>>()?,
        }),
        x => Ok(x.clone()),
    }
}

/// Convert SQL data type to relational representation of data type
pub fn convert_data_type(sql: &SQLDataType) -> Result<DataType> {
    match sql {
        SQLDataType::Boolean => Ok(DataType::Boolean),
        SQLDataType::SmallInt => Ok(DataType::Int16),
        SQLDataType::Int => Ok(DataType::Int32),
        SQLDataType::BigInt => Ok(DataType::Int64),
        SQLDataType::Float(_) | SQLDataType::Real => Ok(DataType::Float64),
        SQLDataType::Double => Ok(DataType::Float64),
        SQLDataType::Char(_) | SQLDataType::Varchar(_) => Ok(DataType::Utf8),
        SQLDataType::Timestamp => Ok(DataType::Timestamp(TimeUnit::Nanosecond, None)),
        other => Err(DataFusionError::NotImplemented(format!(
            "Unsupported SQL type {:?}",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{logical_plan::create_udf, sql::parser::DFParser};
    use functions::ScalarFunctionImplementation;

    #[test]
    fn select_no_relation() {
        quick_test(
            "SELECT 1",
            "Projection: Int64(1)\
             \n  EmptyRelation",
        );
    }

    #[test]
    fn select_scalar_func_with_literal_no_relation() {
        quick_test(
            "SELECT sqrt(9)",
            "Projection: sqrt(Int64(9))\
             \n  EmptyRelation",
        );
    }

    #[test]
    fn select_simple_filter() {
        let sql = "SELECT id, first_name, last_name \
                   FROM person WHERE state = 'CO'";
        let expected = "Projection: #id, #first_name, #last_name\
                        \n  Filter: #state Eq Utf8(\"CO\")\
                        \n    TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_neg_filter() {
        let sql = "SELECT id, first_name, last_name \
                   FROM person WHERE NOT state";
        let expected = "Projection: #id, #first_name, #last_name\
                        \n  Filter: NOT #state\
                        \n    TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_compound_filter() {
        let sql = "SELECT id, first_name, last_name \
                   FROM person WHERE state = 'CO' AND age >= 21 AND age <= 65";
        let expected = "Projection: #id, #first_name, #last_name\
            \n  Filter: #state Eq Utf8(\"CO\") And #age GtEq Int64(21) And #age LtEq Int64(65)\
            \n    TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn test_timestamp_filter() {
        let sql = "SELECT state FROM person WHERE birth_date < CAST (158412331400600000 as timestamp)";

        let expected = "Projection: #state\
            \n  Filter: #birth_date Lt CAST(Int64(158412331400600000) AS Timestamp(Nanosecond, None))\
            \n    TableScan: person projection=None";

        quick_test(sql, expected);
    }

    #[test]
    fn select_all_boolean_operators() {
        let sql = "SELECT age, first_name, last_name \
                   FROM person \
                   WHERE age = 21 \
                   AND age != 21 \
                   AND age > 21 \
                   AND age >= 21 \
                   AND age < 65 \
                   AND age <= 65";
        let expected = "Projection: #age, #first_name, #last_name\
                        \n  Filter: #age Eq Int64(21) \
                        And #age NotEq Int64(21) \
                        And #age Gt Int64(21) \
                        And #age GtEq Int64(21) \
                        And #age Lt Int64(65) \
                        And #age LtEq Int64(65)\
                        \n    TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_binary_expr() {
        let sql = "SELECT age + salary from person";
        let expected = "Projection: #age Plus #salary\
                        \n  TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_binary_expr_nested() {
        let sql = "SELECT (age + salary)/2 from person";
        let expected = "Projection: #age Plus #salary Divide Int64(2)\
                        \n  TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_simple_aggregate() {
        quick_test(
            "SELECT MIN(age) FROM person",
            "Aggregate: groupBy=[[]], aggr=[[MIN(#age)]]\
             \n  TableScan: person projection=None",
        );
    }

    #[test]
    fn test_sum_aggregate() {
        quick_test(
            "SELECT SUM(age) from person",
            "Aggregate: groupBy=[[]], aggr=[[SUM(#age)]]\
             \n  TableScan: person projection=None",
        );
    }

    #[test]
    fn select_simple_aggregate_with_groupby() {
        quick_test(
            "SELECT state, MIN(age), MAX(age) FROM person GROUP BY state",
            "Aggregate: groupBy=[[#state]], aggr=[[MIN(#age), MAX(#age)]]\
             \n  TableScan: person projection=None",
        );
    }

    #[test]
    fn test_wildcard() {
        quick_test(
            "SELECT * from person",
            "Projection: #id, #first_name, #last_name, #age, #state, #salary, #birth_date\
            \n  TableScan: person projection=None",
        );
    }

    #[test]
    fn select_count_one() {
        let sql = "SELECT COUNT(1) FROM person";
        let expected = "Aggregate: groupBy=[[]], aggr=[[COUNT(UInt8(1))]]\
                        \n  TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_count_column() {
        let sql = "SELECT COUNT(id) FROM person";
        let expected = "Aggregate: groupBy=[[]], aggr=[[COUNT(#id)]]\
                        \n  TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_scalar_func() {
        let sql = "SELECT sqrt(age) FROM person";
        let expected = "Projection: sqrt(#age)\
                        \n  TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_aliased_scalar_func() {
        let sql = "SELECT sqrt(age) AS square_people FROM person";
        let expected = "Projection: sqrt(#age) AS square_people\
                        \n  TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_order_by() {
        let sql = "SELECT id FROM person ORDER BY id";
        let expected = "Sort: #id ASC NULLS FIRST\
                        \n  Projection: #id\
                        \n    TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_order_by_desc() {
        let sql = "SELECT id FROM person ORDER BY id DESC";
        let expected = "Sort: #id DESC NULLS FIRST\
                        \n  Projection: #id\
                        \n    TableScan: person projection=None";
        quick_test(sql, expected);
    }

    #[test]
    fn select_order_by_nulls_last() {
        quick_test(
            "SELECT id FROM person ORDER BY id DESC NULLS LAST",
            "Sort: #id DESC NULLS LAST\
            \n  Projection: #id\
            \n    TableScan: person projection=None",
        );

        quick_test(
            "SELECT id FROM person ORDER BY id NULLS LAST",
            "Sort: #id ASC NULLS LAST\
            \n  Projection: #id\
            \n    TableScan: person projection=None",
        );
    }

    #[test]
    fn select_group_by() {
        let sql = "SELECT state FROM person GROUP BY state";
        let expected = "Aggregate: groupBy=[[#state]], aggr=[[]]\
                        \n  TableScan: person projection=None";

        quick_test(sql, expected);
    }

    #[test]
    fn select_group_by_needs_projection() {
        let sql = "SELECT COUNT(state), state FROM person GROUP BY state";
        let expected = "\
        Projection: #COUNT(state), #state\
        \n  Aggregate: groupBy=[[#state]], aggr=[[COUNT(#state)]]\
        \n    TableScan: person projection=None";

        quick_test(sql, expected);
    }

    #[test]
    fn select_group_by_needs_projection_with_replace() {
        let sql = "SELECT SUM(salary) / NULLIF(COUNT(state), 0), state FROM person GROUP BY state";
        let expected = "\
        Projection: #SUM(salary) Divide if(#COUNT(state) NotEq Int64(0), #COUNT(state)), #state\
        \n  Aggregate: groupBy=[[#state]], aggr=[[SUM(#salary), COUNT(#state)]]\
        \n    TableScan: person projection=None";

        quick_test(sql, expected);
    }

    #[test]
    fn select_7480_1() {
        let sql = "SELECT c1, MIN(c12) FROM aggregate_test_100 GROUP BY c1, c13";
        let err = logical_plan(sql).expect_err("query should have failed");
        assert_eq!(
            "Plan(\"Projection references non-aggregate values\")",
            format!("{:?}", err)
        );
    }

    #[test]
    fn select_7480_2() {
        let sql = "SELECT c1, c13, MIN(c12) FROM aggregate_test_100 GROUP BY c1";
        let err = logical_plan(sql).expect_err("query should have failed");
        assert_eq!(
            "Plan(\"Projection references non-aggregate values\")",
            format!("{:?}", err)
        );
    }

    #[test]
    fn create_external_table_csv() {
        let sql = "CREATE EXTERNAL TABLE t(c1 int) STORED AS CSV LOCATION 'foo.csv'";
        let expected = "CreateExternalTable: \"t\"";
        quick_test(sql, expected);
    }

    #[test]
    fn create_external_table_csv_no_schema() {
        let sql = "CREATE EXTERNAL TABLE t STORED AS CSV LOCATION 'foo.csv'";
        let err = logical_plan(sql).expect_err("query should have failed");
        assert_eq!(
            "Plan(\"Column definitions required for CSV files. None found\")",
            format!("{:?}", err)
        );
    }

    #[test]
    fn create_external_table_parquet() {
        let sql =
            "CREATE EXTERNAL TABLE t(c1 int) STORED AS PARQUET LOCATION 'foo.parquet'";
        let err = logical_plan(sql).expect_err("query should have failed");
        assert_eq!(
            "Plan(\"Column definitions can not be specified for PARQUET files.\")",
            format!("{:?}", err)
        );
    }

    #[test]
    fn create_external_table_parquet_no_schema() {
        let sql = "CREATE EXTERNAL TABLE t STORED AS PARQUET LOCATION 'foo.parquet'";
        let expected = "CreateExternalTable: \"t\"";
        quick_test(sql, expected);
    }

    fn logical_plan(sql: &str) -> Result<LogicalPlan> {
        let planner = SqlToRel::new(&MockSchemaProvider {});
        let ast = DFParser::parse_sql(&sql).unwrap();
        planner.statement_to_plan(&ast[0])
    }

    /// Create logical plan, write with formatter, compare to expected output
    fn quick_test(sql: &str, expected: &str) {
        let plan = logical_plan(sql).unwrap();
        assert_eq!(expected, format!("{:?}", plan));
    }

    struct MockSchemaProvider {}

    impl SchemaProvider for MockSchemaProvider {
        fn get_table_meta(&self, name: &str) -> Option<SchemaRef> {
            match name {
                "person" => Some(Arc::new(Schema::new(vec![
                    Field::new("id", DataType::UInt32, false),
                    Field::new("first_name", DataType::Utf8, false),
                    Field::new("last_name", DataType::Utf8, false),
                    Field::new("age", DataType::Int32, false),
                    Field::new("state", DataType::Utf8, false),
                    Field::new("salary", DataType::Float64, false),
                    Field::new(
                        "birth_date",
                        DataType::Timestamp(TimeUnit::Nanosecond, None),
                        false,
                    ),
                ]))),
                "aggregate_test_100" => Some(Arc::new(Schema::new(vec![
                    Field::new("c1", DataType::Utf8, false),
                    Field::new("c2", DataType::UInt32, false),
                    Field::new("c3", DataType::Int8, false),
                    Field::new("c4", DataType::Int16, false),
                    Field::new("c5", DataType::Int32, false),
                    Field::new("c6", DataType::Int64, false),
                    Field::new("c7", DataType::UInt8, false),
                    Field::new("c8", DataType::UInt16, false),
                    Field::new("c9", DataType::UInt32, false),
                    Field::new("c10", DataType::UInt64, false),
                    Field::new("c11", DataType::Float32, false),
                    Field::new("c12", DataType::Float64, false),
                    Field::new("c13", DataType::Utf8, false),
                ]))),
                _ => None,
            }
        }

        fn get_function_meta(&self, name: &str) -> Option<Arc<ScalarUDF>> {
            let f: ScalarFunctionImplementation =
                Arc::new(|_| Err(DataFusionError::NotImplemented("".to_string())));
            match name {
                "my_sqrt" => Some(Arc::new(create_udf(
                    "my_sqrt",
                    vec![DataType::Float64],
                    Arc::new(DataType::Float64),
                    f,
                ))),
                _ => None,
            }
        }

        fn get_aggregate_meta(&self, _name: &str) -> Option<Arc<AggregateUDF>> {
            unimplemented!()
        }
    }
}
