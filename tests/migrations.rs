use itertools::Itertools;
use sqlparser::ast::{
    AlterColumnOperation, AlterTableOperation, ColumnOption, Ident, ObjectName, Statement, Visit,
    Visitor,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::ops::ControlFlow;
use std::path::PathBuf;

struct CheckNotNullWithoutDefault {
    error: Option<String>,
    columns_set_to_not_null: HashSet<(ObjectName, Ident)>,
    columns_set_default_value: HashSet<(ObjectName, Ident)>,
}

impl Visitor for CheckNotNullWithoutDefault {
    type Break = ();

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        if let Statement::AlterTable {
            operations, name, ..
        } = statement
        {
            for op in operations {
                match op {
                    AlterTableOperation::AddColumn { column_def, .. } => {
                        let has_not_null = column_def
                            .options
                            .iter()
                            .any(|option| option.option == ColumnOption::NotNull);
                        let has_default = column_def
                            .options
                            .iter()
                            .any(|option| matches!(option.option, ColumnOption::Default(_)));
                        if has_not_null && !has_default {
                            self.error = Some(format!(
                                "Column `{name}.{}` is NOT NULL, but no DEFAULT value was configured!",
                                column_def.name
                            ));
                            return ControlFlow::Break(());
                        }
                    }
                    AlterTableOperation::AlterColumn { column_name, op } => match op {
                        AlterColumnOperation::SetNotNull => {
                            self.columns_set_to_not_null
                                .insert((name.clone(), column_name.clone()));
                        }
                        AlterColumnOperation::SetDefault { .. } => {
                            self.columns_set_default_value
                                .insert((name.clone(), column_name.clone()));
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        ControlFlow::Continue(())
    }
}

impl CheckNotNullWithoutDefault {
    fn compute_error(self) -> Option<String> {
        if let Some(error) = self.error {
            return Some(error);
        }

        let missing_default = self
            .columns_set_to_not_null
            .difference(&self.columns_set_default_value)
            .collect::<BTreeSet<_>>();
        if !missing_default.is_empty() {
            return Some(format!(
                "Column(s) {} were modified to NOT NULL, but no DEFAULT value was set for them",
                missing_default.iter().map(|v| format!("{v:?}")).join(",")
            ));
        }

        None
    }
}

/// Check that there is no migration that would add a NOT NULL column (or make an existing column
/// NOT NULL) without also providing a DEFAULT value.
#[test]
fn check_non_null_column_without_default() {
    let root = env!("CARGO_MANIFEST_DIR");
    let migrations = PathBuf::from(root).join("migrations");
    for file in std::fs::read_dir(migrations).unwrap() {
        let file = file.unwrap();
        if file.path().extension() == Some(OsStr::new("sql")) {
            let contents =
                std::fs::read_to_string(&file.path()).expect("cannot read migration file");

            let ast = Parser::parse_sql(&PostgreSqlDialect {}, &contents).expect(&format!(
                "Cannot parse migration {} as SQLL",
                file.path().display()
            ));
            let mut visitor = CheckNotNullWithoutDefault {
                error: None,
                columns_set_to_not_null: Default::default(),
                columns_set_default_value: Default::default(),
            };
            ast.visit(&mut visitor);

            if let Some(error) = visitor.compute_error() {
                panic!(
                    "Migration {} contains error: {error}",
                    file.path().display()
                );
            }
        }
    }
}
