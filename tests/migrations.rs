use itertools::Itertools;
use sqlparser::ast::{
    AlterColumnOperation, AlterTableOperation, ColumnOption, Ident, ObjectName, Statement, Visit,
    Visitor,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlx::{Executor, PgPool};
use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

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
                std::fs::read_to_string(file.path()).expect("cannot read migration file");

            let ast = Parser::parse_sql(&PostgreSqlDialect {}, &contents).unwrap_or_else(|_| {
                panic!("Cannot parse migration {} as SQLL", file.path().display())
            });
            let mut visitor = CheckNotNullWithoutDefault {
                error: None,
                columns_set_to_not_null: Default::default(),
                columns_set_default_value: Default::default(),
            };
            let _ = ast.visit(&mut visitor);

            if let Some(error) = visitor.compute_error() {
                panic!(
                    "Migration {} contains error: {error}",
                    file.path().display()
                );
            }
        }
    }
}

fn get_up_migrations() -> Vec<PathBuf> {
    let root = env!("CARGO_MANIFEST_DIR");
    let migrations_dir = PathBuf::from(root).join("migrations");

    let mut migrations = Vec::new();
    for entry in std::fs::read_dir(migrations_dir).unwrap() {
        let path = entry.unwrap().path();

        if path.is_file() && path.to_string_lossy().ends_with(".up.sql") {
            migrations.push(path);
        }
    }

    // Sort by timestamp to enforce migration order
    migrations.sort_by(|a, b| a.file_name().unwrap().cmp(b.file_name().unwrap()));
    migrations
}

fn get_test_data_path(migration_path: &Path) -> PathBuf {
    let filename = migration_path.file_name().unwrap().to_str().unwrap();
    // e.g. "20240517094752_create_build.up.sql" -> "20240517094752_create_build"
    let migration_name = filename.split('.').next().unwrap();

    let root = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(root)
        .join("tests/data/migrations")
        .join(format!("{}.sql", migration_name))
}

#[test]
fn check_migrations_have_sample_data() {
    let migrations = get_up_migrations();
    assert!(!migrations.is_empty());
    for migration_path in migrations {
        let test_data_path = get_test_data_path(&migration_path);

        assert!(
            test_data_path.exists(),
            "Migration {:?} does not have an associated test data file at {:?}.
            Add a test data file there that fills some test data into the database after that migration is applied.",
            migration_path,
            test_data_path
        );
    }
}

/// Apply all migrations incrementally in sequence and load their corresponding test data
/// after each migration is applied.
#[sqlx::test(migrations = false)]
async fn apply_migrations_with_test_data(pool: PgPool) -> sqlx::Result<()> {
    let migrations = get_up_migrations();

    for migration_path in migrations {
        let migration_sql = std::fs::read_to_string(&migration_path)
            .unwrap_or_else(|_| panic!("Failed to read migration file: {:?}", migration_path));

        pool.execute(&*migration_sql)
            .await
            .unwrap_or_else(|e| panic!("Failed to apply migration {:?}: {}", migration_path, e));

        let test_data_path = get_test_data_path(&migration_path);
        let test_data = std::fs::read_to_string(&test_data_path).expect(&format!(
            "Failed to read test data file: {}",
            test_data_path.display()
        ));

        pool.execute(&*test_data).await.unwrap_or_else(|e| {
            panic!(
                "Failed to apply migration test data {:?}: {}",
                test_data_path, e
            )
        });
    }

    Ok(())
}
